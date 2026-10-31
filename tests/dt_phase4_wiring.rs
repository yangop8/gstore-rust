//! Phase 4 cross-package wiring: the reasoning rules (`reasonManage`, GAP_PLAN
//! §3.2) meet the PFN user functions (§3.3), the advanced graph functions
//! and the caches.
//!
//! * **Caches.** Executing a rule adds `<Rule:label>` edges and disabling it
//!   retracts them through the database's update path, so the query cache
//!   and the CSR the graph functions and PFN functions run on are rebuilt:
//!   a SELECT, a graph function (`kHopCount`, `shortestPathLen`) and — with
//!   the `pfn-wasm` runtime — a PFN function computed before see the new
//!   edges after, and stop seeing them once the rule is disabled, each
//!   answered twice (the second time from the caches).

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;

use gstore::json::{self, Json};
use gstore::{Database, QueryResult};
use harness::*;

/// `a`'s mother `m` and `u` share their father `g`; `u` is male: `u` is
/// `a`'s uncle.
const FAMILY: &str = "<http://ex/a> <http://ex/mother> <http://ex/m> .\n\
                      <http://ex/m> <http://ex/father> <http://ex/g> .\n\
                      <http://ex/u> <http://ex/father> <http://ex/g> .\n\
                      <http://ex/u> <http://ex/sex> \"male\" .\n\
                      <http://ex/m> <http://ex/sex> \"female\" .\n";

/// Upstream's relation rule (`type` 1, `logic` 1) over [`FAMILY`].
const UNCLE: &str = r#"{
    "rulename": "uncle",
    "description": "a mother's brother",
    "isenable": 1,
    "type": 1,
    "logic": 1,
    "conditions": [{
        "patterns": [
            {"subject": "?x", "predicate": "<http://ex/mother>", "object": "?y"},
            {"subject": "?y", "predicate": "<http://ex/father>", "object": "?z"},
            {"subject": "?k", "predicate": "<http://ex/father>", "object": "?z"},
            {"subject": "?k", "predicate": "<http://ex/sex>", "object": "\"male\""}
        ],
        "filters": []
    }],
    "return": {"source": "?x", "target": "?k", "label": "uncle"}
}"#;

const COUNT: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
const UNCLES: &str = "SELECT ?k WHERE { <http://ex/a> <Rule:uncle> ?k }";
const HOPS: &str = "SELECT (kHopCount(<http://ex/a>, true, 1, {<Rule:uncle>}) AS ?c) WHERE {}";
const PATH: &str =
    "SELECT (shortestPathLen(<http://ex/a>, <http://ex/u>, true, {}) AS ?l) WHERE {}";

/// `POST /api` as `user` (`root`'s password is upstream's `123456`, every
/// other user's `pw`) with `fields`.
fn api_as(srv: &Server, user: &str, fields: Vec<(&str, Json)>) -> Json {
    let pass = if user == USERNAME { PASSWORD } else { "pw" };
    let mut body = Json::obj().with("username", user).with("password", pass);
    for (k, v) in fields {
        body.set(k, v);
    }
    srv.post_raw(&body.to_string())
}

/// `reasonManage` `type` on `db`: `rulename` for types 2-8, the
/// [`UNCLE`] object (as upstream clients send it) for type 1.
fn reason(srv: &Server, db: &str, ty: &str) -> Json {
    let mut fields = vec![
        ("operation", Json::from("reasonManage")),
        ("db_name", db.into()),
        ("type", ty.into()),
    ];
    if ty == "1" {
        fields.push(("ruleinfo", json::parse(UNCLE).unwrap()));
    } else {
        fields.push(("rulename", "uncle".into()));
    }
    api_as(srv, USERNAME, fields)
}

/// The single cell of a one-row answer, in gStore's text form.
fn cell(j: &Json) -> String {
    ok(j);
    let t = answer_table(j);
    assert_eq!(t.rows.len(), 1, "{j}");
    t.rows[0][0].clone().unwrap_or_default()
}

/// What `db` answers now: (triple count, `a`'s uncles, the `kHopCount`
/// document along `<Rule:uncle>`, the `shortestPathLen` document) — each
/// asked twice, the second time from the query cache and the cached CSR.
fn state(srv: &Server, db: &str) -> (String, usize, String, String) {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let n = cell(&srv.query(db, COUNT));
        let uncles = srv.query(db, UNCLES);
        ok(&uncles);
        let uncles = answer_table(&uncles).rows.len();
        let hops = cell(&srv.query(db, HOPS));
        let path = cell(&srv.query(db, PATH));
        seen.push((n, uncles, hops, path));
    }
    assert_eq!(seen[0], seen[1], "a repeated query answers the same");
    seen.pop().unwrap()
}

