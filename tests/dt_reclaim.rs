//! Phase 6 R4-RECLAIM: immediate id reclamation on every layout.
//!
//! A seeded workload (direct inserts and removes, `DELETE DATA`,
//! `DELETE WHERE`, named-graph quads, `CLEAR` / `DROP` / `MOVE` / `COPY`,
//! committed and rolled-back transactions, reopens) runs on a memory, a
//! Stream-disk, a Materialize-disk and (feature `rocksdb`) a RocksDB
//! database, each against an in-memory oracle with reclamation off. After
//! every operation the live term counts equal the distinct terms the data
//! references; at regular checkpoints the full contents (default graph and
//! quads, as strings), LUBM Q1–Q14 and `verify_integrity` are compared.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gstore::db::{IdReclaim, OpenOptions};
use gstore::{Database, QueryResult, Term, Triple};

// ---- helpers -----------------------------------------------------------------

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!(
            "gstore_dt_reclaim_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// SplitMix64.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn lubm_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_queries() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    (1..=14)
        .map(|i| std::fs::read_to_string(dir.join(format!("lubm_q{i}.rq"))).unwrap())
        .collect()
}

/// The LUBM fixture's triples about Department0 (the ones Q1–Q14 look at)
/// and a pool of the others, as terms.
fn lubm_triples() -> (Vec<Triple>, Vec<Triple>) {
    let db = Database::build_from_files("pool", &[lubm_path()]).unwrap();
    let dict = db.dict();
    let mut base = Vec::new();
    let mut pool = Vec::new();
    for t in db.store().iter_all() {
        let term = |s: Option<gstore::dict::TermStr<'_>>| Term::from_dict_key(&s.unwrap()).unwrap();
        let tr = Triple::new(
            term(dict.term(t.sub)),
            term(dict.pred(t.pred)),
            term(dict.term(t.obj)),
        );
        let text = format!("{} {}", tr.subject, tr.object);
        if text.contains("Department0.University0") {
            base.push(tr);
        } else if pool.len() < 4000 {
            pool.push(tr);
        }
    }
    (base, pool)
}

fn nt_line(t: &Triple) -> String {
    format!(
        "{} {} {} .\n",
        gstore::model::term::export_key(&t.subject.dict_key()),
        gstore::model::term::export_key(&t.predicate.dict_key()),
        gstore::model::term::export_key(&t.object.dict_key())
    )
}

fn rows(db: &mut Database, q: &str) -> Vec<String> {
    match db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        QueryResult::Select(rs) => {
            let mut v: Vec<String> = rs
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| c.as_ref().map(|t| t.to_string()).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("{other:?}"),
    }
}

/// The whole contents as sorted strings: the default graph, then the quads.
fn contents(db: &mut Database) -> (Vec<String>, Vec<String>) {
    (
        rows(db, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }"),
        rows(db, "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }"),
    )
}

/// The distinct terms the data references: `(nodes, predicates)` — nodes
/// are subjects, objects and graph IRIs (entities and literals).
fn referenced(db: &mut Database) -> (usize, usize) {
    let mut nodes = BTreeSet::new();
    let mut preds = BTreeSet::new();
    for r in rows(db, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }") {
        let c: Vec<&str> = r.split('\t').collect();
        nodes.insert(c[0].to_string());
        preds.insert(c[1].to_string());
        nodes.insert(c[2].to_string());
    }
    for r in rows(db, "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }") {
        let c: Vec<&str> = r.split('\t').collect();
        nodes.insert(c[0].to_string());
        nodes.insert(c[1].to_string());
        preds.insert(c[2].to_string());
        nodes.insert(c[3].to_string());
    }
    (nodes.len(), preds.len())
}

fn live(db: &Database) -> (usize, usize) {
    (db.entity_num() + db.literal_num(), db.predicate_num())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    Memory,
    Stream,
    Materialize,
    #[cfg(feature = "rocksdb")]
    Rocks,
}

fn options(layout: Layout) -> OpenOptions {
    match layout {
        Layout::Materialize => OpenOptions::materialize(),
        _ => OpenOptions::stream(),
    }
}

