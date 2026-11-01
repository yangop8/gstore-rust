# Phase 6 R1 merge — integration of R1-KV, R1-CORE and R1-BENCH

## Changelog

- **`DiskStore` answers the C1 counts itself**: `impl TripleSource for
  DiskStore` overrides `count_po_by_s` … `count_so_by_p`, `distinct_p_of_s/o`
  and `has_subject/object/predicate` with R1-KV's prefix counts (no range is
  materialized to count it). Each returns exactly what the trait default
  returns, so plans and solution order are unchanged; the `has_*` methods
  answer `true` on a read error (C1).
- **Exports run inside a read scope (C2)**: `zipio::write_export` (behind
  `gexport` and `/api export`) fails with "a read from the store failed while
  exporting" when a backend reported a read error, instead of writing a
  partial export. `Database::export_ntriples` now delegates to it (same
  output).
- **`Database::build_disk_from_sources` signs the VS-tree in one streaming
  pass** over SPO and OSP (`DiskStore::for_each_entity_adjacency`) instead of
  two range reads per entity; `vstree.kv` is byte-identical, and a read error
  fails the build.
- **Streaming and Top-K evaluation use candidate lookups** like the
  materializing executor: a `?s p ?o` scan with a small VS-tree candidate set
  looks the candidates up instead of scanning the predicate (same solutions,
  same order).
- **Command-line tools no longer panic on a closed pipe**: `gserver -S |
  head -1`, `gshow | head`, `gquery … | head`, every tool's `--help | head`
  and `gconsole --local` used to end with "failed printing to stdout: Broken
  pipe". New `gstore::stdout` (`outln!` / `out!`): the first broken-pipe write
  closes standard output for the rest of the process, and the tool finishes
  its work and exits with its usual status, silently. A mutating tool or a
  running server is never cut off halfway because its reader went away.

## GAP_PLAN status line

none (integration of the R1 packages; their own fragments carry the status
lines).

## Docs notes

- docs/DEPLOY.md (the command-line tools' exit statuses): output to a pipe
  that closes early is dropped silently; the tool still finishes and exits
  with its usual status (`gquery … | head` exits 0).
- docs/STORAGE_BACKEND.md (with R1-CORE's C2 notes): exports (`gexport`,
  `/api export`, `Database::export_ntriples`) run inside a read scope and fail
  on a read error.

## Measured numbers

None of these changes has its own benchmark. The C1 overrides and the
streaming VS-tree signing only remove work (exactness is tested); the
round's numbers are re-measured with `gstore-bench` against
`bench/results/pre-phase6-lubm8.json` after the merge.

## Requests/Not yet

- **R3-SRV / R4-SNAP:** `ConcurrentDb` snapshot and version-view queries
  (`src/concurrent.rs`) still evaluate outside a `ReadScope` (R1-CORE request
  2, left to the owners of that file). Today their store is always an
  in-memory `TripleStore`, so no read error can occur there; `Snapshot::capture`
  over a non-memory backend reads the whole store unscoped as well.
- **rustfmt:** the one-time `cargo fmt --all` commit and its
  `.git-blame-ignore-revs` entry (plan §5.6e) are a separate stage; until it
  lands the new `fmt` CI job fails.
