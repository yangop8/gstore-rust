//! Phase 4 (reasoning rules, PFN functions) joined with Phase 5 (zip
//! backups, the legacy command-line tools, the logs, the import pipeline):
//! a database's reasoning rules and their ownership ledgers travel with it
//! through every backup path — `gbackup` / `grestore` (directory and zip),
//! the `/api` `backup` / `restore` (plain and `compress=true`) and a `drop`
//! kept as `.bak` — while `export` writes data only, as upstream's;
//! `reasonManage` and the PFN operations reach the `/api` access and query
//! logs like every other operation, and a PFN query over REST the REST
//! query log.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use gstore::json::{self, Json};
use gstore::{Database, StorageKind};
use harness::{code, int, ok, path_str, scratch, string, Server};

/// Removes a directory when dropped.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Two hops along `<http://ex/p>`: `?x <Rule:next2> ?z`.
const NEXT2: &str = r#"{"rulename":"next2","description":"two hops","isenable":1,
    "type":1,"logic":1,
    "conditions":[{"patterns":[
        {"subject":"?x","predicate":"<http://ex/p>","object":"?y"},
        {"subject":"?y","predicate":"<http://ex/p>","object":"?z"}],"filters":[]}],
    "return":{"source":"?x","target":"?z","label":"next2"}}"#;

/// A chain a → b → c → d, and a `<Rule:next2>` triple a user asserted.
const CHAIN: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                     <http://ex/b> <http://ex/p> <http://ex/c> .\n\
                     <http://ex/c> <http://ex/p> <http://ex/d> .\n\
                     <http://ex/x> <Rule:next2> <http://ex/y> .\n";

fn ask(db: &mut Database, triple: &str) -> bool {
    match db.query(&format!("ASK {{ {triple} }}")).unwrap() {
        gstore::query::QueryResult::Ask(b) => b,
        other => panic!("{other:?}"),
    }
}

fn rule_count(db: &mut Database) -> u64 {
    match db
        .query("SELECT (COUNT(*) AS ?n) WHERE { ?s <Rule:next2> ?o }")
        .unwrap()
    {
        gstore::query::QueryResult::Select(rs) => {
            let cell = rs.rows[0][0].clone().unwrap();
            cell.trim_start_matches('"')
                .split('"')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        }
        other => panic!("{other:?}"),
    }
}

/// Build `dir` from [`CHAIN`] in `layout`, add, compile and execute
/// [`NEXT2`] (two triples inferred, the asserted one left unowned), and
/// persist it.
fn executed_db(dir: &Path, layout: StorageKind) {
    let nt = dir.with_extension("nt");
    fs::write(&nt, CHAIN).unwrap();
    match layout {
        StorageKind::Disk => Database::build_disk(dir, &[&nt]).unwrap(),
        _ => Database::build_from_files("fam", &[&nt])
            .unwrap()
            .save(dir)
            .unwrap(),
    }
    assert_eq!(Database::storage_kind(dir), Some(layout));
    let mut db = Database::open(dir).unwrap();
    db.reason_add(&json::parse(NEXT2).unwrap()).unwrap();
    db.reason_compile("next2").unwrap();
    let x = db.reason_execute("next2").unwrap();
    assert_eq!((x.inserted, x.owned), (2, 2));
    db.checkpoint().unwrap();
}

/// The rule file and its ledger are in database directory (or backup) `dir`.
fn has_rule_files(dir: &Path) -> bool {
    let rules = dir.join("reason_rule_files");
    rules.join("next2.json").is_file() && rules.join("inferred").join("next2.nt").is_file()
}

/// The database in `dir` holds `next2` executed, with its ledger: disabling
/// it retracts exactly the two inferred triples, never the asserted one.
fn check_restored_rule(dir: &Path) {
    assert!(has_rule_files(dir), "{}", dir.display());
    let mut db = Database::open(dir).unwrap();
    let list = db.reason_list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        db.reason_show("next2")
            .unwrap()
            .get("status")
            .and_then(Json::as_str),
        Some("已执行")
    );
    assert_eq!(rule_count(&mut db), 3);
    let d = db.reason_disable("next2").unwrap();
    assert_eq!(d.removed, 2);
    assert!(ask(&mut db, "<http://ex/x> <Rule:next2> <http://ex/y>"));
    assert!(!ask(&mut db, "<http://ex/a> <Rule:next2> <http://ex/c>"));
    assert_eq!(rule_count(&mut db), 1);
}

