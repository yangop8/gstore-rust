//! Regression tests for the review of the Phase 4 merge (reasoning rules
//! over Phases 0-3 and 5):
//!
//! * a rule's ledger reads back every term exactly — IRIs holding `>`, `\`,
//!   a tab or a newline (which the readers decode from `\u` escapes since
//!   Phase 5), an IRI holding a backslash-`u` sequence, a language tag with
//!   a tab — so disabling it retracts exactly what it inferred, never a
//!   triple a split line names; a ledger line that cannot be read fails the
//!   disable instead of being skipped;
//! * execute records the triples it is about to insert before it inserts
//!   them, so a failed ledger write never leaves unrecorded inferred
//!   triples;
//! * disable saves a snapshot database before it empties the ledger (also on
//!   a server with `check_point` off);
//! * incremental backups carry the rules and ledgers of their time;
//! * the REST `/drop`, `/restore` and rebuilding `/build`, and the `/api`
//!   `rename`, wait for a `reasonManage` file operation in progress; a REST
//!   rebuild in place starts without the old database's rules.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::reason::store::RuleStore;
use gstore::{Database, LoadPolicy};
use harness::{code, http_with, int, ok, path_str, scratch, Server};

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_m4fix_{tag}_{}_{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// One hop: `?x <http://ex/p> ?y` gives `?x <Rule:r> ?y`.
const ONE_HOP: &str = r#"{"rulename":"r","type":1,"logic":1,
    "conditions":[{"patterns":[{"subject":"?x","predicate":"<http://ex/p>","object":"?y"}]}],
    "return":{"source":"?x","target":"?y","label":"r"}}"#;

/// Two hops along `<http://ex/p>`: `?x <Rule:next2> ?z`.
const NEXT2: &str = r#"{"rulename":"next2","type":1,"logic":1,
    "conditions":[{"patterns":[
        {"subject":"?x","predicate":"<http://ex/p>","object":"?y"},
        {"subject":"?y","predicate":"<http://ex/p>","object":"?z"}]}],
    "return":{"source":"?x","target":"?z","label":"next2"}}"#;

fn ask(db: &mut Database, pattern: &str) -> bool {
    match db.query(&format!("ASK {{ {pattern} }}")).unwrap() {
        gstore::query::QueryResult::Ask(b) => b,
        other => panic!("{other:?}"),
    }
}

fn ledger_text(dir: &Path, rule: &str) -> String {
    fs::read_to_string(dir.join(format!("reason_rule_files/inferred/{rule}.nt")))
        .unwrap_or_default()
}