/// A database of `layout` holding `base`, in `dir`.
fn make(layout: Layout, dir: &Path, base: &[Triple]) -> Database {
    let text: String = base.iter().map(nt_line).collect();
    match layout {
        Layout::Memory => Database::build_from_str("mem", &text).unwrap(),
        Layout::Stream | Layout::Materialize => {
            let src = dir.join("base.nt");
            std::fs::write(&src, &text).unwrap();
            let db_dir = dir.join("db");
            Database::build_disk(&db_dir, &[&src]).unwrap();
            Database::open_with(&db_dir, &options(layout)).unwrap()
        }
        #[cfg(feature = "rocksdb")]
        Layout::Rocks => Database::build_rocksdb_from_str("rocks", dir.join("db"), &text).unwrap(),
    }
}

fn reopen(layout: Layout, db: Database, dir: &Path) -> Database {
    match layout {
        Layout::Memory => db,
        _ => {
            db.sync().unwrap();
            drop(db);
            Database::open_with(dir.join("db"), &options(layout)).unwrap()
        }
    }
}

/// Apply operation `op` to `db` (the same random choices on both sides).
fn apply(db: &mut Database, op: &Op) {
    match op {
        Op::Insert(t) => {
            db.try_insert_triple(t).unwrap();
        }
        Op::Remove(t) => {
            db.try_remove_triple(t).unwrap();
        }
        Op::Update(text) => {
            db.query(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        }
        Op::Txn(ops, commit) => {
            db.begin().unwrap();
            for o in ops {
                apply(db, o);
            }
            if *commit {
                db.commit().unwrap();
            } else {
                db.rollback().unwrap();
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Op {
    Insert(Triple),
    Remove(Triple),
    Update(String),
    Txn(Vec<Op>, bool),
}

fn fresh_triple(rng: &mut Rng) -> Triple {
    let n = rng.below(400);
    let o = if rng.below(2) == 0 {
        Term::iri(format!("http://ex/o{}", rng.below(400)))
    } else {
        Term::plain_literal(format!("value {}", rng.below(600)))
    };
    Triple::new(
        Term::iri(format!("http://ex/s{n}")),
        Term::iri(format!("http://ex/p{}", rng.below(12))),
        o,
    )
}

fn graph_iri(rng: &mut Rng) -> String {
    format!("http://ex/g{}", rng.below(5))
}

fn ground(t: &Triple) -> String {
    format!(
        "{} {} {}",
        gstore::model::term::export_key(&t.subject.dict_key()),
        gstore::model::term::export_key(&t.predicate.dict_key()),
        gstore::model::term::export_key(&t.object.dict_key())
    )
}

/// One random operation over the base / pool / fresh terms.
fn random_op(rng: &mut Rng, base: &[Triple], pool: &[Triple], in_txn: bool) -> Op {
    let pick = |rng: &mut Rng| -> Triple {
        match rng.below(3) {
            0 => base[rng.below(base.len())].clone(),
            1 => pool[rng.below(pool.len())].clone(),
            _ => fresh_triple(rng),
        }
    };
    match rng.below(100) {
        0..=34 => Op::Insert(pick(rng)),
        35..=69 => Op::Remove(pick(rng)),
        70..=77 => Op::Update(format!("DELETE DATA {{ {} }}", ground(&pick(rng)))),
        78..=83 => {
            let t = pick(rng);
            Op::Update(format!(
                "INSERT DATA {{ GRAPH <{}> {{ {} }} }}",
                graph_iri(rng),
                ground(&t)
            ))
        }
        84..=87 => {
            let t = pick(rng);
            Op::Update(format!(
                "DELETE DATA {{ GRAPH <{}> {{ {} }} }}",
                graph_iri(rng),
                ground(&t)
            ))
        }
        88..=89 => Op::Update(format!(
            "DELETE WHERE {{ <http://ex/s{}> ?p ?o }}",
            rng.below(400)
        )),
        90 => Op::Update(format!("CLEAR SILENT GRAPH <{}>", graph_iri(rng))),
        91 => Op::Update(format!("DROP SILENT GRAPH <{}>", graph_iri(rng))),
        92 => Op::Update(format!(
            "COPY SILENT GRAPH <{}> TO GRAPH <{}>",
            graph_iri(rng),
            graph_iri(rng)
        )),
        93 => Op::Update(format!(
            "MOVE SILENT GRAPH <{}> TO GRAPH <{}>",
            graph_iri(rng),
            graph_iri(rng)
        )),
        94 => Op::Update(format!(
            "ADD SILENT GRAPH <{}> TO GRAPH <{}>",
            graph_iri(rng),
            graph_iri(rng)
        )),
        _ if !in_txn => {
            let n = 1 + rng.below(6);
            let ops = (0..n).map(|_| random_op(rng, base, pool, true)).collect();
            Op::Txn(ops, rng.below(3) != 0)
        }
        _ => Op::Insert(fresh_triple(rng)),
    }
}

/// Run `ops` operations on `layout` against an oracle; reopen now and then.
fn workload(layout: Layout, seed: u64, ops: usize, check_every: usize) {
    let tmp = TempDir::new(&format!("{layout:?}_{seed}"));
    let (base, pool) = lubm_triples();
    let text: String = base.iter().map(nt_line).collect();
    let mut oracle = Database::build_from_str("oracle", &text).unwrap();
    oracle.set_id_reclaim(IdReclaim::Off);
    let mut db = make(layout, tmp.path(), &base);
    assert!(db.reclaims_ids(), "{layout:?} frees ids");
    let queries = lubm_queries();
    let mut rng = Rng(seed);
    let mut peak_live = 0usize;
    for step in 0..ops {
        let op = random_op(&mut rng, &base, &pool, false);
        apply(&mut oracle, &op);
        apply(&mut db, &op);
        let full = step % check_every == check_every - 1 || step + 1 == ops;
        if full {
            assert_eq!(
                contents(&mut db),
                contents(&mut oracle),
                "{layout:?} step {step}"
            );
            let want = referenced(&mut oracle);
            assert_eq!(live(&db), want, "{layout:?} step {step}: live counts");
            db.verify_integrity()
                .unwrap_or_else(|e| panic!("{layout:?} step {step}: {e}"));
            let stats = db.id_space_stats();
            peak_live = peak_live.max(want.0 + want.1);
            assert!(
                stats.live.iter().sum::<u64>() as usize == want.0 + want.1,
                "{layout:?} step {step}: {stats:?}"
            );
        }
        if step % (check_every * 8) == check_every * 8 - 1 {
            for (i, q) in queries.iter().enumerate() {
                assert_eq!(
                    rows(&mut db, q),
                    rows(&mut oracle, q),
                    "{layout:?} step {step}: Q{}",
                    i + 1
                );
            }
        }
        if layout != Layout::Memory && rng.below(400) == 0 {
            db = reopen(layout, db, tmp.path());
        }
    }
    // The id space stays near the live terms: lowest-first reuse.
    let stats = db.id_space_stats();
    let limit: u64 = stats.limits[0] + stats.limits[1];
    let live_nodes = stats.live[0] + stats.live[1];
    assert!(
        limit <= live_nodes + stats.free[0] + stats.free[1],
        "{layout:?}: {stats:?}"
    );
    drop(db);
}

#[test]
fn memory_matches_the_oracle() {
    workload(Layout::Memory, 1, 10_000, 50);
}

#[test]
fn stream_disk_matches_the_oracle() {
    workload(Layout::Stream, 2, 3_000, 50);
}

#[test]
fn materialize_disk_matches_the_oracle() {
    workload(Layout::Materialize, 3, 3_000, 50);
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocksdb_matches_the_oracle() {
    workload(Layout::Rocks, 4, 3_000, 50);
}

// ---- targeted cases -------------------------------------------------------------

fn small() -> Database {
    Database::build_from_str(
        "small",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
         <http://ex/a> <http://ex/q> \"lit\" .\n\
         <http://ex/c> <http://ex/p> <http://ex/b> .\n",
    )
    .unwrap()
}

fn eid(db: &Database, iri: &str) -> Option<u32> {
    db.dict().entity_id(&Term::iri(iri).dict_key())
}

#[test]
fn only_unreferenced_terms_are_freed() {
    let mut db = small();
    db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    // a (still a subject), b (still an object) and p (still used) stay.
    assert!(eid(&db, "http://ex/a").is_some());
    assert!(eid(&db, "http://ex/b").is_some());
    assert!(db.dict().predicate_id("<http://ex/p>").is_some());
    db.query("DELETE DATA { <http://ex/a> <http://ex/q> \"lit\" }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/a"), None);
    assert_eq!(db.dict().literal_id("\"lit\""), None);
    assert_eq!(db.dict().predicate_id("<http://ex/q>"), None);
    assert_eq!(db.literal_num(), 0);
    db.verify_integrity().unwrap();
}

#[test]
fn named_graph_and_graph_iri_references_pin_a_term() {
    let mut db = small();
    db.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/x> <http://ex/p> <http://ex/a> } }")
        .unwrap();
    // <a> loses its default-graph triples but a quad still uses it.
    db.query("DELETE WHERE { <http://ex/a> ?p ?o }").unwrap();
    assert!(eid(&db, "http://ex/a").is_some(), "a quad's object");
    // A term used only as a graph IRI is kept while the graph exists.
    db.query("INSERT DATA { <http://ex/k> <http://ex/p> <http://ex/g> }")
        .unwrap();
    db.query("DELETE DATA { <http://ex/k> <http://ex/p> <http://ex/g> }")
        .unwrap();
    assert!(eid(&db, "http://ex/g").is_some(), "the graph IRI");
    assert_eq!(eid(&db, "http://ex/k"), None);
    db.query("DROP GRAPH <http://ex/g>").unwrap();
    assert_eq!(eid(&db, "http://ex/g"), None);
    assert_eq!(eid(&db, "http://ex/x"), None);
    assert_eq!(eid(&db, "http://ex/a"), None);
    db.verify_integrity().unwrap();
}

#[test]
fn a_transaction_frees_at_commit() {
    let mut db = small();
    db.begin().unwrap();
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert!(eid(&db, "http://ex/c").is_some(), "deferred to the commit");
    db.query("INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_ne!(
        eid(&db, "http://ex/n"),
        Some(2),
        "c's id is not reused inside the transaction"
    );
    db.commit().unwrap();
    assert_eq!(eid(&db, "http://ex/c"), None, "freed at commit");
    db.verify_integrity().unwrap();
}

#[test]
fn rollback_frees_the_terms_the_transaction_minted() {
    let mut db = small();
    let before = (db.entity_num(), db.literal_num(), db.predicate_num());
    db.begin().unwrap();
    db.query(
        "INSERT DATA { <http://ex/new1> <http://ex/newp> \"new lit\" . \
                       GRAPH <http://ex/ng> { <http://ex/new2> <http://ex/p> <http://ex/a> } }",
    )
    .unwrap();
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    db.rollback().unwrap();
    assert_eq!(
        (db.entity_num(), db.literal_num(), db.predicate_num()),
        before
    );
    assert_eq!(eid(&db, "http://ex/new1"), None);
    assert_eq!(eid(&db, "http://ex/ng"), None);
    assert!(eid(&db, "http://ex/c").is_some(), "the delete was undone");
    db.verify_integrity().unwrap();
}

#[test]
fn off_reproduces_the_former_behaviour() {
    let mut db = small();
    db.set_id_reclaim(IdReclaim::Off);
    let n = db.entity_num();
    db.query("DELETE WHERE { ?s ?p ?o }").unwrap();
    assert_eq!(db.entity_num(), n, "nothing is freed");
    db.query("INSERT DATA { <http://ex/z> <http://ex/p> <http://ex/w> }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/z"), Some(n as u32), "the space grows");
    assert_eq!(
        db.reclaim_unused().unwrap(),
        5,
        "a, b, c, the literal and <q>"
    );
}

#[test]
fn a_pin_keeps_an_id_from_being_reused() {
    let mut db = small();
    let c = eid(&db, "http://ex/c").unwrap();
    let pin = db.dict().pin();
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/c"), None, "retired");
    db.query("INSERT DATA { <http://ex/d> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_ne!(eid(&db, "http://ex/d"), Some(c), "held by the pin");
    drop(pin);
    db.query("INSERT DATA { <http://ex/e> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/e"), Some(c), "released after the pin");
}

#[test]
fn clear_drop_move_copy_replay_from_the_redo_log() {
    let tmp = TempDir::new("redo");
    let log = tmp.path().join("redo.log");
    let mut db = small();
    db.enable_redo_log(&log);
    let start = small();
    for q in [
        "INSERT DATA { GRAPH <http://ex/g1> { <http://ex/x> <http://ex/p> <http://ex/y> } }",
        "INSERT DATA { GRAPH <http://ex/g2> { <http://ex/u> <http://ex/p> <http://ex/v> } }",
        "COPY GRAPH <http://ex/g1> TO GRAPH <http://ex/g3>",
        "MOVE GRAPH <http://ex/g2> TO DEFAULT",
        "CLEAR GRAPH <http://ex/g1>",
        "COPY DEFAULT TO GRAPH <http://ex/g4>",
        "DROP DEFAULT",
        "MOVE GRAPH <http://ex/g4> TO GRAPH <http://ex/g5>",
        "INSERT DATA { <http://ex/k> <http://ex/p> \"x\" }",
        "CLEAR ALL",
        "INSERT DATA { <http://ex/after> <http://ex/p> <http://ex/clear> . \
                       GRAPH <http://ex/g6> { <http://ex/q> <http://ex/p> <http://ex/r> } }",
        "CLEAR NAMED",
    ] {
        db.query(q).unwrap();
    }
    let mut replayed = start;
    replayed.replay_redo_log(&log).unwrap();
    assert_eq!(contents(&mut replayed), contents(&mut db));
    assert_eq!(live(&replayed), live(&db));
    assert_eq!(
        contents(&mut db).0,
        vec!["<http://ex/after>\t<http://ex/p>\t<http://ex/clear>".to_string()]
    );
}

#[test]
fn reclaim_unused_is_refused_inside_a_transaction() {
    let mut db = small();
    db.begin().unwrap();
    assert!(db.reclaim_unused().is_err());
    db.rollback().unwrap();
    let before = db.mutation_count();
    db.set_id_reclaim(IdReclaim::Off);
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    let mid = db.mutation_count();
    assert!(mid > before);
    assert_eq!(db.reclaim_unused().unwrap(), 1);
    assert!(db.mutation_count() > mid, "a free is a data change");
}

#[test]
fn snapshot_layout_persists_frees_at_save() {
    let tmp = TempDir::new("snap");
    let mut db = small();
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    db.save(tmp.path()).unwrap();
    let back = Database::open(tmp.path()).unwrap();
    assert_eq!(eid(&back, "http://ex/c"), None);
    assert_eq!(back.entity_num(), db.entity_num());
    back.verify_integrity().unwrap();
}

/// A free in Stream mode waits for a clone sharing the on-disk dictionary
/// (a snapshot): an id it read never changes meaning under it.
#[test]
fn stream_frees_wait_for_a_dictionary_clone() {
    let tmp = TempDir::new("stream_pin");
    let base: Vec<Triple> = vec![
        Triple::new(
            Term::iri("http://ex/a"),
            Term::iri("http://ex/p"),
            Term::iri("http://ex/b"),
        ),
        Triple::new(
            Term::iri("http://ex/c"),
            Term::iri("http://ex/p"),
            Term::iri("http://ex/b"),
        ),
    ];
    let mut db = make(Layout::Stream, tmp.path(), &base);
    let c = eid(&db, "http://ex/c").unwrap();
    let snapshot = db.dict().clone();
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(
        snapshot.term(c).as_deref(),
        Some("<http://ex/c>"),
        "the clone still resolves it"
    );
    db.query("INSERT DATA { <http://ex/d> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_ne!(eid(&db, "http://ex/d"), Some(c));
    drop(snapshot);
    // The next statement frees the deferred id; the one after reuses it.
    db.query("DELETE DATA { <http://ex/d> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/c"), None);
    db.query("INSERT DATA { <http://ex/e> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/e"), Some(c));
    db.verify_integrity().unwrap();
}

/// A kvstore.kv without the FREE_IDS feature (written with reclamation off,
/// as by a pre-Phase-6 binary) keeps reclamation off and stays version 1;
/// `enable_reclaim` (gcompact --enable-reclaim) turns it on.
#[test]
fn a_v1_store_keeps_reclamation_off_until_upgraded() {
    let tmp = TempDir::new("v1");
    let kv_dir = tmp.path().join("db");
    std::fs::create_dir_all(&kv_dir).unwrap();
    {
        let mut ds =
            gstore::kvstore::DiskStore::open_with(kv_dir.join("kvstore.kv"), 64, false).unwrap();
        ds.insert_keys("<http://ex/a>", "<http://ex/p>", "<http://ex/b>", false)
            .unwrap();
        ds.insert_keys("<http://ex/c>", "<http://ex/p>", "<http://ex/b>", false)
            .unwrap();
        ds.flush().unwrap();
    }
    for opts in [OpenOptions::stream(), OpenOptions::materialize()] {
        let mut db = Database::open_with(&kv_dir, &opts).unwrap();
        assert!(!db.reclaims_ids());
        db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
            .unwrap();
        assert!(
            eid(&db, "http://ex/c").is_some(),
            "kept: reclamation is off"
        );
        assert!(db.reclaim_unused().is_err(), "the explicit pass is refused");
        db.query("INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
            .unwrap();
    }
    let bytes = std::fs::read(kv_dir.join("kvstore.kv")).unwrap();
    assert_eq!(&bytes[0..8], b"GSTOREKV", "still version 1");
    {
        let mut ds = gstore::kvstore::DiskStore::open(kv_dir.join("kvstore.kv"), 64).unwrap();
        ds.enable_reclaim().unwrap();
        ds.flush().unwrap();
    }
    let bytes = std::fs::read(kv_dir.join("kvstore.kv")).unwrap();
    assert_eq!(&bytes[0..8], b"GSTOREK2");
    let mut db = Database::open_with(&kv_dir, &OpenOptions::stream()).unwrap();
    assert!(db.reclaims_ids());
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(eid(&db, "http://ex/c"), None);
    db.verify_integrity().unwrap();
}

// ---- tools ----------------------------------------------------------------------

fn run_tool(bin: &str, args: &[&str], cwd: &Path) -> (bool, String) {
    let out = std::process::Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env_remove("GSTORE_ID_RECLAIM")
        .output()
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// `gcompact --enable-reclaim` upgrades a pre-Phase-6 store, `--reclaim`
/// frees the orphans reclamation-off left, `--check` verifies; gmonitor
/// shows the id spaces.
#[test]
fn gcompact_enables_reclaims_and_checks() {
    let tmp = TempDir::new("gcompact");
    let dir = tmp.path().join("old.db");
    std::fs::create_dir_all(&dir).unwrap();
    {
        let mut ds =
            gstore::kvstore::DiskStore::open_with(dir.join("kvstore.kv"), 64, false).unwrap();
        for (s, o) in [("a", "b"), ("c", "b"), ("d", "e")] {
            ds.insert_keys(
                &format!("<http://ex/{s}>"),
                "<http://ex/p>",
                &format!("<http://ex/{o}>"),
                false,
            )
            .unwrap();
        }
        // An orphan: the triple goes, its terms stay (reclamation off).
        ds.delete_keys("<http://ex/d>", "<http://ex/p>", "<http://ex/e>", false)
            .unwrap();
        ds.flush().unwrap();
    }
    let gcompact = env!("CARGO_BIN_EXE_gcompact");
    let (ok, out) = run_tool(gcompact, &["old"], tmp.path());
    assert!(ok, "{out}");
    assert!(out.contains("--enable-reclaim"), "{out}");
    let (ok, out) = run_tool(gcompact, &["old", "--reclaim"], tmp.path());
    assert!(!ok, "refused on a version-1 store: {out}");
    let (ok, out) = run_tool(
        gcompact,
        &["old", "--enable-reclaim", "--reclaim", "--check"],
        tmp.path(),
    );
    assert!(ok, "{out}");
    assert!(out.contains("freed 2 unreferenced id(s)"), "{out}");
    assert!(out.contains("integrity ok"), "{out}");
    assert_eq!(
        &std::fs::read(dir.join("kvstore.kv")).unwrap()[0..8],
        b"GSTOREK2"
    );
    let (ok, out) = run_tool(env!("CARGO_BIN_EXE_gmonitor"), &["old"], tmp.path());
    assert!(ok, "{out}");
    assert!(out.contains("id_reclaim"), "{out}");
    assert!(out.contains("entity_ids"), "{out}");
    let db = Database::open_with(&dir, &OpenOptions::stream()).unwrap();
    assert_eq!(eid(&db, "http://ex/d"), None);
    assert_eq!(db.entity_num(), 3);
}
