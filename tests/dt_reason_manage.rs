//! Data test: upstream gStore's reasoning-rule management, `reasonManage`
//! types 1-8, over the `/api` protocol of an in-process `ApiServer` — the
//! upstream documentation's examples (compiled text, inferred triples), the
//! lifecycle and its refusals, exact retraction, persistence (unload / load,
//! restart, drop, backup / restore), privileges, hardening (rule names,
//! SPARQL injection) and query-cache coherence — plus the `Database`
//! methods of the embedded API.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::Database;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_rsn_{tag}_{}_{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
    thread: Option<thread::JoinHandle<()>>,
}

impl Srv {
    fn db_dir(&self, db: &str) -> PathBuf {
        self.root.join(format!("{db}.db"))
    }
    fn rule_file(&self, db: &str, rule: &str) -> PathBuf {
        self.db_dir(db)
            .join("reason_rule_files")
            .join(format!("{rule}.json"))
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A server on `root` (created or reused).
fn start_at(root: PathBuf) -> Srv {
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_config(config);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let thread = thread::spawn(move || {
        s2.serve_forever();
    });
    Srv {
        server,
        addr,
        root,
        thread: Some(thread),
    }
}

fn start(tag: &str) -> Srv {
    start_at(tmp_root(tag))
}

fn post(addr: &SocketAddr, body: &Json) -> Json {
    let body = body.to_string();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let head = format!(
        "POST /api HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("header end");
    assert!(head.starts_with("HTTP/1.1 200"), "{text}");
    json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

/// `/api` as `user` (password `pw`; `root` for root) with `fields`.
fn api_as(addr: &SocketAddr, user: &str, fields: Vec<(&str, Json)>) -> Json {
    let pass = if user == "root" { "root" } else { "pw" };
    let mut body = Json::obj().with("username", user).with("password", pass);
    for (k, v) in fields {
        body.set(k, v);
    }
    post(addr, &body)
}

fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    api_as(
        addr,
        "root",
        pairs.iter().map(|(k, v)| (*k, Json::from(*v))).collect(),
    )
}

/// `reasonManage` of `type` on `db` with `rulename`.
fn reason(addr: &SocketAddr, db: &str, ty: &str, rule: &str) -> Json {
    api(
        addr,
        &[
            ("operation", "reasonManage"),
            ("db_name", db),
            ("type", ty),
            ("rulename", rule),
        ],
    )
}

/// `reasonManage` type 1 with `ruleinfo` (a JSON object).
fn add_rule(addr: &SocketAddr, db: &str, ruleinfo: &str) -> Json {
    api_as(
        addr,
        "root",
        vec![
            ("operation", "reasonManage".into()),
            ("db_name", db.into()),
            ("type", "1".into()),
            ("ruleinfo", json::parse(ruleinfo).unwrap()),
        ],
    )
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn int(j: &Json, key: &str) -> i64 {
    j.get(key)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no integer {key} in {j}"))
}

fn string(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
        .to_string()
}

#[track_caller]
fn ok(j: &Json) {
    assert_eq!(code(j), 0, "{j}");
}

#[track_caller]
fn fails(j: &Json, code_: i64, text: &str) {
    assert_eq!(code(j), code_, "{j}");
    assert!(msg(j).contains(text), "expected {text:?} in {j}");
}

/// The rows (`bindings`) of a SELECT on `db`.
fn select(addr: &SocketAddr, db: &str, sparql: &str) -> Vec<Json> {
    let j = api(
        addr,
        &[("operation", "query"), ("db_name", db), ("sparql", sparql)],
    );
    ok(&j);
    j.get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .map(<[Json]>::to_vec)
        .unwrap_or_default()
}

fn update(addr: &SocketAddr, db: &str, sparql: &str) -> i64 {
    let j = api(
        addr,
        &[("operation", "query"), ("db_name", db), ("sparql", sparql)],
    );
    ok(&j);
    int(&j, "AnsNum")
}

/// Whether `triple` (a ground pattern) holds in `db` (an ASK query).
fn ask(addr: &SocketAddr, db: &str, triple: &str) -> bool {
    let rows = select(addr, db, &format!("ASK WHERE {{ {triple} }}"));
    let value = rows
        .first()
        .and_then(|r| r.get("_askResult"))
        .and_then(|c| c.get("value"))
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no _askResult in {rows:?}"))
        .to_string();
    value == "true"
}

fn triple_count(addr: &SocketAddr, db: &str) -> usize {
    select(addr, db, "SELECT * WHERE { ?s ?p ?o }").len()
}

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// A loaded database `db` holding the family of the upstream examples.
fn family_db(srv: &Srv, db: &str) {
    let up = srv.root.join("upload");
    fs::create_dir_all(&up).unwrap();
    let seed = up.join(format!("{db}.nt"));
    fs::write(&seed, "<http://ex/seed> <http://ex/p> <http://ex/o> .\n").unwrap();
    ok(&api(
        &srv.addr,
        &[
            ("operation", "build"),
            ("db_name", db),
            ("db_path", seed.to_str().unwrap()),
        ],
    ));
    ok(&api(&srv.addr, &[("operation", "load"), ("db_name", db)]));
    let s = |v: &str| format!("\"{v}\"^^<{XSD_STRING}>");
    let data = format!(
        "INSERT DATA {{ \
         <张三> <母亲> <李梅> . <李梅> <父亲> <李老爷> . <李四> <父亲> <李老爷> . \
         <李四> <性别> {male} . <李梅> <性别> {female} . \
         <王五> <父亲> <张大> . <王五> <母亲> <王丽> . \
         <张大> <状态> {dead} . <王丽> <状态> {dead} . \
         <赵六> <父亲> <张大> . <赵六> <母亲> <孙七> . }}",
        male = s("男"),
        female = s("女"),
        dead = s("去世"),
    );
    assert_eq!(update(&srv.addr, db, &data), 11);
}

/// Upstream `API_USAGE.md`'s relation example.
const UNCLE: &str = r#"{
    "rulename": "舅舅",
    "description": "母亲的男性同胞是舅舅",
    "isenable": 1,
    "type": 1,
    "logic": 1,
    "conditions": [{
        "patterns": [
            {"subject": "?x ", "predicate": "<母亲>", "object": "?y"},
            {"subject": "?y ", "predicate": "<父亲>", "object": "?z"},
            {"subject": "?k", "predicate": "<父亲>", "object": "?z"},
            {"subject": "?k", "predicate": "<性别>", "object": "\"男\"^^<http://www.w3.org/2001/XMLSchema#string>"}
        ],
        "filters": [],
        "count_info": {}
    }],
    "return": {"source": "?x", "target": "?k", "label": "舅舅"}
}"#;

/// Upstream `API_USAGE.md`'s property example.
const ORPHAN: &str = r#"{
    "rulename": "孤儿",
    "description": "父母双亡为孤儿",
    "isenable": 1,
    "type": 0,
    "logic": 1,
    "conditions": [{
        "patterns": [
            {"subject": "?x ", "predicate": "<父亲>", "object": "?y"},
            {"subject": "?x", "predicate": "<母亲>", "object": "?z"},
            {"subject": "?y", "predicate": "<状态>", "object": "\"去世\"^^<http://www.w3.org/2001/XMLSchema#string>"},
            {"subject": "?z", "predicate": "<状态>", "object": "\"去世\"^^<http://www.w3.org/2001/XMLSchema#string>"}
        ],
        "filters": [],
        "count_info": {}
    }],
    "return": {"source": "?x", "label": "状态", "value": "\"孤儿\"^^<http://www.w3.org/2001/XMLSchema#string>"}
}"#;

/// The uncle rule's WHERE clause, as upstream 1.4's `Reason.cpp` compiles it.
fn uncle_where() -> String {
    format!("{{ ?x  <母亲> ?y. ?y  <父亲> ?z. ?k <父亲> ?z. ?k <性别> \"男\"^^<{XSD_STRING}>.}}")
}

// ---------------------------------------------------------------------------
// The upstream examples, end to end
// ---------------------------------------------------------------------------

#[test]
fn relation_rule_full_lifecycle() {
    let srv = start("uncle");
    let a = &srv.addr;
    family_db(&srv, "fam");

    // 1: add.
    let j = add_rule(a, "fam", UNCLE);
    ok(&j);
    let path = srv.rule_file("fam", "舅舅");
    assert_eq!(
        msg(&j),
        format!(
            "Add Reason Rule done.Save Successfully! the file path is {}",
            path.display()
        )
    );
    assert!(path.is_file());
    // The file is compact JSON with sorted keys (upstream's dump()).
    let text = fs::read_to_string(&path).unwrap();
    let stored = json::parse(&text).unwrap();
    assert_eq!(stored.sorted().to_string(), text);
    assert_eq!(string(&stored, "status"), "新建");
    let created = string(&stored, "createtime");
    assert_eq!(created.len(), 19, "YYYY-MM-DD HH:MM:SS: {created}");

    // 2: list.
    let j = reason(a, "fam", "2", "");
    ok(&j);
    assert_eq!(msg(&j), "ok");
    assert_eq!(int(&j, "num"), 1);
    let list = j.get("list").and_then(Json::as_array).unwrap();
    assert_eq!(string(&list[0], "rulename"), "舅舅");
    assert_eq!(string(&list[0], "description"), "母亲的男性同胞是舅舅");
    assert_eq!(int(&list[0], "isenable"), 1);

    // 3: compile — upstream's text, byte for byte.
    let j = reason(a, "fam", "3", "舅舅");
    ok(&j);
    assert_eq!(msg(&j), "ok");
    let insert = format!("insert {{ ?x <Rule:舅舅> ?k. }} where {}", uncle_where());
    assert_eq!(string(&j, "insert_sparql"), insert);
    assert_eq!(string(&j, "delete_sparql"), "delete where {?x <Rule:舅舅> ?y.}");
    assert_eq!(
        string(&j, "check_sparql"),
        format!("select (count(*) as ?result) where {}", uncle_where())
    );
    let shown = reason(a, "fam", "6", "舅舅");
    ok(&shown);
    let info = shown.get("ruleinfo").unwrap();
    assert_eq!(string(info, "status"), "已编译");
    assert_eq!(string(info, "insert_sparql"), insert);
    assert_eq!(string(info, "checkResult"), "");
    assert_eq!(
        string(info, "select_sparql"),
        "select ?s ?p ?o where { bind(<Rule:舅舅> as ?p). ?s <Rule:舅舅> ?o. }"
    );

    // 8: check — the matches, before anything is inferred.
    let j = reason(a, "fam", "8", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "effectNum"), 1);
    assert_eq!(
        string(&j, "check_sparql"),
        format!("select (count(*) as ?result) where {}", uncle_where())
    );
    let info = reason(a, "fam", "6", "舅舅");
    let info = info.get("ruleinfo").unwrap();
    assert_eq!(string(info, "status"), "已校验");
    assert_eq!(int(info, "effectNum"), 1);
    assert_eq!(string(info, "checkResult"), "ok");

    // 4: execute — the inferred triple is visible to SELECT.
    assert!(!ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    let j = reason(a, "fam", "4", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    assert_eq!(string(&j, "insert_sparql"), insert);
    let select_sparql = string(&j, "select_sparql");
    let rows = select(a, "fam", "SELECT ?x ?k WHERE { ?x <Rule:舅舅> ?k }");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    // The reported select_sparql lists it too.
    assert_eq!(select(a, "fam", &select_sparql).len(), 1);
    let info = reason(a, "fam", "6", "舅舅");
    assert_eq!(string(info.get("ruleinfo").unwrap(), "status"), "已执行");
    // Running it again infers nothing new.
    let j = reason(a, "fam", "4", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 0);

    // An executed rule can be neither deleted nor recompiled.
    fails(&reason(a, "fam", "7", "舅舅"), 1005, "该规则已经执行，请先让该规则失效后再执行");
    fails(&reason(a, "fam", "3", "舅舅"), 1005, "该规则已经执行，请先让该规则失效后再执行");
    // A check keeps it executed.
    ok(&reason(a, "fam", "8", "舅舅"));
    let info = reason(a, "fam", "6", "舅舅");
    assert_eq!(string(info.get("ruleinfo").unwrap(), "status"), "已执行");

    // 5: disable — the inferred triple is retracted.
    let j = reason(a, "fam", "5", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    assert_eq!(string(&j, "delete_sparql"), "delete where {?x <Rule:舅舅> ?y.}");
    assert!(!ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    let info = reason(a, "fam", "6", "舅舅");
    assert_eq!(string(info.get("ruleinfo").unwrap(), "status"), "已失效");
    // Disabling again retracts nothing.
    let j = reason(a, "fam", "5", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 0);

    // 7: delete.
    let j = reason(a, "fam", "7", "舅舅");
    ok(&j);
    assert_eq!(
        msg(&j),
        format!(
            "the reason file has been remove successfully! file path:{}",
            path.display()
        )
    );
    assert!(!path.exists());
    let j = reason(a, "fam", "2", "");
    ok(&j);
    assert_eq!(int(&j, "num"), 0);
    assert_eq!(j.get("list").and_then(Json::as_array).map(<[Json]>::len), Some(0));
}

#[test]
fn property_rule_infers_and_retracts_an_attribute() {
    let srv = start("orphan");
    let a = &srv.addr;
    family_db(&srv, "fam");
    ok(&add_rule(a, "fam", ORPHAN));
    let j = reason(a, "fam", "3", "孤儿");
    ok(&j);
    let v = format!("\"孤儿\"^^<{XSD_STRING}>");
    let d = format!("\"去世\"^^<{XSD_STRING}>");
    assert_eq!(
        string(&j, "insert_sparql"),
        format!(
            "insert {{?x <Rule:状态> {v}. }} where {{ ?x  <父亲> ?y. ?x <母亲> ?z. ?y <状态> {d}. \
             ?z <状态> {d}.}}"
        )
    );
    assert_eq!(
        string(&j, "delete_sparql"),
        format!("delete where {{?x <Rule:状态> {v}.}}")
    );
    let j = reason(a, "fam", "4", "孤儿");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    assert_eq!(
        string(&j, "select_sparql"),
        format!(
            "select ?s ?p ?o where {{ bind(<Rule:状态> as ?p). bind({v} as ?o). ?s <Rule:状态> {v}. }}"
        )
    );
    // 王五's parents are both dead, 赵六's mother is not.
    let rows = select(a, "fam", "SELECT ?x WHERE { ?x <Rule:状态> ?v }");
    assert_eq!(rows.len(), 1);
    assert!(ask(a, "fam", &format!("<王五> <Rule:状态> {v}")));
    let j = reason(a, "fam", "5", "孤儿");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    assert!(!ask(a, "fam", &format!("<王五> <Rule:状态> {v}")));
}

#[test]
fn union_logic_and_filters() {
    let srv = start("union");
    let a = &srv.addr;
    family_db(&srv, "fam");
    // A parent is a father or a mother (logic 0 = UNION); 李老爷 excluded by
    // a filter on the first condition, and only IRIs kept on the second.
    let rule = r#"{"rulename":"parent","type":1,"logic":0,"conditions":[
        {"patterns":[{"subject":"?c","predicate":"<父亲>","object":"?p"}],"filters":["?p != <李老爷>"]},
        {"patterns":[{"subject":"?c","predicate":"<母亲>","object":"?p"}],"filters":["isIRI(?p)","?c != <赵六>"]}],
        "return":{"source":"?c","target":"?p","label":"parent"}}"#;
    ok(&add_rule(a, "fam", rule));
    let j = reason(a, "fam", "3", "parent");
    ok(&j);
    let c1 = "{ ?c <父亲> ?p. filter(  ?p != <李老爷> ).}";
    let c2 = "{ ?c <母亲> ?p. filter(  isIRI(?p)  && ?c != <赵六> ).}";
    assert_eq!(
        string(&j, "insert_sparql"),
        format!("insert {{ ?c <Rule:parent> ?p. }} where {{ {c1} union {c2} }}")
    );
    let j = reason(a, "fam", "8", "parent");
    ok(&j);
    // Fathers: 王五→张大, 赵六→张大 (李梅, 李四 → 李老爷 filtered out);
    // mothers: 张三→李梅, 王五→王丽 (赵六 filtered out).
    assert_eq!(int(&j, "effectNum"), 4);
    let j = reason(a, "fam", "4", "parent");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 4);
    for t in [
        "<王五> <Rule:parent> <张大>",
        "<赵六> <Rule:parent> <张大>",
        "<张三> <Rule:parent> <李梅>",
        "<王五> <Rule:parent> <王丽>",
    ] {
        assert!(ask(a, "fam", t), "{t}");
    }
    assert!(!ask(a, "fam", "<李梅> <Rule:parent> <李老爷>"));
    assert!(!ask(a, "fam", "<赵六> <Rule:parent> <孙七>"));

    // logic 1 (AND): both conditions must hold — a child with a father and
    // a mother.
    let both = r#"{"rulename":"both","type":1,"logic":1,"conditions":[
        {"patterns":[{"subject":"?c","predicate":"<父亲>","object":"?f"}]},
        {"patterns":[{"subject":"?c","predicate":"<母亲>","object":"?m"}]}],
        "return":{"source":"?f","target":"?m","label":"spouse"}}"#;
    ok(&add_rule(a, "fam", both));
    let j = reason(a, "fam", "3", "both");
    ok(&j);
    assert_eq!(
        string(&j, "insert_sparql"),
        "insert { ?f <Rule:spouse> ?m. } where { { ?c <父亲> ?f.}{ ?c <母亲> ?m.} }"
    );
    let j = reason(a, "fam", "4", "both");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 2, "张大-王丽, 张大-孙七");
    assert!(ask(a, "fam", "<张大> <Rule:spouse> <王丽>"));
}