fn status(db: &mut Database, name: &str) -> String {
    db.reason_show(name)
        .unwrap()
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A database whose rules live in memory (`None`) or in directory `dir`.
fn fresh(dir: Option<&Path>) -> Database {
    let mut db = Database::new("m4");
    if let Some(d) = dir {
        db.set_reason_db_dir(d);
    }
    db
}

/// Add, compile and execute `rule` (named `name`) on `db`: (inserted, owned).
fn run_rule(db: &mut Database, rule: &str, name: &str) -> (usize, usize) {
    db.reason_add(&json::parse(rule).unwrap()).unwrap();
    db.reason_compile(name).unwrap();
    let x = db.reason_execute(name).unwrap();
    (x.inserted, x.owned)
}

/// Finding: the ledger wrote IRIs raw and read them back with the strict
/// N-Triples reader, so an inferred triple over an IRI holding `>`, `\` or
/// a tab (all of which LOAD / INSERT DATA accept as `\u` escapes) was never
/// retracted — the disable reported success and emptied the ledger.
#[test]
fn every_inferred_triple_is_retracted_whatever_its_terms_hold() {
    let cases = [
        // subject IRIs
        "<http://ex/a\\u003Eb> <http://ex/p> <http://ex/c>",
        "<http://ex/a\\u005Cb> <http://ex/p> <http://ex/c>",
        "<http://ex/a\\u0009b> <http://ex/p> <http://ex/c>",
        "<http://ex/a\\u000Ab> <http://ex/p> <http://ex/c>",
        "<http://ex/a\\u000Db> <http://ex/p> <http://ex/c>",
        "<http://ex/a\\u0020b> <http://ex/p> <http://ex/c>",
        // object IRIs and literals
        "<http://ex/a> <http://ex/p> <http://ex/c\\u005Cd>",
        "<http://ex/a> <http://ex/p> \"x\\ty\\n\\\"z\\\\\"",
        "<http://ex/a> <http://ex/p> \"x\"^^<http://ex/d\\u003Et\\u0009>",
    ];
    for case in cases {
        for in_dir in [false, true] {
            let root = tmp_root("iri");
            let mut db = fresh(in_dir.then_some(root.as_path()));
            db.query(&format!("INSERT DATA {{ {case} . }}")).unwrap();
            assert_eq!(run_rule(&mut db, ONE_HOP, "r"), (1, 1), "{case}");
            assert!(ask(&mut db, "?s <Rule:r> ?o"), "{case}");
            if in_dir {
                let text = ledger_text(&root, "r");
                assert_eq!(text.lines().count(), 1, "{case}: {text:?}");
            }
            let d = db.reason_disable("r").unwrap();
            assert_eq!(d.removed, 1, "{case} (in a directory: {in_dir})");
            assert!(!ask(&mut db, "?s <Rule:r> ?o"), "{case}");
            let _ = fs::remove_dir_all(&root);
        }
    }
}

/// A language tag holding a tab (`STRLANG` does not check its tag) does not
/// split the ledger line either.
#[test]
fn a_language_tag_with_a_tab_is_retracted() {
    let root = tmp_root("lang");
    let mut db = fresh(Some(&root));
    db.query(
        "INSERT { <http://ex/a> <http://ex/p> ?o } WHERE { BIND(STRLANG(\"v\", \"en\\tx\") AS ?o) }",
    )
    .unwrap();
    assert_eq!(run_rule(&mut db, ONE_HOP, "r"), (1, 1));
    assert_eq!(db.reason_disable("r").unwrap().removed, 1);
    assert!(!ask(&mut db, "?s <Rule:r> ?o"));
    let _ = fs::remove_dir_all(&root);
}

/// Finding: an IRI holding a backslash-`u` sequence read back as the IRI
/// the escape names, so disabling the rule deleted a user's triple and kept
/// the inferred one.
#[test]
fn a_backslash_u_iri_never_retracts_another_triple() {
    for in_dir in [false, true] {
        let root = tmp_root("bsu");
        let mut db = fresh(in_dir.then_some(root.as_path()));
        // `\u005C` is a backslash: the stored IRI is `http://ex/x\u0041`.
        db.query(
            "INSERT DATA { <http://ex/x\\u005Cu0041> <http://ex/p> <http://ex/c> . \
             <http://ex/xA> <Rule:r> <http://ex/c> . }",
        )
        .unwrap();
        assert_eq!(run_rule(&mut db, ONE_HOP, "r"), (1, 1));
        assert_eq!(db.reason_disable("r").unwrap().removed, 1);
        assert!(
            ask(&mut db, "<http://ex/xA> <Rule:r> <http://ex/c>"),
            "the asserted triple survives (in a directory: {in_dir})"
        );
        assert!(
            !ask(&mut db, "<http://ex/x\\u005Cu0041> <Rule:r> <http://ex/c>"),
            "the inferred triple is retracted (in a directory: {in_dir})"
        );
        let _ = fs::remove_dir_all(&root);
    }
}

/// Finding: an IRI holding a newline and tabs split its ledger line into one
/// that named an unrelated triple, which the disable then deleted.
#[test]
fn a_split_ledger_line_never_names_a_victim() {
    let root = tmp_root("victim");
    let mut db = fresh(Some(&root));
    db.query(
        "INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . \
         <http://ex/victim> <http://ex/q> <http://ex/o> . \
         <http://ex/b> <http://ex/p> <http://x\\u000A\\u003Chttp://ex/victim\\u003E\\u0009\
         \\u003Chttp://ex/q\\u003E\\u0009\\u003Chttp://ex/o\\u003E\\u0009.> . }",
    )
    .unwrap();
    assert_eq!(run_rule(&mut db, NEXT2, "next2"), (1, 1));
    assert_eq!(ledger_text(&root, "next2").lines().count(), 1);
    assert_eq!(db.reason_disable("next2").unwrap().removed, 1);
    assert!(ask(
        &mut db,
        "<http://ex/victim> <http://ex/q> <http://ex/o>"
    ));
    assert!(!ask(&mut db, "?s <Rule:next2> ?o"));
    let _ = fs::remove_dir_all(&root);
}

/// Finding: a ledger line that could not be read was skipped and the ledger
/// emptied anyway, so the triple it recorded could never be retracted.
#[test]
fn an_unreadable_ledger_line_fails_the_disable() {
    let root = tmp_root("bad");
    let mut db = fresh(Some(&root));
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . }")
        .unwrap();
    assert_eq!(run_rule(&mut db, ONE_HOP, "r"), (1, 1));
    let ledger = root.join("reason_rule_files/inferred/r.nt");
    let mut text = fs::read_to_string(&ledger).unwrap();
    text.push_str("garbage\n");
    fs::write(&ledger, &text).unwrap();
    assert!(db.reason_disable("r").is_err());
    // Nothing was retracted, and the ledger still holds both lines.
    assert!(ask(&mut db, "<http://ex/a> <Rule:r> <http://ex/b>"));
    assert_eq!(fs::read_to_string(&ledger).unwrap(), text);
    assert_eq!(status(&mut db, "r"), "已执行");
    let _ = fs::remove_dir_all(&root);
}

