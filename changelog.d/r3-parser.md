# r3-parser — RDF parsers: one RFC 3986 resolver, literal fidelity, streaming token caps, tokenizer memory, numeric/prefix/comment edge cases

## Changelog
- **Behaviour change (review parser#4): one RFC 3986 resolver for every RDF
  reader.** `parser::iri::resolve` (RFC 3986 §5.2) now resolves relative
  references in Turtle / TriG (`@base`, `BASE`), RDF/XML (`xml:base`,
  `rdf:about`, `rdf:ID`, `rdf:resource`, `rdf:datatype`), JSON-LD (`@base`,
  `@id`) and RDFa (`<base href>`, `xml:base`, `about` / `resource` /
  `href` / `src`). With a base of `http://a/b/c/d;p?q`, RDF/XML, JSON-LD and
  RDFa used to give `http://a/b/c/../g` for `../g`, `http://a/b/c///g` for
  `//g` and `http://a/b/c/?y` for `?y`. They now give the RFC's
  `http://a/b/g`, `http://g` and `http://a/b/c/d;p?y`, the same IRI as
  Turtle. A relative `xml:base`, a nested JSON-LD `@base` or an RDFa
  `xml:base` is resolved against the base in force. An empty RDFa `about` /
  `resource` names the base. Documents loaded before keep the IRIs they were
  stored with; reloading such a document can give different IRIs.
- **Relative IRI resolution is linear (parser#15).** `remove_dot_segments`
  walks the path once, with a stack of segments. It used to move the rest of
  the path once per segment, which was quadratic: 11 s for one 1.4 MB
  relative IRI under `@base` in the test profile.
- **Behaviour change (parser#9): RDF/XML literals keep their data.**
  - A literal property element's value is its text exactly (§7.2.16). It was
    trimmed, so `"  a  b  "` became `"a  b"` and whitespace-only text became
    `""`.
  - `rdf:parseType="Literal"` gives an `rdf:XMLLiteral` whose value is the
    content as exclusive canonical XML with comments (§7.2.17). Elements,
    attributes and comments are kept, and each element declares the
    namespaces it visibly uses. Only the text children were kept before.
  - An empty property element with property attributes (§7.2.21) has
    `rdf:resource`, `rdf:nodeID` or a fresh blank node as its object, and
    the attributes (and `rdf:type`) describe that object. Before, the
    attributes were dropped next to `rdf:resource`, and without it the
    element became the literal `""`.
- **Behaviour change (parser#10): Turtle / TriG numbers follow the grammar.**
  A number is the longest match of the INTEGER / DECIMAL / DOUBLE terminals.
  - `1.e5` and `123.E+1` are `xsd:double`. They were refused, and a tolerant
    import stored `"1"^^xsd:integer`.
  - `1e`, `1E+`, `1e-`, `1.5e`, `1.5.5`, `1e5e5` and `1e5.5` fail their
    statement. They used to be stored as numbers with nothing logged.
  - A `.` followed by a digit never ends a statement, so `( 1.5.5 )` is the
    two items `1.5` and `.5`.
- **Prefixes with inner dots (parser#11).** `@prefix ex.v2: <…>` (and
  `PREFIX`) declares a prefix that names like `ex.v2:s` can use (PN_PREFIX).
  Before, the name stopped at the dot, so every statement using such a
  prefix failed. A declared prefix that starts or ends with `.` or holds a
  term separator is refused, because no name could use it.
- **Invalid UTF-8 in a Turtle / TriG comment (parser#12)** is an error of the
  comment's line (`invalid UTF-8 in a comment`), and the statements around
  the comment load. Before, the next statement was dropped and its line
  reported. A strict import still fails, now at the comment's line.
- **New limit (parser#5): one token of a streaming RDF reader.** The token is
  a Turtle / TriG string, IRI, name, number or language tag, or one
  N-Triples / N-Quads line. The limit is `[upload] rdf_max_token_size`
  (default 64 MiB, `0` = no limit; `ImportOptions::max_token_bytes`,
  `StatementSink::max_token_bytes`, `stream::default_max_token_bytes`).
  - A longer token fails its statement once the reader has held the
    limit's worth of it. The reader moves past the rest without holding it,
    and a tolerant import goes on with the next statement.
  - Before, an unterminated `"""` string, a newline-free string, IRI or name
    was held whole, as was a `.gz` input's expansion within one token.
  - The N-Triples / N-Quads scanner reads the line in place. It used to copy
    the line into a `Vec<char>`, six times the line in memory.
- **New limit (parser#8): one SPARQL request in gserver.** `[upload]
  sparql_max_size` (default 16 MiB, `0` = no limit) is installed by gserver
  and gbuild (`GStoreConfig::install_storage_defaults`, also on
  `refreshconf`). A longer query or update is refused before it is
  tokenized: `the request is N bytes long, more than the limit of M bytes
  ([upload] sparql_max_size)`. Upstream has no limit. Processes that install
  no configuration have no limit: gquery, gconsole, library callers and
  gnode.
- **The SPARQL tokenizer lexes the request in place (parser#8).** It used to
  copy the request into a `Vec<char>` plus a byte-offset table, 12 bytes per
  request byte: a 64 MiB body took about 770 MB more before parsing started.
- **JSON-LD contexts and RDFa prefix maps are shared (parser#7).** A
  JSON-LD node object without its own `@context` shares its parent's
  context instead of copying it. One with an `@context` gets a layer of its
  own term definitions over the shared ones, and the members of a
  `@context` array fill one layer, so nothing is copied in proportion to
  the terms in scope. An RDFa element that declares a prefix (`prefix`,
  `xmlns:`) likewise adds a layer over the shared prefix map. Every node
  used to copy the whole map: O(nodes × terms), 10.9 s for 20000 empty node
  objects under a 4000-term context. Copying only for a node with its own
  `@context` (this package's first version) left the same cost one
  `"@context": {}` per node away (1.8 s for 5000 such nodes under 4000
  terms, now 4 ms), and a `@context` array of 8000 one-term members took
  10.5 s (quadratic, now 8 ms).
- **RDF/XML: namespace declarations on inner elements (found in review).**
  An `xmlns:p` declaration on any element but `rdf:RDF` (a node element, a
  property element, or a node element used as the document element, RDF/XML
  §7.2.11) failed the whole document with `undefined namespace prefix
  'xmlns:'`. The reserved prefix `xmlns` is now bound, so such declarations
  are read as declarations, scoped to their element.
- **RDF/XML shares the namespace bindings and the base (parser#7's pattern,
  found in review).** Every node and property element copied every
  namespace binding declared above it: 40000 elements took 0.12 s under 10
  declarations and 3.8 s under 1000. Bindings are now layers, like the
  JSON-LD and RDFa maps (56 ms under 1000), and the `xml:base` in scope is
  shared instead of copied per element. The canonical form of
  `parseType="Literal"` content keeps its rendered namespaces the same way.
- **Blank graph labels in large documents (parser#14).** In N-Quads / TriG,
  the check of whether a blank graph label was used before as a subject or
  object is now a growing (scalable) Bloom filter.
  - The false-positive rate stays about 0.5%: 0.51% measured at 5Mi labels,
    in 20.6 MB of filters, allocated as labels come. This holds up to 67M
    labels and 201 MB.
  - The old fixed 2^23-bit filter with two hash bits reached 4.5% false
    positives at 1M labels and 51% at 5M.
  - Each false positive kept a graph apart from the statements describing
    its label.
- New files: `src/parser/iri.rs`, `tests/rdf_parser_conformance.rs`
  (hand-authored cases: RFC 3986 §5.4 in all four formats, Turtle 1.1 §6.5
  terminals, PN_PREFIX and comments, RDF/XML §7.2.16/17/21, JSON-LD /
  RDFa scoping, resource bounds), `tests/parser_limits_from_config.rs`.
  New public items: `parser::iri::{resolve, is_absolute, is_valid_scheme}`,
  `parser::stream::{DEFAULT_MAX_TOKEN_BYTES, default_max_token_bytes,
  set_default_max_token_bytes, token_too_long}`,
  `StatementSink::max_token_bytes` (provided method),
  `ImportOptions::max_token_bytes`,
  `parser::sparql::lexer::{DEFAULT_MAX_REQUEST_BYTES, max_request_bytes,
  set_max_request_bytes, check_request_size}`,
  `CharSource::flag_bad_utf8`.

## GAP_PLAN status line
Phase 5 RDF readers (r3-parser):
- One RFC 3986 resolver for Turtle / TriG, RDF/XML, JSON-LD and RDFa.
- RDF/XML `parseType="Literal"` (`rdf:XMLLiteral`, canonical XML) and empty
  property elements per §7.2.21.
- Turtle numbers and prefixes per the 1.1 grammar.
- Per-token limit `[upload] rdf_max_token_size` and SPARQL request limit
  `[upload] sparql_max_size`.

## Docs notes
- README.md, the "Other gstore-rust keys" paragraph (`[upload]`): add
  `rdf_max_token_size` (default 64 MiB, `0` = no limit). It is the longest
  token the streaming RDF readers hold (string, IRI, name, number or
  language tag) and the longest N-Triples / N-Quads line. A longer one fails
  its statement, and a tolerant build skips and logs it. Also add
  `sparql_max_size` (default 16 MiB, `0` = no limit). It is the longest
  SPARQL query or update gserver accepts, refused before parsing. gquery,
  gconsole and library callers read no configuration and have no limit.
- docs/DEPLOY.md, the upload limits paragraph (next to `unzip_max_entries`
  / `unzip_max_size`): add the same two keys. `sparql_max_size` is a limit
  key, not an upstream-compat switch: upstream has no limit. A deployment
  that sends INSERT DATA bodies over 16 MiB raises it or sets it to `0`, but
  large loads belong in build / batchInsert. conformance.sh needs no switch.
- README.md and CHANGELOG.md, where blank-node graph labels are described
  (the "used as a subject or object before it named a graph" case, around
  CHANGELOG line 932): such a graph is kept apart. In a document with
  millions of blank labels, a graph whose label was not used before is also
  kept apart about 0.5% of the time: a false positive of the bounded-memory
  check, about 4 bytes per label. A warning names each such graph.
- README.md, RDF format support: RDF/XML literals keep their whitespace,
  `parseType="Literal"` gives `rdf:XMLLiteral`, and relative IRIs resolve
  per RFC 3986 in every format. A database built from RDF/XML, JSON-LD or
  RDFa with relative references (`../x`, `//host/x`, `?q`) under a base has
  the old, wrong IRIs until it is rebuilt.
- docs/REFACTOR_BACKLOG.md (review parser#7, the same pattern in another
  dimension; not deferred work of this package's ids, which are fixed): the
  tree-walking RDF readers still copy inherited *strings* per element. The
  RDFa reader clones the base, `vocab`, language and the parent subject /
  object terms for every element; JSON-LD clones the base, `@vocab` and
  `@language` for a node with its own `@context`; RDF/XML clones `xml:lang`.
  With a 1 MiB `<base href>`, 10000 RDFa elements took 4.2 s (JSON-LD,
  5000 nodes with `"@context": {}` under a 1 MiB `@base`: 0.18 s). Sharing
  them needs `Rc<str>` contexts and shared terms through the RDFa walk, a
  refactor of the extractors; the documents are bounded by
  `max_document_bytes` (256 MiB) and the reader needs a privileged import
  path.

## Measured numbers
From the tests of `tests/rdf_parser_conformance.rs` (test profile,
`opt-level = 1`, per-thread counting allocator; 4-vCPU machine shared with
two other agents' builds):
- **parser#5, N-Quads.** A 16 MiB line with no cap peaked at 33.6 MB. On the
  previous reader it took 100.7 MB, measured when the change was made in
  this package. With a 1 MiB cap, a 16 MiB N-Quads or N-Triples line peaks
  at 1.12 MB and fails its statement.
- **parser#5, Turtle.** With a 256 KiB cap, a 16 MiB string, IRI, prefixed
  name, number or unterminated `"""` string peaks at 0.59 MB. Before, the
  whole token was held: a 16 MiB peak or more.
- **parser#8.** Tokenizing 16 MiB of whitespace peaks at 1.2 KB, and 16 MiB
  of comments at 296 B. A 16 MiB string literal peaks at 16.8 MB, which is
  its own token. Before, the tokenizer took 201 MB for 16 MiB of whitespace.
- **RDF/XML namespaces (review follow-up).** 20000 `rdf:Description`s
  (40000 elements) under 10 / 1000 namespace declarations: 66 ms / 3.46 s
  before (52.3x), 83 ms / 56 ms after (`rdfxml_elements_share_the_namespace_bindings`).
- **parser#14.**
  - 5Mi labels (import.rs unit test): 0.51% false positives over 200000
    probes, filters of 20,561,600 bytes, no false negatives.
  - `import_reader` on generated N-Quads with 1Mi blank labels and 4000
    blank graphs: 13-14 graphs kept apart (varies with the random
    blank-node scope).
  - The previous filter kept 181 graphs apart in the same test.
- **parser#15.** An IRI 8 times longer takes at most 24 times as long; a
  quadratic resolver gives 64. The previous code needed 11 s for 1.4 MB.
- **parser#7.** 16 times the JSON-LD context terms, or 100 times the RDFa
  prefixes, cost under 4 times as much. The previous code measured 14.8x and
  30.1x. Review follow-up (layers instead of copy-on-write), measured with
  the same test profile: 5000 node objects each with `"@context": {}` under
  250 / 4000 terms, 106 ms / 1.65 s before (15.5x), 2 ms / 4 ms after; a
  `@context` array of 2000 / 8000 one-term members, 655 ms / 10.5 s before,
  1.2 ms / 8 ms after; 4000 RDFa elements each declaring a prefix under 10 /
  1000 prefixes, 13 ms / 325 ms before (24.7x), about 4 ms either way after.
- **Bench smoke gate** (D9), branch head `db97fcc`. Release build of
  `gbuild` + `gstore-bench`; `gstore-bench report --check README.md
  CHANGELOG.md` clean. Command: `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=… target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`.
  - Result: PASS, 456 gates checked, 0 failed.
  - Time geomean vs the committed baseline: LUBM-like snapshot 0.94, disk
    1.09; WatDiv-like snapshot 1.07, disk 1.12 (bound 1.50). Base 0f74615,
    from r2-integration's run: 1.01, 1.04, 1.13, 1.20.
  - Build time vs baseline: LUBM-like 1.15 / 1.08, WatDiv-like 0.64 / 0.67
    (bounds 2.0 / 2.5). Peak build RSS 1.00-1.01.
  - Machine state: load average 7.7 at the start, with other agents' builds
    running, so the time ratios are noisy. No gate came near its bound.
- **Verification at the branch head.**
  - `cargo fmt --all -- --check` clean.
  - `cargo clippy --workspace --all-targets -- -D warnings` clean.
  - `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore --no-deps --locked`
    clean.
  - `cargo test --workspace --no-fail-fast`: 2757 passed, 0 failed, 7
    ignored (160 result lines), both with `GSTORE_DISK_QUERY_MODE=memory`
    and with `stream`.
  - `scripts/conformance.sh`: all six suites pass. The parser suite fails
    only the known cases 15 and 17; Python 49/49.
- **Verification after the review follow-ups** (JSON-LD / RDFa / RDF/XML
  only; Turtle, the SPARQL lexer and the HTTP paths are unchanged since the
  conformance run above): fmt, workspace clippy `-D warnings` and rustdoc
  `-D warnings` clean; `cargo test --workspace --no-fail-fast` 2764 passed,
  0 failed, 7 ignored in both `GSTORE_DISK_QUERY_MODE=memory` and `stream`;
  `cargo test -p gstore --features gzip` on the lib, `dt_rdf_import`,
  `dt_merge5_review`, `rdf_parser_conformance` and
  `parser_limits_from_config`: all pass. Every new review test was run
  against the code before its fix and failed there (the scoping guards
  excepted, which pass on both).

## Requests/Not yet
- Edits outside the owned files, all inside the plan's shared regions:
  - `src/config.rs`: the keys `rdf_max_token_size` and `sparql_max_size`
    directly after `unzip_max_size` in KNOWN_KEYS, the struct, Default,
    `set()` and `value()`; the `[upload]` line of the module doc;
    `install_storage_defaults()`, two calls and their doc; the config
    round-trip test (two assertions).
  - `conf/conf.ini.example`: two keys after `unzip_max_size`.
  - `src/parser/sparql/mod.rs`: `parse()`, which calls
    `lexer::check_request_size`.
  - `src/parser/mod.rs`: one `pub mod iri;` line.
- gnode (cluster, frozen per D4) does not call
  `install_storage_defaults`, so its SPARQL requests have no size limit. The
  in-place tokenizer still removes the 12× amplification. Wiring the limit
  is left to r5-cluster if wanted.
- For a later owner of `src/http_api.rs` / `src/gapi/` (r4-api-hygiene):
  `sparql_max_size` is checked when the request text is parsed, after the
  HTTP layer has read the body (bounded by `upload_max_body_size`). Refusing
  a `/sparql`, `/query` or `/api` query body longer than `sparql_max_size`
  before reading it would also save buffering the body. The tokenizer no
  longer amplifies it, so this is not a parser#8 residual.
- For the owner of `src/db/mod.rs` (library API, not used by any binary):
  `Database::build_from_ntriples_parallel` reads its chunks with
  `read_until`, so one line longer than `rdf_max_token_size` is held whole
  before `ntriples::parse_str` refuses it. The streaming readers that every
  binary uses (`import_reader`, the bulk loaders) are bounded.
- Nothing deferred.
