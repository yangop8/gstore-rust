# Upstream gStore `parser_test` cases (vendored)

Verbatim copies of the 50 SPARQL parser/semantics cases of upstream gStore's
`scripts/test/parser_test.sh`:

| file | content |
|------|---------|
| `parser_d<N>.ttl` | the data the case builds a database from |
| `parser_q<N>.sql` | the SPARQL query |
| `parser_r<N>.txt` | upstream's printed answer (ASCII table, or the aggregated JSON of a graph function) |

`N` = 1..50. The script's expected numbers (`success_expected[N]`, the triples
imported by `build`; `ans_expected[N]`, the query's `AnsNum`) are transcribed
into `tests/dt_upstream_parser_suite.rs`, which runs the suite in-process.

- **Source**: <https://github.com/pkumod/gStore>, `scripts/test/parser_test/`
  and `scripts/test/parser_test.sh`, gStore 1.4.0 (`CPACK_PACKAGE_VERSION`),
  commit `bebe1ea752ef7f5e9bad90e1c035635737129cb0` (2026-04-09).
- **Licence**: BSD 3-Clause, Copyright (c) 2014, Institute of Computer Science
  & Technology of Peking University — see `LICENSE` in this directory (copied
  from upstream). The files are unmodified; do not edit them, add new cases
  elsewhere.

## Running

Upstream runs the cases against a live `ghttp` server
(`bash scripts/test/parser_test.sh`: `build` → `load` → `query` → `unload` →
`drop` per case, comparing `success_num` and `AnsNum`). The Rust port does the
same in-process, additionally compares the result rows with `parser_r<N>.txt`,
and prints a per-case table:

```sh
cargo test --test dt_upstream_parser_suite -- --nocapture
```

A single case runs as e.g. `cargo test --test dt_upstream_parser_suite case_42`.
Documented divergences from the script's numbers (W3C SPARQL 1.1 wins) are in
the `DIVERGENCES` table of the test file.
