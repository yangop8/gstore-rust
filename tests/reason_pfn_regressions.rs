//! Regression tests of the reasoning and PFN review findings (review ext#14
//! asks for them):
//!
//! * RDFS materialization never makes a literal or blank node a predicate,
//!   and a materialized database survives `gexport` → `gbuild` (ext#3);
//! * the textual rule parser refuses non-ASCII garbage instead of
//!   panicking (ext#4), and its join scales linearly (ext#7);
//! * a rule directory written by a backup is all-or-nothing when a write
//!   fails part-way — a real `EFBIG` from a child process's
//!   `RLIMIT_FSIZE` (durability#11);
//! * PFN (feature `pfn-wasm`): a function larger than a fuel slice runs
//!   (ext#2), instantiation is charged and one query's calls share a budget
//!   (ext#1), and compiling builds what `data.json` records (ext#8).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gstore::reason::store::RuleStore;
use gstore::{Database, QueryResult};

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_reason_pfn_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The export of `db`, sorted. `named` keeps blank-node labels; otherwise
/// every `_:label` reads `_:` (a loader may relabel blank nodes).
fn export_with(db: &Database, named: bool) -> Vec<String> {
    let mut out = Vec::new();
    db.export_ntriples(&mut out).unwrap();
    let mut lines: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| {
            if named {
                return l.to_string();
            }
            l.split(' ')
                .map(|t| if t.starts_with("_:") { "_:" } else { t })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    lines.sort();
    lines
}

fn export(db: &Database) -> Vec<String> {
    export_with(db, false)
}

fn rows(r: gstore::Result<QueryResult>) -> Vec<Vec<Option<String>>> {
    match r.unwrap() {
        QueryResult::Select(rs) => rs.rows,
        other => panic!("not a SELECT result: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// RDFS (ext#3)
// ---------------------------------------------------------------------------

const SUBPROP: &str = "<http://www.w3.org/2000/01/rdf-schema#subPropertyOf>";

/// Legal data with a literal and a blank node where a property is expected
/// (OWL writes `:p2 rdfs:subPropertyOf [ owl:inverseOf :q ]`).
fn odd_schema() -> String {
    format!(
        "<http://ex/p> {SUBPROP} \"lit\" .\n\
         <http://ex/p2> {SUBPROP} _:b .\n\
         _:b {SUBPROP} <http://ex/q> .\n\
         <http://ex/a> <http://ex/p> <http://ex/b> .\n\
         <http://ex/a> <http://ex/p2> <http://ex/c> .\n"
    )
}

/// Review ext#3: materialization interned `"lit"` and `_:b` as predicates and
/// copied data under them (`<a> "lit" <b> .`), which no reader loads back.
#[test]
fn rdfs_materialization_never_creates_a_non_iri_predicate() {
    let mut db = Database::build_from_str("rdfs_odd", &odd_schema()).unwrap();
    let added = db.materialize_rdfs();
    // p2 ⊑ _:b ⊑ q: `p2 ⊑ q` and `a q c`.
    assert_eq!(added, 2);
    let preds = rows(db.query("SELECT DISTINCT ?p WHERE { ?s ?p ?o }"));
    for p in &preds {
        let p = p[0].as_deref().unwrap();
        assert!(p.starts_with('<') && p.ends_with('>'), "predicate {p}");
    }
    assert_eq!(
        rows(db.query("SELECT ?o WHERE { <http://ex/a> <http://ex/q> ?o }")),
        [[Some("<http://ex/c>".to_string())]]
    );
    // The export reads back, statement for statement.
    let text = export_with(&db, true).join("\n") + "\n";
    let again = Database::build_from_str("rdfs_odd_again", &text)
        .unwrap_or_else(|e| panic!("the export does not load back: {e}\n{text}"));
    assert_eq!(export(&again), export(&db));
}

/// The acceptance check of ext#3 with the tools: a saved, RDFS-materialized
/// database goes through `gexport` and `gbuild` without a statement lost
/// (`gbuild` exits 3 when it skipped any).
#[test]
fn an_rdfs_materialized_database_round_trips_through_gexport_and_gbuild() {
    let work = TmpDir::new("roundtrip");
    let mut db = Database::build_from_str("m", &odd_schema()).unwrap();
    assert!(db.materialize_rdfs() > 0);
    let before = export(&db);
    db.save(work.path("m.db")).unwrap();
    drop(db);
    let nt = work.path("m.nt");
    let out = Command::new(env!("CARGO_BIN_EXE_gexport"))
        .arg(work.path("m"))
        .arg(&nt)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "gexport: {out:?}");
    let out = Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .arg(work.path("n"))
        .arg(&nt)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "gbuild skipped statements or failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let rebuilt = Database::open(work.path("n.db")).unwrap();
    assert_eq!(export(&rebuilt), before);
}

// ---------------------------------------------------------------------------
// Textual rules (ext#4, ext#7)
// ---------------------------------------------------------------------------

/// Review ext#4: `Database::add_rule` panicked on non-ASCII input ("end byte
/// index 1 is not a char boundary"); it returns an error.
#[test]
fn textual_rules_refuse_non_ascii_garbage_without_panicking() {
    let mut db = Database::new("rules_utf8");
    for bad in [
        "é <p> ?y => ?x <q> ?y",
        "?x <p> \"\\é\" => ?x <q> ?x",
        "?x <p> ?y => ?x <q> \"\\😀\"",
        "?x <p> ?y => ?x <q> ?y 😀",
        "?x <p> ?y => ?x <q> \"x\"@😀",
    ] {
        assert!(db.add_rule("bad", bad).is_err(), "{bad}");
    }
    assert!(db.list_rules().is_empty());
    db.add_rule("ok", "?x <http://ex/p> \"é\" => ?x <http://ex/q> \"😀\"")
        .unwrap();
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> \"é\" }")
        .unwrap();
    assert_eq!(db.run_rules(), 1);
    assert_eq!(
        rows(db.query("SELECT ?o WHERE { <http://ex/a> <http://ex/q> ?o }")),
        [[Some("\"😀\"".to_string())]]
    );
}

/// A chain `n0 → n1 → … → n{n}` under `<http://ex/p>`.
fn chain(n: usize) -> String {
    (0..n)
        .map(|i| format!("<http://ex/n{i}> <http://ex/p> <http://ex/n{}> .\n", i + 1))
        .collect()
}

/// The fastest of `tries` timed runs of a two-hop rule over a chain of `n`
/// edges.
fn two_hop(n: usize, tries: usize) -> Duration {
    (0..tries)
        .map(|_| {
            let mut db = Database::build_from_str("chain", &chain(n)).unwrap();
            db.add_rule(
                "two",
                "?x <http://ex/p> ?y . ?y <http://ex/p> ?z => ?x <http://ex/q> ?z",
            )
            .unwrap();
            let t = Instant::now();
            assert_eq!(db.run_rules(), n - 1);
            t.elapsed()
        })
        .min()
        .unwrap()
}

/// Review ext#7: the textual rules joined with a nested loop that compared
/// every binding with every stored pair (10.8 / 41.3 / 149.7 ms for 500 /
/// 1000 / 2000 edges). Eight times the data takes about eight to ten times
/// as long now (measured 2.1 ms -> 20 ms), not sixty-four (the nested loop:
/// 53 ms -> 2.6 s). The bound sits between the two; the work counter's
/// version of this check is the unit test
/// `reason::tests::rule_joins_scale_linearly`.
#[test]
fn textual_rule_joins_scale_linearly() {
    let small = two_hop(1000, 5);
    let large = two_hop(8000, 5);
    assert!(
        large < small * 24,
        "8x the edges: {small:?} -> {large:?} (a quadratic join takes 64x)"
    );
}

// ---------------------------------------------------------------------------
// Rule directories under a failing write (durability#11)
// ---------------------------------------------------------------------------

/// The file-size limit and the signal it raises (Linux, 64-bit `rlim_t`).
mod rlimit {
    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }

    extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
        fn signal(sig: i32, handler: usize) -> usize;
    }

    const RLIMIT_FSIZE: i32 = 1;
    const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;

    /// Writes past `bytes` per file fail with `EFBIG` (`SIGXFSZ` ignored).
    pub fn limit_file_size(bytes: u64) {
        let mut cur = RLimit { cur: 0, max: 0 };
        // SAFETY: plain syscall wrappers on a properly sized struct, and the
        // predefined SIG_IGN disposition.
        unsafe {
            assert_ne!(signal(SIGXFSZ, SIG_IGN), usize::MAX);
            assert_eq!(getrlimit(RLIMIT_FSIZE, &mut cur), 0);
            let new = RLimit {
                cur: bytes.min(cur.max),
                max: cur.max,
            };
            assert_eq!(setrlimit(RLIMIT_FSIZE, &new), 0);
        }
    }
}

