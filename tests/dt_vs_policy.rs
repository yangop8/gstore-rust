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
//! The perturbation battery (review gap-accel-soundness#5) adds, on every
//! layout and under every policy:
//!
//! - insert churn, id reuse (`reclaim_unused`, then new terms take the
//!   freed ids), rolled-back transactions and restarts, each checked against
//!   an oracle database built afresh from the surviving triples;
//! - the substring pushdown (`CONTAINS` / `STRSTARTS` / `STRENDS` / plain
//!   `REGEX`) over literals of every datatype family, against the same filter
//!   with `|| false` appended (which no index can serve);
//! - `ORDER BY … LIMIT` over extreme numeric keys under every top-k setting
//!   (DP-B off / auto / always, the bounded heap, nothing), in order against
//!   the full evaluation, and `LIMIT` / `OFFSET` pages that tile the full
//!   ordered answer;
//! - a per-predicate counter forced to its maximum (review
//!   gap-accel-soundness#4): it saturates, and `Auto` still answers.
//!
//! Policies are switched per thread with `scoped_vs_policy`; the query cache
//! is bypassed by a unique comment per statement.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use gstore::kvstore::DiskStore;
use gstore::query::engine::{DpbMode, TopKOptions};
use gstore::query::{reset_vs_stats, scoped_vs_policy, vs_stats, VsPolicy, VsStats};
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult, Term};

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

// ---- the perturbation battery (review gap-accel-soundness#5) -------------

/// Open `dir` in `mode`, rebuilding a stale `vstree.kv` (so a restart puts
/// an out-of-core tree back in use in Stream mode too).
fn reopen(dir: &Path, mode: DiskQueryMode) -> Database {
    let opts = OpenOptions::new()
        .with_disk_query_mode(mode)
        .with_vstree_on_open(true);
    let db = Database::open_with(dir, &opts).unwrap();
    assert_eq!(db.disk_query_mode(), Some(mode));
    db
}

/// How a layout of the battery is restarted.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// In memory, restarted through a snapshot save and load.
    Memory,
    /// An on-disk directory opened in this mode.
    Disk(DiskQueryMode),
}

/// One database of the battery.
struct Layout {
    kind: Kind,
    dir: PathBuf,
    db: Option<Database>,
}

impl Layout {
    fn db(&mut self) -> &mut Database {
        self.db.as_mut().unwrap()
    }

    /// Whether its VS-tree, if in use, is out of core.
    fn disk(&self) -> bool {
        matches!(self.kind, Kind::Disk(_))
    }

    /// Close and reopen it (the in-memory one through a snapshot).
    fn restart(&mut self) {
        let db = self.db.take().unwrap();
        match self.kind {
            Kind::Memory => {
                db.save(&self.dir).unwrap();
                drop(db);
                let mut db = Database::open(&self.dir).unwrap();
                if !db.stats().index_valid {
                    db.rebuild_index();
                }
                self.db = Some(db);
            }
            Kind::Disk(mode) => {
                drop(db);
                self.db = Some(reopen(&self.dir, mode));
            }
        }
        assert!(
            self.db().stats().index_valid,
            "{:?}: the VS-tree is back in use after a restart",
            self.kind
        );
    }

    fn update(&mut self, sparql: &str) {
        if let Err(e) = self.db().query(sparql) {
            panic!("{:?}: {sparql}: {e}", self.kind);
        }
    }
}

/// The three layouts over `text` (N-Triples): in memory, and two on-disk
/// directories (one per mode, so their writes never meet), each with a
/// usable VS-tree.
fn layouts(tmp: &TempDir, tag: &str, text: &str) -> Vec<Layout> {
    let nt = tmp.0.join(format!("{tag}.nt"));
    fs::write(&nt, text).unwrap();
    let mut mem = Database::build_from_files("vs", &[&nt]).unwrap();
    if !mem.stats().index_valid {
        mem.rebuild_index();
    }
    let mut out = vec![Layout {
        kind: Kind::Memory,
        dir: tmp.0.join(format!("{tag}-mem")),
        db: Some(mem),
    }];
    for (name, mode) in [
        ("stream", DiskQueryMode::Stream),
        ("mat", DiskQueryMode::Materialize),
    ] {
        let dir = tmp.0.join(format!("{tag}-{name}"));
        Database::build_disk(&dir, &[&nt]).unwrap();
        out.push(Layout {
            kind: Kind::Disk(mode),
            db: Some(reopen(&dir, mode)),
            dir,
        });
    }
    for l in &mut out {
        assert!(l.db().stats().index_valid, "{:?}: a usable VS-tree", l.kind);
    }
    out
}

