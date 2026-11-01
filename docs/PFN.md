# PFN — personalized graph functions

gStore 1.4 lets every user write their own graph functions ("PFN",
personalized functions, 自定义算子函数), manage them through the `/api`
operations `funquery` / `funcudb` / `funreview`, and call them from SPARQL:

```sparql
SELECT (PFN.outDegree({"src":"<http://ex/a>"}) AS ?r) WHERE {}
```

Upstream compiles a C++ function body with `g++` into a shared library and
`dlopen()`s it into the server. gstore-rust keeps the management API, the
storage layout, the call syntax and the answers, but runs the functions as
**sandboxed WebAssembly**: a function body is WebAssembly *text* (WAT), the
"compile" step turns it into a validated module, and the module runs in an
interpreter (`wasmi`) that can only call the gStore host interface described
below — no file system, no network, no clock, no other library — under an
instruction budget, a memory cap, an output cap and the query's cancel flag.

The runtime is the optional cargo feature **`pfn-wasm`**:

```sh
cargo build --release --features pfn-wasm
```

Without it, creating, updating, deleting, listing and reviewing functions
still works (metadata only); compiling a function or calling one in a query
fails with an error that names the `pfn-wasm` feature.

## Managing functions (`/api`)

All three operations need only valid credentials: every user has them
(upstream grants `funquery`, `funcudb` and `funreview` to every new user).
Each user works on **their own functions only**; there is no `db_name`.

`funInfo` is upstream's `PFNInfo`, a JSON object (or a string holding one):

