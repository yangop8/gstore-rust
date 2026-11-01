//! Regression tests for the Phase 4 review findings (reasoning rules and
//! PFN functions):
//!
//! * reasoning: a rule's WHERE clause is evaluated once, so a random filter
//!   never leaves inferred triples off the ledger; rule files upstream gStore
//!   executed and then checked / recompiled (no ledger, any status) are
//!   retracted upstream's way; an empty ledger does not make a rule
//!   executed, and a cancelled execute leaves the rule as it was; a rule
//!   file whose conditions or name this implementation refuses can still be
//!   disabled and deleted; `restore_incremental` keeps the base's rules;
//!   copying a rule directory never sees a half-done operation;
//! * PFN (feature `pfn-wasm`): see the second half of the file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::json::{self, Json};
use gstore::reason::manage;
use gstore::reason::store::{copy_rule_dir, RuleStore};
use gstore::Database;

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_p4fix_{tag}_{}_{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Upstream `API_USAGE.md`'s relation example.
const UNCLE: &str = r#"{
    "rulename": "舅舅", "description": "母亲的男性同胞是舅舅", "isenable": 1,
    "type": 1, "logic": 1,
    "conditions": [{
        "patterns": [
            {"subject": "?x ", "predicate": "<母亲>", "object": "?y"},
            {"subject": "?y ", "predicate": "<父亲>", "object": "?z"},
            {"subject": "?k", "predicate": "<父亲>", "object": "?z"},
            {"subject": "?k", "predicate": "<性别>", "object": "\"男\"^^<http://www.w3.org/2001/XMLSchema#string>"}
        ],
        "filters": [], "count_info": {}
    }],
    "return": {"source": "?x", "target": "?k", "label": "舅舅"}
}"#;

fn family(db: &mut Database) {
    let male = format!("\"男\"^^<{XSD_STRING}>");
    db.query(&format!(
        "INSERT DATA {{ <张三> <母亲> <李梅> . <李梅> <父亲> <李老爷> . \
         <李四> <父亲> <李老爷> . <李四> <性别> {male} . }}"
    ))
    .unwrap();
}

fn count(db: &mut Database, q: &str) -> usize {
    db.select(q).unwrap().rows.len()
}

fn status(db: &mut Database, name: &str) -> String {
    db.reason_show(name)
        .unwrap()
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// An in-memory database keeping its rules in `dir/reason_rule_files`.
fn db_with_rule_dir(tag: &str) -> (Database, PathBuf) {
    let dir = tmp_root(tag);
    let mut db = Database::new(tag);
    db.set_reason_db_dir(&dir);
    (db, dir)
}

fn rule_file(dir: &Path, name: &str) -> PathBuf {
    dir.join("reason_rule_files").join(format!("{name}.json"))
}

fn ledger_file(dir: &Path, name: &str) -> PathBuf {
    dir.join("reason_rule_files")
        .join("inferred")
        .join(format!("{name}.nt"))
}

/// Make rule `name` look like upstream gStore left it: no ledger, `status`.
fn as_upstream_left_it(dir: &Path, name: &str, status: &str) {
    let _ = fs::remove_file(ledger_file(dir, name));
    let path = rule_file(dir, name);
    let mut doc = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    doc.set("status", status);
    fs::write(&path, doc.sorted().to_string()).unwrap();
}

// ---------------------------------------------------------------------------
// reasoning
// ---------------------------------------------------------------------------

/// Finding: the WHERE clause ran twice (once for the ledger, once for the
/// insert), so a `RAND()` filter inserted triples the ledger never held and
/// disable left them behind.
#[test]
fn a_random_filter_never_leaves_inferred_triples_off_the_ledger() {
    let mut db = Database::new("rand");
    let mut data = String::from("INSERT DATA {");
    for i in 0..400 {
        data.push_str(&format!(" <s{i}> <p> <o{i}> ."));
    }
    data.push_str(" }");
    db.query(&data).unwrap();
    let rule = r#"{"rulename":"R","type":1,"logic":1,
        "conditions":[{"patterns":[{"subject":"?x","predicate":"<p>","object":"?y"}],
                       "filters":["RAND() < 0.5"]}],
        "return":{"source":"?x","target":"?y","label":"R"}}"#;
    db.reason_add(&json::parse(rule).unwrap()).unwrap();
    db.reason_compile("R").unwrap();
    let q = "SELECT * WHERE { ?s <Rule:R> ?o }";
    for _ in 0..2 {
        let x = db.reason_execute("R").unwrap();
        let present = count(&mut db, q);
        assert!(present > 0 && present < 400, "{present}");
        assert_eq!(x.inserted, present, "AnsNum counts what is there");
        assert_eq!(x.owned, present, "the ledger holds every inferred triple");
        let d = db.reason_disable("R").unwrap();
        assert_eq!(d.removed, present);
        assert_eq!(count(&mut db, q), 0, "nothing is orphaned");
    }
}