/// The answers of a database built afresh from `lines` (no VS-tree
/// history), sorted, for each query.
fn oracle(lines: &BTreeSet<String>, queries: &[String]) -> Vec<Rows> {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut db = Database::build_from_str("oracle", &text).unwrap();
    queries
        .iter()
        .map(|q| sorted(run(&mut db, q, VsPolicy::Off).0))
        .collect()
}

/// Check every query on every layout under every policy (see [`check`];
/// trees may hold stale bits, so `exact` is off) against the oracle.
fn check_all(layouts: &mut [Layout], queries: &[String], want: &[Rows], when: &str) {
    let mut decided = VsStats::default();
    for (q, want) in queries.iter().zip(want) {
        for l in layouts.iter_mut() {
            let disk = l.disk();
            let got = check(l.db(), q, disk, false, &mut decided);
            assert_eq!(&sorted(got), want, "{:?} {when}: {q}", l.kind);
        }
    }
}

/// `DELETE DATA` / `INSERT DATA` over `lines`, 50 triples per request.
fn data_updates(verb: &str, lines: &[String]) -> Vec<String> {
    lines
        .chunks(50)
        .map(|c| format!("{verb} DATA {{ {} }}", c.join("\n")))
        .collect()
}

/// Whether N-Triples `line` mentions entity `e` (as subject or object).
fn mentions(line: &str, e: &str) -> bool {
    line.starts_with(e) || line.trim_end_matches(" .").ends_with(e)
}

#[test]
fn answers_match_an_oracle_under_insert_id_reuse_rollback_and_restart_churn() {
    let tmp = TempDir::new("churn");
    let nt = random_nt(31);
    let mut lines: BTreeSet<String> = nt.lines().map(str::to_string).collect();
    let mut layouts = layouts(&tmp, "churn", &nt);
    let mut rng = Rng(0x5eed);
    let queries: Vec<String> = queries(37).into_iter().take(40).collect();
    let mut fresh = 0usize;
    let (mut freed_total, mut reused_total) = (0u64, 0u64);
    for round in 0..3 {
        let free_start: Vec<u64> = layouts
            .iter_mut()
            .map(|l| l.db().id_space_stats().free[0])
            .collect();
        // 1. Deletes: every triple of three entities (their ids die: freed
        //    at once under the default `id_reclaim=on`), and a tenth of the
        //    rest.
        let dying: Vec<String> = (0..3).map(|_| entity(rng.below(ENTITIES - 5))).collect();
        let gone: Vec<String> = lines
            .iter()
            .filter(|l| dying.iter().any(|e| mentions(l, e)) || rng.chance(10))
            .cloned()
            .collect();
        for l in &gone {
            lines.remove(l);
        }
        for u in data_updates("DELETE", &gone) {
            for l in &mut layouts {
                l.update(&u);
            }
        }
        // 2. Free whatever is still dead (everything, under
        //    `id_reclaim=off`), so the new terms below take the ids.
        let mut free_before = Vec::new();
        for (l, start) in layouts.iter_mut().zip(&free_start) {
            let db = l.db();
            if db.id_space_stats().enabled {
                db.reclaim_unused().unwrap();
            }
            let free = db.id_space_stats().free[0];
            freed_total += free.saturating_sub(*start);
            free_before.push(free);
        }
        // 3. Inserts: new entities (taking freed ids) linked to old ones,
        //    and a third of the deleted triples back.
        let mut added: Vec<String> = Vec::new();
        for _ in 0..6 {
            let n = format!("<http://ex/n{fresh}>");
            fresh += 1;
            for _ in 0..5 {
                let p = predicate(rng.below(PREDICATES));
                let other = entity(rng.below(ENTITIES - 5));
                added.push(if rng.chance(70) {
                    format!("{n} {p} {other} .")
                } else {
                    format!("{other} {p} {n} .")
                });
            }
            added.push(format!(
                "{n} {} {} .",
                predicate(1),
                literal(rng.below(LITERALS))
            ));
        }
        added.extend(gone.iter().filter(|_| rng.chance(33)).cloned());
        for l in &added {
            lines.insert(l.clone());
        }
        for u in data_updates("INSERT", &added) {
            for l in &mut layouts {
                l.update(&u);
            }
        }
        for (l, before) in layouts.iter_mut().zip(free_before) {
            let after = l.db().id_space_stats().free[0];
            reused_total += before.saturating_sub(after);
        }
        // 4. A rolled-back transaction: its inserts and deletes leave no
        //    trace in any answer.
        let junk: Vec<String> = (0..20)
            .map(|i| {
                format!(
                    "<http://ex/junk{round}_{i}> {} {} .",
                    predicate(rng.below(PREDICATES)),
                    entity(rng.below(ENTITIES))
                )
            })
            .collect();
        let doomed: Vec<String> = lines.iter().filter(|_| rng.chance(5)).cloned().collect();
        for l in &mut layouts {
            l.db().begin().unwrap();
            for u in data_updates("INSERT", &junk)
                .into_iter()
                .chain(data_updates("DELETE", &doomed))
            {
                l.update(&u);
            }
            l.db().rollback().unwrap();
        }
        let want = oracle(&lines, &queries);
        check_all(&mut layouts, &queries, &want, &format!("round {round}"));
        // 5. Restart: the disk layouts rebuild their stale vstree.kv, the
        //    in-memory one reloads its snapshot.
        for l in &mut layouts {
            l.restart();
        }
        check_all(
            &mut layouts,
            &queries,
            &want,
            &format!("round {round} restarted"),
        );
    }
    assert!(freed_total > 0, "ids were freed");
    assert!(reused_total > 0, "freed ids were reused");
}

