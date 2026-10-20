//! Regression tests for the Phase 0 data-safety review: crash-safe removal of
//! a replaced `rocksdb/` directory, and — with the `rocksdb` feature — that a
//! failed RocksDB side write (dictionary term, named-graph quad, undo journal)
//! never leaves the in-memory state ahead of what RocksDB holds.
//!
//! Failures are injected with test-only failpoints
//! ([`fsutil::CRASH_BEFORE_DISCARD_DELETE`], `RocksStore::inject_failures`).

use std::path::PathBuf;

use super::fsutil;
use super::{Database, StorageKind, ROCKS_SUBDIR};

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "gstore_review_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Saving a snapshot over a directory that held a RocksDB store commits the
/// snapshot first and then removes `rocksdb/`. A crash during that removal
/// must not leave a half-deleted `rocksdb/` — which layout detection would
/// prefer over the committed snapshot — so the store is renamed out of the way
/// before it is deleted.
#[test]
fn crash_while_removing_a_replaced_rocksdb_dir_keeps_the_new_snapshot() {
    let dir = tmp_dir("rocks_discard");
    // A (stand-in) RocksDB store from an earlier build of this database.
    let rocks = dir.join(ROCKS_SUBDIR);
    std::fs::create_dir_all(&rocks).unwrap();
    for f in ["CURRENT", "MANIFEST-000005", "000008.log", "000010.sst"] {
        std::fs::write(rocks.join(f), b"old store").unwrap();
    }
    assert_eq!(Database::storage_kind(&dir), Some(StorageKind::RocksDb));

    let db = Database::build_from_str("r", "<http://ex/a> <http://ex/p> <http://ex/b> .").unwrap();
    fsutil::CRASH_BEFORE_DISCARD_DELETE.with(|c| c.set(true));
    let saved = db.save(&dir);
    fsutil::CRASH_BEFORE_DISCARD_DELETE.with(|c| c.set(false));
    assert!(saved.is_err(), "the simulated crash interrupts the save");

    // What a restart finds: the committed snapshot, not the old store.
    assert!(!rocks.exists(), "rocksdb/ was renamed away before deletion");
    assert_eq!(Database::storage_kind(&dir), Some(StorageKind::Snapshot));
    let mut back = Database::open(&dir).unwrap();
    assert_eq!(back.triple_num(), 1);
    assert_eq!(
        back.query("ASK { <http://ex/a> <http://ex/p> <http://ex/b> }").unwrap(),
        crate::QueryResult::Ask(true)
    );
    assert!(
        !fsutil::with_suffix(&rocks, ".old").exists(),
        "the leftover is cleaned up on open"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "rocksdb")]
mod rocks {
    use std::sync::atomic::Ordering;

    use super::tmp_dir;
    use crate::{Database, QueryResult};

    const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                        <http://ex/g> <http://ex/p> <http://ex/b> .\n";

    fn open_rocks(tag: &str, content: &str) -> (std::path::PathBuf, Database) {
        let dir = tmp_dir(tag);
        drop(Database::build_rocksdb_from_str("r", &dir, content).unwrap());
        let db = Database::open(&dir).unwrap();
        (dir, db)
    }

    fn fail_side_writes(db: &Database, on: bool) {
        db.store
            .as_rocks()
            .unwrap()
            .inject_failures()
            .side_writes
            .store(on, Ordering::SeqCst);
    }

    fn rows(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
        let mut out = db.select(q).unwrap().rows;
        out.sort();
        out
    }

