//! Data test (Phase 6 R3-STREAM): transactions on a Stream-mode on-disk
//! database. The default graph's transaction is the on-disk store's overlay
//! (`DiskTriples`): read-your-writes, nothing of it reaches `kvstore.kv`
//! before `commit`, `rollback` never touches the store, a commit survives a
//! reopen in either mode, named-graph quads ride along, and an injected
//! commit failure leaves the database broken (every statement refused)
//! until it is reopened.

use std::fs;
use std::path::{Path, PathBuf};

use gstore::{Database, DiskQueryMode, OpenOptions};

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
    let d = std::env::temp_dir().join(format!("gstore_dt_stxn_{tag}_{}", std::process::id()));
    TMP_DIRS.with(|t| t.0.borrow_mut().push(d.clone()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const BASE: &str = "\
<http://ex/a> <http://ex/p> <http://ex/b> .
<http://ex/b> <http://ex/p> <http://ex/c> .
<http://ex/a> <http://ex/name> \"Alice\" .
<http://ex/a> <http://ex/p> <http://ex/c> <http://ex/g1> .
";

/// A bulk-built disk database of `BASE` in `<root>/d.db`.
fn build(root: &Path) -> PathBuf {
    let src = root.join("base.nq");
    fs::write(&src, BASE).unwrap();
    let dir = root.join("d.db");
    Database::build_disk(&dir, &[&src]).unwrap();
    dir
}

fn open(dir: &Path, mode: DiskQueryMode) -> Database {
    let db = Database::open_with(dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap();
    assert_eq!(db.disk_query_mode(), Some(mode));
    db
}

fn stream(dir: &Path) -> Database {
    open(dir, DiskQueryMode::Stream)
}

fn sorted(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    let mut rows = db.select(q).unwrap().rows;
    rows.sort();
    rows
}

const ALL: &str = "SELECT ?s ?p ?o WHERE { ?s ?p ?o }";
const QUADS: &str = "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }";

/// The bytes of `kvstore.kv` and whether its WAL exists.
fn on_disk(dir: &Path) -> (Vec<u8>, bool) {
    (
        fs::read(dir.join("kvstore.kv")).unwrap(),
        dir.join("kvstore.kv.wal").exists(),
    )
}

#[test]
fn read_your_writes_and_rollback_leave_the_store_untouched() {
    let root = tmp("ryw");
    let dir = build(&root);
    let mut db = stream(&dir);
    let all0 = sorted(&mut db, ALL);
    let quads0 = sorted(&mut db, QUADS);
    let disk0 = on_disk(&dir);

    db.begin().unwrap();
    db.query(
        "INSERT DATA { <http://ex/new> <http://ex/fresh> \"brand new\" . \
         GRAPH <http://ex/g2> { <http://ex/new> <http://ex/p> <http://ex/a> } }",
    )
    .unwrap();
    db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    db.query("DELETE WHERE { GRAPH <http://ex/g1> { ?s ?p ?o } }")
        .unwrap();
    // Read-your-writes, new terms included (they were interned on disk).
    assert_eq!(
        sorted(
            &mut db,
            "SELECT ?o WHERE { <http://ex/new> <http://ex/fresh> ?o }"
        ),
        vec![vec![Some("\"brand new\"".to_string())]]
    );
    assert!(sorted(
        &mut db,
        "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"
    )
    .is_empty());
    assert_eq!(db.triple_num(), 3);
    assert_eq!(
        sorted(&mut db, QUADS),
        vec![vec![
            Some("<http://ex/g2>".to_string()),
            Some("<http://ex/new>".to_string()),
            Some("<http://ex/p>".to_string()),
            Some("<http://ex/a>".to_string()),
        ]]
    );
    let in_txn = db.mutation_count();
    // A commit point inside the transaction (a checkpoint) writes none of
    // its triples.
    db.sync().unwrap();
    assert_eq!(
        on_disk(&dir).0,
        disk0.0,
        "no triple reaches the pager before commit"
    );

    db.rollback().unwrap();
    assert!(db.mutation_count() > in_txn, "a rollback is a change");
    assert_eq!(sorted(&mut db, ALL), all0);
    assert_eq!(sorted(&mut db, QUADS), quads0);
    assert_eq!(on_disk(&dir).0, disk0.0, "rollback never touches the store");
    drop(db);
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let mut db = open(&dir, mode);
        assert_eq!(sorted(&mut db, ALL), all0, "{mode}");
        assert_eq!(sorted(&mut db, QUADS), quads0, "{mode}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_commit_survives_a_reopen_in_either_mode() {
    let root = tmp("commit");
    let dir = build(&root);
    let mut db = stream(&dir);
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/x> <http://ex/q> <http://ex/y> }")
        .unwrap();
    db.query("INSERT DATA { GRAPH <http://ex/g1> { <http://ex/x> <http://ex/q> \"z\" } }")
        .unwrap();
    db.query("DELETE DATA { <http://ex/b> <http://ex/p> <http://ex/c> }")
        .unwrap();
    db.commit().unwrap();
    let all = sorted(&mut db, ALL);
    let quads = sorted(&mut db, QUADS);
    assert_eq!(all.len(), 3);
    assert_eq!(quads.len(), 2);
    drop(db);
    for mode in [
        DiskQueryMode::Materialize,
        DiskQueryMode::Stream,
        DiskQueryMode::Materialize,
    ] {
        let mut db = open(&dir, mode);
        assert_eq!(sorted(&mut db, ALL), all, "{mode}");
        assert_eq!(sorted(&mut db, QUADS), quads, "{mode}");
        // Each mode writes the same file the other reads.
        db.query(&format!(
            "INSERT DATA {{ <http://ex/by-{mode}> <http://ex/q> <http://ex/y> }}"
        ))
        .unwrap();
        db.query(&format!(
            "DELETE DATA {{ <http://ex/by-{mode}> <http://ex/q> <http://ex/y> }}"
        ))
        .unwrap();
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_large_transaction_commits_and_reopens() {
    let root = tmp("large");
    let dir = build(&root);
    let mut db = stream(&dir);
    let mut oracle = Database::build_from_files("o", &[root.join("base.nq")]).unwrap();
    db.begin().unwrap();
    oracle.begin().unwrap();
    for chunk in 0..25 {
        let mut ins = String::from("INSERT DATA {");
        for i in 0..200 {
            let n = chunk * 200 + i;
            ins.push_str(&format!(
                " <http://ex/s{n}> <http://ex/p{}> <http://ex/o{}> . <http://ex/s{n}> <http://ex/v> \"{n}\" .",
                n % 7,
                n % 113
            ));
        }
        ins.push('}');
        db.query(&ins).unwrap();
        oracle.query(&ins).unwrap();
    }
    let del = "DELETE WHERE { ?s <http://ex/p3> ?o }";
    db.query(del).unwrap();
    oracle.query(del).unwrap();
    assert_eq!(db.triple_num(), oracle.triple_num());
    db.commit().unwrap();
    oracle.commit().unwrap();
    let want = sorted(&mut oracle, ALL);
    assert!(want.len() > 9000, "{}", want.len());
    assert_eq!(sorted(&mut db, ALL), want);
    drop(db);
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let mut db = open(&dir, mode);
        assert_eq!(sorted(&mut db, ALL), want, "{mode}");
        assert_eq!(
            sorted(
                &mut db,
                "SELECT ?s WHERE { ?s <http://ex/p5> <http://ex/o5> }"
            ),
            sorted(
                &mut oracle,
                "SELECT ?s WHERE { ?s <http://ex/p5> <http://ex/o5> }"
            )
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn named_graphs_follow_the_transaction() {
    let root = tmp("named");
    let dir = build(&root);
    let mut db = stream(&dir);
    let quads0 = sorted(&mut db, QUADS);
    db.begin().unwrap();
    db.query("INSERT DATA { GRAPH <http://ex/g3> { <http://ex/a> <http://ex/p> \"q\" } }")
        .unwrap();
    db.query("CLEAR GRAPH <http://ex/g1>").unwrap();
    db.query("COPY <http://ex/g3> TO <http://ex/g4>").unwrap();
    assert_eq!(sorted(&mut db, QUADS).len(), 2);
    db.rollback().unwrap();
    assert_eq!(sorted(&mut db, QUADS), quads0);

    db.begin().unwrap();
    db.query("INSERT DATA { GRAPH <http://ex/g3> { <http://ex/a> <http://ex/p> \"q\" } }")
        .unwrap();
    db.query("MOVE <http://ex/g1> TO <http://ex/g5>").unwrap();
    db.commit().unwrap();
    let quads = sorted(&mut db, QUADS);
    assert_eq!(quads.len(), 2);
    assert!(quads
        .iter()
        .all(|r| r[0].as_deref() != Some("<http://ex/g1>")));
    drop(db);
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let mut db = open(&dir, mode);
        assert_eq!(sorted(&mut db, QUADS), quads, "{mode}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_injected_commit_failure_breaks_the_database_until_reopen() {
    let root = tmp("broken");
    let dir = build(&root);
    let mut db = stream(&dir);
    let all0 = sorted(&mut db, ALL);
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/x> <http://ex/q> <http://ex/y> . <http://ex/x> <http://ex/q> <http://ex/z> }")
        .unwrap();
    // The commit's second pager write fails (the first is applied).
    db.backend()
        .as_disk()
        .expect("a Stream-mode database")
        .debug_fail_write_after(1);
    let err = db.commit().unwrap_err().to_string();
    assert!(err.contains("injected"), "{err}");
    assert!(!db.in_transaction(), "the transaction is gone");
    // Every later statement is refused until the database is reopened.
    for q in [ALL, "ASK {}"] {
        let e = db.query(q).unwrap_err().to_string();
        assert!(e.contains("broken"), "{q}: {e}");
    }
    let e = db
        .query("INSERT DATA { <http://ex/w> <http://ex/q> <http://ex/y> }")
        .unwrap_err()
        .to_string();
    assert!(e.contains("broken"), "{e}");
    assert!(db.sync().is_err(), "no commit point claims durability");
    assert!(db.begin().is_err());
    drop(db);
    // Reopened: the committed state before the transaction (the store was
    // never flushed after the failure).
    let mut db = stream(&dir);
    assert_eq!(sorted(&mut db, ALL), all0);
    db.query("INSERT DATA { <http://ex/x> <http://ex/q> <http://ex/y> }")
        .unwrap();
    assert_eq!(db.triple_num(), 4);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reasoning_inside_a_transaction_rolls_back() {
    let root = tmp("reason");
    let dir = build(&root);
    let mut db = stream(&dir);
    let all0 = sorted(&mut db, ALL);
    db.begin().unwrap();
    let added = db.materialize_rdfs();
    db.query(
        "INSERT DATA { <http://ex/p> <http://www.w3.org/2000/01/rdf-schema#domain> <http://ex/Thing> }",
    )
    .unwrap();
    let typed = db.materialize_rdfs();
    assert!(added + typed > 0);
    assert!(db.triple_num() > all0.len() as u64);
    db.rollback().unwrap();
    assert_eq!(sorted(&mut db, ALL), all0);
    drop(db);
    let mut db = stream(&dir);
    assert_eq!(sorted(&mut db, ALL), all0);
    let _ = fs::remove_dir_all(&root);
}
