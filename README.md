# gStore-rust

A Rust rewrite of the trunk of [pkumod/gStore](https://github.com/pkumod/gStore)
— Peking University's RDF triple store / graph database that answers SPARQL
queries by subgraph matching.

This project re-implements gStore's core data path in idiomatic, well-tested
Rust:

```
RDF file ─▶ parse ─▶ dictionary (string↔id) ─▶ six-way triple index ─▶ disk
SPARQL   ─▶ parse ─▶ graph-pattern algebra ─▶ index match + join ─▶ FILTER ─▶ results
```

It loads RDF (Turtle / N-Triples), encodes it with the same integer-id scheme as
gStore (entities, literals offset by `LITERAL_FIRST_ID`, separate predicate
space), stores it in the classic `s2xx` / `o2xx` / `p2xx` indexes, and evaluates
SPARQL queries through a cost-based optimizer and a VS-tree signature filter.

Implemented (the gStore subsystems originally deferred are now built):

* **Cost-based optimizer** — Selinger DP join ordering over predicate statistics.
* **VS-tree signature index** — a faithful port of gStore's 944-bit signatures +
  signature tree, used as a sound query-time candidate filter.
* **Full SPARQL 1.1 subset** — `SELECT`/`ASK`/`CONSTRUCT`, BGP, `UNION`,
  `OPTIONAL`, `MINUS`, `FILTER`, `BIND`, `VALUES`, sub-`SELECT`, aggregates
  (`GROUP BY`/`HAVING`/`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`/`SAMPLE`/`GROUP_CONCAT`),
  projected `(expr AS ?v)`, property paths (`/ ^ | * + !`), `ORDER BY`,
  `LIMIT`/`OFFSET`, `DISTINCT`, and `INSERT/DELETE DATA`.
* **On-disk B+ tree KVstore** — a paged file (4 KiB blocks) with an LRU page
  cache, B+ trees for the dictionary and the SPO/POS/OSP triple indexes; build,
  persist, and reopen a database entirely on disk (`gbuild --disk`).

Still deferred (tracked in [`docs/REFACTOR_BACKLOG.md`](docs/REFACTOR_BACKLOG.md)):
HTTP/gRPC server, cluster, MVCC/transactions, reasoning, `GRAPH`/`SERVICE`,
streaming query directly off disk.

## Status

* **186 tests pass** — 136 unit tests + 50 data/CLI integration tests.
* The full **LUBM** benchmark (~100k triples, all 14 standard queries) builds in
  ~0.13s in memory and every query runs in ≤1 ms, returning the published answer
  counts; the same dataset also builds to / queries from the on-disk B+ tree
  KVstore with identical results.

## Build & test

```bash
cargo build --release      # library + gbuild / gquery / gconsole
cargo test                 # unit tests + data tests
```

## Command-line tools

Mirrors gStore's CLI. A database is a `<name>.db` directory.

```bash
# Build a database from RDF (Turtle or N-Triples)
gbuild mydb data.nt
gbuild --disk mydb data.nt    # build on the on-disk B+ tree KVstore

# Run a SPARQL query (from a file, or inline with -e)
gquery mydb query.rq
gquery mydb -e "SELECT ?o WHERE { <root> <contain> ?o }"

# Interactive REPL (end a query with ';'; 'help' for commands)
gconsole mydb
```

End-to-end example with the bundled demo data:

```bash
gbuild /tmp/lubm testdata/lubm/lubm.nt
gquery /tmp/lubm testdata/lubm/lubm_q1.rq
```

## Library API

```rust
use gstore::Database;

let mut db = Database::build_from_files("demo", &["data.nt"])?;
db.query("INSERT DATA { <a> <p> <b> }")?;
let rs = db.select("SELECT ?o WHERE { <a> <p> ?o }")?;
println!("{}", rs.to_table_string());
db.save("demo.db")?;             // persist
let db = Database::load("demo.db")?;   // reload
```

## Layout

| module        | role                                       | gStore counterpart            |
|---------------|--------------------------------------------|-------------------------------|
| `model`       | RDF terms, triples, id conventions         | `Util/Triple`, `GlobalTypedef`|
| `dict`        | bidirectional string↔id dictionaries       | `KVstore` *2id / id2* trees   |
| `store`       | in-memory `s2xx`/`o2xx`/`p2xx` six-way index | `KVstore` *ID2values         |
| `kvstore`     | on-disk pager + B+ trees + `DiskStore`      | `KVstore` (`Tree`/`IVArray`…) |
| `signature`   | signatures + VS-tree candidate filter       | `Signature` + VSTree          |
| `parser`      | N-Triples / Turtle / SPARQL                 | `Parser`                      |
| `query`       | algebra eval, joins, optimizer, FILTER      | `Query`, `Database/Executor`, `Optimizer` |
| `db`          | `Database` facade + persistence (mem & disk) | `Database`                   |
| `bin/`        | `gbuild`, `gquery`, `gconsole`              | `gbuild`, `gquery`, `gconsole`|

See [`docs/DESIGN.md`](docs/DESIGN.md) for the architecture and the mapping to
gStore's C++ modules, and [`docs/REFACTOR_BACKLOG.md`](docs/REFACTOR_BACKLOG.md)
for the deferred large-refactor roadmap.

## License

Follows upstream gStore (BSD-3-Clause). This is an independent reimplementation
for study and engineering purposes.
