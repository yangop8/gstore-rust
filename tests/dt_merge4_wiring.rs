//! Phase 4 (reasoning rules, PFN functions) joined with Phase 5 (zip
//! backups, the legacy command-line tools, the logs, the import pipeline):
//! a database's reasoning rules and their ownership ledgers travel with it
//! through every backup path — `gbackup` / `grestore` (directory and zip),
//! the `/api` `backup` / `restore` (plain and `compress=true`) and a `drop`
//! kept as `.bak` — while `export` writes data only, as upstream's;
//! `reasonManage` and the PFN operations reach the `/api` access and query
//! logs like every other operation, and a PFN query over REST the REST
//! query log; and rules over the blank nodes the import pipeline mints
//! (per-load scopes) retract exactly, whatever is imported after them.

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
    assert!(fs::read_dir(export_dir).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("reason")));

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
        assert_eq!(
            j.get("results").map(Json::to_string).as_deref(),
            Some("1"),
            "{j}"
        );
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
    let r = logged(&srv, "query", "the REST PFN query", |r| {
        field(r, "sparql") == rest_pfn
    });
    assert_eq!(field(&r, "dbname"), "fam", "{r}");
    assert_eq!(
        r.get("statusCode").and_then(Json::as_i64) == Some(0),
        cfg!(feature = "pfn-wasm"),
        "{r}"
    );
    if cfg!(feature = "pfn-wasm") {
        let r = logged(&srv, "query", "the /api PFN query", |r| {
            field(r, "sparql") == api_pfn
        });
        assert_eq!(field(&r, "dbname"), "fam", "{r}");
        assert_eq!(r.get("statusCode").and_then(Json::as_i64), Some(0), "{r}");
    } else {
        assert!(log_records(&srv, "query")
            .iter()
            .all(|r| field(r, "sparql") != api_pfn));
    }
}

/// The `<Rule:next2>` triples of `db` (`?s ?o` pairs, N-Triples).
fn rule_pairs(srv: &Server, db: &str) -> Vec<String> {
    let j = srv.op(
        "query",
        &[
            ("db_name", db),
            ("sparql", "SELECT ?s ?o WHERE { ?s <Rule:next2> ?o }"),
            ("format", "n-triple"),
        ],
    );
    ok(&j);
    let mut out: Vec<String> = j
        .get("results")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .map(|row| row.to_string())
        .collect();
    out.sort();
    out
}

fn count(srv: &Server, db: &str, pattern: &str) -> i64 {
    let j = srv.query(
        db,
        &format!("SELECT (COUNT(*) AS ?n) WHERE {{ {pattern} }}"),
    );
    ok(&j);
    let cell = j
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .and_then(|b| b.first().cloned())
        .and_then(|b| b.get("n").and_then(|n| n.get("value")).cloned())
        .unwrap_or_else(|| panic!("{j}"));
    cell.as_str().unwrap().parse().unwrap()
}

