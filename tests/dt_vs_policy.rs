//! Data test (Phase 6 R4-VS): the VS-tree policy (`gstore::query::VsPolicy`).
//!
//! On every layout a VS-tree can serve — the in-memory build (in-memory
//! tree), and an on-disk database in Stream and in Materialize mode
//! (`vstree.kv`) — and for LUBM Q1–Q14 plus seeded random BGPs over a graph
//! whose predicates share signature bits:
//!
//! - `Auto` (the default) answers exactly what `Tree` (the tree for every
//!   subject variable, the behaviour before the policy) answers, rows in the
//!   same order; `Off` (no tree) gives the same answers (compared as
//!   multisets: a different candidate set may change the plan, hence the
//!   order of solutions without `ORDER BY`; with `ORDER BY` in order too);
//! - the policy's decisions are deterministic (the same counts for the same
//!   query), and `Auto` never reads the out-of-core tree's pages while
//!   `Tree` does;
//! - the two disk modes answer identically in order, and the in-memory
//!   build as a multiset;
//! - after deletes (the tree then holds stale bits) the answers stay equal
//!   under every policy.
//!
//! Policies are switched per thread with `scoped_vs_policy`; the query cache
//! is bypassed by a unique comment per statement.

use std::fs;
use std::path::{Path, PathBuf};

use gstore::query::{reset_vs_stats, scoped_vs_policy, vs_stats, VsPolicy, VsStats};
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult};

/// A temporary directory, removed on drop (also when an assertion fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d =
            std::env::temp_dir().join(format!("gstore_dt_vs_policy_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_queries() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    (1..=14)
        .map(|i| fs::read_to_string(dir.join(format!("lubm_q{i}.rq"))).unwrap())
        .collect()
}

/// xorshift64*: deterministic test data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const ENTITIES: usize = 80;
const LITERALS: usize = 12;
/// More than 100 predicates, so some share a signature bit (ids `p` and
/// `p + 100` encode to the same bit).
const PREDICATES: usize = 130;

fn entity(i: usize) -> String {
    format!("<http://ex/e{i}>")
}
fn literal(i: usize) -> String {
    format!("\"l{i}\"")
}
fn predicate(i: usize) -> String {
    format!("<http://ex/p{i}>")
}

/// A random graph as N-Triples: half the triples on a dozen common
/// predicates, a fifth literal objects, a few entities unused.
fn random_nt(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let mut out = String::new();
    for _ in 0..1500 {
        let s = entity(rng.below(ENTITIES - 5));
        let p = if rng.chance(50) {
            predicate(rng.below(PREDICATES))
        } else {
            predicate(rng.below(12))
        };
        let o = if rng.chance(20) {
            literal(rng.below(LITERALS))
        } else {
            entity(rng.below(ENTITIES - 5))
        };
        out.push_str(&format!("{s} {p} {o} .\n"));
    }
    out
}

/// A random BGP over `?a`–`?d` (predicate variables `?p`, `?q` apart),
/// sometimes ordered.
fn random_query(rng: &mut Rng) -> String {
    let var = |rng: &mut Rng| format!("?{}", ["a", "b", "c", "d"][rng.below(4)]);
    let mut body = String::new();
    for _ in 0..1 + rng.below(4) {
        let s = if rng.chance(75) {
            var(rng)
        } else {
            entity(rng.below(ENTITIES))
        };
        let p = if rng.chance(80) {
            let bound = if rng.chance(50) { 12 } else { PREDICATES };
            predicate(rng.below(bound))
        } else {
            ["?p", "?q"][rng.below(2)].to_string()
        };
        let o = match rng.below(20) {
            0..=9 => var(rng),
            10..=16 => entity(rng.below(ENTITIES)),
            _ => literal(rng.below(LITERALS)),
        };
        body.push_str(&format!("{s} {p} {o} . "));
    }
    if rng.chance(25) {
        format!("SELECT * WHERE {{ {body}}} ORDER BY ?a ?b ?c ?d ?p ?q")
    } else {
        format!("SELECT * WHERE {{ {body}}}")
    }
}

type Rows = Vec<Vec<Option<String>>>;

/// The rows of `q` under `policy`, with the decisions it took. A unique
/// comment bypasses the query cache, so every call evaluates.
fn run(db: &mut Database, q: &str, policy: VsPolicy) -> (Rows, VsStats) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _policy = scoped_vs_policy(policy);
    reset_vs_stats();
    let rows = match db.query(&format!("# dt_vs_policy {n}\n{q}")) {
        Ok(QueryResult::Select(rs)) => rs.rows,
        other => panic!("{q}: {other:?}"),
    };
    (rows, vs_stats())
}

fn sorted(mut rows: Rows) -> Rows {
    rows.sort();
    rows
}

