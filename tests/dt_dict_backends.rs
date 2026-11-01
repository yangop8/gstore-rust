//! Data test: the dictionary API of contract C4 (Phase 6, R2-DICT) — `TermStr`,
//! routing through an out-of-core backend, read errors through the statement's
//! read scope, the kvstore's `DiskDict` over a LUBM-1 `kvstore.kv` (round trips,
//! bounded caches, the out-of-core query path) and `dict.bin` compatibility.

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gstore::dict::{CacheStats, Dictionary, DiskTermSource, Space, TermStr};
use gstore::kvstore::{DiskDict, DiskStore};
use gstore::model::id::{INVALID_ENTITY_LITERAL_ID, INVALID_PRED_ID, LITERAL_FIRST_ID};
use gstore::parser::sparql;
use gstore::query::Evaluator;
use gstore::store::read_scope::ReadScope;
use gstore::store::TripleStore;
use gstore::{Database, GStoreError, IdTriple, QueryResult, Result};

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_queries() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    (1..=14)
        .map(|i| {
            let name = format!("Q{i}");
            let text = std::fs::read_to_string(dir.join(format!("lubm_q{i}.rq"))).unwrap();
            (name, text)
        })
        .collect()
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "gstore_dt_dict_backends_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_file(&d);
    d
}

fn rows(r: QueryResult) -> Vec<Vec<Option<String>>> {
    match r {
        QueryResult::Select(rs) => rs.rows,
        QueryResult::Ask(b) => vec![vec![Some(b.to_string())]],
        other => panic!("unexpected result {other:?}"),
    }
}

// ---- a mock out-of-core backend ---------------------------------------------

/// An in-memory [`DiskTermSource`] with counters and an injectable failure.
#[derive(Debug, Default)]
struct Mock {
    terms: Mutex<BTreeMap<(Space, u32), String>>,
    fail: AtomicBool,
    lookups: AtomicUsize,
    resolves: AtomicUsize,
    prefetched: AtomicUsize,
}

impl Mock {
    fn with(terms: &[(Space, u32, &str)]) -> Mock {
        let m = Mock::default();
        for &(s, id, k) in terms {
            m.terms.lock().unwrap().insert((s, id), k.to_string());
        }
        m
    }
    fn check(&self) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            Err(GStoreError::Database("injected read failure".into()))
        } else {
            Ok(())
        }
    }
}

impl DiskTermSource for Mock {
    fn lookup(&self, space: Space, key: &str) -> Result<Option<u32>> {
        self.check()?;
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .terms
            .lock()
            .unwrap()
            .iter()
            .find(|((s, _), k)| *s == space && k.as_str() == key)
            .map(|((_, id), _)| *id))
    }
    fn resolve(&self, space: Space, id: u32) -> Result<Option<Arc<str>>> {
        self.check()?;
        self.resolves.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .terms
            .lock()
            .unwrap()
            .get(&(space, id))
            .map(|s| Arc::from(s.as_str())))
    }
    fn prefetch(&self, _space: Space, ids: &[u32]) -> Result<()> {
        self.check()?;
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
        self.prefetched.fetch_add(ids.len(), Ordering::SeqCst);
        Ok(())
    }
    fn for_each(
        &self,
        space: Space,
        from: u32,
        f: &mut dyn FnMut(u32, &str) -> ControlFlow<()>,
    ) -> Result<()> {
        self.check()?;
        for ((s, id), k) in self.terms.lock().unwrap().iter() {
            if *s == space && *id >= from && f(*id, k).is_break() {
                break;
            }
        }
        Ok(())
    }
    fn count(&self, space: Space) -> usize {
        self.terms
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| *s == space)
            .count()
    }
    fn id_limit(&self, space: Space) -> u32 {
        self.terms
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| *s == space)
            .map(|(_, id)| id + 1)
            .max()
            .unwrap_or(space.first_id())
    }
}

fn mock_terms() -> Vec<(Space, u32, &'static str)> {
    vec![
        (Space::Entity, 0, "<http://ex/a>"),
        (Space::Entity, 1, "<http://ex/b>"),
        (Space::Literal, LITERAL_FIRST_ID, "\"hello\""),
        (Space::Predicate, 0, "<http://ex/p>"),
    ]
}

