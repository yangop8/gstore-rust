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
const REACH: &str =
    "SELECT (shortestPathLen(<http://ex/a>, <http://ex/d>, true, {}) AS ?l) WHERE {}";
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
        let j = srv.op(
            "tquery",
            &[("db_name", "cache"), ("tid", tid), ("sparql", q)],
        );
        ok(&j);
        j
    };
    let tid = begin(&srv);
    tquery(&srv, &tid, INSERT);
    assert!(cell(&tquery(&srv, &tid, REACH)).contains("\"length\":3"));
    assert_eq!(
        state(&srv, "cache"),
        without,
        "others read the begin-time snapshot"
    );
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

#[test]
fn update_forms_need_the_update_privilege_and_graph_functions_only_query() {
    let data = scratch("xphase_priv_data");
    let base = write_file(&data, "base.nt", &format!("{BASE}{EXTRA}"));
    let srv = Server::start_with_dir("xphase_priv", Some(&data));
    ok(&srv.build("priv", Path::new(&base)));
    srv.load("priv");
    for (user, privileges) in [("reader", "query"), ("writer", "query,update")] {
        ok(&srv.op(
            "usermanage",
            &[("type", "1"), ("op_username", user), ("op_password", "pw")],
        ));
        ok(&srv.op(
            "userprivilegemanage",
            &[
                ("type", "1"),
                ("op_username", user),
                ("db_name", "priv"),
                ("privileges", privileges),
            ],
        ));
    }
    let as_user = |user: &str, sparql: &str| {
        let body = Json::obj()
            .with("operation", "query")
            .with("username", user)
            .with("password", "pw")
            .with("db_name", "priv")
            .with("sparql", sparql);
        srv.post_raw(&body.to_string())
    };
    let rest_as = |user: &str, target: &str, sparql: &str| {
        let auth = match user {
            "reader" => "Basic cmVhZGVyOnB3", // reader:pw
            _ => "Basic d3JpdGVyOnB3",        // writer:pw
        };
        http_with(
            &srv.addr,
            "POST",
            target,
            &[("Authorization", auth)],
            sparql.as_bytes(),
        )
    };

    // Graph-function SELECTs (and a Phase 3 SELECT) are reads.
    for q in [
        REACH,
        "SELECT (PR(true, {}, 2, alpha=0.85, maxIter=10, tol=0.001) AS ?r) WHERE {}",
        "SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(?o IN (<http://ex/b>, <http://ex/c>)) } \
         VALUES ?s { <http://ex/a> }",
    ] {
        let j = as_user("reader", q);
        assert_eq!(code(&j), 0, "{q}: {j}");
        let (s, b) = rest_as("reader", "/sparql?db_name=priv", q);
        assert_eq!(s, 200, "{q}: {b}");
    }

    // Every Phase 3 update form is an update.
    let updates = [
        "ADD DEFAULT TO <http://ex/g1>",
        "COPY DEFAULT TO <http://ex/g2>",
        "MOVE SILENT <http://ex/g2> TO <http://ex/g3>",
        "WITH <http://ex/g1> DELETE { ?s ?p ?o } INSERT { ?s <http://ex/q> ?o } WHERE { ?s ?p ?o }",
        "WITH <http://ex/g1> DELETE WHERE { ?s <http://ex/q> ?o }",
        "INSERT { GRAPH <http://ex/g4> { ?s ?p ?o } } USING <http://ex/g3> WHERE { ?s ?p ?o }",
        "LOAD SILENT <file:///nonexistent/data.nt> INTO GRAPH <http://ex/g5>",
        "INSERT DATA { <http://ex/x> <http://ex/p> [ <http://ex/q> ( 1 2 ) ] }",
        "PREFIX ex: <http://ex/>\n# a comment cannot hide it\nCOPY SILENT GRAPH ex:g1 TO DEFAULT",
    ];
    for u in updates {
        let j = as_user("reader", u);
        assert_eq!(
            (code(&j), msg_of(&j).as_str()),
            (1004, "You have no update privilege"),
            "{u}"
        );
        let (s, b) = rest_as("reader", "/sparql?db_name=priv", u);
        assert_eq!(s, 403, "{u}: {b}");
        let (s, b) = rest_as("reader", "/update?db_name=priv", u);
        assert_eq!(s, 403, "{u}: {b}");
    }
    // Nothing changed meanwhile.
    assert_eq!(
        cell(&srv.query("priv", COUNT))
            .trim_start_matches('"')
            .split('"')
            .next(),
        Some("3")
    );
    for u in updates {
        let j = as_user("writer", u);
        assert_eq!(code(&j), 0, "{u}: {j}");
    }
    let (s, b) = rest_as(
        "writer",
        "/update?db_name=priv",
        "COPY DEFAULT TO <http://ex/g6>",
    );
    assert_eq!(s, 200, "{b}");
    // Transactions need the update privilege up front (upstream's
    // check_privilege for begin / tquery / commit / rollback), so a
    // query-only user cannot run any of these forms inside one either.
    let (s, b) = rest_as("reader", "/begin?db_name=priv", "");
    assert_eq!(s, 403, "{b}");
    let j = srv.post_raw(
        &Json::obj()
            .with("operation", "begin")
            .with("username", "reader")
            .with("password", "pw")
            .with("db_name", "priv")
            .to_string(),
    );
    assert_eq!(code(&j), 1004, "{j}");
}

fn msg_of(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}