const CHILD_SCENARIO: &str = "GSTORE_RULE_DIR_CHILD";
const CHILD_ROOT: &str = "GSTORE_RULE_DIR_ROOT";
const CHILD_OK: &str = "rule-dir child: done";
/// The file-size limit of the child: above every rule file, below the big
/// ledger.
const LIMIT: u64 = 64 * 1024;

/// A database directory with rules `names`, each with a ledger; `big`'s
/// ledger is larger than [`LIMIT`].
fn db_with_rules(root: &Path, db: &str, names: &[&str], big: Option<&str>) -> PathBuf {
    let dir = root.join(db);
    fs::create_dir_all(&dir).unwrap();
    let mut store = RuleStore::dir(&dir);
    for &n in names {
        assert!(store
            .create(n, &format!("{{\"rulename\":\"{n}\"}}"))
            .unwrap());
        let lines = if big == Some(n) { 5_000 } else { 1 };
        let ledger = (0..lines)
            .map(|i| format!("<http://ex/{n}/{i}> <Rule:{n}> <http://ex/o> ."))
            .collect();
        store.set_ledger(n, Some(&ledger)).unwrap();
    }
    dir
}

/// The rules a database directory shows, each with whether it has its
/// ledger — read under the rule lock, as every rule operation reads them.
fn shown(db_dir: &Path) -> Vec<(String, bool)> {
    let store = RuleStore::dir(db_dir);
    let _lock = store.lock();
    store
        .names()
        .unwrap()
        .into_iter()
        .map(|n| {
            let has = store.ledger(&n).unwrap().is_some();
            (n, has)
        })
        .collect()
}

