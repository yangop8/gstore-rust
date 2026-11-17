# r4-api-hygiene — Shared request helpers, API/HTTP docs, unloaded-database statistics, lock scope of handle creation, apilog robustness, storage-backend conformance suite and test organisation (design#17)

## Changelog

Behaviour changes first.

- **`gserver` built without the `tls` feature refuses `--tls-cert` /
  `--tls-key`** (review http#19): `Cli::parse` returns an argument error
  ("--tls-cert/--tls-key need a gserver built with the `tls` feature …"), so
  `main` exits with its argument-error status and nothing is served. Before,
  it printed a warning and served plaintext HTTP, while a `tls` build refuses
  a cert without its key.
- **`/api` takes no parameters from a raw SPARQL body** (review design#17): a
  body labelled `application/sparql-query` or `application/sparql-update` is
  never read as a form any more, so text inside it such as
  `&sparql=DELETE WHERE {…}&db_name=other` can no longer replace the
  statement or the database of the URL (the parity test ran that DELETE on
  the other database before). JSON, form and multipart bodies are read as
  before; a body with another or no `Content-Type` keeps upstream's sniffing.
- **`/api` `show` lists every database the system registry finds** (review
  gapi#15): it scanned the root with `/api`'s own name rule
  (`check_db_name`, names of at least three bytes), so a database REST
  `/build` or `gbuild` created as `ab` was in REST `/show` and
  `system.json` but missing from `/api show`. It now uses the registry's
  scan (`sysdb::database_dirs`) plus its storage check. `/api` operations
  on such a name still answer upstream's `1003` (see Docs notes).
- **The transaction log is rotated at `TXN_LOG_MAX_BYTES` (32 MiB)** and read
  incrementally (review gapi#18): an append that would take
  `logs/transaction.json` past the cap moves it to `transaction.json.1`
  (replacing an older generation; hard-linked first, so the log is never
  missing) and starts a new file holding the records of the transactions
  still running. `txnlog` lists the transactions of the current generation.
  A `txnlog` read parses only the lines appended since the previous one
  (the server keeps where each transaction's last record is — about 40
  bytes per transaction — not the records), then the records of its page;
  before, every call read and parsed the whole file (about 8× its size in
  memory per call).
- **A torn last line of an API log no longer swallows the next record**
  (review durability#13): an append to the transaction, query or access log
  first ends an unterminated last line (a crash, a full disk, a zero-filled
  tail after a power loss), so the torn line stays one unparsable line,
  which readers skip, and the next record is kept. Before, the first
  transaction record after a restart could be lost and its transaction
  listed as `RUNNING`.
- **`monitor` / `schema` of an unloaded database are answered from cached
  statistics** (review gapi#17): the first request reads the database (as
  before); later ones walk its directory — the walk `diskUsed` needs anyway —
  and reuse the statistics while the directory shows the same files (count,
  total size, latest modification of any file or directory, and a digest of
  each one's path, inode, size and modification time, so a directory
  swapped for a look-alike — same names, sizes and timestamps — is read
  anew). Before, every
  request opened the database (or read its whole snapshot), made a pass over
  its data and held its name busy meanwhile, refusing a concurrent `load`
  by root and a second `monitor` (1007).
- **Creating a database handle no longer holds the server-wide state lock
  during the directory walk** (review http#13, conc#7): REST `/load`,
  `/build`, `/upload` (into an unloaded or a new name), `/restore`, the
  reopen of a broken store, a failed rebuild's re-serve, and `/api`
  `load` / `restore` lock the database directory and measure its size
  (`dir_size`, a readdir and a stat per file) before taking the lock;
  only the insertion of the handle (with the `LOAD` policy and query
  settings in effect then) runs under it. `/status`, `/cancel`, logins and
  every other request no longer wait for a walk of a RocksDB store's
  thousands of files. `DbSlot::handle` is split into `DbSlot::prepare` and
  `PreparedSlot::serve`.
- **One form / URL decoder and one base64 decoder** (review gapi#15,
  robust#16, design#13): the new public module `gstore::util` holds
  `util::form` (`url_decode`, `url_decode_bytes`, `parse_form` — every
  pair, the `/api` reading — and `form_lookup` — one name, the REST
  reading) and `util::base64` (`decode(s, Rules)` with `BASIC_AUTH` and
  `LENIENT`, a typed `Base64Error`). `server::form_get` / `form_lookup` /
  `base64_decode`, `gapi::params::parse_form` / `url_decode` and
  `pfn::url_decode` / `pfn::base64_decode` delegate to it, behaviour
  unchanged (public APIs kept, D2). The duplicated JSON reader of
  `http_client` (robust#16's other half) was removed in round 1 (H-3).
- Docs: the `http_api` module docs described real TLS as out of scope "as
  with `crate::server`"; they now describe the in-process TLS of the `tls`
  feature (`ApiServer::serve_forever_tls`, `load_tls_config`, `gserver
  --tls-cert/--tls-key`) and the reverse proxy for the default build. The
  doc comment of `retire_loaded` sat above `open_db` (rustdoc joined both
  onto `open_db`); it is back on `retire_loaded` (review http#19).
- New tests, named by behaviour (design#17): `tests/api_hygiene.rs` (handle
  creation off the state lock on every path, torn log tails, txnlog
  rotation and incremental reads, unloaded statistics read once per
  layout, the `tls` argument error, the shared decoders, the `/api show`
  scan); `tests/storage_backend_conformance.rs` (one generic suite over
  `TripleStore`, `Backend::Memory`/`Disk`/`Rocks`, `DiskTriples`,
  `DiskStore`, `DiskSource`, `OverlayView`, `ShardedStore` and
  `RocksStore`: the model in contract order, the write answers, and
  contract C1); `tests/rest_api_parity.rs` gains the Content-Type parity of
  REST, the single-database server and `/api`, and the deepest allowed
  statements run as `async=true` operations.
- Test hooks (`#[doc(hidden)]`): `http_api::debug_park_dir_size` (park a
  directory measurement), `ApiLogs::txn_log_bytes_parsed`,
  `gapi::unloaded_stats_reads`.

## GAP_PLAN status line

none

## Docs notes

- **docs/REFACTOR_BACKLOG.md §F** (review http#19): the "缺" line lists
  HTTPS/鉴权、内容协商、流式响应 as missing, and says SPARQL `SERVICE` only
  parses and errors. All of these exist: HTTPS (feature `tls`,
  `serve_forever_tls`, `gserver --tls-cert/--tls-key`; a build without the
  feature refuses the options), authentication (HTTP Basic, `/login`
  sessions, per-database privileges of `sysdb`), content negotiation (four
  SPARQL results serializations, `ResultFormat::negotiate`), streaming
  (`?stream=true`, chunked) and `SERVICE` federation (with its host policy
  and limits). What remains missing there: gRPC and clustering /
  distributed query (the `cluster` module is experimental).
- **docs/REFACTOR_BACKLOG.md §I.5** (review gapi#17): add that an unloaded
  database's `monitor` / `schema` statistics are now cached until a walk of
  its directory shows its files changed; the remaining I.5 item (a loaded
  database whose statistics were never computed computes them on the
  request path) stays.
- **docs/REFACTOR_BACKLOG.md, new entries (D3/D8 remainders):**
  - gapi#15, design#13 (duplicates kept, with reasons): `civil_from_days`
    date math in `gapi/ops_query.rs`, `sysdb.rs` and `query/value.rs` (and
    the UTC-vs-local naming split between `ops_query::compact_utc_now` and
    `ops_file`/`sysdb::clock`: changing file names is a visible change);
    REST `/show`'s own directory scan (`http_api::handle_show` lists any
    `<name>.db` directory, also one holding no database — R4 left
    `handle_show` untouched; it should use `sysdb::database_dirs` plus the
    storage check like `/api show`); the `not_loaded` answers of `ops_db`
    and `ops_reason` (the codes that differ are deliberate upstream parity,
    pinned by `dt_gapi_core::query_on_missing_or_unloaded_databases`); the
    two console pages (`server.rs` and `http_api.rs`, deliberately
    different pages, a mode flag would merge them); the hand-written HTTP
    clients (`http_client.rs`, `console/client.rs` — only the latter
    decodes chunked) and `gnlqa`'s own HTTP server (`gnlqa/src/http_server.rs`,
    r5-gnlqa's area) and the ~20 hand-written test clients (tests-a#16).
  - design#17 / design#4: REST accepts database names of 1–2 characters
    (`valid_db_name`) that `/api` operations refuse (`check_db_name`,
    upstream's "length at least need 3"); both surfaces now list them, but
    one name rule for creation is still to be chosen (refusing them on REST
    is a visible change for existing databases).
  - design#17 part 3: consolidate the round-named test files by behaviour
    (`dt_merge4_fixes`, `dt_merge4_wiring`, `dt_merge5_review`,
    `dt_phase0_review`, `dt_phase3_review_fixes`, `dt_phase4_review_fixes`,
    `dt_r2_integration`, `dt_r3_integration`, `dt_r4_merge`,
    `dt_review_fixes`, `dt_p1_review`, `dt_security_review`, and
    `src/db/{review_tests,r4_review_tests,stream_review_tests}.rs`),
    keeping the history in commit messages; and fold the per-backend C1
    helpers (`dt_count_methods.rs::check_counts`,
    `dt_rocksdb.rs::assert_counts_equal_the_defaults`,
    `kvstore/store.rs::assert_c1_overrides_match_defaults`,
    `dt_overlay.rs::check_same`) into `tests/storage_backend_conformance.rs`.
    Not done here: D6 deletes nothing, and the moves touch files other
    packages own this round.
  - gapi#18: the daily query / access logs are still read whole per
    `querylog` / `accesslog` request (bounded by one day and the retention
    count); an index like the transaction log's would make them
    incremental too.
- **docs/DEPLOY.md** (logs section): the transaction log rotates at 32 MiB
  into `logs/transaction.json.1` (one older generation kept; transactions
  still running are carried into the new file); `txnlog` lists the current
  generation. No compat switch was added.
- **README.md / docs/DEPLOY.md** (HTTPS): a `gserver` built without the
  `tls` feature now refuses `--tls-cert` / `--tls-key` with an argument
  error instead of serving plaintext HTTP.
- **README.md** (library): `gstore::util::{form, base64}` are the shared
  decoders behind the REST endpoints, `/api` and PFN.

## Measured numbers

- http#13/conc#7: no timing claim; the test parks the directory walk with
  a hook and proves `/status` and `/api show` are answered meanwhile on all
  seven handle-creation paths (each hung until the 30 s hang guard before
  the fix).
- gapi#18: `txnlog_reads_parse_only_what_was_appended_since_the_last_read`
  — after 200 transactions (50,384 bytes), a second read with nothing
  appended parses 0 bytes (before: 50,384 again) and a read after one
  appended begin record parses only that record's bytes. The rotation test
  fills the log to the 32 MiB cap and checks the file shrinks to the
  carried-over records.
- gapi#17: `unloaded_*_statistics_are_read_once_until_the_files_change` —
  seven `monitor`/`schema` requests of an unchanged unloaded database read
  it once (before: 7), for the snapshot and disk layouts (and RocksDB with
  the feature).
- Backend conformance suite: verified locally that it fails when one
  backend breaks a C1 rule (an off-by-one put into
  `DiskSource::distinct_p_of_s`, and one into `TripleStore::count_o_by_sp`;
  both reported as "C1 …", then reverted).
- Bench smoke gate (D9; the REST parameter lookup now calls
  `util::form::form_lookup`, the same code moved; the HTTP read path is
  otherwise unchanged): `cargo build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench`, then `flock
  /tmp/gstore-bench.lock env GSTORE_BENCH_CACHE=/home/user/fix-campaign/bench-cache
  target/release/gstore-bench ci --baseline bench/baselines/smoke.json` on
  the branch head: `Gates: PASS (456 checked, 0 failed)`, calibration
  score 65.21 ms. Shared 4-vCPU machine with other packages' builds and
  tests running (load average 6.5-7.8); branch only — no base-vs-branch
  run, as no measured path changed.
- `scripts/conformance.sh` (dev build): every suite passes (basic,
  update, transaction, api, parser with its known cases 15 and 17, python
  49/49); no compat switch added.
- Workspace tests on the branch head: 2961 passed, 0 failed in each of
  `GSTORE_DISK_QUERY_MODE=memory` and `stream`; `cargo test -p gstore
  --features rocksdb`: 2826 passed, 0 failed (clippy clean with the
  feature); `--features tls`: clippy clean, `gserver` unit tests,
  `api_hygiene`, `http_preauth_limits`, `rest_api_parity` pass; workspace
  clippy `-D warnings` and `RUSTDOCFLAGS=-D warnings cargo doc -p gstore`
  clean.

## Requests/Not yet

- Edits outside the owned files (for the integrator):
  - `src/gapi/params.rs` `Params::from_parts`: the raw-SPARQL-body rule
    (one `matches!` and one condition) and a module-doc paragraph;
    `parse_form` / `url_decode` delegate (sanctioned).
  - `src/gapi/mod.rs`: one `#[doc(hidden)] pub use
    ops_db::unloaded_stats_reads;` line after the `stats_refresh_hook`
    re-export.
  - `src/sysdb.rs`: `database_dirs` made `pub(crate)` (+ one doc
    sentence).
  - `src/pfn/mod.rs`: `url_decode` and `base64_decode` delegate to
    `gstore::util` (their private `hex` helper is gone); r4-ext-docs edits
    other parts of this file.
  - `src/lib.rs`: `pub mod util;` (convention 6).
  - `src/http_api.rs`: besides the granted regions, two doc links that
    named `DbSlot::handle` (the `ApiState::query_settings` field doc and
    `ApiInner::apply_query_settings`' doc) now name `PreparedSlot::serve`,
    since `DbSlot::handle` is test-only now.
  - Two code comments that named `DbSlot::handle` as where a served
    database learns its reasoning home now name `DbSlot::prepare`: one in
    `src/gapi/ops_db.rs` `backup()` and one in `src/gapi/ops_reason.rs`
    `file_store()` (one comment line each).
- design#17 part 3 (consolidating round-named test files) and the
  remaining duplicates of gapi#15/design#13 are backlog entries (Docs
  notes), not done.