// ---------------------------------------------------------------------------
// Retraction is exact
// ---------------------------------------------------------------------------

#[test]
fn disable_retracts_exactly_the_inferred_triples() {
    let srv = start("exact");
    let a = &srv.addr;
    family_db(&srv, "fam");
    // Asserted facts: one with an ordinary predicate, and one with the
    // rule's own predicate, asserted before the rule ran.
    assert_eq!(
        update(
            a,
            "fam",
            "INSERT DATA { <张三> <舅舅> <李四> . <王五> <Rule:舅舅> <某人> . }"
        ),
        2
    );
    // Make the asserted Rule: triple derivable too: 王五's mother 王丽 has a
    // male sibling 某人.
    let male = format!("\"男\"^^<{XSD_STRING}>");
    update(
        a,
        "fam",
        &format!(
            "INSERT DATA {{ <王丽> <父亲> <王老爷> . <某人> <父亲> <王老爷> . <某人> <性别> {male} . }}"
        ),
    );
    let before = triple_count(a, "fam");
    ok(&add_rule(a, "fam", UNCLE));
    ok(&reason(a, "fam", "3", "舅舅"));
    let j = reason(a, "fam", "8", "舅舅");
    assert_eq!(int(&j, "effectNum"), 2);
    let j = reason(a, "fam", "4", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1, "王五 <Rule:舅舅> 某人 existed already");
    assert_eq!(triple_count(a, "fam"), before + 1);
    let j = reason(a, "fam", "5", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    // Exactly the inferred triple went; both asserted ones survive.
    assert_eq!(triple_count(a, "fam"), before);
    assert!(!ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    assert!(ask(a, "fam", "<张三> <舅舅> <李四>"));
    assert!(ask(a, "fam", "<王五> <Rule:舅舅> <某人>"));
}

#[test]
fn a_triple_two_rules_infer_survives_until_both_are_disabled() {
    let srv = start("shared");
    let a = &srv.addr;
    family_db(&srv, "fam");
    let second = UNCLE.replace("\"rulename\": \"舅舅\"", "\"rulename\": \"舅舅2\"");
    for r in [UNCLE, second.as_str()] {
        ok(&add_rule(a, "fam", r));
    }
    for r in ["舅舅", "舅舅2"] {
        ok(&reason(a, "fam", "3", r));
    }
    assert_eq!(int(&reason(a, "fam", "4", "舅舅"), "AnsNum"), 1);
    assert_eq!(int(&reason(a, "fam", "4", "舅舅2"), "AnsNum"), 0);
    // Upstream's `delete where` would take the shared triple with the first.
    let j = reason(a, "fam", "5", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 0);
    assert!(ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    let j = reason(a, "fam", "5", "舅舅2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    assert!(!ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
}

#[test]
fn a_rule_executed_elsewhere_is_retracted_upstream_style() {
    let srv = start("legacy");
    let a = &srv.addr;
    family_db(&srv, "fam");
    ok(&add_rule(a, "fam", UNCLE));
    ok(&reason(a, "fam", "3", "舅舅"));
    // As upstream gStore leaves it: executed, no ledger.
    let path = srv.rule_file("fam", "舅舅");
    let mut doc = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    doc.set("status", "已执行");
    fs::write(&path, doc.sorted().to_string()).unwrap();
    update(
        a,
        "fam",
        "INSERT DATA { <a> <Rule:舅舅> <b> . <c> <Rule:舅舅> <d> . }",
    );
    fails(&reason(a, "fam", "7", "舅舅"), 1005, "该规则已经执行");
    let j = reason(a, "fam", "5", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 2);
    assert!(select(a, "fam", "SELECT * WHERE { ?s <Rule:舅舅> ?o }").is_empty());
    ok(&reason(a, "fam", "7", "舅舅"));
}

// ---------------------------------------------------------------------------
// Refusals and errors
// ---------------------------------------------------------------------------

#[test]
fn parameters_and_lifecycle_refusals() {
    let srv = start("errors");
    let a = &srv.addr;
    family_db(&srv, "fam");
    // type
    fails(
        &api(a, &[("operation", "reasonManage"), ("db_name", "fam")]),
        1003,
        "the value of type can not be empty!",
    );
    fails(&reason(a, "fam", "9", "x"), 1003, "The operation is not support.");
    fails(&reason(a, "fam", "0", "x"), 1003, "The operation is not support.");
    // db_name
    fails(&reason(a, "", "2", ""), 1003, "the value of db_name can not be empty!");
    fails(&reason(a, "system", "2", ""), 1003, "system database");
    fails(&reason(a, "nodb", "2", ""), 1005, "database[nodb] does not exist");
    fails(&reason(a, "nodb", "4", "x"), 1005, "the reason file is not exist");
    // add
    fails(
        &api(
            a,
            &[("operation", "reasonManage"), ("db_name", "fam"), ("type", "1")],
        ),
        1003,
        "the data has not the rule information",
    );
    ok(&add_rule(a, "fam", UNCLE));
    fails(
        &add_rule(a, "fam", UNCLE),
        1005,
        "Add Reason Rule Fail. The rule file has been exist, please remove it before!",
    );
    fails(
        &add_rule(a, "fam", r#"{"rulename":"x","type":1,"return":{}}"#),
        1005,
        "Add Reason Rule Fail. the reason file has not the conditons information",
    );
    fails(
        &add_rule(
            a,
            "fam",
            r#"{"rulename":"x","type":1,"conditions":[{"patterns":[{"subject":"?a","predicate":"<p>","object":"?b"}]}]}"#,
        ),
        1005,
        "Add Reason Rule Fail. the reason file has not the return information",
    );
    fails(
        &add_rule(a, "fam", r#"{"type":1}"#),
        1003,
        "the value of rulename can not be empty!",
    );
    // Missing rules.
    for ty in ["3", "4", "5", "8"] {
        fails(&reason(a, "fam", ty, "nope"), 1005, "the reason file is not exist");
    }
    for ty in ["6", "7"] {
        fails(&reason(a, "fam", ty, "nope"), 1005, "the reason file is not exists");
    }
    fails(&reason(a, "fam", "3", ""), 1003, "the value of rulename can not be empty!");
    // Not compiled yet.
    for ty in ["4", "5", "8"] {
        fails(
            &reason(a, "fam", ty, "舅舅"),
            1005,
            "The sparql of the reason rule is not exist! ",
        );
    }
    ok(&reason(a, "fam", "3", "舅舅"));
    // Edited after compiling: refused until compiled again.
    let path = srv.rule_file("fam", "舅舅");
    let mut doc = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    doc.set("return", json::parse(r#"{"source":"?k","target":"?x","label":"外甥"}"#).unwrap());
    fs::write(&path, doc.sorted().to_string()).unwrap();
    fails(&reason(a, "fam", "4", "舅舅"), 1005, "compile it again");
    ok(&reason(a, "fam", "3", "舅舅"));
    let j = reason(a, "fam", "4", "舅舅");
    ok(&j);
    assert!(ask(a, "fam", "<李四> <Rule:外甥> <张三>"));
    ok(&reason(a, "fam", "5", "舅舅"));
    // A rule file that is not JSON.
    fs::write(srv.rule_file("fam", "broken"), "{not json").unwrap();
    fails(&reason(a, "fam", "3", "broken"), 1005, "the reason file is not fit the json format");
    fails(&reason(a, "fam", "6", "broken"), 1005, "pasrse ruleinfo error:{not json");
    fails(&reason(a, "fam", "2", ""), 1005, "pasrse rulefiles error:[");
    // A broken rule that is not executed can be deleted.
    ok(&reason(a, "fam", "7", "broken"));
    ok(&reason(a, "fam", "2", ""));

    // Execute / disable / check need the database loaded.
    ok(&api(a, &[("operation", "unload"), ("db_name", "fam")]));
    for ty in ["4", "5", "8"] {
        fails(&reason(a, "fam", ty, "舅舅"), 1005, "database[fam] is not loaded yet");
    }
    // The file operations do not.
    ok(&reason(a, "fam", "2", ""));
    ok(&reason(a, "fam", "6", "舅舅"));
    ok(&api(a, &[("operation", "load"), ("db_name", "fam")]));

    // An open transaction refuses execute / disable (not check).
    let j = api(a, &[("operation", "begin"), ("db_name", "fam")]);
    ok(&j);
    let tid = string(&j, "TID");
    fails(&reason(a, "fam", "4", "舅舅"), 1004, "open transaction");
    fails(&reason(a, "fam", "5", "舅舅"), 1004, "open transaction");
    ok(&reason(a, "fam", "8", "舅舅"));
    ok(&api(
        a,
        &[("operation", "rollback"), ("db_name", "fam"), ("tid", &tid)],
    ));
    ok(&reason(a, "fam", "4", "舅舅"));
}

#[test]
fn lenient_parameter_forms() {
    let srv = start("lenient");
    let a = &srv.addr;
    family_db(&srv, "fam");
    // `type` as a JSON number, `ruleinfo` as a string holding the object.
    let j = api_as(
        a,
        "root",
        vec![
            ("operation", "reasonManage".into()),
            ("db_name", "fam".into()),
            ("type", Json::from(1)),
            ("ruleinfo", Json::from(UNCLE)),
        ],
    );
    ok(&j);
    let j = api_as(
        a,
        "root",
        vec![
            ("operation", "reasonManage".into()),
            ("db_name", "fam".into()),
            ("type", Json::from(3)),
            ("rulename", "舅舅".into()),
        ],
    );
    ok(&j);
    // Compiled fields sent by a client are dropped, never executed.
    let spoof = UNCLE
        .replace("\"rulename\": \"舅舅\"", "\"rulename\": \"spoof\"")
        .replace(
            "\"isenable\": 1,",
            "\"isenable\": 1, \"insert_sparql\": \"DROP ALL\", \"status\": \"已编译\",",
        );
    ok(&add_rule(a, "fam", &spoof));
    let info = reason(a, "fam", "6", "spoof");
    let info = info.get("ruleinfo").unwrap();
    assert!(info.get("insert_sparql").is_none(), "{info}");
    assert_eq!(string(info, "status"), "新建");
    fails(&reason(a, "fam", "4", "spoof"), 1005, "The sparql of the reason rule is not exist!");
}

// ---------------------------------------------------------------------------
// Hardening
// ---------------------------------------------------------------------------

#[test]
fn rule_names_cannot_escape_the_rule_directory() {
    let srv = start("names");
    let a = &srv.addr;
    family_db(&srv, "fam");
    let long = "x".repeat(200);
    for name in ["../evil", "a/b", "a\\b", "..", ".hidden", "a\u{0}b", "a\nb", long.as_str()] {
        let rule = UNCLE.replace("\"rulename\": \"舅舅\"", &format!("\"rulename\": {}", json::quote(name)));
        fails(&add_rule(a, "fam", &rule), 1003, "rule");
        for ty in ["3", "4", "6", "7"] {
            fails(&reason(a, "fam", ty, name), 1003, "rule");
        }
    }
    assert!(!srv.db_dir("fam").join("evil.json").exists());
    assert!(!srv.root.join("evil.json").exists());
    let dir = srv.db_dir("fam").join("reason_rule_files");
    let n = fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(n, 0, "nothing written");
}

#[test]
fn rules_cannot_inject_sparql() {
    let srv = start("inject");
    let a = &srv.addr;
    family_db(&srv, "fam");
    let before = triple_count(a, "fam");
    let pattern = |s: &str, p: &str, o: &str| {
        format!(
            r#"{{"rulename":"evil","type":1,"logic":1,"conditions":[{{"patterns":[
                {{"subject":{},"predicate":{},"object":{}}}]}}],
                "return":{{"source":"?x","target":"?x","label":"L"}}}}"#,
            json::quote(s),
            json::quote(p),
            json::quote(o)
        )
    };
    let with_return = |source: &str, label: &str, value: &str| {
        format!(
            r#"{{"rulename":"evil","type":0,"logic":1,"conditions":[{{"patterns":[
                {{"subject":"?x","predicate":"<母亲>","object":"?y"}}]}}],
                "return":{{"source":{},"label":{},"value":{}}}}}"#,
            json::quote(source),
            json::quote(label),
            json::quote(value)
        )
    };
    let with_filter = |f: &str| {
        format!(
            r#"{{"rulename":"evil","type":1,"logic":1,"conditions":[{{"patterns":[
                {{"subject":"?x","predicate":"<母亲>","object":"?y"}}],"filters":[{}]}}],
                "return":{{"source":"?x","target":"?y","label":"L"}}}}"#,
            json::quote(f)
        )
    };
    let attempts = [
        pattern("?x", "<母亲>", "?y . } ; DELETE WHERE { ?s ?p ?o } ; INSERT DATA { <a> <b> <c>"),
        pattern("?x } ; DROP ALL ; INSERT DATA { <a> <b> <c> } ; INSERT { ?x <q> ?x } WHERE {", "<母亲>", "?y"),
        pattern("?x", "<母亲> ?y . ?s", "?y"),
        pattern("?x #", "<母亲>", "?y"),
        with_return("?x", "L> \"v\" . } ; DROP ALL ; INSERT { ?x <x", "\"v\""),
        with_return("?x", "L", "\"v\" . } where { ?x ?p ?o } ; DROP ALL ; INSERT { <a> <b> \"v\""),
        with_return("?x <p> ?y . ?x", "L", "\"v\""),
        with_return("?x", "L", "\"v\" #"),
        with_filter("true ) . } ; DROP ALL ; INSERT DATA { <a> <b> <c> } ; INSERT { ?x <q> ?x } WHERE { FILTER( true"),
        with_filter("true) } ; DROP ALL"),
        with_filter("true # )"),
        with_filter("?y = <a> } UNION { ?s ?p ?o"),
    ];
    for rule in &attempts {
        fails(&add_rule(a, "fam", rule), 1005, "Add Reason Rule Fail.");
    }
    // A rule file planted directly is refused by compile (and so never runs).
    let dir = srv.db_dir("fam").join("reason_rule_files");
    fs::create_dir_all(&dir).unwrap();
    for rule in &attempts {
        let mut doc = json::parse(rule).unwrap();
        doc.set("status", "新建");
        fs::write(dir.join("evil.json"), doc.sorted().to_string()).unwrap();
        assert_eq!(code(&reason(a, "fam", "3", "evil")), 1005);
        // Even a forged compiled statement is never executed.
        doc.set("insert_sparql", "DROP ALL");
        doc.set("delete_sparql", "DROP ALL");
        doc.set("check_sparql", "DROP ALL");
        doc.set("status", "已执行");
        fs::write(dir.join("evil.json"), doc.sorted().to_string()).unwrap();
        for ty in ["4", "5", "8"] {
            assert_eq!(code(&reason(a, "fam", ty, "evil")), 1005, "type {ty}");
        }
    }
    assert_eq!(triple_count(a, "fam"), before, "nothing changed");
}