#[test]
fn term_str_semantics() {
    let shared: TermStr<'static> = TermStr::Shared(Arc::from("<x>"));
    let borrowed = TermStr::Borrowed("<x>");
    assert_eq!(shared, borrowed);
    assert_eq!(shared, "<x>");
    assert_eq!(&*shared, "<x>");
    assert_eq!(shared.as_ref() as &str, "<x>");
    assert_eq!(format!("{shared}"), "<x>");
    assert_eq!(borrowed.clone().into_string(), String::from("<x>"));
    let mut m = HashMap::new();
    m.insert(shared, 1);
    assert_eq!(m.get("<x>"), Some(&1), "hashes as its text");
}

#[test]
fn backed_dictionary_routes_lookups_resolves_and_interns() {
    let mock = Arc::new(Mock::with(&mock_terms()));
    let mut d = Dictionary::from_backing(mock.clone());
    assert!(d.is_disk_backed());
    assert_eq!(d.entity_id("<http://ex/b>"), Some(1));
    assert_eq!(d.literal_id("\"hello\""), Some(LITERAL_FIRST_ID));
    assert_eq!(d.predicate_id("<http://ex/p>"), Some(0));
    assert_eq!(d.entity_id("<http://ex/missing>"), None);
    assert!(mock.lookups.load(Ordering::SeqCst) >= 4);
    let t = d.term(0).unwrap();
    assert!(matches!(t, TermStr::Shared(_)));
    assert_eq!(t, "<http://ex/a>");
    assert_eq!(d.term(LITERAL_FIRST_ID).as_deref(), Some("\"hello\""));
    assert_eq!(d.pred(0).as_deref(), Some("<http://ex/p>"));
    assert_eq!(d.term(7), None);
    assert_eq!(d.entity_num(), 2);
    assert_eq!(d.id_limit(Space::Literal), LITERAL_FIRST_ID + 1);
    // A read-only backend refuses to intern (the trait's default) — before,
    // a backed dictionary silently interned into its empty in-memory side.
    let err = d.try_intern_entity("<http://ex/new>").unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    assert_eq!(d.entity_id("<http://ex/new>"), None);
    // Batches: sorted, deduplicated prefetches per space; input order kept.
    d.prefetch(&[1, 0, 1, LITERAL_FIRST_ID], &[0, 0]);
    assert_eq!(mock.prefetched.load(Ordering::SeqCst), 4);
    let many = d.resolve_many(Space::Entity, &[1, 0, 1, 9]).unwrap();
    let many: Vec<Option<&str>> = many.iter().map(|t| t.as_deref()).collect();
    assert_eq!(
        many,
        vec![
            Some("<http://ex/b>"),
            Some("<http://ex/a>"),
            Some("<http://ex/b>"),
            None
        ]
    );
    // An in-memory copy keeps every id.
    let m = d.materialize().unwrap();
    assert!(!m.is_disk_backed());
    assert_eq!(m.term(1).as_deref(), Some("<http://ex/b>"));
    assert_eq!(m.term(LITERAL_FIRST_ID).as_deref(), Some("\"hello\""));
    assert_eq!(m.pred(0).as_deref(), Some("<http://ex/p>"));
    // ... and serves the legacy `&str` API, which is for in-memory dictionaries.
    assert_eq!(m.id_to_string(1), Some("<http://ex/b>"));
    assert_eq!(m.predicate_to_string(0), Some("<http://ex/p>"));
    let _: CacheStats = d.cache_stats();
}

#[test]
fn backend_errors_fail_the_statement_through_the_read_scope() {
    let mock = Arc::new(Mock::with(&mock_terms()));
    let dict = Dictionary::from_backing(mock.clone());
    let mut store = TripleStore::new();
    store.insert(IdTriple::new(0, 0, 1));
    store.insert(IdTriple::new(1, 0, LITERAL_FIRST_ID));
    let q = sparql::parse("SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }").unwrap();

    // Healthy: two rows, no error.
    let scope = ReadScope::enter();
    let ok = rows(Evaluator::new(&dict, &store).evaluate(&q).unwrap());
    assert_eq!(scope.take_error(), None);
    drop(scope);
    assert_eq!(ok.len(), 2);

    // A failing backend: the error reaches the scope, so the caller fails
    // the statement instead of answering with holes.
    mock.fail.store(true, Ordering::SeqCst);
    let scope = ReadScope::enter();
    let _ = Evaluator::new(&dict, &store).evaluate(&q);
    let err = scope.take_error().expect("the backend error is recorded");
    assert!(err.contains("injected read failure"), "{err}");
    drop(scope);
    // The fallible forms return it directly.
    assert!(dict.try_term(0).is_err());
    assert!(dict.try_entity_id("<http://ex/a>").is_err());
}

