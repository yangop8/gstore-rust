# R4-REVIEW — Phase 6 Round 4 review fixes

Fixes for the confirmed findings of the Round 4 review (lenses
reclaim-dict, reclaim-layouts, reclaim-concurrency, snap-server,
bench-watdiv, cross-lock-order, cross-cutting), on `phase6` from
`f5ad314`. Each fix has a regression test that fails on `f5ad314` (see
Measured numbers) unless noted there. Findings reported more than once
(the RocksDB replay buffer three times; the pinned Stream backlog, the
snapshot CSR build and gbuild's `id_reclaim` twice each) are fixed once.

## Changelog

- **Id reclamation (`db::reclaim`):**
  - A Stream-mode free now waits only for the dictionary pins taken
    *before* the term's candidacy (each deferred candidate carries a stamp
    of the pin clock; a reader that pinned later never saw the id in use —
    any later use would have to be deleted again, a newer candidacy). The
    waiting candidates are kept ordered by stamp, so a statement finds the
    ones no live pin predates without reading the store. While a pin
    holds them, nothing is re-checked: every statement and every commit
    used to re-probe the whole backlog (`has_subject` + `has_object` per
    id), so a pinned database got slower with each delete — 14,640 probes
    for 120 single deletes under one pin in the regression test, none
    now; once the pin is gone each candidate is checked once.
  - A read error during the reference check no longer drops the
    candidates — the statement's own and the deferred backlog: they wait
    for the next check (they used to stay allocated until `gcompact
    --reclaim`, against "nothing is lost but the reuse").
  - `reclaim_unused` on a Stream database a pin holds defers what it
    finds, stamped at the call (it waits for every pin alive then).
- **`ConcurrentDb` over a Stream-mode database:** its construction-time
  base snapshot pins the dictionary for the wrapper's life, so it still
  frees no id while it lives — but its updates no longer re-check the
  backlog (they did, at every update, INSERT DATA included), and dropping
  the wrapper lets its own snapshots go, runs the frees nothing holds any
  more and makes them durable (they were lost, the terms orphaned on
  disk). A snapshot a reader still holds at the drop keeps its ids; those
  frees are lost with the database, as at a restart.
- **`CLEAR` / `DROP` redo records (RocksDB):** a clear RocksDB partly
  refused (a write error, a stale undo journal) is logged as what it
  removed — one `D` record per removed triple or quad — not as a clear of
  the target; `CLEAR NAMED` logs `N` only when every graph went, else a
  `C` per graph emptied. Replaying the log (`replay_redo_log`,
  `restore_incremental`) used to empty a graph the live database kept.
- **RocksDB journal replays:** the terms of the inserts a replay undoes
  are handed over only once the whole replay has run (a replay cut short
  hands over nothing; the retry that finishes it, once), and the database
  takes them after every statement and at every commit, not only at open.
  Every rolled-back transaction used to leave 3–4 entries per inserted
  triple in the store's `replayed` buffer for the life of the store (30
  for 10 inserts in the regression test), and the terms minted by a
  rollback whose replay failed — then finished by a later heal — stayed
  as orphans (5 in the test).
- **`gbuild` applies `[system] id_reclaim` (and the RocksDB tuning keys)
  of `./conf/conf.ini`**, as gserver does: with `id_reclaim=off` a new
  disk database — bulk, `--incremental`, or by `[storage] default_layout`
  — is version 1 (`GSTOREKV`), which binaries older than Phase 6 open.
  It used to apply only `GSTORE_ID_RECLAIM` and build version 2
  regardless. `conf.ini.example` says which tools read the key (gserver,
  gbuild; the other command-line tools follow `GSTORE_ID_RECLAIM`).
- **REST `/build` in the disk or RocksDB layout:**
  - A loaded database with acknowledged writes not yet saved (check_point
    off) is saved before it is retired: a build that then fails serves it
    again with those writes (they were lost).
  - A failed build of a new name leaves no `<name>.db` directory (it left
    one holding only `LOCK`, which made `/api build` of the name answer
    "database already built.").
  - A body keeps its blank-node labels, as the snapshot layout does (the
    bulk path rewrote `_:alice` to `_:<scope>_alice`).
  - A build whose install committed but could not finish its cleanup (the
    replaced layout or rules still there: `BulkReport::warnings`) answers
    `500` and is not served — opening it might read what was left behind;
    it used to answer `200 built`. The install removes the replaced rules
    under the rule-directory lock (see the second pass below).