#[test]
fn only_root_may_manage_rules() {
    let srv = start("priv");
    let a = &srv.addr;
    family_db(&srv, "fam");
    ok(&api(
        a,
        &[
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", "alice"),
            ("op_password", "pw"),
        ],
    ));
    ok(&api(
        a,
        &[
            ("operation", "userprivilegemanage"),
            ("type", "1"),
            ("op_username", "alice"),
            ("db_name", "fam"),
            ("privileges", "1,2,3,4,5,6,7"),
        ],
    ));
    ok(&add_rule(a, "fam", UNCLE));
    ok(&reason(a, "fam", "3", "舅舅"));
    for ty in ["1", "2", "3", "4", "5", "6", "7", "8"] {
        let j = api_as(
            a,
            "alice",
            vec![
                ("operation", "reasonManage".into()),
                ("db_name", "fam".into()),
                ("type", ty.into()),
                ("rulename", "舅舅".into()),
                ("ruleinfo", json::parse(UNCLE).unwrap()),
            ],
        );
        fails(&j, 1004, "You have no reasonManage privilege");
    }
    // A query-only user can read the result of root's reasoning, though.
    ok(&reason(a, "fam", "4", "舅舅"));
    let j = api_as(
        a,
        "alice",
        vec![
            ("operation", "query".into()),
            ("db_name", "fam".into()),
            ("sparql", "SELECT * WHERE { ?x <Rule:舅舅> ?k }".into()),
        ],
    );
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[test]
fn rules_survive_unload_load_and_a_restart() {
    let root = tmp_root("persist");
    {
        let srv = start_at(root.clone());
        let a = &srv.addr;
        family_db(&srv, "fam");
        ok(&add_rule(a, "fam", UNCLE));
        ok(&add_rule(a, "fam", ORPHAN));
        ok(&reason(a, "fam", "3", "舅舅"));
        ok(&reason(a, "fam", "4", "舅舅"));
        ok(&api(a, &[("operation", "unload"), ("db_name", "fam")]));
        ok(&api(a, &[("operation", "load"), ("db_name", "fam")]));
        assert!(ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
        let info = reason(a, "fam", "6", "舅舅");
        assert_eq!(string(info.get("ruleinfo").unwrap(), "status"), "已执行");
    }
    let srv = start_at(root);
    let a = &srv.addr;
    let j = reason(a, "fam", "2", "");
    ok(&j);
    assert_eq!(int(&j, "num"), 2);
    ok(&api(a, &[("operation", "load"), ("db_name", "fam")]));
    assert!(ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    // The ledger survived too: disabling retracts exactly the triple.
    fails(&reason(a, "fam", "7", "舅舅"), 1005, "该规则已经执行");
    let j = reason(a, "fam", "5", "舅舅");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    assert!(!ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
}

#[test]
fn drop_removes_the_rules_and_backup_restore_carries_them() {
    let srv = start("dropbk");
    let a = &srv.addr;
    family_db(&srv, "fam");
    ok(&add_rule(a, "fam", UNCLE));
    ok(&reason(a, "fam", "3", "舅舅"));
    ok(&reason(a, "fam", "4", "舅舅"));
    // A backup carries the rules (and the ledger).
    let j = api(a, &[("operation", "backup"), ("db_name", "fam")]);
    ok(&j);
    let shown = string(&j, "backupfilepath");
    let backup = fs::read_dir(srv.root.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| shown.ends_with(p.file_name().unwrap().to_str().unwrap()))
        .expect("the backup directory");
    assert!(backup.join("reason_rule_files").join("舅舅.json").is_file());
    assert!(backup
        .join("reason_rule_files")
        .join("inferred")
        .join("舅舅.nt")
        .is_file());
    // Change the rules, then restore: the backup's come back.
    ok(&reason(a, "fam", "5", "舅舅"));
    ok(&reason(a, "fam", "7", "舅舅"));
    ok(&add_rule(a, "fam", ORPHAN));
    let j = api(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "fam"),
            ("backup_path", backup.to_str().unwrap()),
        ],
    );
    ok(&j);
    ok(&api(a, &[("operation", "load"), ("db_name", "fam")]));
    let j = reason(a, "fam", "2", "");
    assert_eq!(int(&j, "num"), 1);
    let info = reason(a, "fam", "6", "舅舅");
    assert_eq!(string(info.get("ruleinfo").unwrap(), "status"), "已执行");
    assert!(ask(a, "fam", "<张三> <Rule:舅舅> <李四>"));
    assert_eq!(int(&reason(a, "fam", "5", "舅舅"), "AnsNum"), 1);

    // drop removes the database directory, rules included.
    ok(&api(
        a,
        &[
            ("operation", "drop"),
            ("db_name", "fam"),
            ("is_backup", "false"),
        ],
    ));
    assert!(!srv.db_dir("fam").exists());
    fails(&reason(a, "fam", "2", ""), 1005, "does not exist");
    // A database built again under the name starts without rules.
    family_db(&srv, "fam");
    let j = reason(a, "fam", "2", "");
    ok(&j);
    assert_eq!(int(&j, "num"), 0);
}

// ---------------------------------------------------------------------------
// Query cache
// ---------------------------------------------------------------------------

#[test]
fn a_cached_select_is_not_served_stale() {
    let srv = start("cache");
    let a = &srv.addr;
    family_db(&srv, "fam");
    let q = "SELECT ?k WHERE { <张三> <Rule:舅舅> ?k }";
    assert!(select(a, "fam", q).is_empty());
    assert!(select(a, "fam", q).is_empty(), "cached");
    ok(&add_rule(a, "fam", UNCLE));
    ok(&reason(a, "fam", "3", "舅舅"));
    ok(&reason(a, "fam", "4", "舅舅"));
    assert_eq!(select(a, "fam", q).len(), 1);
    assert_eq!(select(a, "fam", q).len(), 1, "cached");
    ok(&reason(a, "fam", "5", "舅舅"));
    assert!(select(a, "fam", q).is_empty());
}

#[test]
fn concurrent_adds_of_one_name_store_one_rule() {
    let srv = start("race");
    family_db(&srv, "fam");
    let addr = srv.addr;
    let handles: Vec<_> = (0..8)
        .map(|_| thread::spawn(move || code(&add_rule(&addr, "fam", UNCLE))))
        .collect();
    let codes: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(codes.iter().filter(|&&c| c == 0).count(), 1, "{codes:?}");
    assert!(codes.iter().all(|&c| c == 0 || c == 1005), "{codes:?}");
}

// ---------------------------------------------------------------------------
// The embedded API
// ---------------------------------------------------------------------------

fn family_triples() -> String {
    let s = |v: &str| format!("\"{v}\"^^<{XSD_STRING}>");
    format!(
        "<张三> <母亲> <李梅> . <李梅> <父亲> <李老爷> . <李四> <父亲> <李老爷> . \
         <李四> <性别> {} . ",
        s("男")
    )
}

#[test]
fn a_database_without_a_directory_keeps_its_rules_in_memory() {
    let mut db = Database::new("mem");
    db.query(&format!("INSERT DATA {{ {} }}", family_triples()))
        .unwrap();
    assert!(db.reason_db_dir().is_none());
    db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
    assert_eq!(db.reason_list().unwrap().len(), 1);
    let c = db.reason_compile("舅舅").unwrap();
    assert_eq!(
        c.insert_sparql,
        format!("insert {{ ?x <Rule:舅舅> ?k. }} where {}", uncle_where())
    );
    assert_eq!(db.reason_check("舅舅").unwrap().effect_num, 1);
    let x = db.reason_execute("舅舅").unwrap();
    assert_eq!(x.inserted, 1);
    assert_eq!(x.owned, 1);
    let rs = db
        .select("SELECT ?k WHERE { <张三> <Rule:舅舅> ?k }")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);
    let e = db.reason_delete("舅舅").unwrap_err();
    assert_eq!(e.message, "该规则已经执行，请先让该规则失效后再执行");
    // Not inside a transaction.
    db.begin().unwrap();
    assert_eq!(
        db.reason_disable("舅舅").unwrap_err().kind,
        gstore::reason::ReasonErrorKind::Conflict
    );
    db.rollback().unwrap();
    assert_eq!(db.reason_disable("舅舅").unwrap().removed, 1);
    assert!(db
        .select("SELECT ?k WHERE { <张三> <Rule:舅舅> ?k }")
        .unwrap()
        .rows
        .is_empty());
    assert_eq!(
        string(&db.reason_show("舅舅").unwrap(), "status"),
        "已失效"
    );
    // A backup writes the in-memory rules out.
    let dir = tmp_root("membk");
    let bk = dir.join("bk");
    db.backup(&bk).unwrap();
    assert!(bk.join("reason_rule_files").join("舅舅.json").is_file());
    db.reason_delete("舅舅").unwrap();
    assert!(db.reason_list().unwrap().is_empty());
}

#[test]
fn an_on_disk_database_keeps_its_rules_across_reopening() {
    let root = tmp_root("disk");
    let data = root.join("fam.nt");
    fs::write(&data, "<http://ex/seed> <http://ex/p> <http://ex/o> .\n").unwrap();
    let dir = root.join("fam.db");
    Database::build_disk(&dir, &[&data]).unwrap();
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.storage(), gstore::StorageKind::Disk);
        db.query(&format!("INSERT DATA {{ {} }}", family_triples()))
            .unwrap();
        db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
        db.reason_compile("舅舅").unwrap();
        assert_eq!(db.reason_execute("舅舅").unwrap().inserted, 1);
        db.checkpoint().unwrap();
    }
    assert!(dir.join("reason_rule_files").join("舅舅.json").is_file());
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(string(&db.reason_show("舅舅").unwrap(), "status"), "已执行");
    assert_eq!(
        db.select("SELECT ?k WHERE { <张三> <Rule:舅舅> ?k }")
            .unwrap()
            .rows
            .len(),
        1
    );
    assert_eq!(db.reason_disable("舅舅").unwrap().removed, 1);
    assert!(db
        .select("SELECT ?k WHERE { <张三> <Rule:舅舅> ?k }")
        .unwrap()
        .rows
        .is_empty());
    // A restore (library) carries the rules of the backup.
    let bk = root.join("bk");
    db.backup(&bk).unwrap();
    drop(db);
    let restored = root.join("restored.db");
    Database::restore_into(&bk, &restored).unwrap();
    assert!(restored
        .join("reason_rule_files")
        .join("舅舅.json")
        .is_file());
}