#[test]
fn legacy_str_api_on_a_backed_dictionary_fails_closed() {
    let mut terms = vec![(Space::Predicate, 0, "<http://ex/p>")];
    let names: Vec<String> = (0..5000).map(|i| format!("<http://ex/e{i}>")).collect();
    for (i, n) in names.iter().enumerate() {
        terms.push((Space::Entity, i as u32, n.as_str()));
    }
    let mock = Arc::new(Mock::with(&terms));
    let dict = Dictionary::from_backing(mock.clone());
    // Contract C4: on a backed dictionary the legacy `&str` forms answer
    // `None` and record a read error (C2), for every id, whether or not the
    // backend could resolve it: never a leaked or wrong string.
    for id in [0u32, 3, 4095, 4500] {
        let scope = ReadScope::enter();
        assert_eq!(dict.id_to_string(id), None, "entity {id}");
        let err = scope.take_error().expect("recorded");
        assert!(err.contains("term"), "{err}");
    }
    let scope = ReadScope::enter();
    assert_eq!(dict.predicate_to_string(0), None);
    assert!(scope.take_error().is_some());
    // The invalid ids stay plain `None`, with nothing recorded.
    assert_eq!(dict.id_to_string(INVALID_ENTITY_LITERAL_ID), None);
    assert_eq!(dict.predicate_to_string(INVALID_PRED_ID), None);
    assert_eq!(scope.take_error(), None);
    // The legacy forms never reach the backend; the C4 API is unaffected.
    let before = mock.resolves.load(Ordering::SeqCst);
    let _ = dict.id_to_string(7);
    let _ = scope.take_error();
    assert_eq!(mock.resolves.load(Ordering::SeqCst), before);
    assert_eq!(dict.term(4500).as_deref(), Some("<http://ex/e4500>"));
    assert_eq!(dict.pred(0).as_deref(), Some("<http://ex/p>"));
    assert_eq!(scope.take_error(), None);
    // A read error on the C4 path is recorded the same way.
    mock.fail.store(true, Ordering::SeqCst);
    assert_eq!(dict.term(3), None);
    assert!(scope.take_error().is_some());
}

// ---- DiskDict over a LUBM-1 kvstore.kv -----------------------------------------

fn lubm_store(tag: &str) -> (PathBuf, DiskStore) {
    let path = tmp(tag).with_extension("kv");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("kv.wal"));
    let ds = DiskStore::build_files(&path, 4096, &[lubm_nt()]).unwrap();
    (path, ds)
}

fn remove_store(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("kv.wal"));
}

#[test]
fn disk_dict_round_trips_every_id_and_stays_within_budget() {
    let (path, ds) = lubm_store("roundtrip");
    let eager = ds.dictionary().unwrap();
    let dd = DiskDict::open_shared(ds.shared_pager()).unwrap();
    let lazy = Dictionary::from_backing(Arc::new(dd));
    let mut all: Vec<(Space, u32)> = Vec::new();
    for space in Space::ALL {
        assert_eq!(lazy.id_limit(space), eager.id_limit(space));
        for id in space.first_id()..lazy.id_limit(space) {
            let s = lazy.resolve(space, id).expect("every id names a term");
            assert_eq!(Some(&*s), eager.resolve(space, id).as_deref());
            assert_eq!(lazy.id_of(space, &s), Some(id), "{space:?} {id}");
            all.push((space, id));
        }
    }
    assert!(all.len() > 10_000);

    // 1M random look-ups (80% id → string, 20% key → id) through a 1 MiB
    // budget (512 KiB per direction): the resident bytes never exceed it.
    let dd = DiskDict::open_shared(ds.shared_pager())
        .unwrap()
        .with_cache_budgets(512 << 10, 512 << 10);
    let lazy = Dictionary::from_backing(Arc::new(dd));
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for i in 0..1_000_000u32 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let (space, id) = all[(x % all.len() as u64) as usize];
        if i % 5 == 0 {
            let key = eager.resolve(space, id).unwrap();
            assert_eq!(lazy.id_of(space, &key), Some(id));
        } else {
            assert!(lazy.resolve(space, id).is_some());
        }
        if i % 10_000 == 0 {
            let st = lazy.cache_stats();
            assert!(st.resident_bytes() <= 1 << 20, "{st:?}");
        }
    }
    let st = lazy.cache_stats();
    assert!(st.resident_bytes() <= 1 << 20, "{st:?}");
    assert!(st.evictions > 0 && st.hits > 0, "{st:?}");
    drop(lazy);
    drop(ds);
    remove_store(&path);
}

