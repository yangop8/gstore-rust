//! Data test: the bulk build of the disk layout (`gstore::bulk`, `gbuild
//! --disk`) against the incremental one (`Database::build_disk_from_sources_incremental`)
//! and the in-memory build — logically equal stores, ids equal to the memory
//! build's, byte-identical `vstree.kv`, identical query answers and order,
//! deterministic bytes across thread counts and budgets, the same failure for
//! an over-long term, and a bulk image the incremental writers keep mutating.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use gstore::bulk::{self, BulkInput, BulkOptions, BulkReport, BulkTarget};
use gstore::kvstore::DiskStore;
use gstore::parser::import::{self, ImportOptions, Source};
use gstore::{Database, Term};

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_query(n: usize) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("testdata/lubm/lubm_q{n}.rq"));
    std::fs::read_to_string(p).unwrap()
}

/// The temp directories the current test thread made, removed when the
/// thread ends — also when the test fails (a thread-local's destructor runs
/// after the unwinding).
struct TmpDirs(std::cell::RefCell<Vec<std::path::PathBuf>>);

impl Drop for TmpDirs {
    fn drop(&mut self) {
        for p in self.0.borrow().iter() {
            // A directory, or a file (a store path).
            let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
        }
    }
}

thread_local! {
    static TMP_DIRS: TmpDirs = const { TmpDirs(std::cell::RefCell::new(Vec::new())) };
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_bulk_{tag}_{}", std::process::id()));
    TMP_DIRS.with(|t| t.0.borrow_mut().push(d.clone()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// N-Quads mixing graphs, terms shared with LUBM, terms only a graph uses,
/// literal objects, duplicates and a default-graph statement.
fn write_nquads(path: &Path) {
    let mut s = String::new();
    for g in 0..3 {
        for i in 0..200 {
            s.push_str(&format!(
                "<http://www.Department0.University0.edu/GraduateStudent{i}> \
                 <http://ex/rated> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://ex/g{g}> .\n",
                (i * 7 + g) % 11
            ));
            s.push_str(&format!(
                "<http://ex/only/{g}/{i}> <http://ex/p{}> <http://ex/only/{g}/{}> <http://ex/g{g}> .\n",
                i % 4,
                (i + 1) % 200
            ));
        }
        s.push_str(&format!(
            "<http://ex/only/{g}/0> <http://ex/p0> <http://ex/only/{g}/1> <http://ex/g{g}> .\n"
        ));
    }
    s.push_str("<http://ex/default> <http://ex/p9> \"in the default graph\"@en .\n");
    std::fs::write(path, s).unwrap();
}

fn bulk_opts(threads: usize, budget: usize) -> BulkOptions {
    BulkOptions {
        memory_budget: budget,
        threads,
        ..BulkOptions::default()
    }
}

fn bulk_build(dir: &Path, sources: &[Source], opts: &BulkOptions) -> BulkReport {
    bulk::build(
        dir,
        "t",
        BulkInput::Sources(sources),
        &ImportOptions::strict(),
        &mut (),
        BulkTarget::Disk,
        opts,
    )
    .unwrap()
}

/// `(s, p, o, o is literal)` in dictionary-key form.
type KeyTriple = (String, String, String, bool);

/// Everything a disk store holds, by string.
#[derive(Debug, PartialEq)]
struct Dump {
    counts: [u64; 8],
    entities: Vec<String>,
    literals: Vec<String>,
    predicates: Vec<String>,
    triples: Vec<KeyTriple>,
    quads: Vec<(String, KeyTriple)>,
    pred_stats: BTreeMap<String, (usize, usize, usize)>,
    vstree_stale: bool,
}

impl Dump {
    /// The same content regardless of ids: every list sorted.
    fn logical(mut self) -> Dump {
        self.entities.sort();
        self.literals.sort();
        self.predicates.sort();
        self.triples.sort();
        self.quads.sort();
        self
    }
}

fn dump(kv: &Path) -> Dump {
    let ds = DiskStore::open(kv, 2048).unwrap();
    let lit0 = gstore::model::id::LITERAL_FIRST_ID;
    let term = |id: u32| ds.id_to_string(id).unwrap().unwrap();
    let pred = |id: u32| ds.predicate_to_string(id).unwrap().unwrap();
    let entities: Vec<String> = (0..ds.entity_num() as u32).map(term).collect();
    let literals: Vec<String> = (0..ds.literal_num() as u32)
        .map(|i| term(lit0 + i))
        .collect();
    let predicates: Vec<String> = (0..ds.predicate_num() as u32).map(pred).collect();
    // Every *2id entry finds its term's id.
    for (i, k) in entities.iter().enumerate() {
        let t = Term::from_dict_key(k).unwrap();
        assert_eq!(ds.term_id(&t).unwrap(), Some(i as u32), "entity2id {k}");
    }
    for (i, k) in literals.iter().enumerate() {
        let t = Term::from_dict_key(k).unwrap();
        assert_eq!(
            ds.term_id(&t).unwrap(),
            Some(lit0 + i as u32),
            "literal2id {k}"
        );
    }
    for (i, k) in predicates.iter().enumerate() {
        assert_eq!(
            ds.predicate_id(k).unwrap(),
            Some(i as u32),
            "predicate2id {k}"
        );
    }
    let key = |t: gstore::IdTriple| -> KeyTriple {
        (
            term(t.sub),
            pred(t.pred),
            term(t.obj),
            gstore::model::id::is_literal_id(t.obj),
        )
    };
    let triples: Vec<KeyTriple> = ds.try_iter_spo().unwrap().into_iter().map(key).collect();
    let quads = ds
        .quads()
        .unwrap()
        .into_iter()
        .map(|(g, t)| (term(g), key(t)))
        .collect();
    let pred_stats = (0..ds.predicate_num() as u32)
        .map(|p| {
            (
                pred(p),
                (
                    ds.pred_num(p),
                    ds.pred_subject_count(p),
                    ds.pred_object_count(p),
                ),
            )
        })
        .collect();
    Dump {
        counts: [
            ds.triple_count(),
            ds.entity_num() as u64,
            ds.literal_num() as u64,
            ds.predicate_num() as u64,
            ds.quad_count(),
            ds.distinct_subject_count() as u64,
            ds.distinct_object_count() as u64,
            ds.nonempty_predicate_count() as u64,
        ],
        entities,
        literals,
        predicates,
        triples,
        quads,
        pred_stats,
        vstree_stale: ds.vstree_stale(),
    }
}

fn select_rows(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    db.select(q).unwrap().rows
}

/// LUBM: the bulk store equals the incremental one id for id (the ids are
/// first-seen in both), `vstree.kv` is byte-identical, the header is
/// version 2 with an empty free-id list (R4: a new database keeps one), and
/// the 14 queries answer the same rows in the same order as
/// the incremental and the in-memory builds.
#[test]
fn lubm_bulk_equals_incremental_and_memory() {
    let base = tmp("lubm");
    let sources = vec![Source::File(lubm_nt())];
    let (a, b) = (base.join("inc.db"), base.join("bulk.db"));
    Database::build_disk_from_sources_incremental(&a, &sources, &ImportOptions::strict(), &mut ())
        .unwrap();
    let r = bulk_build(&b, &sources, &bulk_opts(0, 64 << 20));
    assert_eq!(r.triples, 100_543);
    assert_eq!(r.predicates, 17);
    assert_eq!(r.import.failed, 0);
    assert!(
        !b.join(bulk::STAGING_DIR).exists(),
        "the staging area is gone"
    );

    let (da, db_) = (dump(&a.join("kvstore.kv")), dump(&b.join("kvstore.kv")));
    assert!(!db_.vstree_stale);
    assert_eq!(da.counts, db_.counts);
    assert_eq!(da.pred_stats, db_.pred_stats);
    assert!(da == db_, "bulk and incremental stores differ");
    assert_eq!(
        std::fs::read(a.join("vstree.kv")).unwrap(),
        std::fs::read(b.join("vstree.kv")).unwrap(),
        "vstree.kv must be byte-identical"
    );
    let kv = std::fs::read(b.join("kvstore.kv")).unwrap();
    // A new database (R4): version 2, the FREE_IDS feature, an empty list.
    assert_eq!(&kv[0..8], b"GSTOREK2");
    assert_eq!(kv[16 + 21 * 8..16 + 22 * 8], 1u64.to_le_bytes());
    assert!(kv[16 + 22 * 8..16 + 26 * 8].iter().all(|&x| x == 0));
    assert!(kv[16 + 24 * 8..4096].iter().all(|&x| x == 0));
    let inc_size = std::fs::metadata(a.join("kvstore.kv")).unwrap().len();
    assert!(
        (kv.len() as u64) * 4 <= inc_size * 3,
        "bulk {} vs incremental {inc_size} bytes",
        kv.len()
    );

    let mut mem = Database::build_from_files("m", &[lubm_nt()]).unwrap();
    let mut inc = Database::open(&a).unwrap();
    let mut blk = Database::open(&b).unwrap();
    for n in 1..=14 {
        let q = lubm_query(n);
        let rows = select_rows(&mut blk, &q);
        assert_eq!(rows, select_rows(&mut inc, &q), "Q{n} bulk vs incremental");
        assert_eq!(rows, select_rows(&mut mem, &q), "Q{n} bulk vs memory");
    }
    drop((inc, blk));
    let _ = std::fs::remove_dir_all(&base);
}

/// LUBM plus N-Quads: the stores hold the same terms, triples, quads and
/// statistics (the incremental build interns a quad's graph first, so its
/// ids differ), and the bulk ids are the in-memory build's.
#[test]
fn nquads_mix_is_logically_equal_and_ids_follow_the_memory_build() {
    let base = tmp("nq");
    let nq = base.join("mix.nq");
    write_nquads(&nq);
    let sources = vec![Source::File(lubm_nt()), Source::File(nq.clone())];
    let (a, b) = (base.join("inc.db"), base.join("bulk.db"));
    Database::build_disk_from_sources_incremental(&a, &sources, &ImportOptions::strict(), &mut ())
        .unwrap();
    let r = bulk_build(&b, &sources, &bulk_opts(2, 1 << 20));
    assert!(r.quads > 0);
    assert!(r.spilled_runs > 0, "a 1 MiB budget spills: {r:?}");
    let (da, db_) = (dump(&a.join("kvstore.kv")), dump(&b.join("kvstore.kv")));
    assert_eq!(da.counts, db_.counts);
    assert_eq!(da.pred_stats, db_.pred_stats);
    assert!(da.logical() == db_.logical(), "logical content differs");

    // Ids: those of the in-memory build of the same sources.
    let (mem, _) =
        Database::build_from_sources("m", &sources, &ImportOptions::strict(), &mut ()).unwrap();
    let ds = DiskStore::open(b.join("kvstore.kv"), 1024).unwrap();
    let dict = mem.dict();
    assert_eq!(ds.entity_num(), dict.entity_num());
    assert_eq!(ds.literal_num(), dict.literal_num());
    assert_eq!(ds.predicate_num(), dict.predicate_num());
    for id in 0..ds.entity_num() as u32 {
        assert_eq!(
            ds.id_to_string(id).unwrap().as_deref(),
            dict.id_to_string(id)
        );
    }
    let lit0 = gstore::model::id::LITERAL_FIRST_ID;
    for i in 0..ds.literal_num() as u32 {
        let id = lit0 + i;
        assert_eq!(
            ds.id_to_string(id).unwrap().as_deref(),
            dict.id_to_string(id)
        );
    }
    for p in 0..ds.predicate_num() as u32 {
        assert_eq!(
            ds.predicate_to_string(p).unwrap().as_deref(),
            dict.predicate_to_string(p)
        );
    }
    let mut want: Vec<gstore::IdTriple> = mem.store().iter_all().collect();
    want.sort_unstable_by_key(|t| (t.sub, t.pred, t.obj));
    assert_eq!(ds.try_iter_spo().unwrap(), want, "same triples by id");
    let mut quads: Vec<(u32, gstore::IdTriple)> = mem
        .named_graphs()
        .iter()
        .flat_map(|(&g, st)| st.iter_all().map(move |t| (g, t)))
        .collect();
    quads.sort_unstable_by_key(|(g, t)| (*g, t.sub, t.pred, t.obj));
    assert_eq!(ds.quads().unwrap(), quads, "same quads by id");
    drop(ds);

    // Queries over the named graphs agree with the memory build.
    let mut blk = Database::open(&b).unwrap();
    let mut mem = mem;
    for q in [
        "SELECT ?g ?s ?o WHERE { GRAPH ?g { ?s <http://ex/rated> ?o } } ORDER BY ?g ?s ?o",
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <http://ex/g1> { ?s ?p ?o } }",
        "SELECT ?o WHERE { <http://ex/default> <http://ex/p9> ?o }",
    ] {
        assert_eq!(select_rows(&mut blk, q), select_rows(&mut mem, q), "{q}");
    }
    drop(blk);
    let _ = std::fs::remove_dir_all(&base);
}

/// The output depends on the data only: one thread or several, everything
/// in memory or spilled in tiny runs, the files are byte-identical.
#[test]
fn bytes_are_identical_across_threads_and_budgets() {
    let base = tmp("bytes");
    // Several sources so the parsers run in parallel too.
    let text = std::fs::read_to_string(lubm_nt()).unwrap();
    let (prefixes, lines): (Vec<&str>, Vec<&str>) =
        text.lines().partition(|l| l.starts_with("@prefix"));
    let head = prefixes.join("\n") + "\n";
    let mut sources = Vec::new();
    for (i, chunk) in lines.chunks(lines.len() / 3 + 1).enumerate() {
        let p = base.join(format!("part{i}.nt"));
        std::fs::write(&p, head.clone() + &chunk.join("\n") + "\n").unwrap();
        sources.push(Source::File(p));
    }
    let nq = base.join("mix.nq");
    write_nquads(&nq);
    sources.push(Source::File(nq));
    let mut reference: Option<(Vec<u8>, Vec<u8>)> = None;
    for (i, (threads, budget, fill)) in [
        (1usize, 256usize << 20, 90u8),
        (4, 256 << 20, 90),
        (2, 64 << 10, 90),
        (3, 512 << 10, 90),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = base.join(format!("db{i}"));
        let opts = BulkOptions {
            fill_percent: fill,
            ..bulk_opts(threads, budget)
        };
        let r = bulk_build(&dir, &sources, &opts);
        if budget < 1 << 20 {
            assert!(r.spilled_runs > 0, "{r:?}");
        } else {
            assert_eq!(r.spilled_runs, 0, "{r:?}");
        }
        let files = (
            std::fs::read(dir.join("kvstore.kv")).unwrap(),
            std::fs::read(dir.join("vstree.kv")).unwrap(),
        );
        match &reference {
            None => reference = Some(files),
            Some(want) => assert!(
                *want == files,
                "threads {threads} budget {budget}: different bytes"
            ),
        }
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// A term longer than the disk layout's key limit fails the bulk build with
/// exactly the incremental build's error, and the database already in the
/// directory stays as it was.
#[test]
fn an_over_long_term_fails_like_the_incremental_build() {
    let base = tmp("long");
    let ok = base.join("ok.nt");
    std::fs::write(&ok, "<http://e/s> <http://e/p> <http://e/o> .\n").unwrap();
    for (tag, term_len) in [("key", 2039usize), ("entry", 4100)] {
        let long = base.join(format!("{tag}.nt"));
        let iri = format!("http://e/{}", "x".repeat(term_len - 11));
        std::fs::write(
            &long,
            format!("<http://e/a> <http://e/p> \"short\" .\n<http://e/a> <http://e/p> <{iri}> .\n"),
        )
        .unwrap();
        let sources = vec![Source::File(long)];
        let inc = Database::build_disk_from_sources_incremental(
            base.join(format!("inc_{tag}.db")),
            &sources,
            &ImportOptions::tolerant(),
            &mut (),
        )
        .unwrap_err()
        .to_string();
        let dir = base.join(format!("bulk_{tag}.db"));
        bulk_build(&dir, &[Source::File(ok.clone())], &BulkOptions::default());
        let blk = bulk::build(
            &dir,
            "t",
            BulkInput::Sources(&sources),
            &ImportOptions::tolerant(),
            &mut (),
            BulkTarget::Disk,
            &BulkOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(blk, inc, "{tag}");
        assert!(
            blk.contains(&format!("{} bytes", term_len))
                || blk.contains(&format!("{term_len}-byte")),
            "{blk}"
        );
        // The old database is untouched and the staging area is gone.
        assert!(!dir.join(bulk::STAGING_DIR).exists());
        assert_eq!(Database::open(&dir).unwrap().triple_num(), 1);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Deterministic xorshift.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A set with O(1) random picks and removal.
#[derive(Default)]
struct Pool<T: Clone + Eq + std::hash::Hash> {
    items: Vec<T>,
    index: HashMap<T, usize>,
}

impl<T: Clone + Eq + std::hash::Hash> Pool<T> {
    fn insert(&mut self, t: T) -> bool {
        if self.index.contains_key(&t) {
            return false;
        }
        self.index.insert(t.clone(), self.items.len());
        self.items.push(t);
        true
    }
    fn remove(&mut self, t: &T) -> bool {
        let Some(i) = self.index.remove(t) else {
            return false;
        };
        self.items.swap_remove(i);
        if i < self.items.len() {
            self.index.insert(self.items[i].clone(), i);
        }
        true
    }
    fn pick(&self, rng: &mut Rng) -> Option<&T> {
        (!self.items.is_empty()).then(|| &self.items[rng.below(self.items.len())])
    }
}

/// A bulk image stays fully writable by the incremental writers: 10,000
/// random inserts and deletes (default graph and named graphs, old and new
/// terms, through a small page cache so pages are evicted, committed and
/// freed and the free list is reused) leave exactly the expected content,
/// and the maintained statistics equal a recomputation.
#[test]
fn bulk_image_survives_10k_random_mutations() {
    let base = tmp("mut");
    let nq = base.join("mix.nq");
    write_nquads(&nq);
    let dir = base.join("db");
    bulk_build(
        &dir,
        &[Source::File(lubm_nt()), Source::File(nq)],
        &BulkOptions::default(),
    );
    let kv = dir.join("kvstore.kv");
    let start = dump(&kv);
    let mut triples: Pool<KeyTriple> = Pool::default();
    for t in &start.triples {
        triples.insert(t.clone());
    }
    let mut quads: Pool<(String, KeyTriple)> = Pool::default();
    for q in &start.quads {
        quads.insert(q.clone());
    }
    let entities = start.entities.clone();
    let predicates = start.predicates.clone();
    let literals = start.literals.clone();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut ds = DiskStore::open(&kv, 128).unwrap();
    let mut fresh = 0u32;
    for step in 0..10_000u32 {
        let r = rng.below(100);
        if r < 45 {
            // Insert: mostly known terms, sometimes new ones.
            let s = if rng.below(10) == 0 {
                fresh += 1;
                format!("<http://new/s{fresh}>")
            } else {
                entities[rng.below(entities.len())].clone()
            };
            let p = if rng.below(50) == 0 {
                format!("<http://new/p{}>", rng.below(5))
            } else {
                predicates[rng.below(predicates.len())].clone()
            };
            let (o, lit) = match rng.below(3) {
                0 => (literals[rng.below(literals.len())].clone(), true),
                1 => (format!("\"new literal {}\"", rng.below(500)), true),
                _ => (entities[rng.below(entities.len())].clone(), false),
            };
            let t = (s, p, o, lit);
            let added = ds.insert_keys(&t.0, &t.1, &t.2, t.3).unwrap();
            assert_eq!(added, triples.insert(t), "step {step} insert");
        } else if r < 90 {
            // Delete: mostly present triples, sometimes absent ones.
            let t = if rng.below(10) == 0 {
                (
                    entities[rng.below(entities.len())].clone(),
                    predicates[rng.below(predicates.len())].clone(),
                    entities[rng.below(entities.len())].clone(),
                    false,
                )
            } else {
                triples.pick(&mut rng).unwrap().clone()
            };
            let removed = ds.delete_keys(&t.0, &t.1, &t.2, t.3).unwrap();
            assert_eq!(removed, triples.remove(&t), "step {step} delete");
        } else if r < 95 {
            let g = format!("<http://ex/g{}>", rng.below(4));
            let t = (
                entities[rng.below(entities.len())].clone(),
                predicates[rng.below(predicates.len())].clone(),
                entities[rng.below(entities.len())].clone(),
                false,
            );
            let added = ds.insert_quad_keys(&g, &t.0, &t.1, &t.2, t.3).unwrap();
            assert_eq!(added, quads.insert((g, t)), "step {step} quad insert");
        } else if let Some(q) = quads.pick(&mut rng).cloned() {
            let (g, t) = &q;
            let removed = ds.delete_quad_keys(g, &t.0, &t.1, &t.2, t.3).unwrap();
            assert!(removed, "step {step} quad delete");
            quads.remove(&q);
        }
        if step % 2500 == 2499 {
            ds.flush().unwrap();
        }
    }
    ds.flush().unwrap();
    let maintained: Vec<(usize, usize, usize)> = (0..ds.predicate_num() as u32)
        .map(|p| {
            (
                ds.pred_num(p),
                ds.pred_subject_count(p),
                ds.pred_object_count(p),
            )
        })
        .collect();
    let distinct = (ds.distinct_subject_count(), ds.distinct_object_count());
    ds.rebuild_stats().unwrap();
    let recomputed: Vec<(usize, usize, usize)> = (0..ds.predicate_num() as u32)
        .map(|p| {
            (
                ds.pred_num(p),
                ds.pred_subject_count(p),
                ds.pred_object_count(p),
            )
        })
        .collect();
    assert_eq!(maintained, recomputed, "maintained statistics");
    assert_eq!(
        distinct,
        (ds.distinct_subject_count(), ds.distinct_object_count())
    );
    drop(ds);

    let end = dump(&kv);
    let want: HashSet<KeyTriple> = triples.items.iter().cloned().collect();
    let got: HashSet<KeyTriple> = end.triples.iter().cloned().collect();
    assert_eq!(got.len(), end.triples.len(), "no duplicate triples");
    assert!(got == want, "triples after the mutations");
    let want_q: HashSet<(String, KeyTriple)> = quads.items.iter().cloned().collect();
    let got_q: HashSet<(String, KeyTriple)> = end.quads.iter().cloned().collect();
    assert!(got_q == want_q, "quads after the mutations");
    assert_eq!(end.counts[0], want.len() as u64);
    assert_eq!(end.counts[4], want_q.len() as u64);
    // The database opens and answers through the ordinary path.
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), want.len() as u64);
    let rows = select_rows(&mut db, "SELECT ?o WHERE { <http://new/s1> ?p ?o }");
    let want_rows = want.iter().filter(|t| t.0 == "<http://new/s1>").count();
    assert_eq!(rows.len(), want_rows);
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// Without the `rocksdb` feature the RocksDB target is refused (with it, it
/// is tested in `dt_bulk_rocks.rs`); an invalid fill factor is refused
/// before anything is touched.
#[test]
fn unsupported_target_and_bad_options() {
    let base = tmp("opts");
    if !cfg!(feature = "rocksdb") {
        let e = bulk::build(
            &base.join("r.db"),
            "r",
            BulkInput::Sources(&[]),
            &ImportOptions::strict(),
            &mut (),
            BulkTarget::RocksDb,
            &BulkOptions::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("RocksDB"), "{e}");
    }
    let e = bulk::build(
        &base.join("f.db"),
        "f",
        BulkInput::Sources(&[]),
        &ImportOptions::strict(),
        &mut (),
        BulkTarget::Disk,
        &BulkOptions {
            fill_percent: 30,
            ..BulkOptions::default()
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("fill"), "{e}");
    assert!(!base.join("r.db").exists() && !base.join("f.db").exists());
    // An empty input builds an empty, openable database.
    let r = bulk_build(&base.join("e.db"), &[], &BulkOptions::default());
    assert_eq!((r.triples, r.entities), (0, 0));
    let db = Database::open(base.join("e.db")).unwrap();
    assert_eq!(db.triple_num(), 0);
    drop(db);
    // A text input.
    let dir = base.join("t.db");
    let r = bulk::build(
        &dir,
        "t",
        BulkInput::Text {
            text: "<a> <b> \"c\" .\n<a> <b> <d> .\n",
            format: import::RdfFormat::Turtle,
            name: "<string>",
        },
        &ImportOptions::strict(),
        &mut (),
        BulkTarget::Disk,
        &BulkOptions::default(),
    )
    .unwrap();
    assert_eq!((r.triples, r.entities, r.literals), (2, 2, 1));
    let _ = std::fs::remove_dir_all(&base);
}

/// `gbuild --disk` builds in bulk and prints its summary; `--incremental`
/// selects the previous path; the bulk knobs are checked and only go with
/// `--disk`.
#[test]
fn gbuild_disk_uses_the_bulk_build() {
    let base = tmp("cli");
    let gbuild = env!("CARGO_BIN_EXE_gbuild");
    let nt = base.join("d.nt");
    std::fs::write(
        &nt,
        "<http://e/s> <http://e/p> <http://e/o> .\n<http://e/s> <http://e/q> \"v\" .\nnot rdf\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(gbuild)
            .args(args)
            .current_dir(&base)
            .output()
            .unwrap()
    };
    let out = run(&[
        "--disk",
        "--memory",
        "16",
        "--threads",
        "2",
        "--fill",
        "70",
        "--tmp-dir",
        "spill",
        "b",
        "d.nt",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("bulk build"), "{stdout}");
    assert!(stdout.contains("triples=2"), "{stdout}");
    assert!(stdout.contains("spills:"), "{stdout}");
    assert!(stdout.contains("RDF parse error num 1"), "{stdout}");
    assert!(Database::is_disk(base.join("b.db")));
    let log = std::fs::read_to_string(base.join("b.db/parse_error.log")).unwrap();
    assert_eq!(
        log.lines().filter(|l| l.starts_with("Line ")).count(),
        1,
        "{log}"
    );

    let out = run(&["--disk", "--incremental", "i", "d.nt"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("bulk build"));
    assert_eq!(Database::open(base.join("i.db")).unwrap().triple_num(), 2);
    assert_eq!(Database::open(base.join("b.db")).unwrap().triple_num(), 2);

    for bad in [
        vec!["--memory", "8", "m", "d.nt"],
        vec!["--incremental", "m", "d.nt"],
        vec!["--disk", "--fill", "20", "m", "d.nt"],
        vec!["--disk", "--memory", "lots", "m", "d.nt"],
        vec!["--disk", "--threads", "-1", "m", "d.nt"],
    ] {
        let out = run(&bad);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {out:?}");
        assert!(!base.join("m.db").exists(), "{bad:?}");
    }
    let _ = std::fs::remove_dir_all(&base);
}