/// Finding: a rule upstream gStore executed and then checked (`已校验`) or
/// recompiled (`已编译`) has no ledger and live triples; disable retracted
/// nothing and delete orphaned them.
#[test]
fn rules_upstream_checked_or_recompiled_after_executing_are_still_retracted() {
    for st in ["已校验", "已编译"] {
        let (mut db, dir) = db_with_rule_dir("upchk");
        family(&mut db);
        db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
        db.reason_compile("舅舅").unwrap();
        // What upstream's execute and then check / compile leave.
        db.query("INSERT DATA { <张三> <Rule:舅舅> <李四> }")
            .unwrap();
        as_upstream_left_it(&dir, "舅舅", st);
        let d = db.reason_disable("舅舅").unwrap();
        assert_eq!(d.removed, 1, "{st}");
        assert_eq!(count(&mut db, "SELECT * WHERE { ?s <Rule:舅舅> ?o }"), 0);
        db.reason_delete("舅舅").unwrap();

        // Executing it again takes its triple over instead.
        db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
        db.reason_compile("舅舅").unwrap();
        db.query("INSERT DATA { <张三> <Rule:舅舅> <李四> }")
            .unwrap();
        as_upstream_left_it(&dir, "舅舅", st);
        let x = db.reason_execute("舅舅").unwrap();
        assert_eq!((x.inserted, x.owned), (0, 1), "{st}");
        assert_eq!(db.reason_disable("舅舅").unwrap().removed, 1);
        assert_eq!(count(&mut db, "SELECT * WHERE { ?s <Rule:舅舅> ?o }"), 0);
    }
}

/// A rule managed here never falls back to upstream's `delete where`: a
/// compiled rule that was never executed retracts nothing, not another
/// rule's triples with the same label.
#[test]
fn a_rule_never_executed_here_retracts_nothing() {
    let (mut db, _dir) = db_with_rule_dir("noexec");
    family(&mut db);
    db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
    db.reason_compile("舅舅").unwrap();
    let other = UNCLE.replace("\"rulename\": \"舅舅\"", "\"rulename\": \"舅舅2\"");
    db.reason_add(&json::parse(&other).unwrap()).unwrap();
    db.reason_compile("舅舅2").unwrap();
    assert_eq!(db.reason_execute("舅舅2").unwrap().inserted, 1);
    assert_eq!(db.reason_disable("舅舅").unwrap().removed, 0);
    assert_eq!(count(&mut db, "SELECT * WHERE { ?s <Rule:舅舅> ?o }"), 1);
    assert_eq!(db.reason_disable("舅舅2").unwrap().removed, 1);
}

/// Finding: a ledger that exists but is empty (what a failed execute left)
/// made the rule "executed": compile and delete were refused.
#[test]
fn an_empty_ledger_does_not_make_a_rule_executed() {
    let (mut db, dir) = db_with_rule_dir("emptyled");
    family(&mut db);
    db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
    db.reason_compile("舅舅").unwrap();
    let ledger = ledger_file(&dir, "舅舅");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    fs::write(&ledger, "").unwrap();
    db.reason_compile("舅舅").unwrap();
    assert_eq!(status(&mut db, "舅舅"), "已编译");
    db.reason_delete("舅舅").unwrap();
}

