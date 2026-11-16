# r2-terms — One term validator and one escaping serializer: language tags, blank-node labels, literals, IRI()/STRLANG(), result serialization

## Changelog
- **Behaviour change (review parser#1, H-13): one term validator for every
  reader.** `gstore::model::term` holds the rules, and RDF/XML, JSON-LD,
  RDFa, N-Triples, N-Quads, Turtle/TriG, the SPARQL lexer and the term
  built-ins all use them. A language tag is LANGTAG
  (`[a-zA-Z]+(-[a-zA-Z0-9]+)*`). Where a format writes the tag as an
  attribute or string value (RDF/XML `xml:lang`, JSON-LD `@language` in a
  value, a term definition or the context, RDFa `lang` / `xml:lang`),
  surrounding whitespace is dropped and `_` reads as `-` (`zh_CN` is stored
  as `zh-CN`). An empty value means no language there (the literal is a
  plain literal; JSON-LD used to store `"x"@`). Any other value fails the
  document (`invalid language tag "en US" …`). In N-Triples, N-Quads,
  Turtle and SPARQL, only LANGTAG is a tag now: `"x"@`, `"x"@-`, `"x"@-en`,
  `"x"@en-` and `"x"@1en` are syntax errors (the line readers used to accept
  `[A-Za-z0-9-]+`, and the SPARQL lexer even an empty tag).
- **Blank-node labels (parser#1).** A label outside BLANK_NODE_LABEL, such as
  JSON-LD `"_:a b"`, an RDF/XML `rdf:nodeID` ending in `.` or an RDFa `_:`
  CURIE holding a control character, is stored as
  `h<16 hex digits>-<label with every other character as _>`. The hex
  digits are a stable FNV-1a hash of the whole label, so one label stays one
  node and `a b` / `a_b` stay two (`BlankLabeler::explicit`, the funnel of
  every minting reader).
- **Redo log and incremental backups replay every stored term (parser#1).**
  A record names the stored terms exactly (`model::term::record_key`). An
  IRI's excluded characters are escaped, as before. A label or language tag
  the strict readers refuse (one an older version stored unchecked) now has
  its other characters as `\uXXXX`. `replay_redo_log` and
  `restore_incremental` read records with the matching parser
  (`parse_record_triple` / `Term::from_record_key`, falling back to the
  N-Triples reader for any other shape). Before, the first record holding
  `"x"@zh_CN`, `"x"@`, `"x"@en US` or `_:a b` failed the replay ("unexpected
  trailing input near '_'"), and no later increment of the chain could be
  restored. Records of well-formed terms are byte-identical to what the
  previous version wrote, so the redo format is unchanged.
- **Behaviour change (sparqlconf#12): SPARQL UPDATE refuses the literals the
  loader refuses.** `INSERT DATA` and the instantiated `INSERT` templates of
  `DELETE/INSERT … WHERE` now apply the tolerant loader's numeric check
  (`parser::import::upstream_literal_check`, upstream's `RDFParser` rules).
  `"x"^^xsd:integer`, `"300"^^xsd:byte`, an `xsd:integer` beyond 64 bits
  (`BIND(9223372036854775807 + 1 AS ?v)`) or a `NaN` double fails the
  operation before any of it is applied: `cannot insert the literal …:
  Object integer out of range (the loader refuses it, so it would not load
  back from an export)`. Before, UPDATE stored them, `gexport` wrote them,
  and `gbuild` (tolerant by default) dropped them. A strict import (the
  library default, `gbuild --strict`, SPARQL `LOAD`) still stores such a
  literal as written; an `INSERT … WHERE` that copies one of those stored
  literals into a template fails as a whole, and a reasoning rule whose
  inferred triples hold one reports that failure as its insert result.
  New library entry point: `parser::import::check_update_literals`.
- **Behaviour change (sparqlconf#12): `gbuild` exits with status 3** when the
  build completed but tolerant mode skipped statements (each one is still
  listed in `parse_error.log`, and `RDF parse error num N` is still printed).
  It used to exit 0, so a scripted `gexport` → `gbuild` migration could lose
  data unnoticed. The database is built either way. Exit statuses are now:
  0 built, 1 failed, 2 invalid arguments, 3 built with statements skipped.
- **Behaviour change (sparqlconf#11, M9): `IRI()` / `URI()` refuse text that
  is not IRIREF.** A string with a space, a control character or one of
  `<>"{}|^`\` is an expression error, so the variable stays unbound and an
  `INSERT` template skips the triple. An IRI argument is returned unchanged.
  `STRLANG()` requires a LANGTAG (`STRLANG("x", "en> bad")` and
  `STRLANG("x", "")` are errors). `STRDT()` given its datatype as a string
  requires IRIREF text. A user-defined IRI function whose result is such an
  IRI or tag is an error too.
- **N-Triples output escapes every term (sparqlconf#11).**
  `Term::to_ntriples` / `Triple::to_ntriples` is the one serializer of
  N-Triples output. It is used by CONSTRUCT / DESCRIBE responses (`/sparql`
  and the REST/`/api` CONSTRUCT bodies through `server::ntriples_body`),
  `gquery`, `gconsole`'s local mode, the TSV and `n-triple-file` result
  formats, and the exports (`gexport`, `/api export`,
  `Database::export_ntriples`, all through `model::term::export_key`). It
  escapes an IRI's or datatype IRI's excluded characters and a literal's
  control characters, and makes a legacy label or tag valid, so a stored term
  can no longer end its line and forge a statement. Before, `CONSTRUCT` over
  an IRI built from `"http://a> <http://evil> <http://c> .\n<http://d"`
  printed a forged triple, with no write access needed (a read-only
  CONSTRUCT mapping stored text through `IRI(CONCAT(…))` did it). A
  well-formed term's output is unchanged. `Term`'s `Display` (= `dict_key`,
  the stored form) is unchanged.
- **Result serializers decode cells as the dictionary keys they are
  (query#15).** JSON, XML, CSV, TSV and upstream's `/api` JSON shapes now read
  each cell with `Term::from_dict_key` before the N-Triples reader. An IRI a
  lenient reader stored with `>` in it is now `"type":"uri"`; it used to
  become a literal `"<http://ex/a>b>"`. The XML format stays well-formed
  XML 1.0. A carriage return is written `&#xD;`, a tab or newline in an
  attribute `&#x9;` / `&#xA;`. A character XML 1.0 cannot carry at all (C0
  controls other than tab, newline and CR, U+FFFE, U+FFFF) is written as
  U+FFFD, where it used to be written raw and break the document. The JSON
  format carries such values exactly.
- **N-Triples / N-Quads: a trailing `.` ends the statement, not the label
  (parser#6).** `_:g.` and `_:g .` are now the same node or graph, as in
  Turtle/TriG. The W3C `minimal_whitespace` form `_:s<http://p>_:o.` reads
  `_:o`. Before, a minimal-whitespace dump split a blank node or named graph
  in two. The two line readers now share one term scanner
  (`ntriples::Scanner`; N-Quads lost its 180-line copy).
- **RDFa: a minted node can no longer be forged (parser#16).** Minted blank
  nodes used to be marked by a leading NUL until the parse ended, and an
  author could write that NUL as `&#0;`. An author label `_:&#0;genid-0` then
  merged with a minted node, and in scoped mode `_:&#0;x` escaped the
  document's blank-node scope. Every blank node of the walk now carries a
  provenance tag set where it is created: author labels where `_:` CURIEs are
  read, minted ones in `fresh_blank`. So document text cannot choose a tag,
  and author labels always go through the labeler.
- **Behaviour change (sparqlconf#14): `sameTerm` follows the store's term
  identity.** BGP matching, joins, `DISTINCT` and `GROUP BY` use that same
  identity. `sameTerm("abc", "abc"^^xsd:string)` is now false (the store
  keeps the two as distinct terms), and language tags compare as written
  (`sameTerm("x"@en, "x"@EN)` is false, as before). Previously sameTerm
  alone folded `xsd:string`, so `FILTER(sameTerm(?m, ?n))` over a join
  disagreed with the join itself.
- New tests: `tests/term_roundtrip.rs` (13 tests: every reader's language
  tags and labels, a generated-document round-trip property over all eight
  readers, redo replay and incremental restore of odd stored terms, the
  review's LOAD repro, UPDATE vs loader literals, `gexport` → `gbuild` via
  the binaries, `gbuild` exit 3, the IRI()/STRLANG() injection, CONSTRUCT
  over HTTP and through `gquery`, result serializers, sameTerm vs matching;
  plus, with the `rocksdb` feature, `gbuild --rocksdb` exit 3 for the bulk
  and `--incremental` builds).
  There are also module tests in `model::term`, `parser::ntriples`,
  `parser::nquads`, `parser::rdfa` and `parser::sparql::lexer`. Every test
  fails on the unfixed tree.

## GAP_PLAN status line
none

## Docs notes
- README.md (format/import section; the `gexport` → `gbuild` downgrade path
  near "To go back, `gexport` with the new binary and `gbuild` with the old
  one"): the import paths now normalise language tags (`zh_CN` → `zh-CN`,
  empty → no tag) and refuse other malformed tags. SPARQL UPDATE refuses the
  numeric literals the tolerant loader refuses. `gbuild` exits 3 when it
  skipped statements: check for it in migration scripts, and rebuild with
  `--strict` to keep ill-typed literals that a strict import or an older
  version stored.
- README.md (SPARQL semantics): `sameTerm` is the store's term identity
  (`"v"` and `"v"^^xsd:string` are two terms). `IRI()` / `URI()` /
  `STRLANG()` / `STRDT()` reject text that is not an IRI or a language tag.
- docs/DEPLOY.md (the tools' exit-status sentence near "the exit status is
  `0` on success, `1` …"): `gbuild` also exits `3` (built, statements
  skipped; see `parse_error.log`). No new configuration key and no
  upstream-compat switch. `scripts/conformance.sh` needed none.
- docs/STORAGE_BACKEND.md (redo log / incremental backup format): records of
  well-formed terms are unchanged. A record of a blank-node label or language
  tag that the strict readers refuse (only an older version could store one)
  writes that label or tag with `\uXXXX` escapes; older binaries could not
  replay such a record at all. No on-disk format change for snapshots, disk
  or RocksDB stores (D7: dictionary keys are untouched, so `Term`'s
  `Display` stays the stored key form).
- docs/REFACTOR_BACKLOG.md:
  - sparqlconf#14 (remainder): RDF 1.1 folds `"v"^^xsd:string` into `"v"`,
    and the DAWG test lang-case-insensitive-eq expects `"x"@en = "x"@EN`. The
    store keeps both forms as written (exact surface keys, like upstream), and
    sameTerm now agrees with that. Folding would need a canonical key
    encoding, possibly behind a compat switch, plus a rewrite or dual lookup
    for existing databases (D7). Deferred: a format decision, not a bug fix.
  - parser#1 (legacy data): terms that an older version stored unchecked
    (`"x"@zh_CN`, `_:a b`) stay stored as they are. Output and exports
    normalise them (`zh-CN`, a hashed label), so an export of such a database
    reloads them in normalised form. An in-place migration command could
    rewrite them; none exists.
  - sparqlconf#11 (remainder): `IRI()` does not resolve a relative IRI
    against the query's `BASE`. `STRLANG` / `STRDT` do not check that their
    first argument is a simple literal. These are SPARQL-function semantics
    for the round-3 SPARQL-functions package.
  - query#15 (design): `ResultSet` still carries cells as dictionary-key
    strings that the serializers decode. Carrying `Term`s or ids would
    remove the decode, but it is a cross-cutting `ResultSet` API change
    (D3).

## Measured numbers
- Bench smoke gate, base b8b1c8e vs branch, measured one after the other in
  the same session. Release builds of `gbuild` + `gstore-bench`
  (`cargo +1.99.1 build --release --locked -p gstore -p gstore-bench --bin
  gbuild --bin gstore-bench`), `GSTORE_BENCH_CACHE` warm, command
  `target/release/gstore-bench ci --baseline bench/baselines/smoke.json`.
  Both PASS, 456 gates checked, 0 failed. Time geomean vs the committed
  baseline, base → branch:
  - LUBM-like snapshot 1.02 → 1.03
  - LUBM-like disk 1.06 → 1.08
  - WatDiv-like snapshot 1.15 → 1.17
  - WatDiv-like disk 1.22 → 1.24
  (bound 1.50). The engine change is limited to four built-in arms and
  `same_term`; the result serializers decode each cell once, as before.
  Machine state: other worktrees' builds and tests were running (load
  average 9 to 11 on 4 CPUs), so the figures are pessimistic, and the
  base/branch differences (up to 0.02) are within run-to-run noise. No
  complexity fix in this package (D9: no scaling test).
- `scripts/conformance.sh --no-build` on the branch (dev binaries built with
  `cargo +1.99.1 build --bins`): all six suites pass. These are basic,
  update, transaction, api, parser (only the two known cases 15 and 17
  fail) and python (sdk_check 49 passed). No compat switch was needed.

## Requests/Not yet
- Edits outside the owned files (smallest change each):
  - `src/db/mod.rs::exec_update_op`: one `import::check_update_literals(…)?`
    line in the `InsertData` arm and one in the `Modify` arm, before
    anything is applied (sparqlconf#12). The plan granted only
    `record_redo` / `apply_redo_records`, but UPDATE has no other choke
    point. R4's r4-db-graph owns the graph-management arms of the same
    function; these two arms are disjoint from them. The `use` line at the top
    of the file imports `record_key` instead of `export_key`.
  - `src/parser/stream.rs::BlankLabeler::explicit`: one line,
    `safe_blank_label` before the existing match (parser#1).
  - `src/parser/turtle.rs`: `is_blank_label_char` delegates to the shared
    rule, and the `@lang` branch of the literal parser checks
    `is_valid_lang_tag` (parser#1). r3-parser owns turtle.rs in round 3.
  - `src/bin/gquery.rs` (`Construct` arm of the output match) and
    `src/console/local.rs` (`Construct` arm of `run_query`): `to_ntriples`
    instead of `Display` (sparqlconf#11).
  - Doc comments only: `src/query/value.rs`'s module docs (UPDATE no longer
    stores every ill-typed literal), `src/zipio.rs::write_export` and
    `src/db/admin.rs::export_ntriples` (every term, not only IRIs, goes
    through `export_key`).
  - Tests changed per convention 13b, each with a `review <id>` comment:
    `tests/dt_phase3_review_fixes.rs` (the one sameTerm assertion that
    pinned the xsd:string folding, sparqlconf#14);
    `tests/dt_bulk_disk.rs::gbuild_disk_uses_the_bulk_build` (two exit-status
    assertions: the input's `not rdf` line now gives exit 3,
    sparqlconf#12);
    `tests/dt_rdf_import.rs::cli_tools_take_directories_are_tolerant_and_have_strict`
    (one exit-status assertion, now 3, sparqlconf#12);
    `tests/dt_sparql_semantics.rs::typed_db` and
    `tests/dt_sparql_phase3_combo.rs::numbers` (fixture setup only: the
    ill-typed literals now come in through a strict `build_from_str` import
    instead of `INSERT DATA`, which refuses them; what the tests assert is
    unchanged, sparqlconf#12);
    `tests/dt_merge4_fixes.rs::a_language_tag_with_a_tab_is_retracted`
    (setup only: the tab-holding tag is inserted with `insert_triple`, since
    `STRLANG` now refuses it, sparqlconf#11).
- Divergences for other packages (sparqlconf#12's literal rule covers SPARQL
  UPDATE, as planned; these write paths still store such literals, and
  `gbuild`'s exit status 3 now flags them when an export is reloaded):
  the REST server's `POST /batchInsert` (`src/http_api.rs`'s
  `handle_batch`, which applies parsed triples through
  `Database::try_insert_triple`; r3-rest-parity's file in round 3) and the
  library's `Database::insert_triple`. (The `/api` `batchInsert` is a
  tolerant import and already applies the rule.) `gadd` is tolerant like `gbuild` but still
  exits 0 when it skipped statements (CLI exit statuses: r4-cli-config).
- Divergence for another package: `engine.rs::render_pattern_term` (the
  `SERVICE` query text, r3-sparql-joins' `eval_service` area) writes a
  constant term with `Display`, the stored form. A stored IRI holding `>`
  (only an older version or a lenient reader could store one) would reach
  the remote query unescaped. `Term::to_ntriples` is the fix.
- Deviation from the plan's wording: the plan said "Term Display escapes".
  `Display` is `dict_key`, the stored key of every term in every database,
  and it is also the cell format of `ResultSet` and synthetic-id rendering.
  Making it escape would change stored keys (D7) and break lookups of every
  existing IRI that holds an excluded character. The escaping serializer is
  therefore the separate `Term::to_ntriples`, and every N-Triples output
  goes through it.