/// The child: copies the rules of `src.db` into `dst.db` under the file-size
/// limit, so the big ledger's copy fails with `EFBIG` part-way.
#[test]
#[ignore = "run by a_rule_directory_write_that_fails_part_way_changes_nothing"]
fn child_rule_dir_write() {
    let Ok(scenario) = std::env::var(CHILD_SCENARIO) else {
        return;
    };
    let root = PathBuf::from(std::env::var(CHILD_ROOT).unwrap());
    let (src, dst) = (root.join("src.db"), root.join("dst.db"));
    let rules = gstore::reason::store::read_rule_dir(&src).unwrap();
    rlimit::limit_file_size(LIMIT);
    let r = match scenario.as_str() {
        "copy" => gstore::reason::store::copy_rule_dir(&src, &dst),
        "memory" => gstore::reason::store::write_memory_rules(&rules, &dst),
        other => panic!("unknown scenario {other}"),
    };
    let e = r.expect_err("the big ledger is larger than the file-size limit");
    println!("{CHILD_OK}: {e}");
}

/// Review durability#11: a backup's rule copy (and the save of a detached
/// database's in-memory rules) removed the destination's rules and wrote the
/// new ones file by file, so a write failing part-way (a full disk; a crash
/// there leaves the same) lost the old rules and left rule files without
/// their ledgers. Staged and swapped in as a whole, a failed write leaves
/// the destination as it was.
#[test]
fn a_rule_directory_write_that_fails_part_way_changes_nothing() {
    for scenario in ["copy", "memory"] {
        let root = TmpDir::new(scenario);
        // `a` sorts first: its rule file is copied before the ledgers.
        db_with_rules(&root.0, "src.db", &["a", "big"], Some("big"));
        let dst = db_with_rules(&root.0, "dst.db", &["old"], None);
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "child_rule_dir_write",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_SCENARIO, scenario)
            .env(CHILD_ROOT, &root.0)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains(CHILD_OK),
            "{scenario}: the child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            shown(&dst),
            [("old".to_string(), true)],
            "{scenario}: the failed write changed the destination ({stdout})"
        );
        // Without the limit the same write goes through.
        let src = root.path("src.db");
        match scenario {
            "copy" => gstore::reason::store::copy_rule_dir(&src, &dst).unwrap(),
            _ => {
                let rules = gstore::reason::store::read_rule_dir(&src).unwrap();
                gstore::reason::store::write_memory_rules(&rules, &dst).unwrap();
            }
        }
        assert_eq!(
            shown(&dst),
            [("a".to_string(), true), ("big".to_string(), true)]
        );
    }
}

// ---------------------------------------------------------------------------
// PFN (ext#1, ext#2, ext#8)
// ---------------------------------------------------------------------------

#[cfg(feature = "pfn-wasm")]
mod pfn {
    use std::sync::Arc;

    use gstore::pfn::{url_encode, PfnCaller, PfnInfo, PfnLimits, PfnRegistry};
    use gstore::{ConcurrentDb, Database, QueryResult};

    use super::TmpDir;