/// Change `dir`'s rules after a backup: disable and delete `next2`.
fn forget_rule(dir: &Path) {
    let mut db = Database::open(dir).unwrap();
    db.reason_disable("next2").unwrap();
    db.reason_delete("next2").unwrap();
    db.checkpoint().unwrap();
    assert!(!has_rule_files(dir));
}

fn tool(name: &str, cwd: &Path, args: &[&str]) -> std::process::Output {
    let bin = match name {
        "gbackup" => env!("CARGO_BIN_EXE_gbackup"),
        "grestore" => env!("CARGO_BIN_EXE_grestore"),
        other => panic!("{other}"),
    };
    Command::new(bin)
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run the tool")
}

fn succeeded(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `gbackup` / `grestore` carry the rules and ledgers of a snapshot and of
/// an on-disk database: a backup directory holds them, restoring brings
/// them back (the restored rule still retracts exactly what it inferred),
/// and — with the `zip` feature — so do `gbackup -z` and `grestore x.zip`.
#[test]
fn gbackup_and_grestore_carry_rules_and_ledgers() {
    for layout in [StorageKind::Snapshot, StorageKind::Disk] {
        let cwd = scratch("m4_cli_backup");
        let _cwd = DirGuard(cwd.clone());
        let db = cwd.join("fam.db");
        executed_db(&db, layout);

        // A backup directory.
        succeeded(&tool("gbackup", &cwd, &["fam", "bk1"]));
        assert!(has_rule_files(&cwd.join("bk1")), "{layout:?}");
        forget_rule(&db);
        succeeded(&tool("grestore", &cwd, &["fam", "bk1"]));
        check_restored_rule(&db);

        // A zipped backup (upstream `gbackup -z 1`).
        executed_db(&cwd.join("fam2.db"), layout);
        let out = tool("gbackup", &cwd, &["-z", "1", "fam2", "bk2"]);
        if !gstore::zipio::AVAILABLE {
            assert!(!out.status.success());
            assert!(String::from_utf8_lossy(&out.stderr).contains("zip"));
            continue;
        }
        succeeded(&out);
        assert!(cwd.join("bk2.zip").is_file());
        forget_rule(&cwd.join("fam2.db"));
        succeeded(&tool("grestore", &cwd, &["fam2", "bk2.zip"]));
        check_restored_rule(&cwd.join("fam2.db"));
    }
}

/// The server's database directory of `db`.
fn served_dir(srv: &Server, db: &str) -> PathBuf {
    srv.root.join(format!("{db}.db"))
}

fn reason(srv: &Server, db: &str, kind: &str, rule: &str) -> Json {
    srv.op(
        "reasonManage",
        &[("db_name", db), ("type", kind), ("rulename", rule)],
    )
}

/// A served database `db` built from [`CHAIN`] with [`NEXT2`] executed.
fn served_executed(srv: &Server, data_dir: &Path, db: &str) {
    let nt = data_dir.join(format!("{db}.nt"));
    fs::write(&nt, CHAIN).unwrap();
    ok(&srv.build(db, &nt));
    srv.load(db);
    ok(&srv.op(
        "reasonManage",
        &[("db_name", db), ("type", "1"), ("ruleinfo", NEXT2)],
    ));
    ok(&reason(srv, db, "3", "next2"));
    let j = reason(srv, db, "4", "next2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 2);
}

/// After a restore: `next2` is back, executed, and disabling it retracts
/// exactly its two triples.
fn served_rule_restored(srv: &Server, db: &str) {
    srv.load(db);
    let j = reason(srv, db, "2", "");
    ok(&j);
    assert_eq!(int(&j, "num"), 1, "{j}");
    let info = reason(srv, db, "6", "next2");
    assert_eq!(string(info.get("ruleinfo").unwrap(), "status"), "已执行");
    let j = reason(srv, db, "5", "next2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 2);
    let j = srv.query(db, "ASK { <http://ex/x> <Rule:next2> <http://ex/y> }");
    assert!(j.to_string().contains("true"), "{j}");
}

/// The `/api` `backup` (plain and, with the `zip` feature,
/// `compress=true`) and `restore` carry the rules and ledgers; a `drop`
/// with `is_backup` keeps them in the `.bak` directory.
#[test]
fn api_backup_restore_and_drop_carry_rules_and_ledgers() {
    let data = scratch("m4_api_backup_data");
    let _data = DirGuard(data.clone());
    let srv = Server::start_with_dir("m4_api_backup", Some(&data));
    served_executed(&srv, &data, "fam");

    // A plain backup holds the rule files; restored over a changed rule
    // set, they come back.
    let j = srv.op("backup", &[("db_name", "fam")]);
    ok(&j);
    let shown = string(&j, "backupfilepath");
    let name = Path::new(&shown).file_name().unwrap().to_owned();
    let plain = srv.root.join("backups").join(&name);
    assert!(has_rule_files(&plain), "{}", plain.display());
    ok(&reason(&srv, "fam", "5", "next2"));
    ok(&reason(&srv, "fam", "7", "next2"));
    assert!(!has_rule_files(&served_dir(&srv, "fam")));
    ok(&srv.op(
        "restore",
        &[("db_name", "fam"), ("backup_path", &path_str(&plain))],
    ));
    assert!(has_rule_files(&served_dir(&srv, "fam")));
    served_rule_restored(&srv, "fam");
    ok(&reason(&srv, "fam", "4", "next2"));

    // A zipped one (which replaces a plain backup of the same second, as
    // upstream's does).
    let j = srv.op("backup", &[("db_name", "fam"), ("compress", "true")]);
    if !gstore::zipio::AVAILABLE {
        assert_eq!(code(&j), 1003, "{j}");
    } else {
        ok(&j);
        let shown = string(&j, "backupfilepath");
        assert!(shown.ends_with(".zip"), "{shown}");
        let zip = srv
            .root
            .join("backups")
            .join(Path::new(&shown).file_name().unwrap());
        assert!(zip.is_file());
        ok(&reason(&srv, "fam", "5", "next2"));
        ok(&reason(&srv, "fam", "7", "next2"));
        assert!(!has_rule_files(&served_dir(&srv, "fam")));
        ok(&srv.op(
            "restore",
            &[("db_name", "fam"), ("backup_path", &path_str(&zip))],
        ));
        assert!(has_rule_files(&served_dir(&srv, "fam")));
        served_rule_restored(&srv, "fam");
        ok(&reason(&srv, "fam", "4", "next2"));
    }

    // drop is_backup=true keeps the directory, rules and ledgers included,
    // as <db>.db.bak; is_backup=false removes them.
    ok(&srv.op("drop", &[("db_name", "fam"), ("is_backup", "true")]));
    let bak = srv.root.join("fam.db.bak");
    assert!(has_rule_files(&bak), "{}", bak.display());
    served_executed(&srv, &data, "fam2");
    srv.drop_db("fam2");
    assert!(!served_dir(&srv, "fam2").exists());
}

/// `export` writes the data — the inferred `<Rule:…>` triples are ordinary
/// triples and are exported — but not the rules or their ledgers: upstream's
/// export (`Database::export`) writes triples only. A database built from
/// the export therefore holds the inferred triples as asserted ones, owned
/// by no rule.
#[test]
fn export_writes_the_inferred_triples_but_not_the_rules() {
    let data = scratch("m4_export_data");
    let _data = DirGuard(data.clone());
    let srv = Server::start_with_dir("m4_export", Some(&data));
    served_executed(&srv, &data, "fam");
    let j = srv.op("export", &[("db_name", "fam")]);
    ok(&j);
    let file = PathBuf::from(string(&j, "filepath"));
    let file = if file.is_absolute() {
        file
    } else {
        srv.root.join(file)
    };
    let text = fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    assert_eq!(text.matches("<Rule:next2>").count(), 3, "{text}");
    let export_dir = file.parent().unwrap();
    assert!(fs::read_dir(export_dir)
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().contains("reason")));

    // Rebuilt from the export, executing the rule takes over nothing: the
    // triples were there before it ran, so disabling it leaves them.
    let copy = data.join("copy.nt");
    fs::copy(&file, &copy).unwrap();
    ok(&srv.build("copy", &copy));
    srv.load("copy");
    ok(&srv.op(
        "reasonManage",
        &[("db_name", "copy"), ("type", "1"), ("ruleinfo", NEXT2)],
    ));
    ok(&reason(&srv, "copy", "3", "next2"));
    let j = reason(&srv, "copy", "4", "next2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 0);
    let j = reason(&srv, "copy", "5", "next2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 0);
    let j = srv.query(
        "copy",
        "SELECT (COUNT(*) AS ?n) WHERE { ?s <Rule:next2> ?o }",
    );
    assert!(j.to_string().contains("\"3\""), "{j}");
}

/// Every record of the `/api` query (`kind` `"query"`) or access
/// (`"access"`) log, all days.
fn log_records(srv: &Server, kind: &str) -> Vec<Json> {
    let dates = srv.op(&format!("{kind}logdate"), &[]);
    ok(&dates);
    let mut out = Vec::new();
    for date in dates
        .get("list")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Json::as_str)
    {
        let j = srv.op(
            &format!("{kind}log"),
            &[("date", date), ("pageNo", "1"), ("pageSize", "10000")],
        );
        ok(&j);
        out.extend(
            j.get("list")
                .and_then(Json::as_array)
                .unwrap_or_default()
                .iter()
                .cloned(),
        );
    }
    out
}

/// Wait (bounded) until `kind`'s log holds a record satisfying `found`.
fn logged(srv: &Server, kind: &str, what: &str, found: impl Fn(&Json) -> bool) -> Json {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let records = log_records(srv, kind);
        if let Some(r) = records.iter().find(|r| found(r)) {
            return r.clone();
        }
        assert!(
            Instant::now() < until,
            "no {kind}-log record of {what}: {records:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn field<'a>(r: &'a Json, key: &str) -> &'a str {
    r.get(key).and_then(Json::as_str).unwrap_or_default()
}

/// `reasonManage` and the PFN operations are recorded in the `/api` access
/// log like every other operation; the statement a rule executes is in the
/// query log, and so is a PFN query — sent to `/api` `query` or to the
/// REST `/sparql` endpoint — whether or not the function could run (it
/// cannot without the `pfn-wasm` runtime; the failure is logged then).
#[test]
fn reason_and_pfn_operations_reach_the_access_and_query_logs() {
    let data = scratch("m4_logs_data");
    let _data = DirGuard(data.clone());
    let srv = Server::start_with_dir("m4_logs", Some(&data));
    served_executed(&srv, &data, "fam");
    let info = Json::obj()
        .with("funName", "one")
        .with("funBody", "(call $emit_i64 (i64.const 1)) (i32.const 1)")
        .to_string();
    ok(&srv.op("funcudb", &[("type", "1"), ("funInfo", &info)]));
    let built = srv.op(
        "funcudb",
        &[("type", "4"), ("funInfo", r#"{"funName":"one"}"#)],
    );
    assert_eq!(code(&built) == 0, cfg!(feature = "pfn-wasm"), "{built}");
    ok(&srv.op("funquery", &[]));
    ok(&srv.op("funreview", &[("funInfo", &info)]));
    let api_pfn = "select (PFN.one({}) AS ?r) where {}";
    let j = srv.query("fam", api_pfn);
    if cfg!(feature = "pfn-wasm") {
        assert_eq!(j.get("results").map(Json::to_string).as_deref(), Some("1"), "{j}");
    } else {
        assert_ne!(code(&j), 0, "{j}");
    }
    let rest_pfn = "SELECT (PFN.one({\"src\":\"rest\"}) AS ?r) WHERE {}";
    let (status, body) = srv.rest("POST", "/sparql?db_name=fam", rest_pfn);
    assert_eq!(status == 200, cfg!(feature = "pfn-wasm"), "{status} {body}");

    // The access log: one record per operation, with its database.
    for (op, db) in [
        ("reasonManage", "fam"),
        ("funcudb", ""),
        ("funquery", ""),
        ("funreview", ""),
    ] {
        let r = logged(&srv, "access", op, |r| field(r, "operation") == op);
        assert_eq!(field(&r, "dbname"), db, "{r}");
        assert_eq!(field(&r, "ip"), "127.0.0.1", "{r}");
    }
    // The query log: the executed rule's statement, and both PFN queries.
    let r = logged(&srv, "query", "the rule's insert", |r| {
        field(r, "sparql").starts_with("insert { ?x <Rule:next2> ?z. }")
    });
    assert_eq!(field(&r, "dbname"), "fam", "{r}");
    assert_eq!(r.get("statusCode").and_then(Json::as_i64), Some(0), "{r}");
    // The REST endpoints log failed statements too; the /api `query`
    // operation logs the ones that ran — as upstream's, whose `catch`
    // returns before the log callback — so the /api PFN query is logged
    // when the runtime is there.
    let r = logged(&srv, "query", "the REST PFN query", |r| field(r, "sparql") == rest_pfn);
    assert_eq!(field(&r, "dbname"), "fam", "{r}");
    assert_eq!(
        r.get("statusCode").and_then(Json::as_i64) == Some(0),
        cfg!(feature = "pfn-wasm"),
        "{r}"
    );
    if cfg!(feature = "pfn-wasm") {
        let r = logged(&srv, "query", "the /api PFN query", |r| field(r, "sparql") == api_pfn);
        assert_eq!(field(&r, "dbname"), "fam", "{r}");
        assert_eq!(r.get("statusCode").and_then(Json::as_i64), Some(0), "{r}");
    } else {
        assert!(log_records(&srv, "query")
            .iter()
            .all(|r| field(r, "sparql") != api_pfn));
    }
}
