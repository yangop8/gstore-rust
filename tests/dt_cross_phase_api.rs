//! Cross-phase integration over the wire protocols (`/api` and REST): the
//! Phase 1 server's write paths and privilege checks meet the Phase 2/3
//! engine.
//!
//! * **Caches.** Every database keeps a byte-bounded query cache (Phase 3)
//!   and a CSR for the advanced graph functions (Phase 2), both keyed to its
//!   data. Every write path the `/api` layer offers — `batchInsert`,
//!   `batchRemove`, an update sent through `query`, a transaction's commit
//!   and rollback, `restore`, a `/build` that overwrites, `drop` and a
//!   rebuild under the same name — must leave no stale entry behind: a
//!   cached `COUNT` and a graph-function answer computed before the write
//!   must not be served after it.
//! * **Classification.** `/api` `query` and REST `/sparql` decide read vs
//!   write (query vs update privilege) by parsing the statement. The Phase 3
//!   update forms (`ADD` / `MOVE` / `COPY`, `WITH … DELETE/INSERT`, `LOAD …
//!   INTO GRAPH`, `INSERT DATA` with `[ … ]` / `( … )`) are updates and need
//!   the update privilege; graph-function SELECTs are reads.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::path::Path;

use gstore::json::Json;
use harness::*;

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n";
const EXTRA: &str = "<http://ex/c> <http://ex/p> <http://ex/d> .\n";
const COUNT: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
const REACH: &str = "SELECT (shortestPathLen(<http://ex/a>, <http://ex/d>, true, {}) AS ?l) WHERE {}";
const INSERT: &str = "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }";
const DELETE: &str = "DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/d> }";

/// The single cell of a one-row answer.
fn cell(j: &Json) -> String {
    assert_eq!(code(j), 0, "{j}");
    let t = answer_table(j);
    assert_eq!(t.rows.len(), 1, "{j}");
    t.rows[0][0].clone().unwrap_or_default()
}

/// `(triple count, whether <a> reaches <d>)` as `db` answers now — each
/// asked twice, so the second answer comes from the query cache when there
/// is an entry (and the graph function from the cached CSR).
fn state(srv: &Server, db: &str) -> (String, bool) {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let n = cell(&srv.query(db, COUNT));
        let reach = cell(&srv.query(db, REACH));
        seen.push((n, reach.contains("\"length\":3")));
    }
    assert_eq!(seen[0], seen[1], "a repeated query answers the same");
    let (n, reach) = seen.pop().unwrap();
    let n = n
        .trim_start_matches('"')
        .split('"')
        .next()
        .unwrap()
        .to_string();
    (n, reach)
}

fn write_file(dir: &Path, name: &str, content: &str) -> String {
    let p = dir.join(name);
    fs::write(&p, content).unwrap();
    path_str(&p)
}

#[test]
fn every_write_path_invalidates_the_query_cache_and_the_csr() {
    let data = scratch("xphase_cache_data");
    let base = write_file(&data, "base.nt", BASE);
    let extra = write_file(&data, "extra.nt", EXTRA);
    let srv = Server::start_with_dir("xphase_cache", Some(&data));
    ok(&srv.build("cache", Path::new(&base)));
    srv.load("cache");
    let without = ("2".to_string(), false);
    let with = ("3".to_string(), true);
    assert_eq!(state(&srv, "cache"), without);

    // batchInsert / batchRemove.
    ok(&srv.op("batchInsert", &[("db_name", "cache"), ("file", &extra)]));
    assert_eq!(state(&srv, "cache"), with, "after batchInsert");
    ok(&srv.op("batchRemove", &[("db_name", "cache"), ("file", &extra)]));
    assert_eq!(state(&srv, "cache"), without, "after batchRemove");

    // An update sent through `query`.
    ok(&srv.query("cache", INSERT));
    assert_eq!(state(&srv, "cache"), with, "after INSERT DATA");
    ok(&srv.query("cache", DELETE));
    assert_eq!(state(&srv, "cache"), without, "after DELETE DATA");

    // A transaction: committed, then rolled back after in-transaction reads
    // (which see, and may cache, the uncommitted state).
    let begin = |srv: &Server| {
        let j = srv.op("begin", &[("db_name", "cache"), ("isolevel", "1")]);
        ok(&j);
        string(&j, "TID")
    };
    let tquery = |srv: &Server, tid: &str, q: &str| {
        let j = srv.op("tquery", &[("db_name", "cache"), ("tid", tid), ("sparql", q)]);
        ok(&j);
        j
    };
    let tid = begin(&srv);
    tquery(&srv, &tid, INSERT);
    assert!(cell(&tquery(&srv, &tid, REACH)).contains("\"length\":3"));
    assert_eq!(state(&srv, "cache"), without, "others read the begin-time snapshot");
    ok(&srv.op("commit", &[("db_name", "cache"), ("tid", &tid)]));
    assert_eq!(state(&srv, "cache"), with, "after commit");
    let tid = begin(&srv);
    tquery(&srv, &tid, DELETE);
    assert!(cell(&tquery(&srv, &tid, COUNT)).contains('2'));
    assert!(!cell(&tquery(&srv, &tid, REACH)).contains("\"length\":3"));
    ok(&srv.op("rollback", &[("db_name", "cache"), ("tid", &tid)]));
    assert_eq!(state(&srv, "cache"), with, "after rollback");

    // restore: back up this state, change it, restore it.
    ok(&srv.op("backup", &[("db_name", "cache")]));
    ok(&srv.query("cache", DELETE));
    assert_eq!(state(&srv, "cache"), without);
    ok(&srv.op("restore", &[("db_name", "cache")]));
    assert_eq!(state(&srv, "cache"), with, "after restore");

    // A REST /build that overwrites the loaded database.
    let (s, b) = srv.rest("POST", "/build?db_name=cache", BASE);
    assert_eq!(s, 200, "{b}");
    assert_eq!(state(&srv, "cache"), without, "after /build overwrote it");

    // drop, then build and load the same name again.
    srv.unload("cache");
    srv.drop_db("cache");
    let both = write_file(&data, "both.nt", &format!("{BASE}{EXTRA}"));
    ok(&srv.build("cache", Path::new(&both)));
    srv.load("cache");
    assert_eq!(state(&srv, "cache"), with, "after drop and rebuild");
}
