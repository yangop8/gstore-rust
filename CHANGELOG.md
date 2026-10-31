# Changelog

## Phase 5 — RDF I/O and gconsole (GAP_PLAN §4.2, §5)

### RDF import

- **Streaming readers.** Turtle, TriG, N-Triples and N-Quads are read one
  statement at a time from a `BufRead` (`src/parser/stream.rs`). Memory stays
  bounded even for one huge TriG block. RDF/XML, JSON-LD and RDFa are still
  parsed whole, up to 256 MiB per document.
- **One import pipeline** (`src/parser/import.rs`) for every bulk load:
  `Database::build_from_*` / `insert_sources` / `remove_sources`, SPARQL
  `LOAD`, `gbuild` (memory, `--disk`, RocksDB), `gadd` / `gsub`, the `/api`
  `build` / `batchInsert` / `batchRemove`, and `gconsole --local`'s `import`.
- **Tolerant import, as upstream does it.** A bad statement is skipped and
  counted, and the reader resumes on the next line. Failures include
  upstream's numeric literal checks and invalid subjects, predicates or
  graphs. Each one is written to `<db>.db/parse_error.log` in upstream's
  format, with the line it was read from. `success_num` / `failed_num` follow
  upstream. Strict mode stays the library default; `gbuild` / `gadd` / `gsub`
  gain `--strict`.
- **Named graphs are kept** by N-Quads and TriG in the in-memory, disk and
  RocksDB backends. A blank-node graph is stored as
  `urn:gstore:bnode:<label>`. *Behaviour change:* quads are no longer
  flattened into the default graph.
- **Blank nodes are scoped per loaded document**, so two batch inserts never
  merge their `_:b`s. Generated labels round-trip through N-Triples. The
  plain `parse_*` APIs keep labels and mint `genid-<n>`.
- **Sources.** Directories (recursive, sorted, policy-checked) and URLs,
  fetched only when the load policy allows remote sources. `/api` accepts
  `remote=true` and answers 1005 when a download fails. `.gz` files need the
  `gzip` feature.
- **Turtle leniency.** Upstream's local-name characters are accepted (loader
  only). An IRI ends at a space or control character, as in upstream's lexer,
  so a missing `>` costs only its own line.
- **RDF/XML.** Internal DTD entities, with expansion budgets; external
  entities are refused. `rdf:parseType="Collection"` is supported.

### gconsole

- `gconsole` is now a client of gserver's upstream `/api` (`src/console/`).
  It implements all 43 commands of upstream 1.4's command table, plus `exit`
  and `history`.
- Output matches upstream's: PrettyPrint tables, the `Query ans num` /
  `Build RDF database` / `After inserted triples num …,failed num …` lines,
  and `RDF parse error num N` when the server's tolerant import skipped
  statements.
- Login with `-u` / `-p`, `$GSTORE_PASSWORD`, or a prompt with echo off.
  Other options: `--host`, `--port` (falls back to `conf.ini`, then 9000),
  `-c`, `-e`, `-i`.
- Interactive features: multi-line statements, `#` comments, `>` / `>>`
  redirection, and a persistent history. Ctrl-C cancels the server's query
  task.
- The optional `readline` feature adds line editing and tab completion.
- `gconsole --local <db>` keeps the embedded REPL; the legacy `gconsole <db>`
  still works. Its `import` command now accepts any RDF format, a directory
  or a URL.

### Not yet

- Zip input and N-Quads/zip export (next round).
- The legacy REST payload endpoints (`/upload`, the `/batchInsert` body)
  still parse whole and drop graph labels.
- JSON-LD `@graph` with an `@id` is flattened into the default graph.
