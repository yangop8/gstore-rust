# R1-REVIEW — Phase 6 Round 1 review fixes

## Changelog

- **Disk B+trees split by bytes, never past a page**: an overflowing node
  used to split at its middle entry. With entries of about 1.4 KB or more
  (long literals, IRIs), one half could still be larger than a page. Writing
  it then panicked, and under R1's write bracket the panic also poisoned the
  shared pager, so every later read, write or flush of that `DiskStore` (and
  of every view sharing its pager) panicked too. `gbuild --disk` or an
  `INSERT` into a disk database could hit this. Now:
  - A node still splits at its middle entry when both halves fit, so stores
    of small entries keep the same layout as before.
  - Otherwise a leaf splits where both halves fit, or into three leaves when
    an entry that nearly fills a page lands between two that together fill
    one.
  - Internal nodes always split in two, because keys are now limited to
    `MAX_KEY_LEN` = 2038 bytes (half a page).
  - A longer key is refused with an error and the store stays usable.
    Before, keys of about 2 to 4 KB were accepted but almost always panicked
    at the next split.
  - A delete's borrow from a sibling now also checks that the new separator
    fits the parent.
  - Every node write checks its size first, as a backstop.
- **A panic inside a `DiskStore` write is rolled back and returned as an
  error.** It no longer poisons the pager lock for every reader and writer
  that shares it. The pages, the header and the counters are restored the
  same way as for an `Err`.
- **Read-only views see VList invalidation at once**: `try_o_by_sp` and
  `is_compacted` read the compacted flag from the shared header, not from
  a copy taken at the view's last `refresh`. Before, a view built after
  `compact` kept answering `o_by_sp` from the outdated VList after the
  writer changed that group, while the view's other index reads were live.
- **A damaged `kvstore.kv` header fails the open with an error and no longer
  aborts the process.**
  - Entity, literal or predicate counts larger than the `id2*` arrays can
    hold are reported as "corrupt kvstore header". The base binary
    reported these as an error too.
  - Counters that only size a buffer (`triple_count`, `quad_count`, the
    distinct and per-predicate counts) reserve that space only if the
    allocator can provide it. Before, a huge count aborted `gshow`, or the
    whole `gserver` on `/api load`.
- **VS-tree maintenance no longer re-reads hub entities.** An insert into a
  valid in-memory VS-tree, and a rollback that re-adds deleted triples,
  used to re-sign the subject and the entity object from all their edges.
  Because deletes now keep the tree valid, that became quadratic: typing new
  members into a large class after any delete, rolling back the deletion of
  a class, or `DELETE … INSERT … WHERE` retyping. Now:
  - The added edge's bits are ORed into the stored entry and up its parent
    chain, in O(height), by the new `VsTree::union_into`.
  - Only an entity that is not yet in the tree is signed from the store.
  - With no prior delete the entry is exactly what re-signing gives (the
  tree state is identical). After deletes it is a superset, which is still
  sound, and answers are unchanged.
- **gstore-bench**:
  - **CI canary**: it now checks the run's own result, slowed 3×, against
    itself. Before, it compared against the blessed `smoke.json`, so it
    stopped failing once the tree became about 2× faster.
  - **Concurrency group**: `bench.yml`'s group now includes the event name,
    and only push and PR runs cancel each other. A push to main no longer
    cancels the weekly or dispatched reference run.
  - **Retries**: a retried open or query phase first rebuilds the database
    that the update phase changed, and a retried open must see the first
    attempt's term counts.
  - **Interrupted runs**: phase children and their `gbuild` die with the
    harness (Linux parent-death signal). Before, they kept running after
    Ctrl-C or `timeout(1)` and held the work directory and an inherited
    `flock`.

## GAP_PLAN status line

none (review fixes to the R1 packages).

## Docs notes

- docs/STORAGE_BACKEND.md (disk layout): dictionary keys (IRIs, literals)
  are limited to 2038 bytes in a disk store. A longer key fails the insert
  or the build with an error naming the limit. The reserved `LONG_KEYS`
  feature (bit 2) would lift this. docs/PHASE6_PLAN.md D7 and the
  feature-bit table say "> 4 KB"; for the B+tree the effective limit is
  half a page (internal separators must leave room for a two-way split).
- docs/STORAGE_BACKEND.md (concurrency): a panic inside a disk write is
  rolled back and reported as a database error; it no longer poisons the
  store.
- bench/README.md has been updated in this change (retries rebuild the
  updated database; the canary checks the result against itself).

## Measured numbers

These fixes are bounded by deterministic tests, not by timings:

- `db::tests::incremental_vstree_maintenance_does_not_rescan_hubs` uses a
  3,000-member class, one delete, then 200 typings. Before the fix the
  inserts re-read 620,100 edges; the test now allows at most 400. For the
  rollback of the class-wide delete and for the retyping, the test allows
  at most 2 re-read edges per class member. Only entities new to the tree
  are read at all.
- The B+tree tests reproduce the reviewer's panics on the base code:
  `range end index 4231 out of range for slice of length 4096` and similar.
  They pass after the fix. These are `large_keys_split_by_bytes_not_by_count`,
  `random_large_entries_never_overflow_a_page` and
  `page_sized_values_split_three_ways`.
- `corrupt_header_counters_are_errors_not_aborts` aborted the test process
  with SIGABRT ("memory allocation of 51539607540 bytes failed") before
  the fix.
- No timing re-measure was run for this change. The univ 8 update-phase
  `chunk_insert_ms` should drop back to about the pre-Phase-6 figure
  (29 ms snapshot), from the 647 ms the reviewer measured on the merged
  tree. Confirm this in the measure stage together with the pending
  `smoke.json` re-bless.

## Requests/Not yet

- Keys longer than 2038 bytes in a disk store need the reserved
  `LONG_KEYS` feature (overflow-stored keys or truncated separators). They
  now fail loudly, as plan D7 intends for over-limit keys.
- A read-only view's counts (`triple_count`, statistics) still catch up
  only on `refresh()`, as documented. R2-DISKLIB builds a fresh view per
  read under the writer gate, so this does not reach it.
