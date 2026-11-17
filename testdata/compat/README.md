# Cross-version format fixtures

Databases written by **old binaries**, which the current code must keep
opening and answering (review gap-doc-claims#8; the compatibility matrix in
`docs/STORAGE_BACKEND.md`). Nothing here is written by the current code: a
"legacy writer" in today's code would only prove that the code agrees with
itself.

## `eaf5320/` — the last commit before Phase 6

Written by the `gbuild` and `gquery` binaries of commit
`eaf53202cb9cf2e81493ed8a49480bf7fbf81d83` ("eaf5320", the semantic reference
of `docs/PHASE6_PLAN.md`), built with Rust 1.99.1 (`cargo build --locked
--bin gbuild --bin gquery`, debug profile) in a scratch worktree of that
commit, by:

```text
scripts/compat-fixtures.sh --generate
```

which runs, in a scratch directory, for `<kind>` = `snap` and `disk`:

```text
<eaf5320>/gbuild [--disk] <kind> testdata/compat/data.nt   # --disk for disk only
<eaf5320>/gquery <kind> testdata/compat/named.ru           # two named graphs, 5 quads
<eaf5320>/gquery <kind> testdata/compat/delete.ru          # one triple deleted
```

and copies `snap.db` to `eaf5320/snapshot/` and `disk.db` to
`eaf5320/disk-v1/`, leaving out the old binary's `LOCK` file (the directory
names avoid `.db`, which `.gitignore` excludes).

| File | Bytes | SHA-256 |
|---|---:|---|
| `snapshot/dict.bin` | 3903 | `3d15e82fa9dfb1708ad8eb67913575f411ac5281b3a2421cdbc539925fd089b5` |
| `snapshot/meta.bin` | 44 | `46b89bb80132cd137053f136ee51d7da151092f6251d66022c2f8335cc20a4f5` |
| `snapshot/named.bin` | 344 | `a8aa9a08ed96946481505388a691936094e31ec8756c5ddbbb4e191e64d3df2d` |
| `snapshot/store.bin` | 980 | `86bb01fc332134aebf14364d591a1a0fa17ad69a3cfcaa45d9f7b7a08dbfc0b5` |
| `snapshot/vstree.bin` | 1009 | `1b2ff4ff621a8b96be37e0edd590800aef2e205932d6dad8c2a3b1c8bfc38ed2` |
| `disk-v1/kvstore.kv` | 86016 | `a08c4722075f92c8b18594fd70cef4b8d071f1c3a27d2a9a11f61331073bf7b2` |
| `disk-v1/vstree.kv` | 8192 | `e4cf8a10893a450f33d8fcddad5c67ec524b0463a4d061a15a2906202b918bcf` |

`disk-v1/kvstore.kv` is a version-1 pager file (magic `GSTOREKV`, no feature
bits): eaf5320 predates the format registry and id reclamation.

The data: `data.nt` (24 default-graph triples: IRIs, a blank node, plain,
language-tagged and escaped literals, a non-canonical `xsd:integer`
`"027"`, `xsd:decimal`, `xsd:double`, `xsd:boolean`, `xsd:date`, a 347-byte
literal, CJK text), `named.ru` (graphs `<http://example.org/graph/contacts>`
with 2 triples and `<http://example.org/graph/hr>` with 3), `delete.ru`
(removes `ex:bob ex:score "9.5E1"^^xsd:double`). The old `gquery` itself counts 23
default-graph triples in both databases.

## What uses them

- `tests/format_compat.rs` (every `cargo test`) copies each fixture to a
  temporary directory and checks, with the current code: the answers
  (hand-written from the three files above) under every VS-tree policy; the
  snapshot in memory, the disk database in Stream and Materialize mode with
  the old `vstree.kv` in use; writes, a save / reopen, and that the disk
  database's `kvstore.kv` stays version 1 with id reclamation off (old files
  are never upgraded implicitly).
- `scripts/compat-fixtures.sh` (weekly in `.github/workflows/compat.yml`,
  or by hand) rebuilds eaf5320, regenerates the fixtures into a scratch
  directory, runs `tests/format_compat.rs` against them and against these,
  and checks the other directions with the old binaries: eaf5320's `gquery`
  reads a snapshot and a `GSTORE_ID_RECLAIM=off` (version-1) disk database
  written by the current `gbuild`; it refuses the default (version-2) disk
  database with "not a gStore KV file"; and eaf5320's `restore_incremental`
  fails with "unknown redo.log op 'C'" on an increment whose `CLEAR DEFAULT`
  record the current code wrote.

Options: `--offline` (cargo `--offline`), `--keep` (keep the scratch
worktree and builds; a rerun with the same `--work DIR` is incremental),
`--work DIR`, and `CARGO="cargo +1.99.1"` to pick the toolchain.
