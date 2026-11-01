//! Phase 4 cross-package wiring: the reasoning rules (`reasonManage`, GAP_PLAN
//! §3.2) meet the PFN user functions (§3.3), the advanced graph functions,
//! the caches, `conf.ini`, the privilege model and the REST layer.
//!
//! * **Caches.** Executing a rule adds `<Rule:label>` edges and disabling it
//!   retracts them through the database's update path, so the query cache
//!   and the CSR the graph functions and PFN functions run on are rebuilt:
//!   a SELECT, a graph function (`kHopCount`, `shortestPathLen`) and — with
//!   the `pfn-wasm` runtime — a PFN function computed before see the new
//!   edges after, and stop seeing them once the rule is disabled, each
//!   answered twice (the second time from the caches).
//! * **conf.ini.** `refreshconf` re-reads the `[pfn]` section: the registry's
//!   limits (`pfn_max_functions`) and — on a database already loaded — the
//!   sandbox limits a query runs under (`pfn_fuel_per_call`).
//! * **Privileges.** A user created over `/api` holds no privilege yet may
//!   manage their own PFN functions (upstream grants `funquery` / `funcudb`
//!   / `funreview` to everyone) but no reasoning rule (`reasonManage` is
//!   root's); `showuser` lists upstream's seven privileges, nothing new.
//! * **REST.** A database the REST layer built in memory keeps its rules in
//!   the directory it is served from, so REST `/backup` carries them even
//!   when no `reasonManage` call reached it in this process.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
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

/// A server with [`FAMILY`] built (from a file in a scratch directory its
/// `build` may read) and loaded as `db`.
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

// ---------------------------------------------------------------------------
// conf.ini [pfn] and refreshconf
// ---------------------------------------------------------------------------