/// Finding: execute inserted (durably) and only then wrote the ledger; when
/// that write failed the inferred triples stayed with no rule owning them.
#[test]
fn a_failed_ledger_write_leaves_no_unowned_inferred_triple() {
    let root = tmp_root("intent");
    let mut db = fresh(Some(&root));
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . }")
        .unwrap();
    db.reason_add(&json::parse(ONE_HOP).unwrap()).unwrap();
    db.reason_compile("r").unwrap();
    // The ledger's temporary file is a directory: every ledger write fails.
    let blocker = root.join("reason_rule_files/inferred/r.nt.tmp");
    fs::create_dir_all(&blocker).unwrap();
    assert!(db.reason_execute("r").is_err());
    assert!(
        !ask(&mut db, "?s <Rule:r> ?o"),
        "nothing is inserted unless the ledger records it first"
    );
    assert_eq!(status(&mut db, "r"), "已编译");
    fs::remove_dir_all(&blocker).unwrap();
    let x = db.reason_execute("r").unwrap();
    assert_eq!((x.inserted, x.owned), (1, 1));
    assert_eq!(db.reason_disable("r").unwrap().removed, 1);
    assert!(!ask(&mut db, "?s <Rule:r> ?o"));
    let _ = fs::remove_dir_all(&root);
}

/// Finding: disabling a rule of a snapshot database emptied its ledger
/// before the deletion was saved; a crash then left the inferred triples
/// on disk with no rule owning them.
#[test]
fn disable_saves_a_snapshot_before_it_empties_the_ledger() {
    let root = tmp_root("snap");
    let dir = root.join("db");
    {
        let mut db = Database::new("snap");
        db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . }")
            .unwrap();
        db.save(&dir).unwrap();
    }
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(run_rule(&mut db, ONE_HOP, "r"), (1, 1));
        db.checkpoint().unwrap();
        assert_eq!(db.reason_disable("r").unwrap().removed, 1);
        // Dropped without saving: a crash right after the disable.
    }
    let mut db = Database::open(&dir).unwrap();
    assert!(
        !ask(&mut db, "?s <Rule:r> ?o"),
        "the retraction is on disk once the ledger is empty"
    );
    assert_eq!(status(&mut db, "r"), "已失效");
    let _ = fs::remove_dir_all(&root);
}

/// Finding: incremental backups carried no rules, so a restore paired the
/// replayed inferred triples with the base's (older) rules and ledgers.
#[test]
fn incremental_backups_carry_the_rules_of_their_time() {
    // The rule executed after the base.
    let root = tmp_root("incr");
    let mut db = Database::new("incr");
    db.enable_redo_log_in(root.join("log")).unwrap();
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . }")
        .unwrap();
    db.reason_add(&json::parse(ONE_HOP).unwrap()).unwrap();
    db.reason_compile("r").unwrap();
    db.backup(root.join("base")).unwrap();
    assert_eq!(db.reason_execute("r").unwrap().inserted, 1);
    db.backup_incremental(root.join("inc1")).unwrap();
    let mut restored =
        Database::restore_incremental(root.join("base"), &[root.join("inc1")]).unwrap();
    assert!(ask(&mut restored, "<http://ex/a> <Rule:r> <http://ex/b>"));
    assert_eq!(status(&mut restored, "r"), "已执行");
    assert_eq!(restored.reason_disable("r").unwrap().removed, 1);
    assert!(!ask(&mut restored, "?s <Rule:r> ?o"));

    // The rule added after the base.
    let root2 = tmp_root("incr2");
    let mut db = Database::new("incr2");
    db.enable_redo_log_in(root2.join("log")).unwrap();
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . }")
        .unwrap();
    db.backup(root2.join("base")).unwrap();
    assert_eq!(run_rule(&mut db, ONE_HOP, "r"), (1, 1));
    db.backup_incremental(root2.join("inc1")).unwrap();
    // Then deleted again (disabled first) in a second increment.
    db.reason_disable("r").unwrap();
    db.reason_delete("r").unwrap();
    db.backup_incremental(root2.join("inc2")).unwrap();
    let mut one = Database::restore_incremental(root2.join("base"), &[root2.join("inc1")]).unwrap();
    assert_eq!(one.reason_list().unwrap().len(), 1);
    assert_eq!(one.reason_disable("r").unwrap().removed, 1);
    let mut two = Database::restore_incremental(
        root2.join("base"),
        &[root2.join("inc1"), root2.join("inc2")],
    )
    .unwrap();
    assert!(two.reason_list().unwrap().is_empty());
    assert!(!ask(&mut two, "?s <Rule:r> ?o"));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&root2);
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