#[test]
fn out_of_core_queries_match_memory_answers_in_order() {
    let (path, ds) = lubm_store("queries");
    let mem_dict = ds.dictionary().unwrap();
    let (mdict, mstore) = ds.to_memory().unwrap();
    let dd = Arc::new(DiskDict::open_shared(ds.shared_pager()).unwrap());
    let lazy = Dictionary::from_backing(dd.clone());
    let mut resident = Vec::new();
    let mut total = 0usize;
    for round in 0..3 {
        for (name, q) in lubm_queries() {
            let parsed = sparql::parse(&q).unwrap();
            let scope = ReadScope::enter();
            let got = rows(Evaluator::new(&lazy, &ds).evaluate(&parsed).unwrap());
            assert_eq!(scope.take_error(), None, "{name}");
            drop(scope);
            // Same store, in-memory dictionary: identical rows, same order.
            let want = rows(Evaluator::new(&mem_dict, &ds).evaluate(&parsed).unwrap());
            assert_eq!(got, want, "{name} round {round}");
            // The convenience path over the store's own lazy dictionary.
            let conv = rows(ds.query(&q).unwrap());
            assert_eq!(conv, want, "{name} DiskStore::query");
            // The fully materialized engine: same multiset.
            let mut mem = rows(Evaluator::new(&mdict, &mstore).evaluate(&parsed).unwrap());
            let mut g = got.clone();
            mem.sort();
            g.sort();
            assert_eq!(g, mem, "{name} vs memory");
            total += got.len();
        }
        resident.push(dd.cache_stats().resident_bytes());
    }
    assert!(total > 1000, "the queries have answers ({total} rows)");
    // Repeating the workload touches no new strings: no growth.
    assert_eq!(resident[1], resident[2], "{resident:?}");
    assert!(resident[2] <= 64 << 20);
    drop(ds);
    remove_store(&path);
}

// ---- dict.bin compatibility ---------------------------------------------------

/// The pre-Phase-6 persisted interner: its derived serde is the old encoding
/// (a `HashMap` written in hash order).
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct OldInterner {
    forward: HashMap<String, u32>,
    backward: Vec<Option<String>>,
    free: Vec<u32>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct OldDictionary {
    entities: OldInterner,
    literals: OldInterner,
    predicates: OldInterner,
}

#[test]
fn dict_bin_reads_both_ways_and_is_reproducible() {
    let dir = tmp("dictbin");
    let mut db = Database::build_from_files("lubm", &[lubm_nt()]).unwrap();
    // Holes and a free stack: delete some data and reclaim its terms.
    db.query(
        "DELETE WHERE { <http://www.Department0.University0.edu/UndergraduateStudent1> ?p ?o }",
    )
    .unwrap();
    assert!(db.reclaim_unused() > 0);
    db.save(&dir).unwrap();
    let bytes = std::fs::read(dir.join("dict.bin")).unwrap();

    // New encoding → old layout: equal contents.
    let old: OldDictionary = bincode::deserialize(&bytes).unwrap();
    assert!(old.entities.backward.iter().any(Option::is_none));
    assert!(!old.entities.free.is_empty());
    for (k, &i) in &old.entities.forward {
        assert_eq!(db.dict().entity_id(k), Some(i));
        assert_eq!(
            old.entities.backward[i as usize].as_deref(),
            Some(k.as_str())
        );
    }
    assert_eq!(old.entities.forward.len(), db.dict().entity_num());

    // Reproducible: saving again writes the same bytes.
    let dir2 = tmp("dictbin2");
    db.save(&dir2).unwrap();
    assert_eq!(std::fs::read(dir2.join("dict.bin")).unwrap(), bytes);

    // Old encoding (hash order) → new code: same database.
    std::fs::write(dir.join("dict.bin"), bincode::serialize(&old).unwrap()).unwrap();
    let mut reopened = Database::open(&dir).unwrap();
    reopened.set_query_cache_size(0);
    db.set_query_cache_size(0);
    for (name, q) in lubm_queries() {
        assert_eq!(
            rows(reopened.query(&q).unwrap()),
            rows(db.query(&q).unwrap()),
            "{name}"
        );
    }
    let dir3 = tmp("dictbin3");
    reopened.save(&dir3).unwrap();
    assert_eq!(std::fs::read(dir3.join("dict.bin")).unwrap(), bytes);
    for d in [dir, dir2, dir3] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn no_box_leak_in_the_dictionary_or_the_kvstore() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let needle = ["Box", "::leak("].concat();
    for sub in ["dict", "kvstore"] {
        for entry in std::fs::read_dir(root.join(sub)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                assert!(!text.contains(&needle), "{} leaks", path.display());
            }
        }
    }
}