/// Finding: a cancel arriving while execute was running left an empty
/// ledger behind (the rule then refused compile / delete although nothing
/// was inferred).
#[test]
fn a_cancelled_execute_leaves_the_rule_as_it_was() {
    let (mut db, dir) = db_with_rule_dir("cancel");
    let mut data = String::from("INSERT DATA {");
    for i in 0..15_000 {
        data.push_str(&format!(" <s{i}> <p> <m{i}> . <m{i}> <q> <o{i}> ."));
    }
    data.push_str(" }");
    db.query(&data).unwrap();
    let rule = r#"{"rulename":"R","type":1,"logic":1,
        "conditions":[{"patterns":[{"subject":"?x","predicate":"<p>","object":"?y"},
                                   {"subject":"?y","predicate":"<q>","object":"?z"}]}],
        "return":{"source":"?x","target":"?z","label":"R"}}"#;
    db.reason_add(&json::parse(rule).unwrap()).unwrap();
    db.reason_compile("R").unwrap();
    let t0 = Instant::now();
    assert_eq!(db.reason_check("R").unwrap().effect_num, 15_000);
    let one = t0.elapsed();
    db.reason_compile("R").unwrap();
    let q = "SELECT * WHERE { ?s <Rule:R> ?o }";
    for factor in [0.3, 0.6, 0.9, 1.2, 1.5, 2.0, 3.0] {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        let delay = one.mul_f64(factor);
        let h = thread::spawn(move || {
            thread::sleep(delay);
            c2.store(true, Ordering::SeqCst);
        });
        let mut store = RuleStore::dir(&dir);
        let r = manage::execute(&mut db, &mut store, "R", Some(cancel));
        h.join().unwrap();
        match r {
            Err(e) => {
                assert_eq!(count(&mut db, q), 0, "{factor}: {e}");
                assert_eq!(status(&mut db, "R"), "已编译");
                // Not "executed": compile is not refused.
                db.reason_compile("R")
                    .unwrap_or_else(|e| panic!("{factor}: {e}"));
            }
            Ok(x) => {
                assert_eq!(x.owned, 15_000);
                assert_eq!(db.reason_disable("R").unwrap().removed, 15_000);
                db.reason_compile("R").unwrap();
            }
        }
    }
    db.reason_delete("R").unwrap();
}

/// Finding: a rule upstream gStore executed whose definition this
/// implementation refuses (an unbound return variable, a property path, a
/// name with `..`) could never be disabled or deleted.
#[test]
fn an_upstream_rule_this_validator_refuses_can_still_be_disabled_and_deleted() {
    let (mut db, dir) = db_with_rule_dir("badup");
    family(&mut db);
    let rules = dir.join("reason_rule_files");
    fs::create_dir_all(&rules).unwrap();
    let upstream = |name: &str, target: &str, predicate: &str| {
        let doc = json::parse(UNCLE).unwrap();
        let mut doc = doc;
        doc.set("rulename", name);
        doc.set("status", "已执行");
        doc.set(
            "return",
            json::parse(&format!(
                r#"{{"source":"?x","target":{},"label":"舅舅"}}"#,
                json::quote(target)
            ))
            .unwrap(),
        );
        if !predicate.is_empty() {
            let mut conds = doc.get("conditions").unwrap().clone();
            let text = conds.to_string().replace("<母亲>", predicate);
            conds = json::parse(&text).unwrap();
            doc.set("conditions", conds);
        }
        doc.set("insert_sparql", "insert { ?x <Rule:舅舅> ?w. } where {}");
        doc.set("delete_sparql", "delete where {?x <Rule:舅舅> ?y.}");
        fs::write(rules.join(format!("{name}.json")), doc.sorted().to_string()).unwrap();
    };
    for (name, target, predicate) in [
        ("舅舅", "?w", ""),
        ("舅2", "?k", "<母亲>/<父亲>"),
        ("舅舅...", "?k", ""),
        (".hidden", "?k", ""),
    ] {
        upstream(name, target, predicate);
        db.query("INSERT DATA { <张三> <Rule:舅舅> <李四> }")
            .unwrap();
        assert_eq!(db.reason_list().unwrap().len(), 1, "{name}");
        let d = db
            .reason_disable(name)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(d.removed, 1, "{name}");
        assert_eq!(d.delete_sparql, "delete where {?x <Rule:舅舅> ?y.}");
        assert_eq!(count(&mut db, "SELECT * WHERE { ?s <Rule:舅舅> ?o }"), 0);
        db.reason_delete(name)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(db.reason_list().unwrap().is_empty());
    }
    // Names are still kept inside the rule directory.
    for name in ["../x", "a/b", "a\\b", "a\u{0}b"] {
        let e = db.reason_show(name).unwrap_err();
        assert_eq!(e.kind, gstore::reason::ReasonErrorKind::Param, "{name}");
    }
}