/// Literals of every datatype family (non-canonical lexical forms, special
/// values, ill-typed ones, language tags, a custom datatype), on entities
/// that also carry an edge between them.
fn typed_literal_nt() -> String {
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let forms: Vec<String> = [
        ("007", "integer"),
        ("+5", "integer"),
        ("-0", "integer"),
        ("12abc", "integer"),
        ("1.50", "decimal"),
        (".5", "decimal"),
        ("-10.0", "decimal"),
        ("1.0E2", "double"),
        ("1e2", "double"),
        ("INF", "double"),
        ("-INF", "float"),
        ("NaN", "float"),
        ("3.25", "float"),
        ("true", "boolean"),
        ("1", "boolean"),
        ("2024-01-05", "date"),
        ("2024-01-05T10:00:00Z", "dateTime"),
        ("2024-01-05T10:00:00.500+02:00", "dateTime"),
        ("P1Y2M", "duration"),
        ("abc", "string"),
        ("x1y", "token"),
    ]
    .iter()
    .map(|(lex, dt)| format!("\"{lex}\"^^<{XSD}{dt}>"))
    .chain(
        [
            "\"abc\"",
            "\"\"",
            "\"1,5\"",
            "\"abc\"@en",
            "\"Abc1\"@en-GB",
            "\"\u{e9}t\u{e9} 2024\"@fr",
            "\"x1y\"^^<http://ex/custom>",
            "\"E=mc2\"",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .collect();
    let mut out = String::new();
    for (i, f) in forms.iter().enumerate() {
        out.push_str(&format!("<http://ex/t{i}> <http://ex/val> {f} .\n"));
        out.push_str(&format!(
            "<http://ex/t{i}> <http://ex/next> <http://ex/t{}> .\n",
            (i + 1) % forms.len()
        ));
    }
    // More than 64 dictionary strings, so the substring index is built.
    for i in 0..80 {
        out.push_str(&format!(
            "<http://ex/f{i}> <http://ex/label> \"filler {i} E1\" .\n"
        ));
    }
    out
}

#[test]
fn substring_pushdown_matches_a_no_index_oracle_over_typed_literals() {
    let tmp = TempDir::new("substr");
    let mut layouts = layouts(&tmp, "substr", &typed_literal_nt());
    let needles = [
        "1", "0", "5", "7", "00", "E", "e", "E2", "abc", "Ab", "2024", "-0", "+", ".", "T1", "NaN",
        "INF", "rue", "\u{e9}", "x1", ",", "Z", ":00", "P1",
    ];
    let mut queries = Vec::new();
    for n in needles {
        let lit = format!("\"{}\"", n);
        let regexable = n.chars().all(|c| c.is_alphanumeric());
        let mut filters = vec![
            format!("CONTAINS(STR(?o), {lit})"),
            format!("STRSTARTS(STR(?o), {lit})"),
            format!("STRENDS(STR(?o), {lit})"),
            format!("CONTAINS(?o, {lit})"),
            format!("STRSTARTS(?o, {lit})"),
            format!("CONTAINS(STR(?s), {lit})"),
        ];
        if regexable {
            filters.push(format!("REGEX(STR(?o), {lit})"));
            filters.push(format!("REGEX(?o, {lit})"));
        }
        for f in filters {
            for shape in [
                "?s <http://ex/val> ?o",
                "?s <http://ex/val> ?o . ?s <http://ex/next> ?n",
                "?s ?p ?o",
            ] {
                queries.push((
                    format!("SELECT ?s ?o WHERE {{ {shape} FILTER({f}) }}"),
                    format!("SELECT ?s ?o WHERE {{ {shape} FILTER(({f}) || false) }}"),
                ));
            }
        }
    }
    let mut nonempty = 0;
    let mut decided = VsStats::default();
    for (q, oracle_q) in &queries {
        let want = {
            let l = &mut layouts[0];
            sorted(run(l.db(), oracle_q, VsPolicy::Off).0)
        };
        if !want.is_empty() {
            nonempty += 1;
        }
        for l in &mut layouts {
            let disk = l.disk();
            let got = check(l.db(), q, disk, true, &mut decided);
            assert_eq!(sorted(got), want, "{:?}: {q}", l.kind);
            let (oracle_rows, _) = run(l.db(), oracle_q, VsPolicy::Tree);
            assert_eq!(sorted(oracle_rows), want, "{:?}: {oracle_q}", l.kind);
        }
    }
    assert!(
        nonempty > queries.len() / 3,
        "{nonempty} of {}",
        queries.len()
    );
}

/// Entities with one extreme ordering key each (pairwise different values
/// of every numeric family, and a few non-numeric terms), and for the
/// numeric ones a second edge, so a two-edge star orders by an expression.
fn topk_nt() -> String {
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let numeric = [
        ("-9223372036854775808", "integer"),
        ("9223372036854775807", "integer"),
        ("9223372036854775808", "integer"),
        ("-99999999999999999999999999", "integer"),
        ("0", "integer"),
        ("1", "integer"),
        ("-1", "integer"),
        ("0.000000000000000000000000001", "decimal"),
        ("123456789012345678901234567890.5", "decimal"),
        ("-2.5", "decimal"),
        ("1.7976931348623157E308", "double"),
        ("-1.7976931348623157E308", "double"),
        ("4.9E-324", "double"),
        ("INF", "double"),
        ("-INF", "double"),
        ("3.5", "float"),
        ("-3.75", "float"),
    ];
    let mut out = String::new();
    for (i, (lex, dt)) in numeric.iter().enumerate() {
        let v = format!("\"{lex}\"^^<{XSD}{dt}>");
        out.push_str(&format!("<http://ex/k{i}> <http://ex/key> {v} .\n"));
        out.push_str(&format!("<http://ex/k{i}> <http://ex/nkey> {v} .\n"));
        out.push_str(&format!(
            "<http://ex/k{i}> <http://ex/w> \"{}\"^^<{XSD}integer> .\n",
            1000 + 7 * i
        ));
    }
    let others = [
        "\"abc\"",
        "\"abd\"@en",
        "<http://ex/iri>",
        "\"2024-01-01\"^^<http://www.w3.org/2001/XMLSchema#date>",
        "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
    ];
    for (i, v) in others.iter().enumerate() {
        out.push_str(&format!("<http://ex/x{i}> <http://ex/key> {v} .\n"));
    }
    out
}

/// The rows of `q` under `policy` with the top-k `options`.
fn run_topk(db: &mut Database, q: &str, policy: VsPolicy, options: TopKOptions) -> Rows {
    let saved = db.topk_options();
    db.set_topk_options(options);
    let rows = run(db, q, policy).0;
    db.set_topk_options(saved);
    rows
}

#[test]
fn ordered_limits_are_exact_under_extreme_keys_and_every_topk_setting() {
    let tmp = TempDir::new("topk");
    let mut layouts = layouts(&tmp, "topk", &topk_nt());
    let settings = {
        let mut v = Vec::new();
        for dpb in [DpbMode::Off, DpbMode::Auto, DpbMode::Always] {
            let mut o = TopKOptions::ALL;
            o.dpb = dpb;
            v.push(o);
        }
        let mut no_heap = TopKOptions::ALL;
        no_heap.bounded_heap = false;
        v.push(no_heap);
        v
    };
    let orders = [
        ("?s ?v", "?s <http://ex/key> ?v", "?v"),
        ("?s ?v", "?s <http://ex/key> ?v", "DESC(?v)"),
        ("?s ?v", "?s <http://ex/key> ?v", "?v ?s"),
        (
            "?s ?v ?w",
            "?s <http://ex/nkey> ?v . ?s <http://ex/w> ?w",
            "(?v + ?w)",
        ),
        (
            "?s ?v ?w",
            "?s <http://ex/nkey> ?v . ?s <http://ex/w> ?w",
            "DESC(-2 * ?v)",
        ),
        (
            "?s ?v ?w",
            "?s <http://ex/nkey> ?v . ?s <http://ex/w> ?w",
            "(?w - ?v)",
        ),
    ];
    for l in &mut layouts {
        for policy in [VsPolicy::Auto, VsPolicy::Tree, VsPolicy::Off] {
            for (proj, bgp, key) in orders {
                let full_q = format!("SELECT {proj} WHERE {{ {bgp} }} ORDER BY {key}");
                let full = run_topk(l.db(), &full_q, policy, TopKOptions::NONE);
                assert!(full.len() >= 17, "{full_q}");
                for limit in [0usize, 1, 4, 1000] {
                    for offset in [0usize, 3, 1000] {
                        let q = format!("{full_q} LIMIT {limit} OFFSET {offset}");
                        let none = run_topk(l.db(), &q, policy, TopKOptions::NONE);
                        let slice: Rows = full.iter().skip(offset).take(limit).cloned().collect();
                        assert_eq!(none, slice, "{:?} {policy:?}: {q}", l.kind);
                        for o in &settings {
                            let got = run_topk(l.db(), &q, policy, *o);
                            assert_eq!(got, none, "{:?} {policy:?} {o:?}: {q}", l.kind);
                        }
                    }
                }
                // Determinism: the same ordered query twice, the same order;
                // and pages of a total order tile it.
                if key == "?v ?s" {
                    let again = run_topk(l.db(), &full_q, policy, TopKOptions::ALL);
                    assert_eq!(again, full, "{:?} {policy:?}: {full_q}", l.kind);
                    let mut tiled = Vec::new();
                    for page in 0..(full.len() + 2) / 3 + 1 {
                        let q = format!("{full_q} LIMIT 3 OFFSET {}", page * 3);
                        tiled.extend(run_topk(l.db(), &q, policy, TopKOptions::ALL));
                    }
                    assert_eq!(tiled, full, "{:?} {policy:?}: pages of {full_q}", l.kind);
                }
            }
        }
    }
    // The full ordered answers agree across layouts (as multisets: ties may
    // be ordered by the plan).
    for (proj, bgp, key) in orders {
        let q = format!("SELECT {proj} WHERE {{ {bgp} }} ORDER BY {key}");
        let mut answers = layouts
            .iter_mut()
            .map(|l| sorted(run_topk(l.db(), &q, VsPolicy::Auto, TopKOptions::ALL)));
        let first = answers.next().unwrap();
        for other in answers {
            assert_eq!(other, first, "{q}");
        }
    }
}

/// Every offset of `needle` in `hay`.
fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    hay.windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .collect()
}

/// The maintained triple count of `pred` in the `kvstore.kv` at `kv`.
fn stored_pred_count(kv: &Path, pred: &str) -> usize {
    let ds = DiskStore::open(kv, 64).unwrap();
    let p = ds
        .predicate_id(&Term::iri(pred).dict_key())
        .unwrap()
        .unwrap();
    ds.pred_num(p)
}

/// review gap-accel-soundness#4: a per-predicate triple count pushed past
/// `u32::MAX` saturates instead of wrapping (to 0 at 2^32, which made
/// `Auto` drop the predicate on an out-of-core tree and answer no rows; a
/// debug build panicked with an overflow), and `Auto` treats the saturated
/// count as an unknown one.
#[test]
fn a_near_max_predicate_count_keeps_auto_answers_correct() {
    const PMAX: &str = "http://ex/pmax";
    let tmp = TempDir::new("saturated");
    let mut text = random_nt(41);
    // 77 subjects, one object: the statistics record (77, 77, 1).
    for i in 0..77 {
        text.push_str(&format!("<http://ex/m{i}> <{PMAX}> <http://ex/hub> .\n"));
    }
    let nt = tmp.0.join("data.nt");
    fs::write(&nt, &text).unwrap();
    let dir = tmp.0.join("db");
    Database::build_disk(&dir, &[&nt]).unwrap();
    // Force the count to u32::MAX - 1 in the file.
    let kv = dir.join("kvstore.kv");
    let mut bytes = fs::read(&kv).unwrap();
    let rec: Vec<u8> = [77u32, 77, 1]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let at = find_all(&bytes, &rec);
    assert_eq!(at.len(), 1, "one statistics record (77, 77, 1): {at:?}");
    bytes[at[0]..at[0] + 4].copy_from_slice(&(u32::MAX - 1).to_le_bytes());
    fs::write(&kv, &bytes).unwrap();
    assert_eq!(stored_pred_count(&kv, PMAX), u32::MAX as usize - 1);

    // Two more triples: u32::MAX, then saturated (it wrapped to 0).
    {
        let mut db = reopen(&dir, DiskQueryMode::Stream);
        db.query(&format!(
            "INSERT DATA {{ <http://ex/n1> <{PMAX}> <http://ex/hub> . \
             <http://ex/n2> <{PMAX}> <http://ex/hub> }}"
        ))
        .unwrap();
    }
    assert_eq!(stored_pred_count(&kv, PMAX), u32::MAX as usize);

    let queries = [
        format!("SELECT ?s ?o WHERE {{ ?s <{PMAX}> ?o }}"),
        format!("SELECT ?s WHERE {{ ?s <{PMAX}> ?o . ?s <http://ex/p1> ?x }}"),
        format!("SELECT ?s ?o WHERE {{ ?o <http://ex/p2> ?x . ?s <{PMAX}> ?o }}"),
    ];
    let check_db = |db: &mut Database, rows_first: usize| {
        let mut decided = VsStats::default();
        for (i, q) in queries.iter().enumerate() {
            let rows = check(db, q, true, true, &mut decided);
            if i == 0 {
                assert_eq!(rows.len(), rows_first, "{q}");
            }
        }
        assert!(decided.listed > 0, "Auto listed a set from the counts");
    };
    for mode in [DiskQueryMode::Stream, DiskQueryMode::Materialize] {
        let mut db = reopen(&dir, mode);
        assert!(db.stats().index_valid, "{mode:?}");
        check_db(&mut db, 79);
    }

    // A delete leaves a saturated count where it is (it is unknown), and
    // the answers stay right.
    {
        let mut db = reopen(&dir, DiskQueryMode::Stream);
        db.query(&format!(
            "DELETE DATA {{ <http://ex/m0> <{PMAX}> <http://ex/hub> }}"
        ))
        .unwrap();
    }
    assert_eq!(stored_pred_count(&kv, PMAX), u32::MAX as usize);
    let mut db = reopen(&dir, DiskQueryMode::Stream);
    check_db(&mut db, 78);
}