#[cfg(feature = "rocksdb")]
#[test]
fn a_rocksdb_database_keeps_its_rules_across_reopening() {
    let root = tmp_root("rocks");
    let dir = root.join("fam.db");
    {
        let mut db = Database::build_rocksdb_from_str(
            "fam",
            &dir,
            "<http://ex/seed> <http://ex/p> <http://ex/o> .\n",
        )
        .unwrap();
        db.query(&format!("INSERT DATA {{ {} }}", family_triples()))
            .unwrap();
        db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
        db.reason_compile("舅舅").unwrap();
        assert_eq!(db.reason_execute("舅舅").unwrap().inserted, 1);
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(string(&db.reason_show("舅舅").unwrap(), "status"), "已执行");
    assert_eq!(db.reason_disable("舅舅").unwrap().removed, 1);
}

#[test]
fn rdfs_range_never_types_a_literal() {
    let mut db = Database::new("rdfs");
    db.query(
        "INSERT DATA { <http://ex/name> <http://www.w3.org/2000/01/rdf-schema#range> <http://ex/Label> . \
         <http://ex/a> <http://ex/name> \"Alice\" . <http://ex/a> <http://ex/name> <http://ex/b> . }",
    )
    .unwrap();
    assert_eq!(db.materialize_rdfs(), 1);
    let rs = db
        .select("SELECT ?x WHERE { ?x a <http://ex/Label> }")
        .unwrap();
    assert_eq!(rs.rows, vec![vec![Some("<http://ex/b>".to_string())]]);
}