const CHAIN: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                     <http://ex/b> <http://ex/p> <http://ex/c> .\n";

/// A server (root `123456`) whose build may read `data`, with `check_point`
/// as given; stopped on drop.
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Srv {
    fn start(tag: &str, data: &Path, check_point: bool) -> Srv {
        let root = scratch(tag);
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        config.thread_num = 4;
        config.check_point = check_point;
        let policy = LoadPolicy::restricted().with_allowed_dir(data);
        let server = Arc::new(
            ApiServer::bind(&root, "123456", "127.0.0.1:0")
                .unwrap()
                .with_config(config)
                .with_load_policy(policy),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        Srv { server, addr, root }
    }

    fn op(&self, operation: &str, params: &[(&str, &str)]) -> Json {
        api(&self.addr, operation, params)
    }
}

fn api(addr: &SocketAddr, operation: &str, params: &[(&str, &str)]) -> Json {
    let mut body = Json::obj()
        .with("operation", operation)
        .with("username", "root")
        .with("password", "123456");
    for (k, v) in params {
        body.set(*k, *v);
    }
    let (status, text) = harness::http(addr, "POST", "/api", body.to_string().as_bytes());
    assert_eq!(status, 200, "{text}");
    json::parse(&text).unwrap()
}

fn rest(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    http_with(
        addr,
        method,
        target,
        &[("Authorization", "Basic cm9vdDoxMjM0NTY=")],
        body.as_bytes(),
    )
}

fn reason(addr: &SocketAddr, db: &str, kind: &str, rule: &str) -> Json {
    api(
        addr,
        "reasonManage",
        &[("db_name", db), ("type", kind), ("rulename", rule)],
    )
}

fn add_rule(addr: &SocketAddr, db: &str, rule: &str) {
    let mut body = Json::obj()
        .with("operation", "reasonManage")
        .with("username", "root")
        .with("password", "123456")
        .with("db_name", db)
        .with("type", "1");
    body.set("ruleinfo", json::parse(rule).unwrap());
    let (status, text) = harness::http(addr, "POST", "/api", body.to_string().as_bytes());
    assert_eq!(status, 200, "{text}");
    ok(&json::parse(&text).unwrap());
}

fn data_file(tag: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_root(tag);
    let file = dir.join("chain.nt");
    fs::write(&file, CHAIN).unwrap();
    (dir, file)
}

/// Finding (server side): with `check_point` off nothing saved the snapshot
/// after a disable, while its ledger was already empty on disk.
#[test]
fn a_served_disable_is_saved_even_with_check_point_off() {
    let (data, file) = data_file("cpoff");
    let srv = Srv::start("m4fix_cpoff", &data, false);
    ok(&srv.op(
        "build",
        &[("db_name", "fam"), ("db_path", &path_str(&file))],
    ));
    ok(&srv.op("load", &[("db_name", "fam")]));
    add_rule(&srv.addr, "fam", NEXT2);
    ok(&reason(&srv.addr, "fam", "3", "next2"));
    let x = reason(&srv.addr, "fam", "4", "next2");
    ok(&x);
    assert_eq!(int(&x, "AnsNum"), 1);
    ok(&srv.op("checkpoint", &[("db_name", "fam")]));
    let d = reason(&srv.addr, "fam", "5", "next2");
    ok(&d);
    assert_eq!(int(&d, "AnsNum"), 1);
    // What a crash now would leave on disk.
    let dir = srv.root.join("fam.db");
    assert_eq!(ledger_text(&dir, "next2"), "");
    let mut on_disk = Database::read_copy(&dir).unwrap();
    assert!(!ask(&mut on_disk, "?s <Rule:next2> ?o"));
    drop(srv);
    let _ = fs::remove_dir_all(&data);
}

/// Run `f` on another thread; whether it finished within `wait`, and its
/// result once it does.
fn while_rule_lock_held<T: Send + 'static>(
    dir: &Path,
    f: impl FnOnce() -> T + Send + 'static,
) -> (bool, T) {
    let store = RuleStore::dir(dir);
    let lock = store.lock();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    let early = rx.recv_timeout(Duration::from_millis(1500));
    let finished_early = early.is_ok();
    drop(lock);
    let out = match early {
        Ok(v) => v,
        Err(_) => rx.recv_timeout(Duration::from_secs(60)).unwrap(),
    };
    (finished_early, out)
}