    /// A database of `subjects` subjects with a registry user `root` calls.
    fn embedded(tag: &str, subjects: usize) -> (Database, Arc<PfnRegistry>, TmpDir) {
        let base = TmpDir::new(tag);
        let reg = Arc::new(PfnRegistry::new(&base.0));
        let mut db = Database::build_from_str(tag, &data(subjects)).unwrap();
        db.set_pfn_caller(Some(PfnCaller::new(Arc::clone(&reg), "root")));
        (db, reg, base)
    }

    /// `subjects` subjects `<s{i}> <p> <o>`; the first ten are also
    /// `<s{i}> <t> <T>`.
    fn data(subjects: usize) -> String {
        (0..subjects.max(1))
            .map(|i| {
                let mut t = format!("<http://ex/s{i}> <http://ex/p> <http://ex/o> .\n");
                if i < 10 {
                    t.push_str(&format!("<http://ex/s{i}> <http://ex/t> <http://ex/T> .\n"));
                }
                t
            })
            .collect()
    }

    fn define(reg: &PfnRegistry, name: &str, subs: &str, body: &str) {
        let info = PfnInfo {
            name: name.into(),
            body: url_encode(body),
            subs: url_encode(subs),
            ..PfnInfo::default()
        };
        match reg.get("root", name).unwrap() {
            Some(_) => reg.update("root", info).unwrap(),
            None => reg.create("root", info).unwrap(),
        };
        let r = reg.compile("root", name).unwrap();
        assert!(r.is_ok(), "compile {name}: {r:?}");
    }

    /// The value of a lone `PFN.f({})` over no solutions.
    fn value(r: gstore::Result<QueryResult>) -> String {
        match r.unwrap() {
            QueryResult::Select(rs) => {
                let v = rs.rows[0][0].clone().unwrap();
                v.trim_matches('"').to_string()
            }
            other => panic!("not a SELECT result: {other:?}"),
        }
    }

    const EMIT_ONE: &str = "(call $emit_i64 (i64.const 1)) (i32.const 1)";

    /// Review ext#2: wasmi translated a function at its first call and
    /// charged 7 fuel per byte for it, reporting a shortfall of the current
    /// fuel slice as an error: every call of a function body above about
    /// 150 KB failed ("ran out of fuel while calling a resumable function:
    /// required_fuel=1260028"), also with an unlimited budget. Translation is
    /// eager now, at compile and load.
    #[test]
    fn a_function_larger_than_a_fuel_slice_runs() {
        let (mut db, reg, _base) = embedded("pfn_big", 1);
        let filler = "i32.const 1 drop ".repeat(60_000);
        let module = format!(
            "(module (import \"gstore\" \"emit_i64\" (func $emit_i64 (param i64))) \
             (memory (export \"memory\") 1) \
             (func (export \"bigf\") (result i32) {filler} {EMIT_ONE}))"
        );
        assert!(module.len() > 1_000_000);
        define(&reg, "bigf", "", &module);
        // The same body in a helper called from a small entry.
        define(
            &reg,
            "helperf",
            &format!("(func $big {filler})"),
            &format!("(call $big) {EMIT_ONE}"),
        );
        for f in ["bigf", "helperf"] {
            let q = format!("SELECT (PFN.{f}({{}}) AS ?v) WHERE {{}}");
            assert_eq!(value(db.query(&q)), "1", "{f}");
            assert_eq!(value(db.query(&q)), "1", "{f}, again");
        }
        db.set_pfn_limits(PfnLimits {
            fuel_per_call: 0,
            ..PfnLimits::default()
        });
        assert_eq!(
            value(db.query("SELECT (PFN.bigf({}) AS ?v) WHERE {}")),
            "1",
            "unlimited"
        );
    }

