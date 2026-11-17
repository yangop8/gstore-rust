# r4-cli-config — CLI tools and bulk install locking, zip safety, configuration hygiene

## Changelog

- **Behaviour change: a bulk build keeps the rest of its process out of the
  directory it rebuilds** (review bulkcli#13). The bulk builds
  (`gstore::bulk::build`, disk and RocksDB) and the incremental builds
  (`Database::build_disk_from_sources_incremental`, the RocksDB
  per-triple build) checked "open in this process?" and then took the
  re-entrant directory lock in a second step, and nothing refused an open
  after that: another thread of the same process (a REST `/load` of the
  `foo.db` alias while `/build?db_name=foo` ran) shared the build's lock,
  and its acknowledged writes went to files the install then replaced.
  New `BuildGuard::lock(dir) -> (BuildGuard, DirLock)` claims the
  directory and takes its lock in one step under the lock registry's
  mutex; while the claim lives, `DirLock::acquire` (so `Database::open`,
  every save, drop or restore of that directory) on any *other* thread of
  the process is refused as a lock conflict ("… is being rebuilt by
  another bulk build of this process", `ErrorKind::Locked`, REST `409`).
  The claiming thread may still take the lock and nest builds in its
  claim. `BuildGuard::enter` / `active` are kept.
- **Behaviour change: `gbuild` claims the database directory before it
  writes anything** (review bulkcli#2). It used to decide once, at
  startup, that a new directory was its own, and removed it on any failure
  without its lock — two `gbuild` of one new name (one slowed by a zip
  unpack or a slow source) ended with the loser deleting the winner's
  directory. `gbuild` now creates the directory and claims it
  (`BuildGuard::lock`) before the parse error log is reset or a package
  unpacked, holds the claim through the build (the library builds nest in
  it), and removes a directory it created only under that claim and only
  while it holds no database. Another process holding the directory makes
  `gbuild` fail at once with status 1 ("cannot build '…': … locked by
  another process"), touching nothing.
- **`gbuild` keeps a database it built when its summary cannot be
  printed** (review bulkcli#3). The database is marked as kept as soon as
  the build (or save, or install) succeeded, before any output; the
  summary is written through a writer that records a write error instead
  of panicking. `gbuild newdb data.nt > /dev/full` used to panic (status
  101) and delete `newdb.db`; it now keeps it and exits with status 1
  ("built '…', but its summary could not be printed").
- **Zip packages are unpacked into fresh, unpredictable scratch
  directories** (review bulkcli#6). `gbuild` / `gadd` / `gsub` unpacked a
  package into `<db>.db.unzip-tmp-<i>`, removing whatever was there first
  — and `gbuild` did so before taking any lock, wiping the scratch
  directory a concurrent `gadd` was reading. Each unpack now creates
  `<db>.db.unzip-tmp-<pid>-<random>` exclusively (mode `0700`; new
  `zipio::ScratchDir::create_unique`), never reusing or removing an
  existing directory, and `gbuild` unpacks only under its claim.
- **The zip entry limit holds before the central directory is decoded**
  (review bulkcli#5). The zip reader decodes every central-directory record
  into memory (about 190 bytes per entry) before it returns, and
  `unzip_max_entries` was compared afterwards: a package of 400,000 empty
  entries grew the peak resident set by about 73 MB before it was
  refused, a 1 GiB upload by gigabytes. The end-of-central-directory
  records (the 64 KiB search window and, for zip64, the record its locator
  names) are now read first, and a package any of whose usable records
  declares more entries than the limit is refused with the same error as
  before (`ZipCode` 3, "the package holds N entries, more than M
  (unzip_max_entries)"). A zip64 end record that is not where its locator
  says, or that fails any of the zip reader's own consistency checks on
  that record (disk number 0, entries on this disk = entries in all, the
  locator's disk with the directory, the minimum record size, room for
  46 bytes per directory header before it), is refused as unsupported
  (code 1): the reader would otherwise scan on towards the locator for
  another zip64 record, and one hidden in the first record's extensible
  data could declare the whole directory while the first declared one
  entry. (The review of the first version of this fix found that bypass —
  the reader decoded every record of such a package — and a check that
  assumed 50 instead of 46 bytes per header, which refused valid zip64
  packages with entry names under four bytes; both fixed with tests.)
  `zipio::read_entry` (the backup-owner lookup of `/api` backup listings)
  is bounded by the default entry limit (10,000).
- **`gquery -f` and `gexport` replace their output only with a complete
  one** (reviews bulkcli#10, #11). `gquery -f FILE` truncated `FILE` before
  it opened the database or parsed the query, so a mistyped database name
  or query destroyed the previous result; `gexport` wrote in place, so a
  write error (a full disk) left a partial file cut mid-statement — or,
  with `-z`, an `<archive>.export-tmp` — and destroyed an existing file of
  that name. Both now write `<dest>.part-<pid>-<n>` (created exclusively)
  beside the destination, sync it and rename it over the destination once
  complete; a failure removes it and leaves the destination as it was. The
  destinations the in-place write served keep working: a symbolic link
  stays a link (the file it names is replaced), a replaced file keeps its
  permission bits and, where the process may set it, its owner, a file the
  user may not write is refused as before, and a FIFO, a device
  (`/dev/null`) or a dangling link is written in place, opened only once
  the output is ready (a writable file in a directory where no file can be
  created too). The first version of this change renamed its temporary
  file over the link, the FIFO and — run as root — the device, and gave
  the result the default mode; fixed before hand-over.
- **`gserver` answers invalid arguments with exit status 2** (review
  bulkcli#18), as every other tool does (it was `1`, which a script or a
  service manager could not tell from a failed start). `0` success and `1`
  failure are unchanged.
- **conf.ini: a documented, narrower inline-comment rule** (review
  bulkcli#9). An unquoted value used to be cut at any whitespace + `#` /
  `;`, so `root_password = Tr0ub4dor #3` set the password `Tr0ub4dor`
  without a word. Now a `#` or `;` starts an inline comment only when it
  stands as a word of its own — whitespace before it, whitespace or the
  end of the line after it — after some of the value (`port=9000  # http`
  reads `9000`); a marker glued to more text stays in the value as
  upstream reads it (`Tr0ub4dor #3`, `/srv/a ;b`); nothing inside double
  quotes is a comment; the value of `root_password` is never cut. Every
  value an inline comment cuts is reported as a warning ("… is read as a
  comment (upstream gStore reads it as part of the value; quote the value
  to keep it)"). **Behaviour change for existing gstore-rust files:** a
  `root_password` value followed by a note (`root_password="s3cret"  ;
  the root password`) used to read `s3cret` and now reads the whole text,
  as upstream reads it — it only seeds a new user store, and such a value
  is now a startup warning that does not quote the password ("the value
  of 'root_password' holds a '#' or ';' standing as a word of its own; all
  of it is the value …"). Upstream files, which have no inline comments,
  parse as before. New `Ini::inline_comments()`. The shipped example's header
  illustrates the rule with a `thread_num` line: upstream's test scripts
  take the port from the first line containing `port=`, so no comment may
  contain `port=` above `[server] port` (a first draft did, and broke every
  shell suite of `scripts/conformance.sh`; a unit test now guards it).
- **conf.ini: settings that cannot take effect are reported** (reviews
  design#11, bulkcli#8): `resource_check=on` (gstore-rust performs no
  resource check; `min_memory`, `min_million_disk`, `min_million_memory`
  are parsed only) and a `build_threads` other than 1 under
  `build_multi_thread=off` (which makes bulk builds single-threaded) are
  warnings now instead of being ignored silently.
- **The shipped `conf/conf.ini.example` no longer forces single-threaded
  bulk builds** (review bulkcli#8). It copied upstream's
  `build_multi_thread=off`; it now leaves the key at its default (`on`,
  shown commented out), so `build_threads` (default 0, every core)
  applies. Existing installations keep their `conf.ini`.
- **`gbuild` reports `conf.ini` warnings** (review bulkcli#7). It read
  `./conf/conf.ini` but dropped its warnings (a mistyped `id_reclaim=maybe`
  silently built a version-2 file) and a read error (then using the
  defaults silently); it now prints both on standard error, as `gserver`
  logs them ("gbuild: conf.ini warning: …"), and loads the file once.
- **`GStoreConfig::db_path` names the directory the servers use** (review
  design#11): `<db_home>/<name>.db` (a name ending in `.db` is taken as
  is), no longer `<db_home>/<name><db_suffix>`, which no server opened
  (they always use `.db`; `gserver` warns about another `db_suffix`). New
  `GStoreConfig::db_root()` (= `db_home`), through which `gserver` chooses
  its API root and `db_path` places a database (behaviour unchanged).
- **The bulk build checks its page ranges in release builds** (review
  bulkcli#12). The POS, OSP, statistics and dictionary writers run side by
  side in page ranges reserved from predicted counts; a writer that took
  more or fewer pages than reserved was only a `debug_assert` (a release
  build would have installed an overlapping image). It now fails the build
  before its install ("the POS index took N page(s), but M were reserved
  for it …"), the old database untouched. No divergence is reachable with
  the current predictor (equal by construction); this is the guard.
- **`GSTORE_BULK_CRASH` is a documented test failpoint and says so when
  set** (review tests-a#19). Whenever the variable is set, every bulk
  install prints what it will do ("GSTORE_BULK_CRASH=staged is set: this
  bulk build aborts right after its install step 'staged' (a crash-test
  failpoint; unset it outside crash tests)"), or that the value names no
  install step and is ignored. Its behaviour is unchanged.
- **The bulk install's `COMMIT` marker is documented as advisory**
  (reviews durability#12, bulkcli#15): it records that the staged files
  were complete and synced when the install began; no recovery reads it
  (no roll-forward exists: before the commit point the old database is the
  database, after it the new store is in place). No format change.
- Tests: new `tests/cli_failure_paths.rs` (the build claim and an open
  during a parked build, two `gbuild` of one name through a FIFO source,
  a `gbuild` that cannot claim the directory, `/dev/full` summaries for
  the snapshot / bulk / incremental layouts, zip scratch directories, the
  inline-comment rule, inert settings, `db_path`, `gbuild` conf.ini
  warnings, `gquery -f` on failure, `gexport` under a file-size limit,
  zipped `gexport`, output destinations that are links, FIFOs, private or
  read-only files, `gserver`'s usage status, the failpoint report) and
  `tests/zip_central_directory_bound.rs` (peak resident set before a
  400,000-entry package is refused; its own binary); unit tests in
  `src/config.rs` (the shipped example keeps `port=9000` the first
  `port=` line), `src/zipio.rs` (end-record parsing; a zip64 record the
  reader would skip is not counted; short entry names) and
  `src/bulk/kvimage.rs` (range check). The kill test of
  `tests/dt_bulk_crash.rs` no longer depends on a 25 s wall-clock budget
  (review bulkcli#17, below).

## GAP_PLAN status line

none

## Docs notes

- **docs/DEPLOY.md** (configuration section):
  - The conf.ini inline-comment rule above (a `#` / `;` that stands as a
    word of its own after a value starts a comment; glued markers stay in
    the value; quoted text is never a comment; `root_password` is never
    cut; each cut is a startup warning). Quote a value that must contain
    ` # ` or ` ; ` as a word. Upgrade note: a `root_password` line with a
    note after the value (`root_password="pw"  # …`) now sets the whole
    text, as upstream does (it seeds a new user store only; a startup
    warning says so without quoting it) — remove such notes.
  - Precedence: command-line flag > `conf.ini` > built-in default — except
    that the environment variables `GSTORE_DISK_QUERY_MODE`,
    `GSTORE_DISK_CACHE_MB`, `GSTORE_VSTREE_ON_OPEN`, `GSTORE_ID_RECLAIM` and
    `GSTORE_ROCKS_BLOCK_CACHE_MB` override their `conf.ini` keys whenever a
    database is opened (a variable left in a service environment wins over
    the file); `gserver` logs the effective values at startup. The tools
    without a `conf.ini` (`gadd`, `gsub`, `gquery`, …) follow the variables
    only.
  - Accepted but not acted on: `tcp_port`, `min_memory`,
    `min_million_disk`, `min_million_memory`, `resource_check` (`on` is a
    warning), `db_suffix` (always `.db`; gserver warns), `root_username`
    (always `root`; gserver warns). `build_threads` has no effect under
    `build_multi_thread=off` (a warning). The `build_threads (0 = every
    core)` line (DEPLOY.md:291) should add "`build_multi_thread=off` means
    1".
  - The shipped example leaves `build_multi_thread` at its default (`on`);
    upstream's file has `off`.
  - Not a compat switch (no D1 secure default changed here), so nothing
    for the compat-switch table.
- **docs/DEPLOY.md** (tools / operations):
  - `gbuild` holds the database directory from its start to its end: a
    `gserver` or another tool holding it makes `gbuild` fail at once
    (status 1); a failed build removes only a directory it created and
    still holds, and only while it holds no database; a build that
    succeeded keeps its database even when its summary cannot be printed
    (status 1).
  - Exit statuses: `gserver` now uses `2` for invalid arguments, like the
    other tools (`0` success, `1` failure, `2` invalid arguments; `gbuild`
    `3` built with skipped statements).
  - `gquery -f FILE` and `gexport` write `<dest>.part-<pid>-<n>` and rename
    it over the destination once complete: a failure leaves the previous
    file. A crash may leave such a `.part-*` file behind (safe to delete).
    A symbolic link keeps pointing at its file (which is replaced, keeping
    its mode and, where possible, its owner); a FIFO or a device is written
    in place; a replaced file loses extra hard links (it is a new inode).
  - Zip packages of `gbuild` / `gadd` / `gsub` are unpacked into
    `<db>.db.unzip-tmp-<pid>-<random>` (mode 0700), removed when the tool
    ends; a crash may leave one behind (safe to delete when no tool runs).
  - `GSTORE_BULK_CRASH=<step>` is a crash-test failpoint, live in release
    binaries: set, every bulk build of the process aborts right after that
    install step (and says so on standard error). Never set it outside
    crash tests. List it with the other `GSTORE_*` variables.
- **docs/STORAGE_BACKEND.md** (bulk install, ~line 248 and the step
  table): the `marked` step writes an advisory `COMMIT` marker that no
  recovery reads (forensic aid only; a stale `.bulk-tmp/` is deleted by the
  next build, marked or not); the bulk build claims its directory with
  `BuildGuard::lock` (claim and lock in one step; other threads of the
  process refused while it runs); the release-mode page-range check of the
  parallel writers (a mismatch fails the build before its install);
  `GSTORE_BULK_CRASH` documented as a test failpoint (with the
  `debug_crash_at` in-process hook). No format change (D7).
- **docs/REFACTOR_BACKLOG.md**:
  - I.6: remove "`gbuild` 清理自己创建的目录(`CreatedDir`)时不取目录锁 …"
    — fixed (bulkcli#2): the cleanup runs under gbuild's claim and only
    while the directory holds no database.
  - design#11 (remaining, D3): gather the process-global storage defaults
    (`set_default_open_options`, `set_default_vs_policy`,
    `set_default_id_reclaim`, `set_default_rocks_tuning`) into one
    `RuntimeDefaults` value installed once and passed explicitly into
    `OpenOptions` / `BulkOptions` / `Evaluator`. Not done: a cross-cutting
    refactor of every open path for no behaviour change; the env-over-conf
    precedence is documented instead (config module docs, the example's
    header, DEPLOY notes above).
  - design#11 (remaining): `gserver` could print the environment
    variables that override explicitly set `conf.ini` keys at startup
    (`report_config` / `install_open_options` are outside this package; it
    already logs the effective values).
  - durability#12 / bulkcli#15: the `COMMIT` marker stays as an advisory
    record (D7: no format change); a roll-forward recovery from a marked
    staging area was not added — nothing would use it (the next build
    replaces the staging area anyway).

## Measured numbers

- `scripts/conformance.sh --no-build` (dev binaries of this branch): all
  six suites pass (basic, update, transaction, api, parser with its known
  cases 15 and 17, python 49/49).
  Re-run in review after the two follow-up fixes (zip64 record checks,
  `root_password` warning): all six pass again.

- bulkcli#5: a hand-written zip64 package of 400,000 empty stored entries
  (36.8 MB) passed to `zipio::unpack` with the default limits:
  peak resident set (`VmHWM`) grew by **73,176 KiB** before the refusal on
  the base (b147336), by less than the test's 16 MiB bound on this branch
  (the check reads the last 64 KiB and one 56-byte zip64 record).
  Command: `cargo test -p gstore --features zip --test
  zip_central_directory_bound`.
- Bench smoke gate: not run. None of the changed code is on a hot path of
  convention 8 (pager, RocksDB reads, engine, top-k, string index,
  optimizer, gfun, HTTP read path): the bulk build gains three integer
  comparisons and one environment read per build, `DirLock::acquire` one
  field test per call.

## Requests/Not yet

- Edits outside the package's owned files (smallest change each):
  - `src/db/persist.rs`: `Database::build_disk_from_sources_incremental`
    and the RocksDB per-triple build's install (`fn` around the former
    line 1431, the function that ends `db.dir_lock = Some(lock)`): the
    `BuildGuard::enter` + `DirLock::held_here` + `DirLock::acquire` lines
    replaced by one `BuildGuard::lock(dir)` call (bulkcli#13, the same
    two-step pattern the review names at persist.rs:992). No signature
    change.
  - `src/bin/gserver.rs`: only `main()` (exit status 2) and
    `Target::resolve` (`cfg.db_root()`), as planned.
- `src/db/persist.rs` `Database::restore_from_zip` (`grestore x.zip`) and
  `src/gapi/ops_db.rs` restore still unpack into `<target>.unzip-tmp` with
  `ScratchDir::fresh` (removing what is there). Both run under the
  database's restore lock / the `/api` name reservation, so the race of
  bulkcli#6 does not apply as such; switching them to
  `ScratchDir::create_unique` is a one-line change for their owners
  (r3-fix / r4-api-hygiene), unless a parallel fix has done so already.
- bulkcli#17's other cases: `gbackup` into the source directory and
  non-ASCII rolled log names were fixed with tests in earlier rounds
  (`tests/layout_replacement.rs` "bulkcli#4", `tests/crash_regressions.rs`
  "bulkcli#1"); this package added the concurrent-gbuild, failed-print,
  conf.ini-warning and `gquery -f` failure tests and made the kill test
  load-independent.
- Residual window of bulkcli#2, recorded for docs/REFACTOR_BACKLOG.md
  (not changed: no deterministic test is possible, D6): `gbuild` decides
  "the directory is new" (`holds_data`) just before it claims the
  directory, two adjacent statements. If another process created, built
  and released the same new name entirely between them (microseconds; a
  real build takes far longer, and while it runs its lock file makes the
  directory non-empty, so the second `gbuild` is refused), a second
  `gbuild` without `--force` would rebuild that database instead of
  refusing; it never deletes it (the cleanup removes only a directory that
  holds no database, under the claim). A fix would re-check
  `Database::storage_kind` under the claim and refuse without `--force`.
- Observed, not this package's: `tests/rdf_parser_conformance.rs`
  `jsonld_node_contexts_and_context_arrays_add_layers_not_copies` (review
  parser#7, r3-parser) is a wall-clock ratio test that fails under load —
  in the stream-mode workspace run at load average 12–15 on 4 vCPUs it
  measured "4x the context array members took 23–30x as long (1.6 ms ->
  38–51 ms)" in five whole-binary runs, and passed when run alone and in
  the memory-mode run. The binary opens no database; nothing in this
  package touches the JSON-LD parser. For the owner of flaky timing tests
  (r3-fix / r5-test-hygiene): a 10 ms parse is preempted on a loaded
  runner while the 1.6 ms one fits a time slice, so fastest-of-3 does not
  filter it; a work counter (context layers created) would be
  load-independent.
- The `tests/dt_zip_io.rs` assertions `!dir.join("z.db.unzip-tmp-0").exists()`
  (and the `-0` variants) became vacuous with the new scratch names; the
  new `gbuild_unpacks_into_a_fresh_scratch_directory_after_taking_the_lock`
  checks that no `*.unzip-tmp*` is left. r5-test-hygiene may update them to
  a prefix scan.