- **Snapshot reads of a Stream-mode transaction (`/begin`):** the graph
  functions' CSR is built predicate by predicate with the gate's cancel
  flag checked between predicates and between the build's phases
  (`CsrGraph::build_cancellable`, the same graph as `CsrGraph::build`), so
  a `/commit` — which drains the snapshot's readers while holding the
  database's exclusive guard — waits for one predicate's read, not for a
  whole-store scan and two sorts (the review measured 3.96 s at 800K
  triples). The build runs in a read scope of its own: a read error fails
  it and nothing is cached (C2); a failed build used to cache the partial
  graph, and the next graph-function read of the transaction answered
  from it with no error.
- **`/api monitor` and `schema` during a transaction** report the
  committed state: the figures (entity, literal, predicate counts) and
  the class statistics / schema graph of the transaction's snapshot,
  cached under the mutation count at `/begin`. Any client allowed to
  monitor used to see the open transaction's classes, subjects and terms.
- **`/commit` and `/rollback`** let the begin-time snapshot go before the
  database commits: on an in-memory database the snapshot shared the
  dictionary's interners, so a commit that freed one term copied the whole
  interner (O(terms), under the exclusive guard) only to drop the copy.
- **The database directory's size (`/api monitor`'s `diskUsed`)** is
  measured only after an exclusive section that changed the data or saved
  it, and with the guard already released; every exclusive section —
  `/begin`, a no-op update — used to walk the directory (a readdir and a
  stat per file, ~1.2 ms for 1,000 files) while readers waited. A change
  no write makes (a RocksDB background compaction) shows after the next
  write.
