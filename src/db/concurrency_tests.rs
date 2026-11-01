//! The `&self` read path and `Database: Send + Sync` (contract C8).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Barrier, RwLock};

use super::*;

fn lubm_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm")
}

fn lubm() -> Database {
    Database::build_from_files("lubm", &[lubm_dir().join("lubm.nt")]).unwrap()
}

fn lubm_queries() -> Vec<String> {
    (1..=14)
        .map(|i| fs::read_to_string(lubm_dir().join(format!("lubm_q{i}.rq"))).unwrap())
        .collect()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gstore_conc_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

const UB: &str = "http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#";
const D0: &str = "http://www.Department0.University0.edu/";
const DEPT: &str = "http://www.Department0.University0.edu";

/// The extra queries: a named graph, a user function and a CSR function.
fn extra_queries() -> Vec<String> {
    vec![
        "SELECT ?s ?o WHERE { GRAPH <http://ex/g> { ?s <http://ex/p> ?o } }".to_string(),
        format!(
            "SELECT ?x (twice(1) AS ?t) WHERE {{ ?x <{UB}takesCourse> <{D0}GraduateCourse0> }}"
        ),
        format!(
            "SELECT (shortestPathLen(<{D0}GraduateStudent44>, <{DEPT}>, true, \
             {{<{UB}memberOf>}}) AS ?y) WHERE {{}}"
        ),
        format!("ASK {{ <{D0}GraduateStudent44> <{UB}memberOf> <{DEPT}> }}"),
        format!(
            "CONSTRUCT {{ ?x <http://ex/in> <{DEPT}> }} WHERE {{ ?x <{UB}memberOf> <{DEPT}> }} LIMIT 5"
        ),
    ]
}

fn prepared() -> Database {
    let mut db = lubm();
    db.query(
        "INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/b> . \
         <http://ex/b> <http://ex/p> <http://ex/c> } }",
    )
    .unwrap();
    db.register_function("twice", |args| {
        Some(crate::query::Value::Double(args.first()?.as_f64()? * 2.0))
    });
    db
}

#[test]
fn read_as_equals_query_with_and_without_the_vstree() {
    let mut db = prepared();
    let queries: Vec<String> = lubm_queries().into_iter().chain(extra_queries()).collect();
    for valid in [true, false] {
        db.index_valid = valid;
        for q in &queries {
            let stmt = Statement::parse(q).unwrap();
            // Without the cache, then cached (the second read_as is a hit).
            db.set_query_cache_size(0);
            let shared = db.read_as(&stmt, None, None).unwrap();
            let via_read = db.query_read(q, None, None).unwrap();
            let exclusive = db.query(q).unwrap();
            assert_eq!(shared, exclusive, "read_as vs query (vstree {valid}): {q}");
            assert_eq!(
                via_read, exclusive,
                "query_read vs query (vstree {valid}): {q}"
            );
            db.set_query_cache_size(DEFAULT_QUERY_CACHE_BYTES);
            let first = db.read_as(&stmt, None, None).unwrap();
            let hit = db.read_as(&stmt, None, None).unwrap();
            assert_eq!(first, exclusive, "{q}");
            assert_eq!(hit, exclusive, "cached {q}");
        }
    }
}

#[test]
fn read_path_refuses_updates_without_side_effects() {
    let dir = temp_dir("refuse");
    let mut db = prepared();
    db.enable_update_log_in(&dir).unwrap();
    let (before, triples) = (db.mutation_count(), db.triple_num());
    let update = "INSERT DATA { <http://ex/x> <http://ex/y> <http://ex/z> }";
    let err = db.query_read(update, None, None).unwrap_err();
    assert!(matches!(err, GStoreError::Query(_)), "{err}");
    let stmt = Statement::parse(update).unwrap();
    assert!(stmt.is_update());
    assert!(db.read_as(&stmt, None, None).is_err());
    assert_eq!(db.mutation_count(), before);
    assert_eq!(db.triple_num(), triples);
    assert!(!dir.join(UPDATE_LOG_FILE).exists(), "nothing was logged");
    // The exclusive path runs it.
    let r = db.execute_as(stmt, None, None).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    assert_eq!(db.mutation_count(), before + 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn eight_threads_share_one_database() {
    let db = prepared();
    let queries: Vec<String> = lubm_queries().into_iter().chain(extra_queries()).collect();
    let expected: Vec<QueryResult> = queries
        .iter()
        .map(|q| db.query_read(q, None, None).unwrap())
        .collect();
    let db = &db;
    let barrier = Barrier::new(8);
    std::thread::scope(|s| {
        for t in 0..8usize {
            let (queries, expected, barrier) = (&queries, &expected, &barrier);
            s.spawn(move || {
                barrier.wait();
                for round in 0..3 {
                    for k in 0..queries.len() {
                        // Each thread walks the workload from its own offset.
                        let i = (k + t * 3 + round) % queries.len();
                        let got = db.query_read(&queries[i], None, None).unwrap();
                        assert_eq!(got, expected[i], "thread {t}: {}", queries[i]);
                    }
                }
            });
        }
    });
}

#[test]
fn concurrent_saves_and_backups_stay_valid_and_contiguous() {
    let root = temp_dir("persist");
    let home = root.join("home");
    let db = lubm();
    db.save(&home).unwrap();
    let mut db = Database::open(&home).unwrap();
    db.enable_redo_log_in(&home).unwrap();
    db.backup(root.join("full")).unwrap();
    let mut incrementals = Vec::new();
    for round in 0..3 {
        db.query(&format!(
            "INSERT DATA {{ <http://ex/r{round}> <http://ex/p> <http://ex/o{round}> }}"
        ))
        .unwrap();
        let dirs: Vec<PathBuf> = (0..4)
            .map(|k| root.join(format!("incr{round}_{k}")))
            .collect();
        let shared = &db;
        std::thread::scope(|s| {
            for (k, dir) in dirs.iter().enumerate() {
                s.spawn(move || shared.backup_incremental(dir).unwrap());
                let snap = root.join(format!("snap{round}_{k}"));
                s.spawn(move || shared.save(snap).unwrap());
                s.spawn(move || shared.sync().unwrap());
                s.spawn(move || shared.checkpoint().unwrap());
            }
        });
        incrementals.extend(dirs);
        // Every concurrent save is a complete, openable snapshot.
        for k in 0..4 {
            let copy = Database::open(root.join(format!("snap{round}_{k}"))).unwrap();
            assert_eq!(copy.triple_num(), db.triple_num());
        }
    }
    // The increments chain in the order they were taken (by base offset).
    let mut chain: Vec<(u64, u64, PathBuf)> = incrementals
        .into_iter()
        .map(|d| {
            let (b, e) = read_incremental_marker(&d.join(INCREMENTAL_MARKER_FILE)).unwrap();
            (b, e, d)
        })
        .collect();
    chain.sort_by_key(|&(b, e, _)| (b, e));
    for w in chain.windows(2) {
        assert_eq!(w[0].1, w[1].0, "a gap or overlap in the increment chain");
    }
    let dirs: Vec<PathBuf> = chain.into_iter().map(|(_, _, d)| d).collect();
    let mut restored = Database::restore_incremental(root.join("full"), &dirs).unwrap();
    assert_eq!(restored.triple_num(), db.triple_num());
    let q = "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o } ORDER BY ?s";
    assert_eq!(restored.query(q).unwrap(), db.query(q).unwrap());
    drop(db);
    let _ = fs::remove_dir_all(&root);
}

/// Readers under a shared lock and a writer under the exclusive lock, with
/// the query cache on: every answer matches the generation the reader saw.
#[test]
fn generation_oracle_under_a_rwlock() {
    let mut db =
        Database::build_from_str("oracle", "<http://ex/s> <http://ex/k> \"0\" .\n").unwrap();
    db.set_query_cache_size(DEFAULT_QUERY_CACHE_BYTES);
    let lock = RwLock::new(db);
    let done = AtomicBool::new(false);
    let checked = AtomicU64::new(0);
    let count_q = "SELECT ?o WHERE { <http://ex/s> <http://ex/v> ?o }";
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    let db = lock.read().unwrap();
                    // One insert per generation (the base data is generation 0).
                    let generation = db.mutation_count();
                    let QueryResult::Select(rs) = db.query_read(count_q, None, None).unwrap()
                    else {
                        panic!("SELECT expected");
                    };
                    assert_eq!(rs.row_count() as u64, generation, "stale answer");
                    // Twice: the second is usually a cache hit.
                    let again = db.query_read(count_q, None, None).unwrap();
                    assert_eq!(again, QueryResult::Select(rs));
                    checked.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        s.spawn(|| {
            for i in 0..60 {
                let mut db = lock.write().unwrap();
                db.query(&format!(
                    "INSERT DATA {{ <http://ex/s> <http://ex/v> \"{i}\" }}"
                ))
                .unwrap();
                drop(db);
                std::thread::yield_now();
            }
            done.store(true, Ordering::Release);
        });
    });
    assert!(checked.load(Ordering::Relaxed) > 0);
    assert_eq!(lock.read().unwrap().mutation_count(), 60);
}

/// A user function that reports a read error fails only the query it runs
/// in; that query's result is not cached.
#[test]
fn a_read_error_fails_only_its_own_statement() {
    let mut db = prepared();
    db.register_function("faulty", |_| {
        crate::store::read_scope::note_read_error("injected");
        Some(crate::query::Value::Bool(true))
    });
    let bad = format!("SELECT ?x WHERE {{ ?x <{UB}memberOf> <{DEPT}> FILTER(faulty(?x)) }}");
    let good = format!("SELECT ?x WHERE {{ ?x <{UB}memberOf> <{DEPT}> }}");
    let expected = db.query_read(&good, None, None).unwrap();
    let db = &db;
    std::thread::scope(|s| {
        let a = s.spawn(|| db.query_read(&bad, None, None));
        let b = s.spawn(|| db.query_read(&good, None, None));
        let err = a.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert_eq!(b.join().unwrap().unwrap(), expected);
    });
}