| field       | meaning                                                         |
|-------------|-----------------------------------------------------------------|
| `funName`   | the function's name: a letter or `_`, then letters, digits, `_` (≤ 64) |
| `funDesc`   | a description                                                    |
| `funBody`   | the function body (see [Writing a function](#writing-a-function)) |
| `funSubs`   | module fields placed before the function (helpers, globals, data) |
| `funStatus` | `"1"` not compiled, `"2"` compiled, `"3"` compilation failed     |
| `lastTime`  | `yyyy-MM-dd HH:mm:ss` of the last create / update / compile      |

Like upstream, the server **URL-decodes** `funBody` and `funSubs`
(`application/x-www-form-urlencoded` rules: `+` is a space, `%XX` a byte)
before it builds the module, because upstream clients send them encoded.
Plain WAT works unchanged as long as it contains no `+` and no `%`; otherwise
encode it (Python: `urllib.parse.quote_plus(body)`).

### `funcudb` — create, update, delete, compile

`type` is `"1"` create, `"2"` update, `"3"` delete, `"4"` compile:

```sh
curl -s localhost:9000/api -d '{"operation":"funcudb","username":"root","password":"123456",
  "type":"1","funInfo":{"funName":"outDegree","funDesc":"out-degree of a vertex",
  "funBody":"…","funSubs":"…"}}'
# {"StatusCode":0,"StatusMsg":"Function create success."}
curl -s localhost:9000/api -d '{"operation":"funcudb","username":"root","password":"123456",
  "type":"4","funInfo":{"funName":"outDegree"}}'
# {"StatusCode":0,"StatusMsg":"Function build success."}
```

* **create** fails with `1005 "Function create fail: function name X already
  exists"` for a name the user already has (ignoring case).
* **update** replaces every field with the request's — fields not sent
  become empty, as upstream — and sets the status back to `"1"`: the function
  is not callable until it is compiled again.
* **delete** removes the function, its source and its module.
* **compile** answers `"Function build success."`, or `1005` with the
  compiler's message (the WAT parser's or the validator's, with line and
  column) and status `"3"`. Without the `pfn-wasm` feature it answers
  `1005 "Function build fail: … pfn-wasm …"` and leaves the status alone.
* A request without `type` is `1003 "the value of type can not be empty!"`,
  one with neither `funName` nor `funBody` `1003 "Neither funName nor funBody
  can be empty!"`, another `type` `1003 "The type is invalid, please look up
  the api document."`.

### `funquery` — list

```sh
curl -s localhost:9000/api -d '{"operation":"funquery","username":"root","password":"123456",
  "funInfo":{"funStatus":"2"}}'
# {"StatusCode":0,"StatusMsg":"success","list":[{"funBody":"…","funDesc":"…",
#   "funName":"outDegree","funStatus":"2","funSubs":"…","lastTime":"2026-10-31 12:00:00"}]}
```

`funInfo.funName` / `funInfo.funStatus` filter (exact match). *Rust
extension*: with `pageNo` / `pageSize` (defaults 1 and 10) the list is one
page, in creation order, and the answer adds `totalSize`, `totalPage`,
`pageNo` and `pageSize`; a page past the end is empty.

### `funreview` — preview the generated module

```sh
curl -s localhost:9000/api -d '{"operation":"funreview","username":"root","password":"123456",
  "funInfo":{"funName":"f","funBody":"(i32.const 1)"}}'
# {"StatusCode":0,"StatusMsg":"success","result":"%3B%3B+gStore+PFN+function…"}
```

`result` is the complete module text the body compiles from (the prelude
below + `funSubs` + the wrapped body), URL-encoded as upstream encodes its
generated C++.

## Calling a function from SPARQL

```sparql
SELECT (PFN.name({"key":"value", "list":["a","b"]}) AS ?v) WHERE { … }
```

`PFN name({…})` (upstream's grammar) is accepted too. The argument is a JSON
object whose values are strings or arrays of strings (upstream's `jsonVars`);
a call may only be the whole expression of a SELECT projection.

* The function receives the argument **canonicalized**: compact, keys
  sorted (upstream re-serializes it with `nlohmann::json::dump()`).
* **No solutions** (`WHERE {}` or no match): the function runs once with the
  argument as written; the value is its output.
* **Solutions**: each top-level value that is a string naming a WHERE
  variable, `"src":"?x"`, is replaced by each distinct term `?x` is bound to,
  in N-Triples form (`<http://ex/a>`, `"text"`, `"1"^^<…#integer>`); the
  function runs once per distinct combination of the variables' values, in
  the byte order of the resulting argument texts; the value is the JSON
  array of the outputs, each parsed as JSON when it is JSON and kept as a
  string otherwise. A variable bound only by `BIND` (or to a computed value)
  has no usable binding, as for the graph functions. When no variable has a
  binding, nothing runs and the value is `[]`.
* The row shape is upstream's: over solutions there is one row per
  solution and the value is on the last row, whether the call is the lone
  projection or next to others (unlike a lone graph function, which gives one
  row — upstream reserves one row for it but not for PFN); with no solution
  the lone projection gives one row; with `GROUP BY` it is computed per
  group. So `AnsNum` counts the solutions, `OFFSET` keeps the value as long
  as the last row is shown, and `DISTINCT` leaves the value row and one
  empty row. An empty output gives the usual result document (no
  `{"results": …}`), as upstream.
* **Whose functions**: the caller's. The `/api` and REST endpoints run every
  statement for the authenticated user, so `PFN.f` is *that user's* `f`
  (upstream looks the module up under the query's user name); a user cannot
  call another user's functions. An embedded `Database` (or `ConcurrentDb`)
  calls the functions of the caller installed with `Database::set_pfn_caller`.

### Answers

In the `/api` `query` operation (`format` `json` or `file`) a PFN value
replaces the whole result document, as upstream's `ResultSet::to_JSON` does:

```json
{"results":{"src":"<http://ex/a>","outDegree":2},"StatusCode":0,"StatusMsg":"success",
 "AnsNum":1,"OutputLimit":-1,"ThreadId":"…","QueryTime":"3"}
```

`AnsNum`, `OutputLimit` and `ThreadId` are left out when the statement
starts with `select (pfn.` (any case), as upstream does for that form. Every
other result format, and the REST `/sparql` endpoint, return the value as a
plain literal holding the JSON text.

Errors (query errors, `1005` on `/api`): `abort function 'f' is not exist`;
`abort function 'f' not compile yet`; `run f fail: …` for a trap, an
exhausted budget, a bad pointer, an output past the bound or a function that
returned 0; with variables, `run dynamic function fail: …` around them; a
cancelled call is reported as cancelled.

## Writing a function

A function is a WebAssembly function that takes no parameters and returns an
`i32`: non-zero for success, `0` for failure (the query then fails with the
text emitted so far). It reads its arguments and the graph through host
functions and builds its result by *emitting* text — normally JSON.

`funBody` is one of:

1. **A function body** (the usual form). The server wraps it, with
   `funSubs`, into a module:

   ```wat
   (module
     ;; one import per host function, named like the function:
     (import "gstore" "vertex_count" (func $vertex_count (result i64)))
     …
     (memory (export "memory") 1)          ;; one 64 KiB page, growable
     <funSubs>                              ;; your helpers, globals, (data …)
     (func (export "<funName>") (result i32)
       <funBody>)                           ;; may start with (local …) declarations
   )
   ```

2. **A complete module**, starting with `(module`. It must export its memory
   as `"memory"` and the function under its `funName`, may import only the
   host functions below (with their exact types), and `funSubs` must be empty.
3. **A binary module**: `wasm-base64:` followed by the base64 of a `.wasm`
   file (standard or URL-safe alphabet), with the same requirements as 2.

Start functions, WASI and any other import are refused at compile time.

### The host interface (module `"gstore"`)

Conventions:

* Vertices `v` are dense ids `0 … vertex_count()-1` — the entities that take
  part in at least one entity→entity edge (triples with a literal object are
  not edges). Terms `t` are dictionary ids of entities and literals;
  predicates `p` are dictionary predicate ids. `-1` means "none" (unknown
  string, vertex out of range, no such neighbour); a predicate argument `-1`
  means "every predicate".
* Neighbours via one predicate are in ascending vertex order; via every
  predicate they are in `(predicate, neighbour)` order, so two vertices linked
  by two predicates are neighbours twice (upstream's multi-edges).
* Strings are UTF-8 in the module's memory. Into the host: `(ptr, len)`. Out
  of the host, `snprintf`-style: at most `cap` bytes are written at `buf` and
  the string's full length is returned (a result `> cap` means it was
  truncated), or `-1` when there is no such string.
* Every `(ptr, len)` / `(buf, cap)` must lie inside the memory, or the call
  traps — a clean query error.
* Term strings are N-Triples: `<http://ex/a>`, `_:b`, `"text"`,
  `"1"^^<http://www.w3.org/2001/XMLSchema#integer>`, `"chat"@fr`.

| function | type | meaning |
|----------|------|---------|
| `vertex_count` | `() -> i64` | number of vertices (upstream `getVertNum`) |
| `edge_count` | `() -> i64` | number of edges, one per predicate (`getEdgeNum`) |
| `pred_count` | `() -> i64` | number of predicates labelling an edge |
| `pred_at` | `(i: i64) -> i64` | the `i`-th of them (ascending ids); `-1` out of range |
| `out_degree` | `(v: i64, p: i64) -> i64` | out-degree of `v` via `p` (`-1`: every predicate); `-1` for a bad vertex |
| `in_degree` | `(v: i64, p: i64) -> i64` | in-degree, likewise |
| `out_neighbor` | `(v: i64, p: i64, i: i64) -> i64` | the `i`-th out-neighbour; `-1` if none |
| `in_neighbor` | `(v: i64, p: i64, i: i64) -> i64` | the `i`-th in-neighbour; `-1` if none |
| `out_edge_pred` | `(v: i64, i: i64) -> i64` | predicate of the `i`-th out-edge in every-predicate order |
| `in_edge_pred` | `(v: i64, i: i64) -> i64` | predicate of the `i`-th in-edge in every-predicate order |
| `vertex_id` | `(ptr: i32, len: i32) -> i64` | term string → vertex; `-1` if it is not a vertex |
| `vertex_term` | `(v: i64) -> i64` | vertex → term id |
| `term_vertex` | `(t: i64) -> i64` | term id → vertex; `-1` if not a vertex |
| `term_id` | `(ptr: i32, len: i32) -> i64` | term string → term id (`getIDByString`) |
| `term_str` | `(t: i64, buf: i32, cap: i32) -> i32` | term id → string (`getStringByID`) |
| `vertex_str` | `(v: i64, buf: i32, cap: i32) -> i32` | vertex → its term string |
| `pred_id` | `(ptr: i32, len: i32) -> i64` | predicate IRI `<…>` → id (`getIDByPredicate`) |
| `pred_str` | `(p: i64, buf: i32, cap: i32) -> i32` | predicate id → `<…>` (`getPredicateByID`) |
| `args` | `(buf: i32, cap: i32) -> i32` | the whole argument JSON (canonical) |
| `arg_str` | `(key: i32, klen: i32, buf: i32, cap: i32) -> i32` | a top-level string (or number / boolean, as text) argument; `-1` if absent |
| `arg_int` | `(key: i32, klen: i32, default: i64) -> i64` | an integer argument (`2` or `"2"`), else `default` |
| `arg_bool` | `(key: i32, klen: i32, default: i32) -> i32` | `1` for `true` / `"true"` / `"1"`, `0` for `false` / `"false"` / `"0"`, else `default` |
| `arg_len` | `(key: i32, klen: i32) -> i32` | length of an array argument; `-1` if absent or not an array |
| `arg_item` | `(key: i32, klen: i32, i: i32, buf: i32, cap: i32) -> i32` | the `i`-th item of an array argument; `-1` if none |
| `emit` | `(ptr: i32, len: i32)` | append UTF-8 text to the result |
| `emit_i64` | `(x: i64)` | append `x` in decimal |
| `emit_f64` | `(x: f64)` | append `x` as a JSON number (`null` if not finite) |
| `emit_json_str` | `(ptr: i32, len: i32)` | append the text as a quoted, escaped JSON string |

The result must be valid UTF-8 and at most `gfun_max_output_bytes` bytes.

## Examples

Both examples are complete: create each with `funcudb` type `"1"` using the
`funSubs` and `funBody` below, compile it with type `"4"`, and call it. The
test suite (`tests/dt_pfn.rs`) runs them from this file.

### `outDegree` — the out-degree of a vertex

Arguments: `src`, a vertex (an IRI, or `"?x"`), and optionally `pred`, a
predicate IRI (every predicate when absent). Output:
`{"src":"<…>","outDegree":N}` (0 for an unknown vertex or predicate).

```sparql
SELECT (PFN.outDegree({"src":"<http://ex/a>","pred":"<http://ex/knows>"}) AS ?r) WHERE {}
SELECT (PFN.outDegree({"src":"?x"}) AS ?r) WHERE { ?x a <http://ex/Person> }
```

```wat funSubs=outDegree
  ;; argument keys and output pieces; strings are read into 1024.. and 8192..
  (data (i32.const 0) "src")
  (data (i32.const 8) "pred")
  (data (i32.const 16) "{\"src\":")
  (data (i32.const 32) ",\"outDegree\":")
  (data (i32.const 48) "}")
```

```wat funBody=outDegree
(local $len i32) (local $plen i32) (local $v i64) (local $p i64) (local $deg i64)
;; src: the vertex; without one the call fails
(local.set $len (call $arg_str (i32.const 0) (i32.const 3) (i32.const 1024) (i32.const 4096)))
(if (i32.or (i32.lt_s (local.get $len) (i32.const 0)) (i32.gt_s (local.get $len) (i32.const 4096)))
  (then (return (i32.const 0))))
(local.set $v (call $vertex_id (i32.const 1024) (local.get $len)))
;; pred: an optional filter (-1 = every predicate)
(local.set $p (i64.const -1))
(local.set $plen (call $arg_str (i32.const 8) (i32.const 4) (i32.const 8192) (i32.const 4096)))
(if (i32.ge_s (local.get $plen) (i32.const 0))
  (then (local.set $p (call $pred_id (i32.const 8192) (local.get $plen)))))
(local.set $deg (i64.const 0))
(if (i64.ge_s (local.get $v) (i64.const 0))
  (then
    ;; a predicate that was given but is unknown has no edges
    (if (i32.or (i32.lt_s (local.get $plen) (i32.const 0)) (i64.ge_s (local.get $p) (i64.const 0)))
      (then (local.set $deg (call $out_degree (local.get $v) (local.get $p)))))))
;; {"src":"<…>","outDegree":N}
(call $emit (i32.const 16) (i32.const 7))
(call $emit_json_str (i32.const 1024) (local.get $len))
(call $emit (i32.const 32) (i32.const 13))
(call $emit_i64 (local.get $deg))
(call $emit (i32.const 48) (i32.const 1))
(i32.const 1)
```

### `kHopCount` — vertices within k hops, along some predicates

Arguments: `src`, a vertex; `k`, the most hops (default 1); `preds`,
predicate IRIs the hops may follow (every predicate when absent or `[]`).
Output: `{"src":"<…>","k":K,"count":N}`, where N is the number of vertices
other than `src` reachable from it along at most K out-edges labelled by one
of `preds`. It runs a breadth-first search with its visited flags and queue
in linear memory, growing the memory as the graph needs.

```sparql
SELECT (PFN.kHopCount({"src":"<http://ex/a>","k":"2","preds":["<http://ex/knows>"]}) AS ?r)
WHERE {}
```

```wat funSubs=kHopCount
  ;; argument keys and output pieces
  (data (i32.const 0) "src")
  (data (i32.const 8) "k")
  (data (i32.const 16) "preds")
  (data (i32.const 32) "{\"src\":")
  (data (i32.const 48) ",\"k\":")
  (data (i32.const 64) ",\"count\":")
  (data (i32.const 80) "}")
  ;; 1024..5119: src; 5120..8191: a predicate IRI; 8192..12287: the
  ;; predicate ids to follow (i64, at most 512); from 65536: the visited
  ;; flags (one byte per vertex), then the BFS queue (one i32 per vertex)
  (global $npred (mut i32) (i32.const 0))
```

```wat funBody=kHopCount
(local $len i32) (local $src i64) (local $k i64) (local $m i32) (local $i i32)
(local $plen i32) (local $p i64) (local $n i64) (local $qbase i32) (local $pages i64)
(local $head i32) (local $tail i32) (local $end i32) (local $depth i64)
(local $u i64) (local $pi i32) (local $d i64) (local $x i64) (local $w i64) (local $count i64)
;; src and k
(local.set $len (call $arg_str (i32.const 0) (i32.const 3) (i32.const 1024) (i32.const 4096)))
(if (i32.or (i32.lt_s (local.get $len) (i32.const 0)) (i32.gt_s (local.get $len) (i32.const 4096)))
  (then (return (i32.const 0))))
(local.set $src (call $vertex_id (i32.const 1024) (local.get $len)))
(local.set $k (call $arg_int (i32.const 8) (i32.const 1) (i64.const 1)))
;; preds: the ids of the listed predicates the data has
(local.set $m (call $arg_len (i32.const 16) (i32.const 5)))
(if (i32.gt_s (local.get $m) (i32.const 512)) (then (return (i32.const 0))))
(local.set $i (i32.const 0))
(block $listed
  (loop $next
    (br_if $listed (i32.ge_s (local.get $i) (local.get $m)))
    (local.set $plen
      (call $arg_item (i32.const 16) (i32.const 5) (local.get $i) (i32.const 5120) (i32.const 3072)))
    (if (i32.and (i32.ge_s (local.get $plen) (i32.const 0)) (i32.le_s (local.get $plen) (i32.const 3072)))
      (then
        (local.set $p (call $pred_id (i32.const 5120) (local.get $plen)))
        (if (i64.ge_s (local.get $p) (i64.const 0))
          (then
            (i64.store (i32.add (i32.const 8192) (i32.shl (global.get $npred) (i32.const 3)))
              (local.get $p))
            (global.set $npred (i32.add (global.get $npred) (i32.const 1)))))))
    (local.set $i (i32.add (local.get $i) (i32.const 1)))
    (br $next)))
;; no list (or an empty one): every predicate
(if (i32.le_s (local.get $m) (i32.const 0))
  (then
    (i64.store (i32.const 8192) (i64.const -1))
    (global.set $npred (i32.const 1))))
(local.set $count (i64.const 0))
(if (i32.and (i64.ge_s (local.get $src) (i64.const 0)) (i64.gt_s (local.get $k) (i64.const 0)))
  (then
    (local.set $n (call $vertex_count))
    (if (i64.gt_u (local.get $n) (i64.const 500000000)) (then (return (i32.const 0))))
    ;; the queue starts after the flags, 4-aligned; grow the memory to fit
    (local.set $qbase
      (i32.add (i32.const 65536)
        (i32.and (i32.add (i32.wrap_i64 (local.get $n)) (i32.const 3)) (i32.const -4))))
    (local.set $pages
      (i64.div_u
        (i64.add (i64.add (i64.extend_i32_u (local.get $qbase)) (i64.mul (local.get $n) (i64.const 4)))
          (i64.const 65535))
        (i64.const 65536)))
    (if (i64.gt_u (local.get $pages) (i64.extend_i32_u (memory.size)))
      (then
        (if (i32.eq
              (memory.grow (i32.sub (i32.wrap_i64 (local.get $pages)) (memory.size)))
              (i32.const -1))
          (then (return (i32.const 0))))))
    ;; breadth-first search, one level per hop
    (i32.store8 (i32.add (i32.const 65536) (i32.wrap_i64 (local.get $src))) (i32.const 1))
    (i32.store (local.get $qbase) (i32.wrap_i64 (local.get $src)))
    (local.set $head (i32.const 0))
    (local.set $tail (i32.const 1))
    (local.set $depth (i64.const 0))
    (block $bfs
      (loop $level
        (br_if $bfs (i64.ge_s (local.get $depth) (local.get $k)))
        (br_if $bfs (i32.ge_u (local.get $head) (local.get $tail)))
        (local.set $end (local.get $tail))
        (block $level_done
          (loop $vertex
            (br_if $level_done (i32.ge_u (local.get $head) (local.get $end)))
            (local.set $u
              (i64.extend_i32_u
                (i32.load (i32.add (local.get $qbase) (i32.shl (local.get $head) (i32.const 2))))))
            (local.set $head (i32.add (local.get $head) (i32.const 1)))
            (local.set $pi (i32.const 0))
            (block $preds_done
              (loop $pred
                (br_if $preds_done (i32.ge_u (local.get $pi) (global.get $npred)))
                (local.set $p
                  (i64.load (i32.add (i32.const 8192) (i32.shl (local.get $pi) (i32.const 3)))))
                (local.set $d (call $out_degree (local.get $u) (local.get $p)))
                (local.set $x (i64.const 0))
                (block $nbrs_done
                  (loop $nbr
                    (br_if $nbrs_done (i64.ge_s (local.get $x) (local.get $d)))
                    (local.set $w (call $out_neighbor (local.get $u) (local.get $p) (local.get $x)))
                    (if (i32.eqz (i32.load8_u (i32.add (i32.const 65536) (i32.wrap_i64 (local.get $w)))))
                      (then
                        (i32.store8 (i32.add (i32.const 65536) (i32.wrap_i64 (local.get $w)))
                          (i32.const 1))
                        (i32.store (i32.add (local.get $qbase) (i32.shl (local.get $tail) (i32.const 2)))
                          (i32.wrap_i64 (local.get $w)))
                        (local.set $tail (i32.add (local.get $tail) (i32.const 1)))
                        (local.set $count (i64.add (local.get $count) (i64.const 1)))))
                    (local.set $x (i64.add (local.get $x) (i64.const 1)))
                    (br $nbr)))
                (local.set $pi (i32.add (local.get $pi) (i32.const 1)))
                (br $pred)))
            (br $vertex)))
        (local.set $depth (i64.add (local.get $depth) (i64.const 1)))
        (br $level)))))
;; {"src":"<…>","k":K,"count":N}
(call $emit (i32.const 32) (i32.const 7))
(call $emit_json_str (i32.const 1024) (local.get $len))
(call $emit (i32.const 48) (i32.const 5))
(call $emit_i64 (local.get $k))
(call $emit (i32.const 64) (i32.const 9))
(call $emit_i64 (local.get $count))
(call $emit (i32.const 80) (i32.const 1))
(i32.const 1)
```

## Sandbox and limits

Every call gets a fresh module instance (nothing survives between calls) and
runs under these limits, set in `conf.ini`'s `[pfn]` section (Rust
extensions) and applied to every served database like the `[query]` keys
(`refreshconf` re-applies them):

| key | default | meaning |
|-----|---------|---------|
| `pfn_fuel_per_call` | `1000000000` | the most WebAssembly instructions one call may run (`0` = unlimited); past it: `run f fail: the instruction budget … is exhausted` |
| `pfn_max_memory_pages` | `1024` (64 MiB) | the most 64 KiB pages the memory may grow to; `memory.grow` past it returns `-1`, as WebAssembly specifies |
| `pfn_max_module_bytes` | `1048576` | the largest source text and compiled module `funcudb` accepts — also `funBody` and `funSubs` together as sent (URL-decoded, whitespace included); `funDesc` may have at most 64 KiB, and `funSubs` must be empty next to a complete module, when the function is created or updated |
| `pfn_max_functions` | `100` | the most functions one user may own |
| `[query] gfun_max_output_bytes` | `67108864` | the most bytes one call may emit (and the most a call's result array may hold) |

The call runs in slices of about a million instructions; between slices the
query's cancel flag is checked, so `operationTaskCancel` (`/api`) and
`POST /cancel` (REST) stop a looping function, and the instruction budget is
enforced. A host function is charged one instruction per 8 bytes of guest
memory it reads or writes (a string lookup, an argument key, `emit`, …) and
checks the cancel flag itself, so a loop of host calls on large strings is
bounded and stopped like any other. `emit_json_str` escapes straight into the
output, checking the bound as it goes. Modules may declare at most 10 000
functions (wasmi's strict limits); compiling validates in a throw-away
engine, and the engine modules are loaded into is replaced after 16 MiB of
modules, so neither leaks memory however often functions are compiled, and
a compilation does not hold up other users' calls. Calls of one statement
are made one argument combination at a time (never all prepared at once),
and the combinations are counted before any call is made. A trap (`unreachable`, an out-of-bounds memory access, a stack
overflow, a division by zero, a host function refusing a bad pointer or too
much output) fails the query with a clean error; the server is unaffected.

PFN results are never served from the query cache: they depend on the calling
user and on the function's current version, which the cache key does not
contain. A recompiled, updated or deleted function takes effect on the next
query. Deleting a user deletes their functions (`usermanage` type 2, and the
REST user drop): an account later created under the name starts with none.

## Storage

Under `pfn_base_path` (`[system]`, upstream's key; when it is not set,
`<db_home>/pfn`):

```text
src/<user>/data.json              one funInfo object per line (upstream's data.json)
src/<user>/<name lower-cased>.wat the generated module text (upstream: cpp/<user>/<name>.cpp)
lib/<user>/lib<name lower-cased><md5(lastTime)>.wasm   the compiled module
```

Upstream keeps its C++ sources under `cpp/`; the WebAssembly sources are
under `src/` so the two never mix (an upstream `pfn/` directory is not
usable as is: its bodies are C++).

## Differences from upstream

* **Runtime**: WebAssembly text in a sandboxed interpreter instead of C++
  compiled with `g++` and loaded with `dlopen()`. Bodies written for upstream
  must be rewritten; the management API, the call syntax and the answers are
  the same. The interpreter is slower than native code, and a function cannot
  crash, hang or take over the server.
* **Limits**: upstream runs user code without any; see
  [Sandbox and limits](#sandbox-and-limits).
* **Return value**: upstream ignores the C++ function's `bool`; here `0`
  fails the query.
* **No variable in the argument**: next to WHERE solutions, a call whose
  argument names no variable runs once (a one-element array); upstream never
  runs it then and answers `[]`.
* **`lastTime`** is stamped on create, update and compile; upstream stamps
  it on compile only.
* **Names** are restricted to what the SPARQL lexer reads after `PFN.` (a
  letter or `_`, then letters, digits, `_`; at most 64), which also makes
  them safe file names.
* **`funquery` paging** (`pageNo` / `pageSize`) is an extension; upstream
  always returns the whole list.
* **`pfn_base_path`**: without the key the functions live under the
  server's database directory (`<db_home>/pfn`), like the backups do here;
  upstream defaults to `./pfn/` relative to its working directory. The key
  is read at startup (as upstream reads it once); `refreshconf` re-applies
  the `[pfn]` limits only.
* **Where PFN runs**: the `/api` and REST endpoints of the multi-database
  `gserver` (API mode, which manages the functions with `funcudb`), and an
  embedded `Database` / `ConcurrentDb` with a caller installed
  (`Database::set_pfn_caller`). The single-database `gserver <db dir>` and
  the command-line tools (`gquery`, `gconsole`) install no caller: a PFN
  call there is a query error. A function called from the `WHERE` clause of
  an UPDATE (inside a sub-SELECT) is the requesting user's, as in reads —
  upstream does not evaluate PFN in updates at all. A failing call inside a
  sub-SELECT fails the statement (it never reads as "no solution").
* **Upstream's extra `funInfo` fields** `funArgs` and `funReturn` (listed in
  its docs, dropped by its code) are not stored, as upstream.
* A PFN value is a plain literal everywhere but the `/api` `query`
  operation's JSON answers, which use upstream's `{"results": …}` document.
