//! `pfn_fuel_per_query` bounds the PFN work of one request (round-4 review
//! of ext#1's fix).
//!
//! Every evaluator of a request installed its PFN caller with a fresh fuel
//! count, and an UPDATE builds one evaluator per `DELETE` / `INSERT … WHERE`
//! operation: a request of N operations could use N times the budget. The
//! request's entry now starts one count, which every operation shares; each
//! request — also one reusing the caller a library passes — still starts
//! from zero.

#![cfg(feature = "pfn-wasm")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gstore::pfn::{url_encode, PfnCaller, PfnInfo, PfnLimits, PfnRegistry};
use gstore::{Database, QueryResult};

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_pfn_request_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Ten subjects `<s{i}> <t> <T>` and a registry whose user `root` owns
/// `spin` (10 000 loop iterations, emitting 1), installed as the
/// database's caller.
fn embedded(tag: &str) -> (Database, Arc<PfnRegistry>, TmpDir) {
    let base = TmpDir::new(tag);
    let reg = Arc::new(PfnRegistry::new(&base.0));
    let data: String = (0..10)
        .map(|i| format!("<http://ex/s{i}> <http://ex/t> <http://ex/T> .\n"))
        .collect();
    let mut db = Database::build_from_str(tag, &data).unwrap();
    let info = PfnInfo {
        name: "spin".into(),
        body: url_encode(
            "(local $i i64) (local.set $i (i64.const 10000)) \
             (block $done (loop $l (br_if $done (i64.eqz (local.get $i))) \
             (local.set $i (i64.sub (local.get $i) (i64.const 1))) (br $l))) \
             (call $emit_i64 (i64.const 1)) (i32.const 1)",
        ),
        ..PfnInfo::default()
    };
    reg.create("root", info).unwrap();
    assert!(reg.compile("root", "spin").unwrap().is_ok());
    db.set_pfn_caller(Some(PfnCaller::new(Arc::clone(&reg), "root")));
    (db, reg, base)
}

/// The fuel of one operation below: ten calls of `spin`, run directly
/// under one caller (no per-query budget).
fn fuel_of_ten(db: &Database, reg: &Arc<PfnRegistry>) -> u64 {
    let caller = PfnCaller::new(Arc::clone(reg), "root").with_limits(PfnLimits {
        fuel_per_query: 0,
        ..PfnLimits::default()
    });
    let f = caller.resolve("spin").unwrap();
    let csr = db.csr().unwrap();
    let graph = gstore::pfn::CsrGraphCtx::new(&csr, db.dict());
    for i in 0..10 {
        let args = format!("{{\"x\":\"<http://ex/s{i}>\"}}");
        caller.run(&f, &args, &graph, None, 1 << 20).unwrap();
    }
    caller.fuel_spent()
}

const SELECT_TEN: &str =
    "SELECT (PFN.spin({\"x\":\"?s\"}) AS ?v) WHERE { ?s <http://ex/t> <http://ex/T> }";

/// One `INSERT … WHERE` operation calling `spin` once per subject (ten
/// calls), writing what it emits under `<p>`.
fn op(p: &str) -> String {
    format!(
        "INSERT {{ <http://ex/r> <http://ex/{p}> ?v }} WHERE {{ SELECT \
         (PFN.spin({{\"x\":\"?s\"}}) AS ?v) WHERE {{ ?s <http://ex/t> <http://ex/T> }} }}"
    )
}

fn changed(r: gstore::Result<QueryResult>) -> gstore::Result<usize> {
    match r? {
        QueryResult::Update { changed } => Ok(changed),
        other => panic!("not an update result: {other:?}"),
    }
}

#[test]
fn the_operations_of_one_update_share_the_budget() {
    let (mut db, reg, _base) = embedded("ops");
    let ten = fuel_of_ten(&db, &reg);
    assert!(ten > 10 * 10_000, "{ten}");
    // One operation fits; two do not.
    db.set_pfn_limits(PfnLimits {
        fuel_per_query: ten + ten / 2,
        ..PfnLimits::default()
    });
    assert!(changed(db.query(&op("one"))).unwrap() > 0);
    let two = format!("{} ; {}", op("a"), op("b"));
    let e = changed(db.query(&two)).unwrap_err().to_string();
    assert!(e.contains("pfn_fuel_per_query"), "{e}");
    // The request's caller passed by a library: the same.
    let caller = PfnCaller::new(Arc::clone(&reg), "root");
    let e = changed(db.query_as(&two, None, Some(&caller)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("pfn_fuel_per_query"), "{e}");

    // Every request starts from zero: one-operation updates and reads in a
    // row, together far past the budget, each fit — also with one caller
    // passed to all of them.
    for i in 0..3 {
        assert!(changed(db.query(&op(&format!("r{i}")))).unwrap() > 0);
        assert!(changed(db.query_as(&op(&format!("c{i}")), None, Some(&caller))).unwrap() > 0);
        db.query(SELECT_TEN).unwrap();
        db.query_as(SELECT_TEN, None, Some(&caller)).unwrap();
    }

    // With room for both, a two-operation request runs.
    db.set_pfn_limits(PfnLimits {
        fuel_per_query: 3 * ten,
        ..PfnLimits::default()
    });
    let two = format!("{} ; {}", op("x"), op("y"));
    assert!(changed(db.query(&two)).unwrap() > 1);
}
