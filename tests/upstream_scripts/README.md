# Upstream gStore test scripts and Python SDK

Copies of upstream gStore's test scripts and HTTP Python SDK, from upstream
commit `bebe1ea752ef7f5e9bad90e1c035635737129cb0` (BSD 3-Clause, see
`testdata/upstream/LICENSE`). `scripts/conformance.sh` runs them against the
Rust `gserver`; the expected results they encode are also ported to Rust
integration tests (`tests/upstream_*.rs`).

| File | Upstream | Adaptations |
|------|----------|-------------|
| `basic_test.sh` | `scripts/test/basic_test.sh` | none |
| `update_test.sh` | `scripts/test/update_test.sh` | none |
| `transaction_test.sh` | `scripts/test/transaction_test.sh` | none |
| `api_test.sh` | `scripts/test/api_test.sh` | none |
| `parser_test.sh` | `scripts/test/parser_test.sh` | none |
| `python/src/GstoreConnector.py` | `api/http/python/src/GstoreConnector.py` | none |
| `python/example/GET-example.py` | `api/http/python/example/GET-example.py` | 1, 2 |
| `python/example/POST-example.py` | `api/http/python/example/POST-example.py` | 1, 2 |
| `python/sdk_check.py` | — (written for this suite) | — |
| `harness.rs` | — (the Rust tests' shared harness) | — |

The shell scripts need no change: they run from `GSTORE_HOME`, read the HTTP
port from `./conf/conf.ini` and their data from `./data/…` and
`scripts/test/…`, so `scripts/conformance.sh` lays out a temporary
`GSTORE_HOME` with exactly that structure (a `conf.ini` carrying a free port,
the vendored `testdata/upstream/` data, `bin/` linking the built binaries)
and starts `bin/gserver -b` there.

Adaptations of the Python examples (each marked `gstore-rust conformance` in
the file):

1. `Port = 9000` → the port from the `GSTORE_PORT` environment variable
   (default 9000), plus the `import os` this needs.
2. `gc.query("lubm", "json", sparql)` / `gc.fquery("lubm", "json", sparql,
   filename)` pass their arguments in an order that does not match
   `GstoreConnector.query(db_name, sparql, format)` / `fquery(db_name,
   sparql, filename, format)`: upstream sends `"json"` as the SPARQL text (a
   parse error) and `fquery` then tries to save into a file named after the
   query (longer than a file name may be). The calls are reordered to match
   the SDK.

What the scripts print is judged by `scripts/conformance.sh` (see its
`check_<suite>` functions): the upstream scripts print the server's answers
but mostly do not check them (`basic_test.sh` marks every query ✅, the
others print nothing to compare).

Known differences between the scripts' recorded expectations and the answers
the Rust server gives (and SPARQL requires), accepted by the checks:

- `basic_test.sh` records `num_ans=(0 0 0 1)`; `num0`–`num2` answer 8, 1 and
  4 (its `extract_json_num` turns an answer without `AnsNum` into 0).
- `parser_test.sh` records `AnsNum` 0 for cases 15 and 17, whose result files
  (`testdata/upstream/scripts/test/parser_test/parser_r15.txt`, `_r17.txt`)
  hold 4 and 1 rows; cases 33–35 need the phase 2 graph functions, 42 and 50
  phase 3 SPARQL fixes.

`GstoreConnector.py`'s `uploadFile` (it posts a file object as JSON) and
`downloadFile` (it names an undefined variable) cannot work against any
server; `sdk_check.py` does not call them.