/// The ledger lines of `next2` in served database `db`.
fn ledger(srv: &Server, db: &str) -> Vec<String> {
    let path = served_dir(srv, db).join("reason_rule_files/inferred/next2.nt");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

/// A chain through blank nodes, as Turtle: explicit labels (`_:b1`, stored
/// under the load's scope as `<scope>_b1`) and anonymous `[ … ]` nodes
/// (minted `<scope>-<n>`), plus one IRI.
const BLANK_CHAIN: &str = "@prefix ex: <http://ex/> .\n\
    _:b1 ex:p _:b2 .\n\
    _:b2 ex:p [ ex:p ex:end ] .\n";

/// Reasoning over the blank nodes the import pipeline mints — scoped per
/// build, `batchInsert` and SPARQL `LOAD`, explicit labels and anonymous
/// nodes alike — executes and retracts exactly: every ledger line parses
/// back (N-Triples `parse_term`) to the stored triple, imports after the
/// execution (the same document again: new blank nodes; asserted
/// `<Rule:…>` triples over blank nodes) leave the ledger as it was, and
/// disabling the rule removes exactly what it inferred.
#[test]
fn rules_over_imported_blank_nodes_retract_exactly() {
    let data = scratch("m4_blank_data");
    let _data = DirGuard(data.clone());
    let srv = Server::start_with_dir("m4_blank", Some(&data));
    let ttl = data.join("chain.ttl");
    fs::write(&ttl, BLANK_CHAIN).unwrap();
    ok(&srv.build("bnode", &ttl));
    srv.load("bnode");
    // The same document LOADed: its own scope, so three more blank nodes.
    let load = format!("LOAD <file://{}>", path_str(&ttl));
    ok(&srv.query("bnode", &load));
    assert_eq!(count(&srv, "bnode", "?s <http://ex/p> ?o"), 6);
    assert_eq!(
        count(&srv, "bnode", "?s <http://ex/p> ?o FILTER(isBlank(?s))"),
        6
    );

    ok(&srv.op(
        "reasonManage",
        &[("db_name", "bnode"), ("type", "1"), ("ruleinfo", NEXT2)],
    ));
    ok(&reason(&srv, "bnode", "3", "next2"));
    let j = reason(&srv, "bnode", "4", "next2");
    ok(&j);
    // Per copy: b1 → [], b2 → ex:end.
    assert_eq!(int(&j, "AnsNum"), 4);
    let inferred = rule_pairs(&srv, "bnode");
    assert_eq!(inferred.len(), 4);
    let lines = ledger(&srv, "bnode");
    assert_eq!(lines.len(), 4, "{lines:?}");
    for line in &lines {
        let t = gstore::reason::manage::parse_ledger_line(line)
            .unwrap_or_else(|| panic!("ledger line does not parse: {line}"));
        assert!(t.subject.to_string().starts_with("_:"), "{line}");
        assert_eq!(gstore::reason::manage::ledger_line(&t), *line);
    }
    // Labels as the importer minted them: a scoped explicit label and a
    // scoped anonymous one.
    assert!(lines.iter().any(|l| l.contains("_b1\t")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l
            .split('\t')
            .nth(2)
            .is_some_and(|o| o.starts_with("_:") && o.contains('-'))),
        "{lines:?}"
    );

    // Imports after the execution: the document again (new blank nodes), and
    // asserted <Rule:next2> triples over blank nodes and IRIs. None of them
    // touches the ledger.
    let before =
        fs::read(served_dir(&srv, "bnode").join("reason_rule_files/inferred/next2.nt")).unwrap();
    let asserted = data.join("asserted.ttl");
    fs::write(
        &asserted,
        "@prefix ex: <http://ex/> .\n\
         _:b1 <Rule:next2> ex:end .\n\
         ex:x <Rule:next2> [ ex:p ex:y ] .\n",
    )
    .unwrap();
    for file in [&ttl, &asserted] {
        let j = srv.op(
            "batchInsert",
            &[("db_name", "bnode"), ("file", &path_str(file))],
        );
        ok(&j);
    }
    ok(&srv.query("bnode", &format!("LOAD <file://{}>", path_str(&asserted))));
    let after =
        fs::read(served_dir(&srv, "bnode").join("reason_rule_files/inferred/next2.nt")).unwrap();
    assert_eq!(before, after, "imports do not touch the ledger");
    // 4 inferred + 2 asserted twice (batchInsert, LOAD: new blank nodes).
    assert_eq!(rule_pairs(&srv, "bnode").len(), 8);

    // Disable: exactly the four inferred triples go.
    let j = reason(&srv, "bnode", "5", "next2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 4);
    let left = rule_pairs(&srv, "bnode");
    assert_eq!(left.len(), 4, "{left:?}");
    assert!(
        inferred.iter().all(|t| !left.contains(t)),
        "{inferred:?} vs {left:?}"
    );
    assert_eq!(count(&srv, "bnode", "?s <http://ex/p> ?o"), 11);
}

/// A tolerant import of a file holding `<Rule:…>` predicates (next to
/// statements it skips) asserts those triples: no rule owns them, so a rule
/// that derives one of them neither counts it as inferred nor retracts it.
#[test]
fn imported_rule_predicates_are_asserted_triples_no_rule_owns() {
    let data = scratch("m4_tolerant_data");
    let _data = DirGuard(data.clone());
    let srv = Server::start_with_dir("m4_tolerant", Some(&data));
    let nt = data.join("with_rules.nt");
    fs::write(
        &nt,
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
         <http://ex/b> <http://ex/p> <http://ex/c> .\n\
         this line is not N-Triples\n\
         <http://ex/a> <Rule:next2> <http://ex/c> .\n\
         <http://ex/q> <Rule:next2> <http://ex/r> .\n",
    )
    .unwrap();
    let j = srv.build("tol", &nt);
    ok(&j);
    srv.load("tol");
    assert_eq!(rule_pairs(&srv, "tol").len(), 2);
    ok(&srv.op(
        "reasonManage",
        &[("db_name", "tol"), ("type", "1"), ("ruleinfo", NEXT2)],
    ));
    ok(&reason(&srv, "tol", "3", "next2"));
    let j = reason(&srv, "tol", "4", "next2");
    ok(&j);
    // a → c was already there: nothing new, nothing owned.
    assert_eq!(int(&j, "AnsNum"), 0);
    assert!(ledger(&srv, "tol").is_empty());
    let j = reason(&srv, "tol", "5", "next2");
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 0);
    assert_eq!(rule_pairs(&srv, "tol").len(), 2);
}

/// One `refreshconf` re-applies the `[query]`, `[pfn]` and diagnostic-log
/// (`[log]`) sections of the re-read `conf.ini` together — to the loaded
/// database, the PFN registry and the process's logger — while
/// `pfn_base_path`, like `db_home`, is read at startup only: the functions
/// stay where they are.
#[test]
fn one_refreshconf_reapplies_query_pfn_and_log_sections() {
    use gstore::config::GStoreConfig;
    use gstore::http_api::ApiServer;
    use gstore::logging::{self, Level, LogConfig};
    use std::sync::Arc;

    let dir = scratch("m4_refresh");
    let _dir = DirGuard(dir.clone());
    let conf = dir.join("conf.ini");
    let home = dir.join("home");
    let log = dir.join("diag.log");
    let write = |division: &str, max_functions: u32, level: &str, pfn_dir: &str| {
        fs::write(
            &conf,
            format!(
                "[server]\nport=0\nip_deny_path=off\nthread_num=4\n\
                 [system]\ndb_home={}\npfn_base_path={}\n\
                 [log]\nlog_mode=none\nlog_level={level}\nlog_file={}\nlog_stderr=off\n\
                 [query]\ninteger_division={division}\n\
                 [pfn]\npfn_max_functions={max_functions}\n",
                home.display(),
                dir.join(pfn_dir).display(),
                log.display()
            ),
        )
        .unwrap();
    };
    write("decimal", 5, "info", "pfn_a");
    let mut cfg = GStoreConfig::load(&conf).unwrap();
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    cfg.listen_addr = "127.0.0.1".into();
    cfg.root_password = harness::PASSWORD.into();
    // What gserver does at startup: install the log from conf.ini.
    let (log_cfg, _) = LogConfig::from_gstore_config(&cfg);
    logging::init(&log_cfg).unwrap();
    let server = Arc::new(
        ApiServer::bind_with_config(cfg)
            .unwrap()
            .with_load_policy(gstore::LoadPolicy::restricted().with_allowed_dir(&dir))
            .with_log_refresh(),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let op = |operation: &str, params: &[(&str, &str)]| {
        let mut body = Json::obj()
            .with("operation", operation)
            .with("username", harness::USERNAME)
            .with("password", harness::PASSWORD);
        for (k, v) in params {
            body.set(*k, *v);
        }
        let (status, text) = harness::http(&addr, "POST", "/api", body.to_string().as_bytes());
        assert_eq!(status, 200, "{text}");
        json::parse(&text).unwrap()
    };
    let nt = dir.join("d.nt");
    fs::write(&nt, CHAIN).unwrap();
    ok(&op(
        "build",
        &[("db_name", "conf"), ("db_path", &path_str(&nt))],
    ));
    ok(&op("load", &[("db_name", "conf")]));
    let div = || {
        let j = op(
            "query",
            &[
                ("db_name", "conf"),
                ("sparql", "SELECT ((7/2) AS ?x) WHERE {}"),
            ],
        );
        ok(&j);
        j.to_string()
    };
    assert!(div().contains("3.5"));
    let registry = server.pfn_registry();
    assert_eq!(registry.base_dir(), dir.join("pfn_a"));
    assert_eq!(registry.limits().max_functions, 5);
    assert!(!logging::enabled(Level::Debug));

    write("truncate", 7, "debug", "pfn_b");
    ok(&op("refreshconf", &[]));
    // [query]: the loaded database divides as upstream now.
    let text = div();
    assert!(!text.contains("3.5") && text.contains("\"3\""), "{text}");
    // [pfn]: the registry's limits; its directory stays.
    let registry = server.pfn_registry();
    assert_eq!(registry.limits().max_functions, 7);
    assert_eq!(registry.base_dir(), dir.join("pfn_a"));
    let info = Json::obj()
        .with("funName", "kept")
        .with("funBody", "(i32.const 1)")
        .to_string();
    ok(&op("funcudb", &[("type", "1"), ("funInfo", &info)]));
    assert!(dir.join("pfn_a/src/root/data.json").is_file());
    assert!(!dir.join("pfn_b").exists());
    // [log]: the new level reaches the installed logger and its file.
    assert!(logging::enabled(Level::Debug));
    gstore::gs_debug!("m4 refreshconf debug record");
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("m4 refreshconf debug record"), "{text}");
    server.shutdown();
    logging::reset();
}