/// Finding: the REST `/drop`, `/restore`, a rebuilding `/build` and the
/// `/api` `rename` removed, replaced or moved a database directory while a
/// `reasonManage` file operation could be half-way through it.
#[test]
fn directory_swaps_wait_for_reason_file_operations() {
    let (data, file) = data_file("lock");
    let srv = Server::start_with_dir("m4fix_lock", Some(&data));
    let addr = srv.addr;
    let build = |db: &str| ok(&srv.build(db, &file));
    let dir = |db: &str| srv.root.join(format!("{db}.db"));

    // REST /drop
    build("dbone");
    let (early, (s, body)) = while_rule_lock_held(&dir("dbone"), move || {
        rest(&addr, "POST", "/drop?db_name=dbone", "")
    });
    assert_eq!(s, 200, "{body}");
    assert!(!early, "/drop waits for the rule lock");

    // REST /restore
    build("dbtwo");
    let b = srv.op("backup", &[("db_name", "dbtwo")]);
    ok(&b);
    // Relative to the server's backup directory.
    let backup = b.get("backupfilepath").and_then(Json::as_str).unwrap();
    let backup = Path::new(backup).file_name().unwrap().to_string_lossy();
    let target = format!("/restore?db_name=dbtwo&path={backup}");
    let (early, (s, body)) =
        while_rule_lock_held(&dir("dbtwo"), move || rest(&addr, "POST", &target, ""));
    assert_eq!(s, 200, "{body}");
    assert!(!early, "/restore waits for the rule lock");

    // REST /build over an existing database
    build("dbthree");
    let (early, (s, body)) = while_rule_lock_held(&dir("dbthree"), move || {
        rest(&addr, "POST", "/build?db_name=dbthree", CHAIN)
    });
    assert_eq!(s, 200, "{body}");
    assert!(!early, "a rebuild waits for the rule lock");

    // /api rename
    build("dbfour");
    let (early, r) = while_rule_lock_held(&dir("dbfour"), move || {
        api(
            &addr,
            "rename",
            &[("db_name", "dbfour"), ("new_name", "dbfive")],
        )
    });
    assert_eq!(code(&r), 0, "{r}");
    assert!(!early, "rename waits for the rule lock");
    drop(srv);
    let _ = fs::remove_dir_all(&data);
}

/// Finding: a REST `/build` over an existing database kept the old
/// database's rules — executed, with ledgers naming triples the rebuilt
/// data holds as asserted ones.
#[test]
fn a_rest_rebuild_starts_without_the_old_rules() {
    let (data, file) = data_file("rebuild");
    let srv = Server::start_with_dir("m4fix_rebuild", Some(&data));
    ok(&srv.build("fam", &file));
    srv.load("fam");
    add_rule(&srv.addr, "fam", NEXT2);
    ok(&reason(&srv.addr, "fam", "3", "next2"));
    assert_eq!(int(&reason(&srv.addr, "fam", "4", "next2"), "AnsNum"), 1);
    let rebuilt = format!("{CHAIN}<http://ex/a> <Rule:next2> <http://ex/c> .\n");
    let (s, body) = srv.rest("POST", "/build?db_name=fam", &rebuilt);
    assert_eq!(s, 200, "{body}");
    assert!(!srv.root.join("fam.db/reason_rule_files").exists());
    let show = reason(&srv.addr, "fam", "6", "next2");
    assert_eq!(code(&show), 1005, "{show}");
    // The rebuilt data is asserted data.
    let q = srv.query("fam", "SELECT ?s WHERE { ?s <Rule:next2> ?o }");
    ok(&q);
    assert!(q.to_string().contains("http://ex/a"), "{q}");
    drop(srv);
    let _ = fs::remove_dir_all(&data);
}