    /// A dictionary-term write that fails is not half-applied: the term is not
    /// left known in memory, so the retry persists it — and after a reopen the
    /// triple still decodes (instead of pointing at a freed id that the next
    /// new term would reuse).
    #[test]
    fn failed_term_write_is_persisted_by_the_retry() {
        let (dir, mut db) = open_rocks("rocks_term_retry", BASE);
        let insert = "INSERT DATA { <http://new> <http://ex/p> <http://o1> }";
        fail_side_writes(&db, true);
        assert!(db.query(insert).is_err(), "the failed write is reported");
        fail_side_writes(&db, false);
        assert_eq!(db.query(insert).unwrap(), QueryResult::Update { changed: 1 });
        drop(db);

        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            rows(&mut db, "SELECT ?o WHERE { <http://new> <http://ex/p> ?o }"),
            vec![vec![Some("<http://o1>".to_string())]]
        );
        let all = rows(&mut db, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }");
        assert!(all.iter().flatten().all(Option::is_some), "{all:?}");
        // A brand-new term must not inherit an orphaned triple.
        db.query("INSERT DATA { <http://zzz> <http://q> <http://yyy> }")
            .unwrap();
        assert_eq!(
            rows(&mut db, "SELECT ?o WHERE { <http://zzz> ?p ?o }"),
            vec![vec![Some("<http://yyy>".to_string())]]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A named-graph quad whose RocksDB write fails is not applied in memory
    /// either, so the retry writes it (instead of finding it "already present"
    /// and silently never persisting it).
    #[test]
    fn failed_quad_write_is_persisted_by_the_retry() {
        let (dir, mut db) = open_rocks("rocks_quad_retry", BASE);
        // Every term is known already: only the quad write itself can fail.
        let insert = "INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/b> } }";
        fail_side_writes(&db, true);
        assert!(db.query(insert).is_err(), "the failed write is reported");
        assert!(
            rows(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }").is_empty(),
            "a quad RocksDB does not hold is not visible either"
        );
        fail_side_writes(&db, false);
        assert_eq!(db.query(insert).unwrap(), QueryResult::Update { changed: 1 });
        drop(db);

        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            rows(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"),
            vec![vec![Some("<http://ex/a>".to_string())]]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reasoning may intern new terms (here `rdf:type`, which the data never
    /// used). Their dictionary entries must reach RocksDB before any inferred
    /// triple that uses them: if they cannot be written, no such triple is
    /// written either, so a reopen never finds undecodable triples.
    #[test]
    fn inference_never_persists_triples_before_their_terms() {
        let data = "<http://ex/p> <http://www.w3.org/2000/01/rdf-schema#domain> <http://ex/C> .\n\
                    <http://ex/x1> <http://ex/p> <http://ex/y> .\n\
                    <http://ex/x2> <http://ex/p> <http://ex/y> .\n";
        let (dir, mut db) = open_rocks("rocks_infer_terms", data);
        fail_side_writes(&db, true);
        assert_eq!(db.materialize_rdfs(), 0, "nothing inferred without its terms");
        assert!(db.sync().is_err(), "the failed term write is reported");
        drop(db); // what a crash right now would leave behind

        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.triple_num(), 3, "no orphaned inferred triples");
        let all = rows(&mut db, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }");
        assert!(all.iter().flatten().all(Option::is_some), "{all:?}");
        // Once the terms can be written, reasoning persists everything.
        assert!(db.materialize_rdfs() > 0);
        drop(db);
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            db.query("ASK { <http://ex/x1> a <http://ex/C> }").unwrap(),
            QueryResult::Ask(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A term a failed reasoning run interned but could not write (here
    /// `rdf:type`) is written by the next insert that uses it, rather than
    /// being taken as already persisted.
    #[test]
    fn a_term_whose_inference_write_failed_is_written_by_the_next_insert() {
        let data = "<http://ex/p> <http://www.w3.org/2000/01/rdf-schema#domain> <http://ex/C> .\n\
                    <http://ex/x1> <http://ex/p> <http://ex/y> .\n";
        let (dir, mut db) = open_rocks("rocks_infer_term_retry", data);
        fail_side_writes(&db, true);
        assert_eq!(db.materialize_rdfs(), 0);
        fail_side_writes(&db, false);
        let _ = db.sync(); // collect the reported failure
        db.query("INSERT DATA { <http://ex/z> a <http://ex/C> }").unwrap();
        drop(db);
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            db.query("ASK { <http://ex/z> a <http://ex/C> }").unwrap(),
            QueryResult::Ask(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// If deleting a committed transaction's undo journal fails, the journal
    /// must not be replayed later (that would undo the commit): writes are
    /// refused until a retry deletes it, and then the commit stands.
    #[test]
    fn stale_journal_after_a_failed_commit_blocks_writes_until_cleared() {
        let (dir, mut db) = open_rocks("rocks_stale_journal", BASE);
        db.begin().unwrap();
        db.query("INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/u> }")
            .unwrap();
        let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
        fp.store(true, Ordering::SeqCst);
        assert!(db.commit().is_err(), "the commit cannot be made final yet");
        assert!(
            db.query("INSERT DATA { <http://ex/v> <http://ex/p> <http://ex/w> }")
                .is_err(),
            "writes are refused while the stale journal exists"
        );
        let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
        fp.store(false, Ordering::SeqCst);
        assert_eq!(
            db.query("INSERT DATA { <http://ex/v> <http://ex/p> <http://ex/w> }")
                .unwrap(),
            QueryResult::Update { changed: 1 },
            "the next write clears the journal first"
        );
        drop(db);

        let mut db = Database::open(&dir).unwrap();
        for (s, o) in [("t", "u"), ("v", "w")] {
            let q = format!("ASK {{ <http://ex/{s}> <http://ex/p> <http://ex/{o}> }}");
            assert_eq!(db.query(&q).unwrap(), QueryResult::Ask(true), "{q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rolling back replays the journal: the default graph and the named
    /// graphs (memory and RocksDB) are back to the begin-time state.
    #[test]
    fn rollback_replays_the_journal_including_named_graphs() {
        let (dir, mut db) = open_rocks("rocks_rollback_journal", BASE);
        db.begin().unwrap();
        db.query(
            "DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> } ; \
             INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/u> . \
                           GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/b> } }",
        )
        .unwrap();
        db.rollback().unwrap();
        let check = |db: &mut Database| {
            assert_eq!(db.triple_num(), 2);
            assert_eq!(
                db.query("ASK { <http://ex/a> <http://ex/p> <http://ex/b> }").unwrap(),
                QueryResult::Ask(true)
            );
            assert_eq!(
                db.query("ASK { <http://ex/t> <http://ex/p> <http://ex/u> }").unwrap(),
                QueryResult::Ask(false)
            );
            assert_eq!(
                db.query("ASK { GRAPH <http://ex/g> { ?s ?p ?o } }").unwrap(),
                QueryResult::Ask(false)
            );
        };
        check(&mut db);
        drop(db);
        let mut db = Database::open(&dir).unwrap();
        check(&mut db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