#[test]
fn refreshconf_applies_the_pfn_section() {
    let dir = scratch("p4w_refresh");
    let conf = dir.join("conf.ini");
    let home = dir.join("home");
    let write_conf = |pfn: &str| {
        fs::write(
            &conf,
            format!(
                "[server]\nport=0\nip_deny_path=off\n[system]\ndb_home={}\n[pfn]\n{pfn}\n",
                home.display()
            ),
        )
        .unwrap();
    };
    write_conf("pfn_max_functions=1");
    let mut cfg = GStoreConfig::load(&conf).unwrap();
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    cfg.listen_addr = "127.0.0.1".to_string();
    cfg.root_password = PASSWORD.to_string();
    let server = Arc::new(ApiServer::bind_with_config(cfg).unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let api = |fields: Vec<(&str, Json)>| {
        let mut body = Json::obj()
            .with("username", USERNAME)
            .with("password", PASSWORD);
        for (k, v) in fields {
            body.set(k, v);
        }
        let (status, text) = http(&addr, "POST", "/api", body.to_string().as_bytes());
        assert_eq!(status, 200, "{text}");
        json::parse(&text).unwrap()
    };
    let create = |name: &str, body: &str| {
        api(vec![
            ("operation", "funcudb".into()),
            ("type", "1".into()),
            ("funInfo", fun_info(name, body, "")),
        ])
    };
    let refresh = || ok(&api(vec![("operation", "refreshconf".into())]));
    ok(&create("first", "(i32.const 1)"));
    let j = create("second", "(i32.const 1)");
    assert_eq!(code(&j), 1005, "{j}");
    assert!(
        string(&j, "StatusMsg").contains("pfn_max_functions = 1"),
        "{j}"
    );
    // A loop of 100 000 iterations — well inside the default instruction
    // budget, far beyond 1 000 instructions.
    let spin = "(local $i i32) (local.set $i (i32.const 100000)) \
                (loop $l (local.set $i (i32.sub (local.get $i) (i32.const 1))) \
                (br_if $l (local.get $i))) (i32.const 1)";
    write_conf("pfn_max_functions=2\npfn_fuel_per_call=1000");
    refresh();
    assert_eq!(server.pfn_registry().limits().max_functions, 2);
    assert_eq!(server.pfn_registry().limits().fuel_per_call, 1000);
    ok(&create("spin", spin));
    assert_eq!(code(&create("third", "(i32.const 1)")), 1005);

    // A database loaded before refreshconf runs its queries under the new
    // sandbox limits.
    if cfg!(feature = "pfn-wasm") {
        // REST /build takes the data in its body.
        let (status, text) = http_with(
            &addr,
            "POST",
            "/build?db_name=fuel",
            &[("Authorization", "Basic cm9vdDoxMjM0NTY=")],
            FAMILY.as_bytes(),
        );
        assert_eq!(status, 200, "{text}");
        ok(&api(vec![
            ("operation", "funcudb".into()),
            ("type", "4".into()),
            ("funInfo", Json::obj().with("funName", "spin")),
        ]));
        let run = || {
            api(vec![
                ("operation", "query".into()),
                ("db_name", "fuel".into()),
                ("sparql", "SELECT (PFN.spin({}) AS ?v) WHERE {}".into()),
            ])
        };
        let j = run();
        assert_eq!(code(&j), 1005, "{j}");
        assert!(
            string(&j, "StatusMsg").contains("instruction budget of 1000"),
            "{j}"
        );
        write_conf("pfn_max_functions=2\npfn_fuel_per_call=0");
        refresh();
        ok(&run());
    }
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Privileges
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_api_user_manages_pfn_functions_but_not_rules() {
    let srv = family_server("p4w_priv", "fam");
    ok(&srv.op(
        "usermanage",
        &[("type", "1"), ("op_username", "bob"), ("op_password", "pw")],
    ));
    ok(&funcudb(
        &srv,
        USERNAME,
        "1",
        fun_info("roots", "(i32.const 1)", ""),
    ));
    // No privilege on any database, yet the PFN operations are bob's — on
    // his own functions only.
    ok(&funcudb(
        &srv,
        "bob",
        "1",
        fun_info("mine", "(i32.const 1)", ""),
    ));
    let names = |user: &str| -> Vec<String> {
        let j = api_as(&srv, user, vec![("operation", "funquery".into())]);
        ok(&j);
        j.get("list")
            .and_then(Json::as_array)
            .unwrap_or_else(|| panic!("no list in {j}"))
            .iter()
            .map(|f| string(f, "funName"))
            .collect()
    };
    assert_eq!(names("bob"), ["mine"]);
    assert_eq!(names(USERNAME), ["roots"]);
    let j = api_as(
        &srv,
        "bob",
        vec![
            ("operation", "funreview".into()),
            ("funInfo", fun_info("mine", "(i32.const 1)", "")),
        ],
    );
    ok(&j);
    assert!(string(&j, "result").contains("i32.const"), "{j}");
    ok(&funcudb(
        &srv,
        "bob",
        "3",
        Json::obj().with("funName", "mine"),
    ));
    // … but no reasoning rule: reasonManage is root's (upstream's
    // check_privilege grants it to nobody else), whatever the type.
    ok(&reason(&srv, "fam", "1"));
    for ty in ["1", "2", "3", "4", "5", "6", "7", "8"] {
        let j = api_as(
            &srv,
            "bob",
            vec![
                ("operation", "reasonManage".into()),
                ("db_name", "fam".into()),
                ("type", ty.into()),
                ("rulename", "uncle".into()),
                ("ruleinfo", json::parse(UNCLE).unwrap()),
            ],
        );
        assert_eq!(code(&j), 1004, "{j}");
        assert_eq!(
            string(&j, "StatusMsg"),
            "You have no reasonManage privilege"
        );
    }
    assert!(names("bob").is_empty());
    // showuser lists upstream's seven privileges: Phase 4 adds none.
    let j = srv.op("showuser", &[]);
    ok(&j);
    let bob = j
        .get("ResponseBody")
        .and_then(Json::as_array)
        .and_then(|users| {
            users
                .iter()
                .find(|u| u.get("username").and_then(Json::as_str) == Some("bob"))
        })
        .unwrap_or_else(|| panic!("bob in {j}"))
        .clone();
    let mut keys: Vec<String> = bob
        .as_object()
        .expect("an object")
        .iter()
        .map(|(k, _)| k.clone())
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "backup_privilege",
            "export_privilege",
            "load_privilege",
            "password",
            "query_privilege",
            "restore_privilege",
            "unload_privilege",
            "update_privilege",
            "username",
        ]
    );
    for (k, v) in bob.as_object().unwrap() {
        if k.ends_with("_privilege") {
            assert_eq!(v.as_str(), Some(""), "{k} of a fresh user");
        }
    }
}

// ---------------------------------------------------------------------------
// REST
// ---------------------------------------------------------------------------

/// A database the REST layer built in memory has no home directory; it
/// still keeps its rules in the directory it is served from, so REST
/// `/backup` carries rules that reached that directory without a
/// `reasonManage` call in this process.
#[test]
fn rest_backup_carries_the_rules_of_a_database_rest_built() {
    let srv = family_server("p4w_rest", "fam");
    // A rule file written through /api on another database …
    ok(&reason(&srv, "fam", "1"));
    let rule = srv.root.join("fam.db/reason_rule_files/uncle.json");
    assert!(rule.is_file());
    // … copied into the directory of one REST builds.
    let (status, text) = srv.rest("POST", "/build?db_name=restdb", FAMILY);
    assert_eq!(status, 200, "{text}");
    let rules = srv.root.join("restdb.db/reason_rule_files");
    fs::create_dir_all(&rules).unwrap();
    fs::copy(&rule, rules.join("uncle.json")).unwrap();
    let (status, text) = srv.rest("POST", "/backup?db_name=restdb", "");
    assert_eq!(status, 200, "{text}");
    let j = json::parse(&text).unwrap();
    let backup = Path::new(&string(&j, "path")).to_path_buf();
    assert!(
        backup.join("reason_rule_files/uncle.json").is_file(),
        "{}",
        backup.display()
    );
    // The rule is the database's own: it compiles and runs there.
    ok(&reason(&srv, "restdb", "3"));
    assert_eq!(int(&reason(&srv, "restdb", "4"), "AnsNum"), 1);
}