/// A server whose `build` may read `data_dir`, with `FAMILY` built and
/// loaded as `db`.
fn family_server(tag: &str, db: &str) -> Server {
    let data = scratch(&format!("{tag}_data"));
    let file = data.join("family.nt");
    fs::write(&file, FAMILY).unwrap();
    let srv = Server::start_with_dir(tag, Some(&data));
    ok(&srv.build(db, &file));
    srv.load(db);
    srv
}

#[test]
fn executing_a_rule_reaches_select_graph_functions_and_their_caches() {
    let srv = family_server("p4w_gfun", "fam");
    // The CSR is built (and the answers cached) before the rule runs.
    ok(&srv.op("loadcsr", &[("db_name", "fam")]));
    let before = state(&srv, "fam");
    assert_eq!(
        before.0,
        "\"5\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );
    assert_eq!(before.1, 0);
    assert!(before.2.contains("\"count\":0"), "{}", before.2);
    // `a` does not reach `u` along directed edges (a → m → g ← u).
    assert!(!before.3.contains("\"length\":1"), "{}", before.3);

    ok(&reason(&srv, "fam", "1"));
    ok(&reason(&srv, "fam", "3"));
    // Compiling and checking change no data: the caches stay valid.
    assert_eq!(int(&reason(&srv, "fam", "8"), "effectNum"), 1);
    assert_eq!(state(&srv, "fam"), before);

    let j = reason(&srv, "fam", "4");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1, "{j}");
    let after = state(&srv, "fam");
    assert_eq!(after.0, "\"6\"^^<http://www.w3.org/2001/XMLSchema#integer>");
    assert_eq!(after.1, 1);
    assert!(after.2.contains("\"count\":1"), "{}", after.2);
    assert!(after.3.contains("\"length\":1"), "{}", after.3);

    let j = reason(&srv, "fam", "5");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1, "{j}");
    let disabled = state(&srv, "fam");
    assert_eq!(disabled.0, before.0);
    assert_eq!(disabled.1, 0);
    assert!(disabled.2.contains("\"count\":0"), "{}", disabled.2);
    assert_eq!(disabled.3, before.3);
}

/// The embedded API: `Database::reason_execute` / `reason_disable` refresh
/// the cached CSR the graph functions use and the query cache.
#[test]
fn the_embedded_rule_lifecycle_refreshes_the_csr_and_the_query_cache() {
    let mut db = Database::build_from_str("p4w_embedded", FAMILY).unwrap();
    db.preload_csr().unwrap();
    let hops = |db: &mut Database| -> String {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let QueryResult::Select(rs) = db.query(HOPS).unwrap() else {
                panic!("not a SELECT answer");
            };
            assert_eq!(rs.rows.len(), 1);
            // The cell is the literal in N-Triples form: unescape its quotes.
            let cell = rs.rows[0][0].clone().unwrap_or_default();
            seen.push(cell.replace("\\\"", "\""));
        }
        assert_eq!(seen[0], seen[1]);
        seen.pop().unwrap()
    };
    let uncles = |db: &mut Database| match db.query(UNCLES).unwrap() {
        QueryResult::Select(rs) => rs.rows.len(),
        other => panic!("{other:?}"),
    };
    let before = hops(&mut db);
    assert!(before.contains("\"count\":0"), "{before}");
    assert_eq!(uncles(&mut db), 0);
    assert!(db.csr_is_cached());

    db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
    db.reason_compile("uncle").unwrap();
    assert_eq!(db.reason_execute("uncle").unwrap().inserted, 1);
    assert!(!db.csr_is_cached(), "the CSR of the old data is not reused");
    let after = hops(&mut db);
    assert!(after.contains("\"count\":1"), "{after}");
    assert_eq!(uncles(&mut db), 1);

    assert_eq!(db.reason_disable("uncle").unwrap().removed, 1);
    assert_eq!(hops(&mut db), before);
    assert_eq!(uncles(&mut db), 0);
}

// ---------------------------------------------------------------------------
// PFN (feature pfn-wasm)
// ---------------------------------------------------------------------------