- **gstore-bench:**
  - `report --update` / `--check` recognize a marker only where it starts
    a line (after optional spaces) outside a fenced code block: the marker
    syntax shown in prose, a table cell or a fenced example is left alone.
    `report --check` failed on the committed bench/README.md ("cannot read
    ARGS") and would have failed on CHANGELOG.md once R4-CLOSURE-A's own
    Changelog bullet is folded in; `--update` replaced the prose between
    an inline example's two markers with a table.
  - `check` / `ci` against a baseline whose main workload is the
    WatDiv-like one measure it once: the profile's WatDiv-like extra is
    dropped (it exited 2, "the watdiv workload is measured twice").
  - WatDiv-like data keeps, at any `--shrink`, the entities `GUARANTEED`
    names (`Product1`, `User1`): from `--shrink 126` (scale 1) the ASK
    walk could not bind F1 and the run failed. Data for smaller shrinks
    is unchanged (the scale-1 hash is still the pinned one).
- **CI:** ci.yml's test job runs `gstore-bench report --check README.md
  CHANGELOG.md` too (bench.yml ignores pushes that change only Markdown,
  so a docs-only edit of a generated table was caught only by the next
  code push). R4-CLOSURE-A request 4.
- **Second pass (the review of the fixes above, on `5105d3d`):**
  - **Id reclamation:** each candidate is decided on its own reference
    probe (a read scope per probe). A candidate whose probe failed waits
    for the next check alone — the rest of its batch is freed — and after
    `CHECK_ATTEMPTS` (4) failed checks it is given up with a warning, left
    for `gcompact --reclaim`. The first version of the read-error fix put
    the whole batch back: one unreadable index page under one deleted
    term's key stopped every later free, and every deleting statement
    re-probed the growing backlog (the review measured 3, 5, …, 81 probes
    for 40 single deletes, nothing freed). `reclaim_unused` frees what it could check
    and logs the rest.
  - **REST `/build` in the snapshot layout** saves a loaded database with
    acknowledged writes not yet saved (check_point off) before it retires
    it, and serves it again when removing the old rules or saving the new
    snapshot fails (the writes were lost and the database left unloaded).
  - **REST `/build` in the disk or RocksDB layout** no longer holds the
    rule-directory lock through the build: the bulk install (`bulk`
    `commit` / `rocks_sink`, so `gbuild` too) takes it only to remove the
    replaced rules. A `reasonManage` execute / disable / check (types 4,
    5, 8) of the database waited for the whole build, an HTTP worker held
    meanwhile; a few such requests stalled every database of the server.
  - A failed REST bulk build of a new name removes the directory it made
    only under the directory's lock, and only when it holds no database:
    one another process (a `gbuild` of the name) created and locked
    meanwhile was deleted under it.
  - A REST bulk build whose install did not finish (or whose result does
    not open) still updates the registry: the grants of a database dropped
    outside the server are forgotten — the new data no longer inherits
    them once loaded — and a new name is registered for its creator. A
    staging area left behind is only logged and the database is served
    (`BulkReport::opens_as_built`): no open reads `.bulk-tmp/`, and the
    next build removes it.

## GAP_PLAN status line

none

## Docs notes

- docs/ARCHITECTURE (id reclamation): a Stream-mode free waits for the
  pins older than its candidacy only; deferred candidates are checked
  once, when no such pin is left (or after a read error, at the next
  check: each candidate is decided on its own probe, and one whose probe
  keeps failing is given up after 4 checks, for `gcompact --reclaim`).
  This replaces R4-RECLAIM's request 2 wording ("retried by every later
  statement"). A `ConcurrentDb` over a Stream database frees its
  deferred ids when it is dropped; while it lives its base snapshot holds
  them (R4-RECLAIM's "today it never frees ids" stays true while it
  lives). Frees a pin still holds at a restart (or at the drop of a plain
  `Database`) are lost, as before: `gcompact --reclaim` finds them.
- docs/ARCHITECTURE (redo log): a `CLEAR`/`DROP` RocksDB partly refused is
  logged per removed triple, not as a `C`/`N` record.
- conf/conf.ini.example (done) and docs/CONFIG: `[system] id_reclaim` is
  read by gserver and gbuild; gadd, gsub, gquery and the other tools follow
  `GSTORE_ID_RECLAIM` only.
- REST API docs (`/build`): in every layout a loaded database is saved
  before it is replaced, and served again when the rebuild fails; with
  `layout=disk|rocksdb` a build whose install did not finish answers `500`
  and leaves the database unloaded until its leftovers are removed (a
  staging area left behind does not count), registered all the same; a
  body's blank-node labels are kept in every layout.
- docs/ARCHITECTURE (server): `/api monitor` and `schema` report the
  committed state while a transaction is open; `diskUsed` is measured
  after a change or a save, outside the lock.
- bench/README.md (Reports): markers count only at the start of a line,
  outside fenced code blocks.

## Measured numbers

None for performance: these are correctness and cost-model fixes,
checked by counting work (reference probes, interner copies, directory
walks) instead of timing it. The review's timings (per-remove cost
growing linearly with the pinned backlog; `/commit` 3.96 s behind a
snapshot CSR build at 800K triples; a 75 ms interner copy per freeing
`ConcurrentDb` delete at 1M entities) were not re-measured after the
fixes.

Regression tests, each run against `f5ad314` with only the tests (and
the test-only probe counter, injected read error and interner address
accessor they use) grafted on — all fail there, and pass here:

| finding | test | on `f5ad314` |
|---|---|---|
| pinned Stream backlog re-probed | `db::r4_review_tests::a_pinned_stream_database_checks_its_backlog_once` | 14,640 probes for 120 deletes under the pin (0 expected) |
| pin age | `db::r4_review_tests::only_a_pin_older_than_a_candidacy_holds_its_free` | a pin younger than the candidacy held it |
| read error drops the backlog | `db::r4_review_tests::a_read_error_in_the_reference_check_keeps_the_candidates` (memory and Stream) | candidates gone |
| ConcurrentDb over Stream | `db::r4_review_tests::a_concurrent_stream_database_frees_its_deferred_ids_when_dropped` | 3,321 probes for 40 updates; ids never freed |
| CLEAR redo record (rocksdb) | `db::r4_review_tests::rocks::a_clear_rocksdb_refused_logs_only_what_it_removed` | replayed graph empty, live graph kept its quads |
| replayed buffer (rocksdb) | `db::r4_review_tests::rocks::rollbacks_leave_no_replayed_terms_behind` | 30 entries after one rollback of 10 inserts |
| healed rollback (rocksdb) | `db::r4_review_tests::rocks::a_rollback_finished_by_a_later_heal_frees_the_minted_terms` | `reclaim_unused` freed 5 orphans |
| gbuild `id_reclaim` | `dt_bulk_disk::gbuild_honours_the_id_reclaim_of_conf_ini` (run with the one-line fix reverted in place) | `GSTOREK2` with `id_reclaim=off` |
| failed REST bulk build | `http_api::tests::a_failed_rest_bulk_build_keeps_the_acknowledged_writes` | the acknowledged insert gone |
| REST bulk build blank nodes | `http_api::tests::rest_builds_from_a_body_keep_blank_node_labels_in_every_layout` | `b…_alice` on disk |
| unfinished install served | `http_api::tests::a_rest_bulk_build_with_an_unfinished_install_is_not_served` | `200 built (1 triples)` |
| monitor during a txn | `http_api::tests::monitoring_shows_no_uncommitted_writes` | `entityNum:5`, class D listed |
| commit copies the interner | `http_api::tests::a_commit_that_frees_terms_copies_no_interner` | entity interner moved |
| snapshot caches a failed CSR | `http_api::tests::a_disk_snapshot_caches_no_csr_built_from_a_failed_read` | partial CSR cached |
| directory walk per section | `http_api::tests::only_a_section_that_changes_the_data_measures_the_directory` | re-measured with nothing changed |
| report markers in prose | `report::marker_syntax_in_prose_and_examples_is_not_a_region` (gstore-bench) | `cannot read ARGS` |
| WatDiv-like shrink ≥ 126 | `watdiv::every_template_binds_at_any_shrink` (gstore-bench) | `template F1: no binding answered in 32 tries` |

Second-pass regression tests, each run against `5105d3d` with only the
tests (and the test hooks they use: a probe that fails for one term, a
hook run right before the REST bulk loader starts) grafted on — all fail
there, and pass here:

| finding | test | on `5105d3d` |
|---|---|---|
| one unreadable candidate stops every free | `db::r4_review_tests::an_unreadable_candidate_holds_back_no_other_free` (memory and Stream) | the rest of its batch kept (`badobj`) |
| snapshot rebuild failing after the retire | `http_api::tests::a_failed_snapshot_rebuild_keeps_the_acknowledged_writes` | `404 not loaded` |
| rule lock held through the bulk build | `http_api::tests::a_rest_bulk_build_holds_no_rule_lock_while_it_loads` | `reasonManage` check unanswered after 5 s |
| unlocked cleanup of a new name | `http_api::tests::a_failed_rest_bulk_build_leaves_a_directory_another_process_locked` | the other process's directory deleted |
| registry skipped on an unfinished install | `http_api::tests::an_unfinished_rest_bulk_build_still_updates_the_registry` | the dropped database's grant kept |

New mechanisms with unit tests only (their API did not exist before):
`bulk::types::tests::only_a_left_staging_area_opens_as_built` (second
pass);
`analytics::csr::tests::the_cancellable_build_equals_build_and_stops_when_cancelled`
(the per-predicate build equals `build`; a flag raised during the first
predicate's read stops it before the second) and
`runner::cli::tests::check_measures_the_baseline_workloads_once_each`
(gstore-bench; the verifier reproduced the exit 2 through the binary).
No test reproduces a `/commit` waiting behind a snapshot CSR build (a
timing race at any size a test can afford); the cancellable build and its
wiring into the gate's flag are what changed.

## Requests/Not yet

- Not fixed, by design: on an in-memory `ConcurrentDb`, a delete that
  frees a term still copies the space's whole interner when the published
  snapshot shares it (`Arc::make_mut` in `Interner::retire`, O(terms) —
  ~75 ms at 1M entities in the review). A `ConcurrentDb` insert that mints
  a term has paid the same copy since Round 2, and the delete is still ~4×
  faster than before the branch (the review measured the base). Removing
  it needs a chunked or persistent interner, or frees deferred until the
  interner is unshared (which would leave live counts above the
  referenced terms meanwhile) — a design change, not a review fix. The
  HTTP `/commit` case (the snapshot held across the commit) is fixed.
- A `ConcurrentDb` over a Stream database still frees no id while it
  lives: its base snapshot (`VersionView`'s fallback) reads the shared
  on-disk dictionary and needs its pin. Freeing then would need a base
  that resolves its own ids (a copy of the dictionary, O(terms)).
- The snapshot CSR build's cancel latency is one predicate's `so_by_p`
  read (a predicate of millions of triples, `rdf:type`, is still read
  whole before the flag is seen).
- `gadd`, `gsub`, `gquery` and the other tools that open databases do not
  read `conf.ini`; they follow `GSTORE_ID_RECLAIM` (now documented).
- Second pass: no test serves a REST bulk build whose only unfinished
  step is the staging area's removal end to end (failing that removal
  needs an I/O fault the suite cannot inject; the review used an
  `LD_PRELOAD` shim) — `BulkReport::opens_as_built` has a unit test.
- Second pass: `gbuild`'s own cleanup of a directory it created
  (`CreatedDir`) still removes it without the directory's lock (it
  predates this round; its `holds_data` check keeps a built database).