/// Check the policy invariants for `q` on `db`; `disk` says whether its tree
/// is out of core. `Auto` equals `Tree` in order unless an out-of-core tree
/// holds stale bits (`exact` false: after deletes `Auto` tests the current
/// store, whose sets can be tighter than the stale tree's, so only the
/// answers are compared).
fn check(db: &mut Database, q: &str, disk: bool, exact: bool, decided: &mut VsStats) -> Rows {
    let (tree, st_tree) = run(db, q, VsPolicy::Tree);
    let (auto, st_auto) = run(db, q, VsPolicy::Auto);
    let (off, st_off) = run(db, q, VsPolicy::Off);
    if exact || !disk {
        assert_eq!(auto, tree, "Auto differs from Tree: {q}");
    } else {
        assert_eq!(
            sorted(auto.clone()),
            sorted(tree.clone()),
            "Auto answers differ: {q}"
        );
    }
    assert_eq!(
        sorted(off.clone()),
        sorted(tree.clone()),
        "Off answers differ: {q}"
    );
    if q.contains("ORDER BY") {
        assert_eq!(off, tree, "Off order differs under ORDER BY: {q}");
    }
    // Deterministic decisions.
    let (again, st_again) = run(db, q, VsPolicy::Auto);
    assert_eq!(
        (&again, st_again),
        (&auto, st_auto),
        "Auto not deterministic: {q}"
    );
    // Every decision is counted once per policy; Off never consults the tree.
    assert_eq!(st_tree.total(), st_auto.total(), "{q}");
    assert_eq!(st_off.total(), st_off.off, "{q}");
    assert_eq!(st_off.off, st_tree.total(), "{q}");
    if disk {
        assert_eq!(st_auto.tree_reads(), 0, "Auto read vstree.kv: {q}");
        assert_eq!(st_tree.tree_reads(), st_tree.total(), "{q}");
    }
    decided.skipped += st_auto.skipped;
    decided.store_checked += st_auto.store_checked;
    decided.listed += st_auto.listed;
    decided.probed += st_auto.probed;
    decided.walked += st_auto.walked;
    decided.seeded += st_auto.seeded;
    auto
}

fn open(dir: &Path, mode: DiskQueryMode) -> Database {
    let db = Database::open_with(dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap();
    assert_eq!(db.disk_query_mode(), Some(mode));
    db
}

/// The three databases over `sources`: in memory, and one on-disk directory
/// opened in Stream and in Materialize mode, each with a usable VS-tree.
fn databases(tmp: &TempDir, sources: &[PathBuf]) -> (Database, Database, Database) {
    let dir = tmp.0.join("db");
    Database::build_disk(&dir, sources).unwrap();
    let mut mem = Database::build_from_files("vs", sources).unwrap();
    if !mem.stats().index_valid {
        mem.rebuild_index();
    }
    let stream = open(&dir, DiskQueryMode::Stream);
    let mat = open(&dir, DiskQueryMode::Materialize);
    for db in [&mem, &stream, &mat] {
        assert!(db.stats().index_valid, "the VS-tree is usable");
    }
    (mem, stream, mat)
}

fn queries(seed: u64) -> Vec<String> {
    let mut rng = Rng(seed);
    (0..120).map(|_| random_query(&mut rng)).collect()
}

#[test]
fn every_policy_answers_identically_on_every_layout() {
    let tmp = TempDir::new("layouts");
    let rand_nt = tmp.0.join("random.nt");
    fs::write(&rand_nt, random_nt(17)).unwrap();
    let (mut mem, mut stream, mut mat) = databases(&tmp, &[lubm_nt(), rand_nt]);

    let mut all = lubm_queries();
    all.extend(queries(5));
    let (mut d_mem, mut d_stream, mut d_mat) =
        (VsStats::default(), VsStats::default(), VsStats::default());
    let mut answered = 0;
    for q in &all {
        let m = check(&mut mem, q, false, true, &mut d_mem);
        let s = check(&mut stream, q, true, true, &mut d_stream);
        let t = check(&mut mat, q, true, true, &mut d_mat);
        // The two disk modes in order; the in-memory build as a multiset
        // (and the same answers under every policy, checked above).
        if !m.is_empty() {
            answered += 1;
        }
        assert_eq!(s, t, "Stream and Materialize differ: {q}");
        assert_eq!(sorted(s), sorted(m), "memory differs: {q}");
    }
    assert!(answered > 30, "many queries answer something ({answered})");
    // Every kind of decision was taken: skipped and store-checked or listed
    // sets on disk, skipped and probed / walked sets in memory.
    assert!(d_stream.skipped > 0 && d_stream.store_checked > 0 && d_stream.listed > 0);
    assert_eq!(d_stream, d_mat, "both disk modes decide alike");
    assert!(d_mem.skipped > 0 && d_mem.probed + d_mem.walked + d_mem.seeded > 0);
    assert_eq!(
        d_mem.store_checked + d_mem.listed,
        0,
        "an in-memory tree is probed"
    );
}

#[test]
fn answers_stay_equal_after_deletes() {
    let tmp = TempDir::new("deletes");
    let rand_nt = tmp.0.join("random.nt");
    let nt = random_nt(23);
    fs::write(&rand_nt, &nt).unwrap();
    let (mut mem, mut stream, mut mat) = databases(&tmp, &[rand_nt]);
    // Delete a third of the triples, in batches, from each database; the
    // trees keep their (now stale) entries and stay in use.
    let mut rng = Rng(99);
    let gone: Vec<&str> = nt.lines().filter(|_| rng.chance(33)).collect();
    for chunk in gone.chunks(50) {
        let update = format!("DELETE DATA {{ {} }}", chunk.join("\n"));
        for db in [&mut mem, &mut stream, &mut mat] {
            db.query(&update).unwrap();
        }
    }
    for db in [&mem, &stream, &mat] {
        assert!(db.stats().index_valid, "deletes keep the VS-tree in use");
        assert!(db.vs_shrink_debt() > 0);
    }
    let mut decided = VsStats::default();
    for q in queries(29) {
        let m = check(&mut mem, &q, false, false, &mut decided);
        let s = check(&mut stream, &q, true, false, &mut decided);
        let t = check(&mut mat, &q, true, false, &mut decided);
        assert_eq!(s, t, "Stream and Materialize differ: {q}");
        assert_eq!(sorted(s), sorted(m), "memory differs: {q}");
    }
}
