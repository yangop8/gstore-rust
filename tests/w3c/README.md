# tests/w3c — a hand-authored W3C-style SPARQL 1.1 subset

These cases are an oracle for the SPARQL engine that does not depend on the
implementation: each one is data, a query (or update) and the answer the
SPARQL 1.1 recommendations require, written by hand from the specification
text — [SPARQL 1.1 Query Language](https://www.w3.org/TR/sparql11-query/)
and [SPARQL 1.1 Update](https://www.w3.org/TR/sparql11-update/) — and
citing the section it checks. The other SPARQL tests of the repository
compare the engine with itself (streaming against materializing executor,
one storage layout against another), so a bug every path shares passes them;
these cases catch it (review parser#18 / query#18 / sparqlconf#18).

`tests/w3c_subset.rs` runs them: `cargo test --test w3c_subset` (add
`--features rocksdb` for the RocksDB layout, `-- --nocapture` for a line per
case, `W3C_FILTER=<substring>` to run only the cases whose `area/case` name
contains the substring).

## Layout of a case

One directory per case, `tests/w3c/<area>/<case>/`, holding exactly these
files (any other file is an error):

| file | required | content |
|------|----------|---------|
| `data.ttl` or `data.trig` | no | the dataset: Turtle for the default graph, TriG for named graphs too (none: the empty dataset) |
| `query.rq` | one of the two | a SELECT, ASK, CONSTRUCT or DESCRIBE query |
| `update.ru` | one of the two | a SPARQL 1.1 Update request |
| `expected.srj` | with a SELECT / ASK | the answer in [SPARQL 1.1 Query Results JSON](https://www.w3.org/TR/sparql11-results-json/) |
| `expected.nt` | with a CONSTRUCT / DESCRIBE | the answer graph in N-Triples |
| `expected.nq` | with `update.ru` | the whole dataset after the update, in N-Quads (default graph: triples without a graph term) |
| `xfail` | no | the case exposes a known engine bug: see below |

The query or update must contain a comment line `# spec: <document> §<section> <title>`
citing what it checks (more comment lines may explain the expected answer).
Areas group related cases; `basic/` is the regression net of standard
features, `subquery/` every position a sub-SELECT may take, `tricky/` the
corner cases (and the known bugs). A new area is just a new directory.

## What a run does

Every case runs on every storage layout — in memory, saved as a snapshot and
reopened, the on-disk store in Materialize and in Stream mode, and RocksDB
with `--features rocksdb` — and through three executor settings: the
materializing evaluator (`TopKOptions::NONE`), and the streaming executor
with the bounded heap and with DP-B forced. The streaming executor only runs
for queries with a `LIMIT` (and for ASK / EXISTS), so the harness appends
`LIMIT 1000000` to a SELECT or CONSTRUCT that has no top-level `LIMIT`
(before a trailing `VALUES`) for the streaming settings. An update runs on a
fresh database per layout and setting; the dataset it leaves is read through
the N-Quads export, and again after the database is closed and reopened.

## How answers compare

- SELECT: a bag of solutions (variables matched by name, the head's
  variable set must be equal). When the top-level query has `ORDER BY`, the
  rows must come in the expected order, except that rows tied on the sort
  keys may come in any order among themselves. Ties are read off the
  expected rows when every key is a projected plain variable (`?x`,
  `ASC(?x)`, `DESC(?x)`); with any other key the expected order is strict, so
  write such cases without ties.
- CONSTRUCT / DESCRIBE / datasets: sets of triples / quads.
- Blank nodes match up to a consistent one-to-one relabelling across the
  whole answer: write any labels in the expected file.
- RDF terms compare exactly: lexical form, datatype and language tag. Numbers
  are **not** normalised — `"01"^^xsd:integer` is not `"1"^^xsd:integer`, and
  an `xsd:decimal` result is written in its canonical form (`"20.0"`). The
  only identities are RDF 1.1's: a simple literal is the same term as the
  `xsd:string` literal, and language tags compare case-insensitively.

## xfail: a known bug

A case whose answer the engine gets wrong today, for a confirmed review
finding, carries a file `xfail` whose **first line is the review id** (for
example `query#2`); the following lines may say what goes wrong. Such a case
must miss on at least one layout or executor; when it passes everywhere the
run fails with `XPASS`, so the change that fixes the bug deletes the marker
in the same commit. A case that fails without the marker fails the run. A
divergence the review did not record is reported in the fixing round's
changelog fragment; its case may carry the id of the finding whose fix it
shares, with a line saying so.

## Writing a case

1. Pick the spec section and copy (or adapt) its example; keep the data
   small. Prefer IRIs under `http://example.org/` or the spec's own.
2. Work the answer out from the spec text, not from what the engine prints.
3. Run `W3C_FILTER=<area>/<case> cargo test --test w3c_subset w3c_subset -- --nocapture`.
   If it fails, decide whether the case or the engine is wrong; an engine bug
   gets an `xfail` marker with its review id (or a new finding).
