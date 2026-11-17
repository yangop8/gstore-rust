# r4-db-graph — Graph management and inference entry points: one existence rule for CREATE/DROP/CLEAR/COPY/MOVE/ADD, chunked default-graph copies, lazy VS-tree rebuild, Result-returning inference, SPARQL grammar leniencies

## Changelog
- **Behaviour change (review sparqlconf#16): one existence rule for every
  graph-management operation** (SPARQL 1.1 Update §3.1.5, §3.2). A named
  graph exists while it holds a triple (the store keeps no empty named
  graphs); the default graph always exists. `CREATE GRAPH <g>` of a graph
  that exists, and `DROP GRAPH <g>` / `CLEAR GRAPH <g>` of one that does
  not, now fail with a request error ("CREATE GRAPH failed: the graph <g>
  already exists (it holds triples); use CREATE SILENT GRAPH …", "DROP
  GRAPH failed: the graph <g> does not exist …"), as `COPY` / `MOVE` / `ADD`
  from a missing source already did; the failing operation changes nothing
  and ends the request (operations after it are not applied, those before
  it stay, as with any failing operation). `SILENT` makes each a no-op.
  `DEFAULT`, `NAMED` and `ALL` never fail; `CREATE GRAPH` of a graph that
  does not exist still succeeds and changes nothing; `ADD` / `MOVE` / `COPY`
  of a graph onto itself stays a no-op, also for a graph that does not
  exist. Before, `CREATE GRAPH` of a populated graph and `DROP` / `CLEAR` of
  a missing one reported `0` changed triples.
- **Behaviour change (sparqlconf#16, parser#13): blank nodes are refused in
  `DELETE DATA`, `DELETE WHERE` and DELETE templates** at parse time, the
  labelled ones (`_:b`) too, as SPARQL 1.1 Update requires (only `[]` was
  refused before). `INSERT DATA`, INSERT templates and WHERE patterns keep
  them; a labelled blank node in a WHERE pattern still names the stored
  node (the documented upstream reading). A triple whose subject or object
  is a stored blank node is deleted with `DELETE WHERE`/`DELETE … WHERE`
  and a variable instead (`DELETE { ?b :p "x" } WHERE { ?b :p "x" FILTER
  isBlank(?b) }`). A reasoning rule's retraction is unaffected (it builds
  its `DELETE DATA` operation directly); its `update.log` record holds the
  stored label and does not replay, as `reason_retract`'s docs already
  said.
- **Behaviour change (sparqlconf#16): `SECONDS` returns an exact
  `xsd:decimal`** (§17.4.5.7): `SECONDS("…T14:45:13.815-05:00"^^xsd:dateTime)`
  is `"13.815"^^xsd:decimal`, whole seconds `"7"^^xsd:decimal` (it returned
  `xsd:integer`, or an `xsd:double` with a binary fraction). On a value
  without a time (an `xsd:date`) it is an error (unbound), not `0`.
- **Behaviour change (parser#13): the SPARQL grammar's structure is
  enforced where a lenient reading changed the request.** `PREFIX
  foaf:knows <…>` is a syntax error (it silently declared `foaf:`); two
  triple statements need a `.` between them in group patterns, CONSTRUCT
  templates, DATA blocks and INSERT/DELETE templates (`{ ?s ?p ?o ?a ?b ?c }`
  was two patterns; a `.` stays optional after the last statement and
  around `FILTER`, `OPTIONAL`, `BIND`, `VALUES`, `GRAPH`, sub-groups, and
  stray ones are still skipped); `ORDER BY`, `LIMIT` and `OFFSET` come at
  most once each, `ORDER BY` first (`LIMIT 1 LIMIT 2` was `LIMIT 2`,
  `OFFSET 3 ORDER BY ?s LIMIT 1` was accepted); `LIMIT` / `OFFSET` keep
  either order. No upstream gStore script or test query relies on the
  lenient forms (scripts/conformance.sh needs no switch).
- **design#7: the library inference entry points report store read
  errors.** New `Database::try_materialize_rdfs()` and
  `Database::try_run_rules()` return `Result<usize>`: the reasoner and its
  book-keeping run inside a read scope (contract C2), so a Stream-mode or
  RocksDB read error fails the run ("a read from the store failed while
  materializing the RDFS closure: …", a storage error) instead of leaving an
  incomplete closure behind with only an unscoped log line. The triples
  inferred before the failure stay inserted (each is entailed by the data);
  a later run completes the closure. `materialize_rdfs()` / `run_rules()`
  keep their signatures, delegate, and log the error. The rule ledger of
  `reasonManage` execute reads which derived triples existed inside a read
  scope (a read error fails it before anything is recorded); when reading
  which ones the insert added fails, it records every derived triple that
  did not exist, so a disable still retracts all the rule may have added.
- **db#4: the in-memory VS-tree is rebuilt instead of staying unused for
  the rest of the session.** A clear of the default graph (`CLEAR` / `DROP
  DEFAULT` or `ALL`, the clears of `COPY` / `MOVE`) rebuilds the tree from
  what is left — usually nothing — at the end of the operation (inside a
  transaction: at its commit or rollback), and the inserts of a `COPY` /
  `MOVE … TO DEFAULT` then fold into it; reasoning folds the inferred
  triples into an in-memory tree like inserts, and a disk-backed tree
  (Materialize mode's `vstree.kv`) is rebuilt in memory at the end of the
  run. Before, `index_valid` stayed `false` until the database was
  reopened, so queries lost the candidate pruning (`gshow` / `gmonitor`
  showed `stale`). Stream mode keeps its behaviour: its `vstree.kv` is
  rewritten by `rebuild_index` or at the next open (with `vstree_on_open`),
  since that rebuild takes commit points an operation must not take
  part-way.
- **db#16: `COPY` / `MOVE` / `ADD` copy by id.** The source graph is read
  once as ids (12 bytes a triple, in a read scope) and inserted by id in
  chunks of 65 536 (an in-memory default graph takes a chunk as one batch);
  every triple used to be turned into three terms (~216 bytes plus their
  strings) and interned again. Only the first triple into a named
  destination takes the term path, which interns the graph IRI (on RocksDB
  writing it to the store first).
- **db#15: `Database::build_from_ntriples_batched` is linear in the file.**
  It still streams the file and encodes each triple to ids as it is read,
  but indexes the ids once at the end, in one bulk load into the empty
  store; loading every `batch` triples into the growing store re-sorted the
  whole store each time (O(N²/batch); 4.6 s instead of 0.72 s for 400 000
  triples in the review's measurement). `batch` only sizes the id buffer's
  first allocation now.
- tests-a#3: `tests/dt_graph.rs` `clear_named_graph_and_clear_all` checks
  the named graphs left after `CLEAR GRAPH` exactly (it asserted
  `.iter().all(|_| true)`).
- Two existing tests pinned the old behaviour and were corrected
  (convention 13b, each with a `review <id>` comment, nothing else they
  check weakened): `tests/dt_merge5_review.rs`
  `drop_or_clear_of_the_empty_graph_iri_is_not_drop_named` asserted that
  `DROP` / `CLEAR GRAPH <>` of a missing graph changed 0 triples (now: the
  existence-rule error, SILENT gives 0, still nothing dropped);
  `tests/sparql_builtins.rs` `delete_templates_keep_labelled_blank_nodes`
  asserted that a labelled blank node in `DELETE WHERE` / a DELETE template
  named the stored node (now: refused, the triple stays, a variable
  deletes it; the INSERT template's label is still a fresh node).
- Tests and tools: `tests/graph_management.rs` (every layout: the existence
  rule, refused blank nodes, SECONDS, the grammar rules, the VS-tree after
  clears / copies / moves / transactions / reasoning, `try_*` on a Stream
  store with a corrupt POS root and on RocksDB with failing reads, the
  batched-build work counter, dictionary look-ups of a Stream-mode COPY);
  W3C-style cases `tests/w3c/update-graph-management/` (16) and
  `tests/w3c/grammar/` (14). The W3C harness (`tests/w3c_subset.rs`) takes
  negative cases: a case may hold `expected.error` (first line `syntax`, or
  `failure` for an update that must fail on every layout and executor
  setting and leave the dataset unchanged, also after a reopen) instead of
  an expected answer; `tests/w3c/README.md` documents it. New work counter
  `gstore::store::bulk_load_sorted()` (`#[doc(hidden)]`, per thread): the
  triples the in-memory bulk loads sorted.

## GAP_PLAN status line
none

## Docs notes
- README.md (SPARQL support / known deviations): graph-management
  operations follow one existence rule (CREATE of an existing graph, DROP /
  CLEAR of a missing one fail unless SILENT); blank nodes are refused in
  DELETE DATA / DELETE WHERE / DELETE templates; SECONDS is `xsd:decimal`;
  PREFIX with a local part, missing `.` between triple patterns and
  repeated / misplaced ORDER BY / LIMIT / OFFSET are syntax errors.
- README.md (library API): `try_materialize_rdfs()` / `try_run_rules()`
  report read errors; `build_from_ntriples_batched` buffers the encoded ids
  (12 bytes a triple) and indexes them once — its "bounded memory" is the
  streamed parse, not a bounded id buffer.
- docs/CPP_PARITY.md line 50 (`build_from_ntriples_batched`(有界内存分批flush)):
  now "streamed parse, ids indexed in one bulk load (12 B/triple buffered)"
  — r4-concurrent owns CPP_PARITY.md this round; for the closing step if it
  did not change it.
- docs/DEPLOY.md: no compat switch and no secure default from this package;
  the behaviour changes above are SPARQL 1.1 conformance fixes that no
  upstream script relies on (scripts/conformance.sh passes unchanged).
- docs/REFACTOR_BACKLOG.md (D3/D8 remainders, with ids):
  - review db#4 (remainder; PHASE6_PLAN §8 D6): the Stream-mode `vstree.kv`
    is not rebuilt in-session after a clear, an insert or reasoning, and an
    out-of-core tree (Materialize mode's `vstree.kv`, `use_streaming_vstree`)
    is still dropped from use by the first insert (only a clear or a
    reasoning run rebuilds it in memory) — the non-incremental paged tree;
    an operation-end rebuild for Stream mode would need commit points
    inside a statement.
  - review design#7 (medium term, D3): the infallible
    `TripleSource` / `MutableStore` seam still forces the out-of-band
    read-scope channel; fallible `try_*` methods on the seam are a refactor
    of every backend.
  - review db#16 (second half): the `USING` merge of `DELETE/INSERT …
    USING` still builds its merged default graph by per-triple
    `TripleStore::insert` (in `eval_modify_changes`, r4-sparql-guard's region
    this round); a `bulk_load` there is the remaining improvement.
  - review parser#13 (deliberately kept): stray `.` tokens (`{ . }`,
    `{ ?s ?p ?o . . }`) are still skipped — they do not change a request's
    meaning; and the labelled-blank-node reading of WHERE patterns stays
    as documented (CHANGELOG Phase 3 known gaps).
  - review sparqlconf#16 (choice recorded): `ADD` / `MOVE` / `COPY` of a
    graph onto itself is a no-op even when the graph does not exist (§3.2.3
    lets a service fail on a missing source; the same-graph no-op is pinned
    by `tests/dt_sparql_syntax.rs` `same_graph_transfers_are_no_ops`).

## Measured numbers
- db#15, work counter `store::bulk_load_sorted()` (triples sorted into the
  in-memory indexes) for `build_from_ntriples_batched` on generated
  `<s{i}> <p{i%8}> <o{i%997}>` with `batch = 100`: before 46 500 for 3 000
  triples (quadratic); after 3 000 for 3 000 and 6 000 for 6 000 (one load).
  `cargo test --test graph_management the_batched` on the unfixed source
  with the counter applied: "work 46500 for 3000 triples".
- db#16, dictionary look-ups (`dict().cache_stats()` hits + misses) of
  `COPY DEFAULT TO <g>` of 600 triples on a Stream-mode database: before
  4 201; after under 100 (asserted; the first triple's term path and the
  graph IRI only).
- Fails-before check: the source of baeae13 plus the two `try_*` methods as
  plain delegations: 12 of the 15 tests of `tests/graph_management.rs`
  fail (the three that pass guard unchanged behaviour), 12 of the 14
  `grammar/` cases and 4 of the 16 `update-graph-management/` cases fail
  (the others are positive regressions of SILENT, COPY / MOVE / ADD).
- Bench smoke gate (`cargo +1.99.1 build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench`, then `target/release/
  gstore-bench ci --baseline bench/baselines/smoke.json`; the machine ran
  other agents' builds and tests meanwhile): **PASS**, 148 rows, 96 of them
  gated, none failing. Warm-query geomean ratios against the committed
  baseline: snapshot 0.93, disk 0.92, watdiv-like snapshot 0.94,
  watdiv-like disk 0.95 (limit 1.50); build 1.16 / 1.16 / 0.68 / 0.79
  (limit 2.00-2.50); open 1.05 / 1.47 / 1.15 / 0.91 (limit 2.50). No base
  build was measured side by side (the change to a hot path is one boolean
  per triple statement in the parser and the SECONDS arm).
- Verification on the branch head: `cargo +1.99.1 test --workspace
  --no-fail-fast` with `GSTORE_DISK_QUERY_MODE=memory` 2 955 passed, 0
  failed (188 targets) and `=stream` 2 955 passed, 0 failed (187 targets;
  the gnlqa doc-test target first failed to compile because
  `libwebpki-*.rlib` had vanished from the worktree's `target/debug/deps`
  while its fingerprint stayed — an artifact problem, not the code: after
  forcing that crate to rebuild, `cargo test -p gnlqa --doc` passes);
  `--features rocksdb` (shared target-rocks): `graph_management`,
  `w3c_subset`, `dt_graph`, `dt_rocksdb`, `dt_reason`, `dt_sparql_syntax`,
  `dt_update` and `--lib db::` all pass; `cargo clippy --workspace
  --all-targets -- -D warnings` clean; `RUSTDOCFLAGS=-D warnings cargo doc
  -p gstore --no-deps` clean; `cargo fmt --all -- --check` clean;
  `scripts/conformance.sh --no-build`: all six suites pass (python
  sdk_check 49 passed), with no compat switch added.

## Requests/Not yet
- Edits outside the owned files (each small; no R4 owner known to edit
  them): `tests/w3c_subset.rs` (negative-case support: `Answer::Rejected`,
  `Rejection`, `run_rejected`, `run_failing_update`, `load_case`'s
  expected-file match, two self-tests and two malformed-case entries) and
  `tests/w3c/README.md` (one table row, one section) — needed for the
  W3C negative cases the acceptance asks for; `src/store/mod.rs` (the
  thread-local `bulk_load_sorted` counter: one line in
  `StoreData::bulk_load`'s general path and one in `bulk_load_into_empty`
  — r4-storage-format edits the statistics counters of this file, a
  textual neighbour at most); `src/db/mod.rs` outside the listed functions:
  `try_insert_triple_id` and `insert_quad_id` delegate to the new
  `insert_default_id` / `insert_named_id` (split out unchanged, plus the
  db#4 doc comment), `record_inferred` (VS-tree fold / stale mark), the
  `vstree_rebuild_pending` field doc of `struct Database`, new private fns
  `mark_vstree_stale`, `clear_target`, `named_graph_exists`,
  `insert_graph_ids`, `persisted_graph_id`, `id_triple_terms`,
  `infer_scoped`, `materialize_rdfs_scoped`, `run_rules_scoped` and the
  `TRANSFER_CHUNK` const; `src/parser/sparql/mod.rs`
  `parse_triples_only_block` (the `.` rule for CONSTRUCT templates) and a
  new `require_triples_separator`.
- Not done, with reasons: see the REFACTOR_BACKLOG notes above (Stream-mode
  in-session `vstree.kv` rebuild, the fallible storage seam, the `USING`
  merge, stray dots).
- For the integrator: never run `git stash` in a worktree of this
  repository while other worktrees run — the stash list is shared. During
  this package a `git stash pop` here applied r4-api-hygiene's stash; it was
  put back at once (`git stash store`, same commit 74dd7af) before that
  package popped it, and this worktree was restored from its own stash.
