# Changelog

## Phase 5 — RDF I/O, gconsole and operations (GAP_PLAN §4.2, §5)

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
  so a missing `>` costs only its own line; so does a short string missing
  its closing quote (it ends at the line break). After a bad statement in a
  TriG file, a `{` or `}` on the rest of its line still opens or closes the
  block, so no statement is filed under the wrong graph.
- **RDF/XML.** Internal DTD entities, with expansion budgets (every expanded
  entity value counts); a value may refer to an entity declared after it;
  external entities are refused. `rdf:parseType="Collection"` is supported.
- **Exports load back.** `gexport` / `/api export` write IRIs holding a
  space or another character IRIREF excludes as `\uXXXX` escapes.

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
  redirection, and a persistent history (created mode `0600`). Ctrl-C
  cancels the server's query task.
- `drop <name>` always drops a database and `clear …` clears the screen, as
  upstream; SPARQL's `DROP` / `CLEAR` / `CREATE` are recognised only in
  forms the commands cannot have (`DROP GRAPH <g>`, `DROP SILENT ALL`, …).
  `sparql <file>` sends a run of update operations (`op ; op`) as one
  request. `use system;` is refused for root too (it cannot be queried).
- The optional `readline` feature adds line editing and tab completion.
- `gconsole --local <db>` keeps the embedded REPL; the legacy `gconsole <db>`
  still works. Its `import` command now accepts any RDF format, a directory
  or a URL.

### Zip packages and export (feature `zip`)

- New optional cargo feature `zip` (zip 9, deflate only, pure Rust); the
  default build gains no dependency. `src/zipio.rs`.
- `/api` `build` / `batchInsert` / `batchRemove` read a `.zip` of RDF files
  (local, uploaded or `remote=true`), unpacked as upstream does into the
  upload directory and removed afterwards; every entry needs an allowed RDF
  extension. Errors carry upstream's zip codes (1–6).
- Safety: a package is checked in full before anything is written. Absolute
  or `..` entry names are refused (zip-slip), symlink entries ignored,
  encrypted entries refused. New `[upload]` keys `unzip_max_entries`
  (default 10000) and `unzip_max_size` (default 10 GiB, also enforced on the
  bytes actually written).
- `backup compress=true` writes `<db>.db_<timestamp>.zip` (upstream's
  layout); `restore` takes it (1005 when it does not unpack); rotation and
  the "newest backup" choice include zips.
- `export` gains `format` (`nt` / `nq`); `compress=true` writes a `.zip`.
  *Behaviour change:* without `format`, a database with named graphs is
  exported as N-Quads (`.nq`) instead of N-Triples that dropped them.
- CLI: `gexport --format nt|nq` and `-z` / `out.zip`; `gbackup -z` /
  `x.zip`; `grestore` of a `.zip`; `gbuild`, `gadd` and `gsub` of a `.zip`
  package (upstream 1.3's `-f x.zip`). `--unzip-max-size` /
  `--unzip-max-entries` change the CLI's limits (`0` = none); `grestore`
  has none by default, so any `gbackup -z` archive restores. `gconsole`'s
  `restore` passes a zip path to the server like a directory.
- Without the feature every zip path answers `1003` (the CLI exits 1),
  naming the feature.

### Diagnostic, query and access logs

- `src/logging.rs`, no new dependency: levels `error`…`trace`, standard
  error and a file rolled at local midnight into `FILE.YYYY-MM-DD`, with
  retention in days. Configured as upstream through `conf.ini`'s `log_mode`
  (a log4cplus properties file; upstream's two are shipped as
  `conf/slog*.properties.example`), plus the keys `log_level`, `log_file`,
  `log_days`, `log_stderr` and the `GSTORE_LOG` variable. As log4cplus,
  `MaxBackupIndex` deletes no daily file (`MaxHistory` / `log_days` set a
  retention), and the file only ever rolls forward.
- `gserver`'s diagnostics go through it (handler panics, accept failures,
  failed saves; each request at `debug`); its startup lines on standard
  output are also recorded in the file.
- The REST `/sparql`, `/update` and `/tquery` now write upstream's JSON
  query-log record (client IP, answer count, latency, status, format,
  database), so `/api querylog` lists them. Each line of
  `<db_home>/access.log` gains the client IP as a sixth field.

### Legacy command-line tools

- All ten tools take upstream 1.3's options (`-db`/`--database`,
  `-f`/`--file`, `-p`/`--path`, `-q`, `-l`, `-z`, …) besides their own,
  answer `-h`/`--help`, and exit `0` (done), `1` (failed) or `2` (invalid
  arguments).
- *Behaviour change:* `grestore` takes the database first, then the backup,
  as upstream; the old order fails with a hint and restores nothing.
  `grestore -l NAME` lists its backups.
- `gshow` with no argument lists every database (upstream's
  database/creator/built_time table); `gshow NAME` still shows its counts.
- `gbuild` without a source builds an empty database, and refuses to
  overwrite an existing one unless `--force`. A failed build removes the
  database directory it created, as upstream.
- `gexport -db NAME [-f DIR]` writes `<db>_<timestamp>.nt` (`.nq` / `.zip`
  as chosen); `gbackup -db NAME [-p DIR]` creates
  `DIR/<db>.db_<timestamp>` (default `./backups/`).

### Packaging

- `Dockerfile` (builder on the MSRV toolchain, `debian:bookworm-slim`
  runtime, user `gstore`, port 9000, volumes `/var/lib/gstore` and
  `/etc/gstore`, a health check on `/api?operation=check`, `--build-arg
  FEATURES`), and `.dockerignore`.
- `packaging/systemd/gserver.service` (graceful stop by `SIGTERM`,
  `Restart=on-failure`, hardening), sysusers.d / tmpfiles.d entries, and
  cargo-deb / cargo-generate-rpm metadata with maintainer scripts.
- `docs/DEPLOY.md` covers the container, the service, the packages, the
  configuration and logs, and the tools.
- `gserver` refuses to listen beyond loopback while `root` still has the
  default password `123456`; `--password-file FILE` /
  `$GSTORE_ROOT_PASSWORD_FILE` set root's initial password without putting it
  on the command line (the container's way).

### CI

- The feature matrix builds, clippies and tests `gzip`, `tls`, `zip` and
  `readline`.

### Not yet

- The Docker image and the deb / rpm packages were built and linted but not
  yet installed and run on a live host in CI.
- `gconsole`'s `backup` / `export` have no compress or format switch (as
  upstream's console); use the `/api` or `gbackup` / `gexport`.
- The legacy REST payload endpoints (`/upload`, the `/batchInsert` body)
  still parse whole and drop graph labels.
- JSON-LD `@graph` with an `@id` is flattened into the default graph.