/// The ```` ```wat <tag> ```` block of `docs/PFN.md`.
#[cfg(feature = "pfn-wasm")]
fn doc_block(tag: &str) -> String {
    const DOC: &str = include_str!("../docs/PFN.md");
    let fence = format!("```wat {tag}\n");
    let start = DOC.find(&fence).unwrap_or_else(|| panic!("no {tag} block")) + fence.len();
    let rest = &DOC[start..];
    rest[..rest.find("\n```").expect("closing fence")].to_string()
}

/// `funcudb` `kind` as `user` with `info`: the answer.
#[cfg(feature = "pfn-wasm")]
fn funcudb(srv: &Server, user: &str, kind: &str, info: Json) -> Json {
    api_as(
        srv,
        user,
        vec![
            ("operation", "funcudb".into()),
            ("type", kind.into()),
            ("funInfo", info),
        ],
    )
}

/// A `funInfo` object.
#[cfg(feature = "pfn-wasm")]
fn fun_info(name: &str, body: &str, subs: &str) -> Json {
    Json::obj()
        .with("funName", name)
        .with("funDesc", "")
        .with("funBody", body)
        .with("funSubs", subs)
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn executing_a_rule_changes_what_a_pfn_function_sees() {
    let srv = family_server("p4w_pfn", "fam");
    ok(&srv.op(
        "usermanage",
        &[
            ("type", "1"),
            ("op_username", "alice"),
            ("op_password", "pw"),
        ],
    ));
    ok(&srv.op(
        "userprivilegemanage",
        &[
            ("type", "1"),
            ("op_username", "alice"),
            ("db_name", "fam"),
            ("privileges", "1"),
        ],
    ));
    // docs/PFN.md's outDegree, as root's and as alice's own function (sent
    // as written: it holds no character URL-decoding would change).
    let (body, subs) = (
        doc_block("funBody=outDegree"),
        doc_block("funSubs=outDegree"),
    );
    assert!(!body.contains(['%', '+']) && !subs.contains(['%', '+']));
    for user in [USERNAME, "alice"] {
        ok(&funcudb(
            &srv,
            user,
            "1",
            fun_info("outDegree", &body, &subs),
        ));
        ok(&funcudb(
            &srv,
            user,
            "4",
            Json::obj().with("funName", "outDegree"),
        ));
    }
    // `a`'s out-degree along `<Rule:uncle>` and along every predicate, as
    // `user` sees it — asked twice (the second time on the cached CSR).
    let degrees = |user: &str| -> (i64, i64) {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let mut d = Vec::new();
            for arg in [
                r#"{"src":"<http://ex/a>","pred":"<Rule:uncle>"}"#,
                r#"{"src":"<http://ex/a>"}"#,
            ] {
                let j = api_as(
                    &srv,
                    user,
                    vec![
                        ("operation", "query".into()),
                        ("db_name", "fam".into()),
                        (
                            "sparql",
                            format!("select (PFN.outDegree({arg}) AS ?r) WHERE {{}}").into(),
                        ),
                    ],
                );
                ok(&j);
                let r = j.get("results").unwrap_or_else(|| panic!("{j}"));
                assert_eq!(
                    r.get("src").and_then(Json::as_str),
                    Some("<http://ex/a>"),
                    "{j}"
                );
                d.push(r.get("outDegree").and_then(Json::as_i64).expect("degree"));
            }
            seen.push((d[0], d[1]));
        }
        assert_eq!(seen[0], seen[1], "a repeated call answers the same");
        seen[0]
    };
    // A graph function first: the CSR is built and cached before the rule.
    ok(&srv.query("fam", HOPS));
    for user in [USERNAME, "alice"] {
        assert_eq!(degrees(user), (0, 1), "{user}");
    }
    ok(&reason(&srv, "fam", "1"));
    ok(&reason(&srv, "fam", "3"));
    assert_eq!(int(&reason(&srv, "fam", "4"), "AnsNum"), 1);
    for user in [USERNAME, "alice"] {
        assert_eq!(degrees(user), (1, 2), "{user}");
    }
    assert_eq!(int(&reason(&srv, "fam", "5"), "AnsNum"), 1);
    for user in [USERNAME, "alice"] {
        assert_eq!(degrees(user), (0, 1), "{user}");
    }
    // Executed again: the same edge again (the rule re-infers it).
    assert_eq!(int(&reason(&srv, "fam", "4"), "AnsNum"), 1);
    assert_eq!(degrees("alice"), (1, 2));
}
