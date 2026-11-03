//! Regression tests for the Phase 6 Round 4 review of id reclamation: a
//! Stream-mode database held by a dictionary pin re-checked its whole
//! deferred backlog at every statement (and a `ConcurrentDb` over one never
//! freed, losing the backlog when dropped), a read error during the check
//! dropped the candidates for good, a `CLEAR` RocksDB partly refused was
//! logged as a full clear, and RocksDB journal replays (every rollback) left
//! their terms in a buffer nothing emptied after open.

use std::path::{Path, PathBuf};

use super::open_options::{DiskQueryMode, OpenOptions};
use super::reclaim::test_hooks;
use super::*;
use crate::concurrent::ConcurrentDb;

/// The temp directories the current test thread made, removed when the
/// thread ends — also when the test fails (a thread-local's destructor runs
/// after the unwinding).
struct TmpDirs(std::cell::RefCell<Vec<PathBuf>>);

impl Drop for TmpDirs {
    fn drop(&mut self) {
        for p in self.0.borrow().iter() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

thread_local! {
    static TMP_DIRS: TmpDirs = const { TmpDirs(std::cell::RefCell::new(Vec::new())) };
}

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_r4_review_{tag}_{}", std::process::id()));
    TMP_DIRS.with(|t| t.0.borrow_mut().push(d.clone()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Build an on-disk database `<base>/<name>` from N-Triples `text`.
fn build_disk(base: &Path, name: &str, text: &str) -> PathBuf {
    let nt = base.join(format!("{name}.nt"));
    std::fs::write(&nt, text).unwrap();
    let dir = base.join(name);
    Database::build_disk(&dir, &[&nt]).unwrap();
    dir
}

fn open_stream(dir: &Path) -> Database {
    let opts = OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream);
    let db = Database::open_with(dir, &opts).unwrap();
    assert!(db.store.as_disk().is_some(), "opened in Stream mode");
    db
}

fn iri(s: &str) -> Term {
    Term::iri(format!("http://ex/{s}"))
}

/// `<s{i}> <p> <o{i}>`.
fn pair(i: usize) -> Triple {
    Triple::new(iri(&format!("s{i}")), iri("p"), iri(&format!("o{i}")))
}

/// N-Triples of `pair(0..n)`.
fn pairs(n: usize) -> String {
    (0..n)
        .map(|i| format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"))
        .collect()
}

fn known(db: &Database, s: &str) -> bool {
    db.dict()
        .entity_id(&iri(s).dict_key())
        .is_some_and(|id| db.dict().term(id).is_some())
}

/// While a pin older than every candidacy holds a Stream-mode database, a
/// statement checks none of the waiting candidates (it used to re-probe the
/// whole backlog — O(backlog) B+-tree probes per statement, quadratic over a
/// run); once the pin is gone, each is checked exactly once and freed.
#[test]
fn a_pinned_stream_database_checks_its_backlog_once() {
    let base = tmp_dir("pinned_backlog");
    let n = 120;
    let dir = build_disk(&base, "d", &pairs(n));
    let mut db = open_stream(&dir);
    let pin = db.dict().clone();
    let before = test_hooks::probes();
    for i in 0..n {
        assert!(db.remove_triple(&pair(i)));
    }
    assert_eq!(
        test_hooks::probes() - before,
        0,
        "no candidate is checked while a pin older than it lives"
    );
    assert_eq!(db.reclaim_deferred.len(), 2 * n + 1, "s_i, o_i and p wait");
    assert_eq!(db.id_space_stats().live[0], 2 * n as u64, "nothing freed");
    drop(pin);

    let before = test_hooks::probes();
    db.query("INSERT DATA { <http://ex/x> <http://ex/q> <http://ex/y> }")
        .unwrap();
    assert_eq!(
        test_hooks::probes() - before,
        2 * n as u64 + 1,
        "each waiting candidate is checked once"
    );
    assert!(!db.has_deferred_frees());
    assert_eq!(db.id_space_stats().live, [2, 0, 1]);
    db.verify_integrity().unwrap();
    assert_eq!(db.reclaim_unused().unwrap(), 0, "no orphan left");
}

/// Only a pin taken before a term's candidacy holds its free: a reader that
/// pinned the dictionary afterwards never saw the term in use.
#[test]
fn only_a_pin_older_than_a_candidacy_holds_its_free() {
    let base = tmp_dir("pin_age");
    let dir = build_disk(
        &base,
        "d",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/c> <http://ex/p> <http://ex/d> .\n",
    );
    let mut db = open_stream(&dir);
    let a = db.dict().entity_id(&iri("a").dict_key()).unwrap();
    let old = db.dict().clone();
    db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert!(known(&db, "a"), "the older pin holds a");
    assert_eq!(
        old.term(a).map(|t| t.to_string()),
        Some("<http://ex/a>".to_string()),
        "and it still reads a under a's id"
    );
    let young = db.dict().clone();
    drop(old);
    db.query("INSERT DATA { <http://ex/e> <http://ex/p> <http://ex/f> }")
        .unwrap();
    assert!(
        !known(&db, "a") && !known(&db, "b"),
        "a pin younger than the candidacy does not hold it"
    );
    db.query("DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/d> }")
        .unwrap();
    assert!(
        known(&db, "c"),
        "the younger pin is older than c's candidacy"
    );
    drop(young);
    db.query("INSERT DATA { <http://ex/g> <http://ex/p> <http://ex/h> }")
        .unwrap();
    assert!(!known(&db, "c") && !known(&db, "d"));
    db.verify_integrity().unwrap();
    assert_eq!(db.reclaim_unused().unwrap(), 0, "no orphan left");
}

/// A read error while checking whether the candidates are still referenced
/// frees none of them — and keeps them, the backlog a pin deferred too, for
/// the next statement's check (they used to stay allocated until `gcompact
/// --reclaim`).
#[test]
fn a_read_error_in_the_reference_check_keeps_the_candidates() {
    const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                        <http://ex/x> <http://ex/q> <http://ex/y> .\n\
                        <http://ex/z> <http://ex/q> <http://ex/w> .\n";
    let base = tmp_dir("check_error");
    for stream in [false, true] {
        let mut db = if stream {
            open_stream(&build_disk(&base, "d", DATA))
        } else {
            Database::build_from_str("m", DATA).unwrap()
        };
        let pin = db.dict().clone();
        db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
            .unwrap();
        // In memory the pin only delays the reuse: a is retired at once.
        assert_eq!(known(&db, "a"), stream, "stream={stream}");
        drop(pin);
        // One delete, one check (`DELETE DATA` checks after the operation
        // and after the statement: the second check would retry at once).
        test_hooks::fail_next_check();
        assert!(db.remove_triple(&Triple::new(iri("x"), iri("q"), iri("y"))));
        assert!(
            known(&db, "x"),
            "nothing of the failed check is freed (stream={stream})"
        );
        assert!(
            db.has_deferred_frees(),
            "the candidates wait (stream={stream})"
        );
        db.query("INSERT DATA { <http://ex/m> <http://ex/q> <http://ex/n> }")
            .unwrap();
        for t in ["a", "b", "x", "y"] {
            assert!(
                !known(&db, t),
                "{t} freed by the next check (stream={stream})"
            );
        }
        assert!(!db.has_deferred_frees());
        db.verify_integrity().unwrap();
        assert_eq!(
            db.reclaim_unused().unwrap(),
            0,
            "no orphan (stream={stream})"
        );
    }
}

/// A `ConcurrentDb` over a Stream-mode database: its base snapshot pins the
/// dictionary for the wrapper's life, so updates defer every free — without
/// re-checking the backlog each time — and dropping the wrapper runs them
/// (they used to be lost, the terms left as orphans on disk).
#[test]
fn a_concurrent_stream_database_frees_its_deferred_ids_when_dropped() {
    let base = tmp_dir("conc_drop");
    let n = 40;
    let dir = build_disk(&base, "d", &pairs(n));
    let cdb = ConcurrentDb::new(open_stream(&dir));
    let before = test_hooks::probes();
    for i in 0..n {
        let q = format!("DELETE DATA {{ <http://ex/s{i}> <http://ex/p> <http://ex/o{i}> }}");
        assert_eq!(cdb.update(&q).unwrap(), 1);
    }
    assert_eq!(
        test_hooks::probes() - before,
        0,
        "the backlog is not re-checked"
    );
    assert_eq!(cdb.write(|db| db.id_space_stats().live[0]), 2 * n as u64);
    drop(cdb);
    let db = open_stream(&dir);
    assert_eq!(db.id_space_stats().live, [0, 0, 0], "freed at the drop");
    db.verify_integrity().unwrap();
}

#[cfg(feature = "rocksdb")]
mod rocks {
    use std::sync::atomic::Ordering;

    use super::*;

    const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                        <http://ex/g> <http://ex/p> <http://ex/b> .\n";

    fn open_rocks(tag: &str) -> (PathBuf, Database) {
        let dir = tmp_dir(tag);
        drop(Database::build_rocksdb_from_str("r", &dir, BASE).unwrap());
        let db = Database::open(&dir).unwrap();
        (dir, db)
    }

    /// Every default-graph triple and named-graph quad, sorted.
    fn quads(db: &mut Database) -> Vec<Vec<Option<String>>> {
        let mut rows = db
            .select("SELECT ?g ?s ?p ?o WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }")
            .unwrap()
            .rows;
        rows.sort();
        rows
    }

    /// `db`'s redo log replayed onto an in-memory database in the state the
    /// log began from (`BASE` plus `setup`).
    fn replayed(log: &Path, setup: &[&str]) -> Vec<Vec<Option<String>>> {
        let mut copy = Database::build_from_str("copy", BASE).unwrap();
        for u in setup {
            copy.query(u).unwrap();
        }
        if log.exists() {
            copy.replay_redo_log(log).unwrap();
        }
        quads(&mut copy)
    }

    /// A `CLEAR` RocksDB partly refused is logged as what it removed, not
    /// as a clear: replaying the redo log reproduces the live state (it used
    /// to replay to an empty graph while the live one kept its triples).
    #[test]
    fn a_clear_rocksdb_refused_logs_only_what_it_removed() {
        // Named graph: RocksDB refuses the quad deletes.
        let setup =
            "INSERT DATA { GRAPH <http://ex/G> { <http://ex/a> <http://ex/p> <http://ex/b> . \
                     <http://ex/c> <http://ex/p> <http://ex/d> } }";
        let (dir, mut db) = open_rocks("clear_redo_named");
        db.query(setup).unwrap();
        let log = dir.join("redo-test.log");
        db.enable_redo_log(&log);
        let fp = &db.store.as_rocks().unwrap().inject_failures().side_writes;
        fp.store(true, Ordering::SeqCst);
        assert!(db.query("CLEAR GRAPH <http://ex/G>").is_err());
        assert!(db.query("CLEAR NAMED").is_err());
        let fp = &db.store.as_rocks().unwrap().inject_failures().side_writes;
        fp.store(false, Ordering::SeqCst);
        let live = quads(&mut db);
        assert_eq!(live.len(), 4, "the graph kept its quads: {live:?}");
        assert_eq!(replayed(&log, &[setup]), live);

        // Default graph: a stale undo journal refuses every write.
        let (dir, mut db) = open_rocks("clear_redo_default");
        let setup = "INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/u> }";
        db.begin().unwrap();
        db.query(setup).unwrap();
        let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
        fp.store(true, Ordering::SeqCst);
        assert!(db.commit().is_err(), "the journal stays stale");
        let log = dir.join("redo-test.log");
        db.enable_redo_log(&log);
        assert!(db.query("CLEAR DEFAULT").is_err());
        let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
        fp.store(false, Ordering::SeqCst);
        let live = quads(&mut db);
        assert_eq!(
            live.len(),
            3,
            "the default graph kept its triples: {live:?}"
        );
        assert_eq!(replayed(&log, &[setup]), live);
    }

    /// Rolled-back transactions leave nothing in the RocksDB store's
    /// replayed-terms buffer (it grew by 3–4 entries per rolled-back insert
    /// for the life of the store) and no orphan term.
    #[test]
    fn rollbacks_leave_no_replayed_terms_behind() {
        let (_dir, mut db) = open_rocks("replayed_drained");
        for r in 0..5 {
            db.begin().unwrap();
            for j in 0..10 {
                db.query(&format!(
                    "INSERT DATA {{ <http://ex/s{r}_{j}> <http://ex/p{j}> \"lit {r} {j}\" }}"
                ))
                .unwrap();
            }
            db.rollback().unwrap();
            assert_eq!(
                db.store.as_rocks().unwrap().take_replayed().len(),
                0,
                "round {r}"
            );
        }
        assert_eq!(db.triple_num(), 2);
        assert_eq!(
            db.reclaim_unused().unwrap(),
            0,
            "the minted terms were freed"
        );
    }

    /// A rollback whose journal replay failed is finished by the next
    /// heal (here `begin`); the terms that transaction minted are then freed
    /// (they used to stay as orphans until `gcompact --reclaim`).
    #[test]
    fn a_rollback_finished_by_a_later_heal_frees_the_minted_terms() {
        let (_dir, mut db) = open_rocks("heal_frees");
        db.begin().unwrap();
        db.query(
            "INSERT DATA { <http://ex/n1> <http://ex/np> \"new 1\" . \
             <http://ex/n2> <http://ex/np> <http://ex/n3> }",
        )
        .unwrap();
        db.store.as_rocks().unwrap().debug_fail_reads(true);
        assert!(db.rollback().is_err(), "the replay fails");
        db.store.as_rocks().unwrap().debug_fail_reads(false);
        db.begin().unwrap();
        db.commit().unwrap();
        assert_eq!(db.triple_num(), 2, "the rollback was finished");
        assert_eq!(db.reclaim_unused().unwrap(), 0, "no orphan left");
        assert!(!known(&db, "n1") && !known(&db, "n3"));
    }
}
