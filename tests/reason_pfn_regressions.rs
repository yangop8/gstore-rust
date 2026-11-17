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
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