    /// Review ext#2 (the verifier's wider case): a 9 KB helper first called
    /// after a loop failed or not depending on how much of the fuel slice
    /// the loop had used (7 of 99 loop lengths failed, and stayed failing).
    /// Every loop length works now.
    #[test]
    fn a_helper_first_called_late_in_a_fuel_slice_runs() {
        let (mut db, reg, _base) = embedded("pfn_late", 1);
        let helper = format!("(func $h {})", "i32.const 1 drop ".repeat(3_000));
        let mut failed = Vec::new();
        for n in (6_000..=720_000).step_by(6_000) {
            let body = format!(
                "(local $i i64) (local.set $i (i64.const {n})) \
                 (block $done (loop $l (br_if $done (i64.eqz (local.get $i))) \
                 (local.set $i (i64.sub (local.get $i) (i64.const 1))) (br $l))) \
                 (call $h) {EMIT_ONE}"
            );
            // A new module for every length: nothing is translated yet.
            define(&reg, "late", &helper, &body);
            if let Err(e) = db.query("SELECT (PFN.late({}) AS ?v) WHERE {}") {
                failed.push(format!("n={n}: {e}"));
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// A module declaring `pages` pages of initial memory, emitting 1.
    fn memory_module(name: &str, pages: u32) -> String {
        format!(
            "(module (import \"gstore\" \"emit_i64\" (func $emit_i64 (param i64))) \
             (memory (export \"memory\") {pages}) \
             (func (export \"{name}\") (result i32) {EMIT_ONE}))"
        )
    }

    /// Review ext#1: instantiating a call's module was free — a function
    /// declaring 64 MiB of memory ran under a budget of 100 instructions
    /// (35 ms per call for a handful of fuel). Its instantiation is charged:
    /// 64 MiB at 8 bytes per unit is 8 Mi units.
    #[test]
    fn instantiation_is_charged_against_the_call_budget() {
        let (mut db, reg, _base) = embedded("pfn_inst", 1);
        define(&reg, "mem", "", &memory_module("mem", 1024));
        define(&reg, "small", "", &memory_module("small", 1));
        db.set_pfn_limits(PfnLimits {
            fuel_per_call: 100_000,
            ..PfnLimits::default()
        });
        let e = db
            .query("SELECT (PFN.mem({}) AS ?v) WHERE {}")
            .unwrap_err()
            .to_string();
        assert!(e.contains("pfn_fuel_per_call"), "{e}");
        // One page is 8 Ki units: well within the budget.
        assert_eq!(
            value(db.query("SELECT (PFN.small({}) AS ?v) WHERE {}")),
            "1"
        );
        db.set_pfn_limits(PfnLimits {
            fuel_per_call: 9 << 20,
            ..PfnLimits::default()
        });
        assert_eq!(value(db.query("SELECT (PFN.mem({}) AS ?v) WHERE {}")), "1");
    }

    /// A function spinning 10 000 loop iterations, emitting 1.
    fn define_spin(reg: &PfnRegistry) {
        define(
            reg,
            "spin",
            "",
            "(local $i i64) (local.set $i (i64.const 10000)) \
             (block $done (loop $l (br_if $done (i64.eqz (local.get $i))) \
             (local.set $i (i64.sub (local.get $i) (i64.const 1))) (br $l))) \
             (call $emit_i64 (i64.const 1)) (i32.const 1)",
        );
    }

    /// The fuel ten calls of `spin` use, run directly under one query's
    /// caller (no per-query budget).
    fn fuel_of_ten() -> u64 {
        let (db, reg, _base) = embedded("pfn_measure", 10);
        define_spin(&reg);
        let caller = PfnCaller::new(Arc::clone(&reg), "root").with_limits(PfnLimits {
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

    /// Review ext#1: nothing summed the fuel of a query's calls, so one query
    /// could run a million calls of a billion instructions each. The calls of
    /// one query share `pfn_fuel_per_query`; every query has its own.
    #[test]
    fn the_calls_of_one_query_share_a_budget() {
        let per_ten = fuel_of_ten();
        assert!(per_ten > 10 * 10_000, "{per_ten}");
        let (mut db, reg, _base) = embedded("pfn_query", 200);
        define_spin(&reg);
        let budget = 3 * per_ten;
        db.set_pfn_limits(PfnLimits {
            fuel_per_query: budget,
            ..PfnLimits::default()
        });
        let all = "SELECT (PFN.spin({\"x\":\"?s\"}) AS ?v) WHERE { ?s <http://ex/p> ?o }";
        let e = db.query(all).unwrap_err().to_string();
        assert!(e.contains("pfn_fuel_per_query"), "{e}");
        // Ten solutions fit; five such queries in a row too (each query
        // counts from zero), together more than the budget.
        let ten =
            "SELECT (PFN.spin({\"x\":\"?s\"}) AS ?v) WHERE { ?s <http://ex/t> <http://ex/T> }";
        for _ in 0..5 {
            db.query(ten).unwrap();
        }
        // The ConcurrentDb snapshots and version views: a budget per query too.
        let c = ConcurrentDb::new(db);
        let snap = c.snapshot();
        for _ in 0..5 {
            c.query(ten).unwrap();
            snap.query(ten).unwrap();
            c.version_view().query(ten).unwrap();
        }
        assert!(snap
            .query(all)
            .unwrap_err()
            .to_string()
            .contains("pfn_fuel_per_query"));
        // 0 = unlimited.
        let (mut db, reg, _base) = embedded("pfn_query_unlimited", 200);
        define_spin(&reg);
        db.set_pfn_limits(PfnLimits {
            fuel_per_query: 0,
            ..PfnLimits::default()
        });
        db.query(all).unwrap();
    }
}
