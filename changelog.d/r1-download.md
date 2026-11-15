# r1-download — File download/export: authenticated and owner-checked by default, streamed, unguessable names, retention cleanup

## Changelog
- **Behaviour change (secure default, review sec#4, gapi#2):** `/file/download`
  (and `/grpc/file/download`) needs credentials — `username` / `password`
  parameters as before, or an HTTP `Authorization: Basic` header. A request
  without them is answered HTTP `401` with `WWW-Authenticate: Basic` and a
  `1001` envelope as the body, before anything is resolved. The new
  upstream-compat switch `[upload] anonymous_download=on` restores upstream's
  public export directory (every file of it, to anyone); `gserver` then
  prints `upstream-compat: anonymous_download=on` at startup.
- **Behaviour change (sec#4, gapi#14):** every file the server writes for a
  client — an upload, an export, a `format=file` / `n-triple-file` query
  result — gets an owner record in `<db home>/file_owners/` (path, kind,
  database, user; never served). A user other than `root` / `admin` reads
  such a file — downloads it, or names it as a `build` / `batchInsert` /
  `batchRemove` source (`file` or a file of `dir`) — only when the record
  allows it: an export with the `export` privilege on its database, a query
  result with `query` on it, an upload when he uploaded it. Files in the
  export and upload directories without a record (put there by an operator,
  or written by an older version) are `root`'s / `admin`'s alone. A refused
  download is upstream's `FileNotFound` (`StatusCode` 11); a refused source
  is `1003` (for a `dir`, the answer names the directory only, never the
  unreadable file in it, so an export's random name does not leak).
- **Behaviour change (sec#4, gapi#2):** export and query-result file names
  carry 128 random bits: `<db>_<YYYYmmddHHMMSS>_<32 hex>.nt|nq|zip` and
  `<db>_<threadId>_<UTC YYYYmmddHHMMSS>_<32 hex>.json|txt`. Clients use the
  reported `filepath` / `FileName`, so the wire format is unchanged.
- **Downloads are streamed (gapi#4, robust#7, sec#11):** the file (or the
  `Range`) is copied to the connection in 64 KiB chunks, with its
  `Content-Length`. Before, each download read the whole file into memory and
  copied it again, so it needed about twice the file's size; backups and
  database files were downloaded the same way. A streamed download is never
  compressed, even with the `gzip` feature and an `Accept-Encoding: gzip`
  request (its compressed length is unknown in advance, and a range counts
  the file's bytes). JSON envelopes are compressed as before. New key
  `[upload] download_max_concurrent` (default `8`, `0` = no limit) limits how
  many downloads stream at once. Further downloads are answered `503` with
  `Retry-After: 1`.
- **`format=file` results are streamed (gapi#11):** `format=file` and
  `n-triple-file` results are written row by row through a buffered writer.
  The whole result is no longer built as a JSON tree and a string first. The
  output is byte-identical (unit-tested against the old serializers).
- **Retention (sec#14, gapi#13):** new key `[upload] file_retention_hours`
  (default `168`, `0` = keep forever). Recorded uploads, exports and query
  results older than this are removed together with their records. Records
  whose file is gone are dropped. Operator files are never touched. Spilled
  asynchronous results under `<root>/tasks` that belong to no task of the
  running server are removed too: these are the results left by an earlier
  run, which were orphaned on every restart or crash. The sweep is lazy, with
  no new thread. It runs on uploads, exports, downloads, `format=file`
  queries and asynchronous submissions, at most once a minute per server.
  `TaskManager::remove_orphan_results` is the new library entry point for
  the task half.
- **Upload quota (gapi#14, sec#14):** new key `[upload] upload_user_quota`
  sets how many bytes of retained uploads each user may keep (default `0`,
  no quota). An upload beyond it answers `1005 "Upload quota exceeded: …"`.
  Uploads stay open to every user, as upstream's privilege table says.
- Library: `gapi::envelope::FileBody` (a file region with an optional permit
  held until it has been sent), `GBody::File`, `GResp::file`,
  `GResp::into_http` and `HttpBody`. `GResp::into_parts` keeps its signature:
  it reads a file body into memory.
- New tests: `tests/gapi_download_policy.rs`, 12 tests plus a `gzip`-feature
  variant. Memory is measured with a counting global allocator.
  `envelope::tests::file_bodies_stay_streamed_and_release_their_permit`,
  `server::streamed_response_tests`, and
  `ops_query::result_file_tests::streamed_result_files_match_the_buffered_documents`.

## GAP_PLAN status line
none

## Docs notes
- docs/DEPLOY.md, secure-defaults / upstream-compat table: `anonymous_download`.
  - Home section: `[upload]`. Secure default: `off`. Upstream value: `on`.
  - Risk when on: anyone who can reach the port and guesses or learns a file
    name downloads whole-database exports and `format=file` query results.
    With the random names this needs a leaked name. Where the CORS policy
    lets an origin in, a page from that origin can also read the file
    through a visitor's browser.
  - `scripts/conformance.sh` turns it on: upstream's `api_test.sh` downloads
    its export anonymously with wget.
  - `gserver` warns `upstream-compat: anonymous_download=on` at startup.
- docs/DEPLOY.md: downloads need credentials. Use `?username=…&password=…`,
  a POST body, or `curl -u user:pass` / `wget --user --password` (HTTP Basic).
  The rules for who may read which file are in the first Changelog bullets.
  Owner records live in `<db home>/file_owners/`: back them up with the
  database home. Without its record, an export or upload is readable by
  `root` / `admin` only.
- docs/DEPLOY.md: the new `[upload]` keys and their defaults:
  - `download_max_concurrent=8`. Each streaming download holds a worker of
    `thread_num`.
  - `file_retention_hours=168`. Exports and query results are deleted after
    a week by default: copy what must be kept out of the export directory.
  - `upload_user_quota=0`.
- docs/DEPLOY.md / README.md (`gzip` feature): downloads are sent
  uncompressed; only buffered responses (envelopes, query results) are
  compressed.
- README.md (~line 435, "public export directory"): the export directory is
  no longer public by default. Describe the owner-checked download and the
  `anonymous_download` switch. Export and query-result names now carry a
  random part.
- README.md / docs (async operations): spilled async results left by an
  earlier run are removed by the lazy sweep.
- docs/REFACTOR_BACKLOG.md:
  - Owner records are kept per file, not in a transactional registry. The
    quota check and the upload are not atomic: two concurrent uploads of one
    user can both pass the check. Accepted for a soft quota. (sec#14,
    gapi#14)
  - The orphan rule for `<root>/tasks` assumes one server per root, which
    gserver's pid-file check enforces ("the server already running"). Two
    `ApiServer`s on one root in one process would remove each other's
    spilled results. (gapi#13)
  - Owner records are written with a plain `fs::write`, neither fsynced nor
    renamed into place. After a crash a file can lose its record; it then
    fails closed (root / admin only, kept by the sweep like an operator
    file), and an unparsable record is skipped by the sweep. (sec#4,
    sec#14)
  - A refused `build` / `batch*` `file` says "refused", a missing one "not
    exist": a user holding `update` somewhere can test whether another
    user's upload of a guessable name (`<stem>_<YYYYmmddHHMMSS>`) exists,
    without reading it. Exports and query results have random names, and a
    download answers `FileNotFound` either way. (gapi#14)
  - Streaming `format=file` removes the serialization multiplier. The
    `ResultSet` itself is still fully materialized and uncapped for the file
    formats, so a huge result still costs its own size. Streaming rows from
    the evaluator into the file is a larger refactor of the query path.
    (gapi#11)
  - A streamed download with `Accept-Encoding: gzip` is sent identity-encoded.
    On-the-fly compression would need chunked framing, which an HTTP/1.0
    client cannot take, and the request version is not kept in `Request`.
    (gapi#4)

## Measured numbers
- Memory: `cargo test --test gapi_download_policy -- --nocapture`. The
  counting global allocator measures peak heap above the start of the
  request, for the whole process. Debug test profile. Base is 7bbf4ae, with
  the new test file run against the unfixed sources via `git stash`.
  - 64 MiB download (root, export directory): base 201,344,287 B peak (3.0x
    the file), branch 83,775 B. Asserted bound: < 8 MiB.
  - Ranged download of 40,000,000 B: branch < 8 MiB (asserted).
  - `format=file` result of 20,000 rows (4,077,846 B file): base 27,669,394 B
    peak (6.8x the file), branch 5,636,642 B (1.4x: the `ResultSet` itself).
    Asserted bound: < 3x the file.
- Bench smoke gate (D9): `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=… gstore-bench ci --baseline
  bench/baselines/smoke.json`. Base binaries were built from 7bbf4ae's
  sources (verified identical to the base commit), branch binaries from
  c4b9dd3. Both ran back to back in one session, from the worktree (so
  both reports name the worktree's commit), while other agents' builds and
  this package's feature tests loaded the 4-vCPU machine (load about 8-9).
  - Calibration score: base 70.6 ms, branch 90.3 ms.
  - Both runs: `Gates: PASS (456 checked, 0 failed)`.
  - `gstore-bench compare base.json branch.json`: `Gates: PASS (456
    checked, 0 failed)`.
  - The HTTP rows are report-only and noisy here. For example
    `snapshot/api/q1/c1` reached 9,192 qps on base and 12,471 qps on the
    branch, and `disk/api/q1/c2` 11,144 and 9,441 qps.
  - The buffered response path changed only by moving the header block
    into a helper. A quiet re-measurement is left to the round's land
    step.
- Conformance: `scripts/conformance.sh --no-build` (dev binaries) passed
  all six suites (basic, update, transaction, api, parser, python 49/49),
  with `upstream-compat: anonymous_download=on` in the start log.
  `check_compat_switches` was proven by running a copy of the script with
  the key misspelled (`anonymous_downlaod`): gserver warned about the
  unknown key and the run failed with `compat switch anonymous_downlaod
  did not take effect`, exit 1.

## Requests/Not yet
- Sanctioned edits outside the owned files, all inside the regions the plan
  grants:
  - `src/http_api.rs`: struct `Resp` (new `stream` field), `Resp::new`,
    `impl From<GResp> for Resp`, the new `Resp::stream`, and
    `ApiInner::handle`'s write (plus one `use crate::server::{…}` import
    line).
  - `src/server.rs`: `write_response_ext` now builds its head with the new
    `response_head` helper. Its signature is unchanged and the reason
    phrases for 206/416 were added. New: `STREAM_CHUNK`,
    `write_response_streamed` and its test module.
  - `src/gapi/ops_db.rs`: `export()` and `source_files()`.
  - `src/gapi/ops_query.rs`: `read_response()`, `write_result_file()`, two
    new writer fns and their test module, and the `use` lines.
  - `src/gapi/tasks.rs`: a new `impl TaskManager` block with
    `remove_orphan_results`, placed after `remove_files`, plus module docs and
    the `Path` import.
  - `src/config.rs`: four keys after `unzip_max_size`, plus the
    `every_key_is_read_from_its_section` test text and asserts. Integrators:
    other R1 packages add keys to the same test string.
  - `conf/conf.ini.example` `[upload]`.
  - `scripts/conformance.sh`: the verbatim helper block, `compat_switch
    upload anonymous_download on`, and `check_compat_switches || exit 1`.
  - `src/bin/gserver.rs` `report_config()`: one standalone warning line, to
    fold into `upstream_compat_warnings()` (r1-http-surface) after the merge.
- One-line edits outside the granted functions (integrators expect them):
  - `src/gapi/ops_db.rs` `dir_files()`: one `ops_file::check_dir_source(…)?`
    call per listed file. Without it, `batchInsert dir=<export dir>` would
    bypass the owner check.
  - `src/gapi/tasks.rs` `run_async()`: one `super::ops_file::sweep_files(ctx)`
    line at its top, away from r1-http-surface's `Ctx { .. }` literal. It
    makes async-only deployments clean `<root>/tasks` as well.
- Existing tests whose assertions pinned the old behaviour (convention 13b,
  with `review <id>` comments):
  - `tests/dt_gapi_db.rs` (owned): `export_writes_a_file_that_downloads_without_credentials`
    became `..._with_credentials`. It asserts the anonymous 401 and then
    downloads as root.
  - `tests/dt_gapi_db.rs` (owned): in `downloads_are_confined_to_their_roots`,
    anonymous requests now answer 401. Another user cannot read an operator
    file in the upload directory, and root can.
  - `tests/dt_gapi_db.rs` (owned): `the_upstream_api_test_script_runs_unmodified`
    turns `anonymous_download` on.
  - `tests/upstream_api.rs`: the export download sends root's HTTP Basic
    credentials (setup line).
  - `tests/dt_zip_io.rs`: the zip export download sends root's credentials
    (setup line).
  - `tests/dt_p1_review.rs` `asynchronous_query_results_are_not_publicly_downloadable`:
    the anonymous answer is now `(401, 1001)` instead of `(200, 11)`. The
    test gained an authenticated root download that must still answer
    `(200, 11)`, so it keeps proving the tasks directory is unreachable.
    This file is not owned by this package: a sanctioned convention-13b
    assertion edit.
- `src/sysdb.rs` is unchanged: uploads stay `Requirement::Anyone`, the
  upstream policy. The exposure of gapi#14 is closed by owner records, the
  quota and retention instead of a new privilege.