/// Finding: `restore_incremental` lost the base backup's rules (the
/// inferred triples could then never be retracted).
#[test]
fn restore_incremental_keeps_the_bases_rules() {
    let root = tmp_root("rinc");
    let mut db = Database::new("rinc");
    family(&mut db);
    db.reason_add(&json::parse(UNCLE).unwrap()).unwrap();
    db.reason_compile("舅舅").unwrap();
    assert_eq!(db.reason_execute("舅舅").unwrap().inserted, 1);
    let base = root.join("base");
    db.backup(&base).unwrap();
    assert!(rule_file(&base, "舅舅").is_file());
    let none: [PathBuf; 0] = [];
    let mut restored = Database::restore_incremental(&base, &none).unwrap();
    assert_eq!(restored.reason_list().unwrap().len(), 1);
    assert_eq!(status(&mut restored, "舅舅"), "已执行");
    // Saved, the rules go with the snapshot.
    let saved = root.join("saved");
    restored.save(&saved).unwrap();
    assert!(rule_file(&saved, "舅舅").is_file());
    assert_eq!(restored.reason_disable("舅舅").unwrap().removed, 1);
    let mut reopened = Database::open(&saved).unwrap();
    assert_eq!(reopened.reason_list().unwrap().len(), 1);
    assert_eq!(reopened.reason_disable("舅舅").unwrap().removed, 1);
    // The backup directory itself is never modified.
    assert!(ledger_file(&base, "舅舅").is_file());
    assert_eq!(
        fs::read_to_string(ledger_file(&base, "舅舅"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

/// Finding: copying a rule directory (backup / restore) did not take the
/// rule lock, so it could copy an operation half-done (a vanished temp file,
/// an empty rule file).
#[test]
fn copying_a_rule_directory_never_sees_a_half_done_operation() {
    let root = tmp_root("copyrace");
    let src = root.join("src.db");
    fs::create_dir_all(&src).unwrap();
    let mut store = RuleStore::dir(&src);
    manage::add(&mut store, &json::parse(UNCLE).unwrap(), "now").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, src2) = (Arc::clone(&stop), src.clone());
    let writer = thread::spawn(move || {
        let mut store = RuleStore::dir(&src2);
        let mut i = 0u64;
        while !s2.load(Ordering::Relaxed) {
            manage::compile(&mut store, "舅舅").unwrap();
            let name = format!("r{i}");
            let rule = UNCLE.replace(
                "\"rulename\": \"舅舅\"",
                &format!("\"rulename\": \"{name}\""),
            );
            manage::add(&mut store, &json::parse(&rule).unwrap(), "now").unwrap();
            manage::delete(&mut store, &name).unwrap();
            i += 1;
        }
    });
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut copies = 0;
    while Instant::now() < deadline {
        let dst = root.join(format!("bk{}", copies % 4));
        fs::create_dir_all(&dst).unwrap();
        copy_rule_dir(&src, &dst).unwrap_or_else(|e| panic!("copy {copies}: {e}"));
        for entry in fs::read_dir(dst.join("reason_rule_files")).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "json") {
                let text = fs::read_to_string(&p).unwrap();
                assert!(json::parse(&text).is_ok(), "{}: {text:?}", p.display());
            }
            assert!(
                !p.to_string_lossy().ends_with(".tmp"),
                "a temp file was copied: {}",
                p.display()
            );
        }
        copies += 1;
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(copies > 10, "{copies}");
}

// ---------------------------------------------------------------------------
// PFN: evaluation (no runtime needed)
// ---------------------------------------------------------------------------

fn select_rows(r: gstore::Result<gstore::QueryResult>) -> Vec<Vec<Option<String>>> {
    match r.unwrap() {
        gstore::QueryResult::Select(rs) => rs.rows,
        other => panic!("not a SELECT result: {other:?}"),
    }
}

/// Finding: a sub-SELECT's error (here: a PFN call with no registry) was
/// swallowed into "no solution": an empty result, an inverted `NOT
/// EXISTS`, and an UPDATE that deleted every triple.
#[test]
fn a_failing_pfn_call_in_a_sub_select_fails_the_statement() {
    let mut db = Database::new("subsel");
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . <http://ex/c> <http://ex/p> <http://ex/d> . }")
        .unwrap();
    for q in [
        "SELECT * WHERE { { SELECT (PFN.f({}) AS ?v) WHERE {} } }",
        "SELECT ?s WHERE { ?s ?p ?o FILTER NOT EXISTS { { SELECT (PFN.f({}) AS ?v) WHERE {} } } }",
        "SELECT ?s WHERE { ?s ?p ?o FILTER EXISTS { { SELECT (PFN.f({}) AS ?v) WHERE {} } } }",
        "CONSTRUCT { ?s ?p ?v } WHERE { ?s ?p ?o { SELECT (PFN.f({}) AS ?v) WHERE {} } }",
        "ASK { { SELECT (PFN.f({}) AS ?v) WHERE {} } }",
    ] {
        let e = db.query(q).unwrap_err().to_string();
        assert!(e.contains("PFN"), "{q}: {e}");
    }
    let e = db
        .query(
            "DELETE { ?s ?p ?o } WHERE { ?s ?p ?o \
             FILTER NOT EXISTS { { SELECT (PFN.f({}) AS ?v) WHERE {} } } }",
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("PFN"), "{e}");
    assert_eq!(
        select_rows(db.query("SELECT * WHERE { ?s ?p ?o }")).len(),
        2
    );
    // An ordinary sub-SELECT is unaffected.
    let rows = select_rows(db.query(
        "SELECT ?s WHERE { ?s ?p ?o FILTER NOT EXISTS { { SELECT ?x WHERE { ?x <http://ex/q> ?y } } } }",
    ));
    assert_eq!(rows.len(), 2);
}

/// Finding: `expand` recursed once per variable key, so an argument with
/// many `"?x"` keys overflowed the stack and aborted the process.
#[test]
fn a_wide_pfn_argument_neither_overflows_the_stack_nor_takes_quadratic_time() {
    let mut text = String::from("{");
    for i in 0..130_000 {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&format!("\"k{i}\":\"?x\""));
    }
    text.push('}');
    let args = gstore::pfn::call::parse_args("f", &text).unwrap();
    // The 2 MiB stack of a `gstore-task` thread.
    let h = thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let t = Instant::now();
            let mut calls = 0;
            let r = gstore::pfn::call::evaluate(
                "f",
                &args,
                false,
                |v| (v == "x").then(|| vec!["<http://ex/a>".to_string()]),
                |p| {
                    calls += 1;
                    assert!(p.contains(r#""k129999":"<http://ex/a>""#));
                    Ok("1".into())
                },
                1 << 20,
            );
            (r.map_err(|e| e.to_string()), calls, t.elapsed())
        })
        .unwrap();
    let (r, calls, took) = h.join().unwrap();
    assert_eq!(r.unwrap(), "[1]");
    assert_eq!(calls, 1);
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// Finding: every argument combination was built (and held) before the
/// first call, so a large constant times many combinations exhausted memory
/// before the combination limit was even reached.
#[test]
fn pfn_argument_combinations_are_counted_before_anything_is_built() {
    let mut text = format!(r#"{{"pad":"{}""#, "x".repeat(1000));
    for i in 0..10 {
        text.push_str(&format!(r#","k{i}":"?x""#));
    }
    text.push('}');
    let args = gstore::pfn::call::parse_args("f", &text).unwrap();
    let values: Vec<String> = (0..4).map(|i| format!("<http://ex/{i}>")).collect();
    let t = Instant::now();
    let e = gstore::pfn::call::evaluate(
        "f",
        &args,
        false,
        |_| Some(values.clone()),
        |_| panic!("no call is made"),
        1 << 20,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("more than 1000000 argument combinations"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    // Under the limit, the calls are made in the order of the argument
    // texts, each once.
    let args = gstore::pfn::call::parse_args("f", r#"{"b":"?y","a":"?x","c":"k"}"#).unwrap();
    let mut seen = Vec::new();
    gstore::pfn::call::evaluate(
        "f",
        &args,
        false,
        |v| {
            Some(match v {
                "x" => vec![
                    "\"a\"".into(),
                    "\"a b\"".into(),
                    "\"a\\\"\"".into(),
                    "\"a\"".into(),
                ],
                _ => vec!["<z>".into(), "<y>".into()],
            })
        },
        |p| {
            seen.push(p.to_string());
            Ok("1".into())
        },
        1 << 20,
    )
    .unwrap();
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen, sorted);
    assert_eq!(seen.len(), 6);
}

// ---------------------------------------------------------------------------
// PFN: the runtime (feature pfn-wasm)
// ---------------------------------------------------------------------------

#[cfg(feature = "pfn-wasm")]
mod runtime {
    use super::*;
    use gstore::pfn::{url_encode, PfnCaller, PfnInfo, PfnLimits, PfnRegistry};
    use gstore::QueryResult;
    use std::sync::mpsc;

    fn registry(tag: &str) -> Arc<PfnRegistry> {
        let base = tmp_root(tag);
        Arc::new(PfnRegistry::new(base.join("pfn")))
    }

    fn define(reg: &PfnRegistry, user: &str, name: &str, body: &str, subs: &str) {
        let info = PfnInfo {
            name: name.into(),
            body: url_encode(body),
            subs: url_encode(subs),
            ..PfnInfo::default()
        };
        reg.create(user, info).unwrap();
        let r = reg.compile(user, name).unwrap();
        assert!(r.is_ok(), "compile {name}: {r:?}");
    }

    /// Run `sparql` on `db` on another thread; `None` when it has not
    /// finished after `wait` (the thread is left running).
    fn run_within(
        mut db: Database,
        sparql: &'static str,
        cancel: Arc<AtomicBool>,
        wait: Duration,
    ) -> Option<(Result<QueryResult, String>, Duration)> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let t = Instant::now();
            let r = db
                .query_cancellable(sparql, cancel)
                .map_err(|e| e.to_string());
            let _ = tx.send((r, t.elapsed()));
        });
        rx.recv_timeout(wait).ok()
    }

    const LOOKUP_LOOP: &str = r#"(module
        (import "gstore" "term_id" (func $term_id (param i32 i32) (result i64)))
        (import "gstore" "arg_int" (func $arg_int (param i32 i32 i64) (result i64)))
        (memory (export "memory") 1024)
        (func (export "big") (result i32)
          (loop $l (drop (call $term_id (i32.const 0) (i32.const 67108864))) (br $l))
          (i32.const 1))
        (func (export "bigkey") (result i32)
          (loop $l (drop (call $arg_int (i32.const 0) (i32.const 67108864) (i64.const 0))) (br $l))
          (i32.const 1)))"#;

    fn lookup_db(tag: &str, limits: PfnLimits) -> Database {
        let reg = registry(tag);
        // One module exports both functions.
        for name in ["big", "bigkey"] {
            define(&reg, "root", name, LOOKUP_LOOP, "");
        }
        let mut db =
            Database::build_from_str(tag, "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
        db.set_pfn_caller(Some(PfnCaller::new(reg, "root")));
        db.set_pfn_limits(limits);
        db
    }

    /// Finding: a host call on a guest string cost ~1 fuel unit whatever its
    /// length, so neither the budget nor a cancel stopped a loop of 64 MiB
    /// lookups.
    #[test]
    fn host_calls_on_large_strings_are_charged_and_cancellable() {
        let budget = PfnLimits {
            fuel_per_call: 100_000,
            ..PfnLimits::default()
        };
        for f in ["big", "bigkey"] {
            let db = lookup_db("hostfuel", budget);
            let q: &'static str = if f == "big" {
                "SELECT (PFN.big({}) AS ?v) WHERE {}"
            } else {
                "SELECT (PFN.bigkey({}) AS ?v) WHERE {}"
            };
            let (r, took) = run_within(
                db,
                q,
                Arc::new(AtomicBool::new(false)),
                Duration::from_secs(20),
            )
            .unwrap_or_else(|| panic!("{f}: not stopped by the instruction budget"));
            let e = r.unwrap_err();
            assert!(e.contains("budget"), "{f}: {e}");
            assert!(took < Duration::from_secs(5), "{f}: {took:?}");
        }
        // Unlimited budget: the query's cancel flag stops it.
        let db = lookup_db(
            "hostcancel",
            PfnLimits {
                fuel_per_call: 0,
                ..PfnLimits::default()
            },
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            c2.store(true, Ordering::SeqCst);
        });
        let (r, took) = run_within(
            db,
            "SELECT (PFN.big({}) AS ?v) WHERE {}",
            cancel,
            Duration::from_secs(20),
        )
        .expect("not stopped by the cancel flag");
        assert!(r.unwrap_err().contains("cancel"));
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// Finding: every compile grew the shared engine for good; a module may
    /// now declare at most 10 000 functions (wasmi's strict limits).
    #[test]
    fn a_module_with_too_many_functions_is_refused() {
        let reg = registry("manyfuncs");
        let mut m = String::from(
            "(module (memory (export \"memory\") 1) (func (export \"f\") (result i32) (i32.const 1))",
        );
        for _ in 0..10_001 {
            m.push_str("(func)");
        }
        m.push(')');
        let info = PfnInfo {
            name: "f".into(),
            body: url_encode(&m),
            ..PfnInfo::default()
        };
        reg.create("root", info).unwrap();
        let msg = reg.compile("root", "f").unwrap().unwrap_err();
        assert!(msg.contains("invalid module"), "{msg}");
    }

    /// Finding: `emit_json_str` built the whole escaped copy (up to 6x the
    /// guest string) before checking the output bound.
    #[test]
    fn emit_json_str_checks_the_output_bound_first() {
        let reg = registry("jsonstr");
        define(
            &reg,
            "root",
            "huge",
            "(drop (memory.grow (i32.const 1023))) \
             (call $emit_json_str (i32.const 0) (i32.const 67108864)) (i32.const 1)",
            "",
        );
        define(
            &reg,
            "root",
            "small",
            "(call $emit_json_str (i32.const 0) (i32.const 4)) (i32.const 1)",
            r#"(data (i32.const 0) "a\01\"b")"#,
        );
        let mut db =
            Database::build_from_str("jsonstr", "<http://ex/a> <http://ex/p> <http://ex/b> .\n")
                .unwrap();
        db.set_pfn_caller(Some(PfnCaller::new(reg, "root")));
        db.set_gfun_max_output_bytes(1 << 20);
        let t = Instant::now();
        let e = db
            .query("SELECT (PFN.huge({}) AS ?v) WHERE {}")
            .unwrap_err()
            .to_string();
        assert!(e.contains("gfun_max_output_bytes"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        let rows = select_rows(db.query("SELECT (PFN.small({}) AS ?v) WHERE {}"));
        let cell = rows[0][0].clone().unwrap();
        let v = gstore::sparql_results::upstream_binding(&cell);
        assert_eq!(
            v.get("value").and_then(Json::as_str),
            Some(r#""a\u0001\"b""#)
        );
    }

    fn fam(tag: &str) -> (Database, Arc<PfnRegistry>) {
        let reg = registry(tag);
        let mut data = String::new();
        for i in 0..6 {
            data.push_str(&format!(
                "<http://ex/x{i}> <http://ex/type> <http://ex/T> .\n"
            ));
        }
        let db = Database::build_from_str(tag, &data).unwrap();
        (db, reg)
    }

    /// Finding: a lone PFN projection collapsed to one row; upstream keeps a
    /// row per solution, the value on the last.
    #[test]
    fn a_lone_pfn_projection_keeps_a_row_per_solution() {
        let (mut db, reg) = fam("lone");
        define(
            &reg,
            "root",
            "one",
            "(call $emit_i64 (i64.const 1)) (i32.const 1)",
            "",
        );
        db.set_pfn_caller(Some(PfnCaller::new(reg, "root")));
        let base =
            r#"SELECT (PFN.one({"src":"?x"}) AS ?v) WHERE { ?x <http://ex/type> <http://ex/T> }"#;
        let rows = select_rows(db.query(base));
        assert_eq!(rows.len(), 6);
        assert!(rows[..5].iter().all(|r| r[0].is_none()));
        assert!(
            rows[5][0].as_deref().unwrap().contains("[1,1,1,1,1,1]"),
            "{rows:?}"
        );
        let rows = select_rows(db.query(&format!("{base} OFFSET 1")));
        assert_eq!(rows.len(), 5);
        assert!(rows[4][0].is_some());
        let rows = select_rows(db.query(&base.replacen("SELECT", "SELECT DISTINCT", 1)));
        assert_eq!(rows.len(), 2);
        // No solution: one row, the call's output as is.
        let rows = select_rows(db.query(r#"SELECT (PFN.one({}) AS ?v) WHERE {}"#));
        assert_eq!(rows.len(), 1);
    }

    /// Findings: a failing PFN call in a sub-SELECT (missing, returning 0)
    /// read as "no solution".
    #[test]
    fn pfn_failures_in_sub_selects_are_errors() {
        let (mut db, reg) = fam("subselfail");
        define(
            &reg,
            "root",
            "ok",
            "(call $emit_i64 (i64.const 1)) (i32.const 1)",
            "",
        );
        define(&reg, "root", "fails", "(i32.const 0)", "");
        db.set_pfn_caller(Some(PfnCaller::new(reg, "root")));
        for f in ["missing", "fails"] {
            for q in [
                format!("SELECT * WHERE {{ {{ SELECT (PFN.{f}({{}}) AS ?v) WHERE {{}} }} }}"),
                format!(
                    "SELECT ?x WHERE {{ ?x ?p ?o FILTER NOT EXISTS {{ {{ SELECT (PFN.{f}({{}}) AS ?v) WHERE {{}} }} }} }}"
                ),
            ] {
                assert!(db.query(&q).is_err(), "{q}");
            }
        }
        let rows =
            select_rows(db.query("SELECT * WHERE { { SELECT (PFN.ok({}) AS ?v) WHERE {} } }"));
        assert_eq!(rows.len(), 1);
    }

    /// Finding: an UPDATE's WHERE clause ignored the request's PFN caller
    /// (the server's), so it matched nothing and reported success.
    #[test]
    fn an_update_calls_the_requesting_users_functions() {
        let (mut db, reg) = fam("updcaller");
        define(
            &reg,
            "alice",
            "f",
            "(call $emit_i64 (i64.const 7)) (i32.const 1)",
            "",
        );
        let caller = PfnCaller::new(reg, "alice");
        let r = db
            .query_as(
                "INSERT { <http://ex/s> <http://ex/q> ?v } WHERE { { SELECT (PFN.f({}) AS ?v) WHERE {} } }",
                None,
                Some(&caller),
            )
            .unwrap();
        assert!(matches!(r, QueryResult::Update { changed: 1 }), "{r:?}");
        let rows = select_rows(db.query("SELECT ?v WHERE { <http://ex/s> <http://ex/q> ?v }"));
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0][0].as_deref().unwrap().starts_with("\"7\""),
            "{rows:?}"
        );
        // Without a caller it is an error, not an empty match.
        assert!(db
            .query("INSERT { <http://ex/s> <http://ex/r> ?v } WHERE { { SELECT (PFN.f({}) AS ?v) WHERE {} } }")
            .is_err());
    }
}
