# Upstream gStore test assets

Test data, queries and expected results copied verbatim from upstream gStore
(<https://github.com/pkumod/gStore>), commit
`bebe1ea752ef7f5e9bad90e1c035635737129cb0` (2026-04-09, "support : delete {}
insert {} where {}", after the 1.4 release). They are the inputs of the
conformance suite: the Rust integration tests `tests/upstream_*.rs` and
`scripts/conformance.sh`, which runs upstream's own shell test scripts
(copied, with documented adaptations, to `tests/upstream_scripts/`) against
the Rust `gserver`.

The directory layout mirrors upstream's `GSTORE_HOME`, so the scripts' paths
(`./data/...`, `scripts/test/...`) resolve unchanged:

| Here | Upstream | Used by |
|------|----------|---------|
| `data/bbug/` | `data/bbug/` (`bbug.nt` + 8 `.sql`) | `basic_test.sh`, `bfs_test` |
| `data/small/` | `data/small/` (`small.nt`, `small_add.nt`, 11 `.sql`) | `basic_test.sh`, `bfs_test` |
| `data/num/` | `data/num/` (`num.nt`, 4 `.sql`) | `basic_test.sh`, `bfs_test` |
| `data/lubm/` | `data/lubm/*.rq` (LUBM queries `lubm_q1`–`lubm_q14`, `lubm_insert`, `lubm_delete`) | `basic_test.sh` |
| `data/update_test.nt`, `data/all.sql` | same | `update_test.sh` |
| `scripts/test/parser_test/` | same (50 × data `parser_d<N>.ttl`, query `parser_q<N>.sql`, expected result `parser_r<N>.txt`) | `parser_test.sh`, `tests/upstream_parser.rs` (over `/api`), `tests/dt_upstream_parser_suite.rs` (in-process, row-by-row, Top-K on and off) |
| `scripts/test/bfs_test/` | same (25 queries `<q>.sql` with expected answers `<q>r.txt`) | `PyTest/auto_test.py BFSTest`, `tests/upstream_bfs.rs` |
| `LICENSE` | `LICENSE` | — |

## Not copied

- **`data/lubm/lubm.nt`** (11.9 MB, 100 543 triples) is not duplicated here:
  it is byte-identical to the repository's `testdata/lubm/lubm.nt` (SHA-256
  `0bfd2e40cc91551d6f7ad5466ab19893aaae8650b1b942dd263e1a1aed0c0f98`), which
  the tests and `scripts/conformance.sh` use in its place.
- The test *scripts* (`scripts/test/*.sh`) live in `tests/upstream_scripts/`,
  and upstream's Python SDK in `tests/upstream_scripts/python/`.
- Nothing else exceeds a few hundred KB; the largest file kept is
  `scripts/test/bfs_test/lubm/lubm_p4r.txt` (≈ 385 KB; 2 MB in all).

## Known properties of the upstream expectations

- `bfs_test/lubm` answers were recorded from gStore's legacy LUBM encoding
  (`<ub:name>`, `<rdf:type>` IRIs, literal values as IRIs such as
  `<FullProfessor0>`), not from the current `data/lubm/lubm.nt`.
  `tests/upstream_bfs.rs` rewrites `lubm.nt` into that encoding before
  building.
- `basic_test.sh` records `num_ans=(0 0 0 1)`; the SPARQL answers of
  `num0`–`num2` are 8, 1 and 4 (see `tests/upstream_basic.rs`).
- `parser_test.sh` records `AnsNum` 0 for cases 15 and 17, whose result files
  hold 4 and 1 rows — the W3C answers. Both parser suites follow the result
  files; the script's numbers and this divergence table (`DIVERGENCES`, with
  the reasons) live once, in `tests/upstream_scripts/parser_cases.rs`, which
  both include.
- These files are the only vendored copy of `parser_test`: the in-process
  suite (`tests/dt_upstream_parser_suite.rs`, added with Phase 3) reads them
  here too. Do not edit them; add new cases elsewhere.

## License

These files are distributed under upstream gStore's BSD 3-Clause license,
reproduced in `LICENSE` in this directory:

```
Copyright (c) 2014, Institute of Computer Science & Technology of Peking University
All rights reserved.
```

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions in `LICENSE` are
met (retain the copyright notice, reproduce it in binary distributions, and do
not use the copyright holder's name to endorse derived products).
