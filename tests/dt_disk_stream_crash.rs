//! Data test (Phase 6 R3-STREAM): crash consistency of a Stream-mode on-disk
//! database. `std::mem::forget` skips every destructor — no flush, no close —
//! the closest in-process stand-in for a crash: only what reached a commit
//! point may be on disk.
//!
//! - Forgotten right after its i-th acknowledged UPDATE, the database reopens
//!   (in either mode) to exactly the oracle's state after i updates.
//! - An uncommitted transaction is absent after the crash, even when a
//!   commit point (`sync`) ran while it was open.
//! - A crash between a dictionary term and the triple that uses it leaves at
//!   most an orphan term: the triple is absent, the term's id is never handed
//!   out twice, and the database works on.

use std::fs;
use std::path::{Path, PathBuf};

use gstore::dict::Space;
use gstore::{Database, DiskQueryMode, OpenOptions};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_scrash_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const BASE: &str = "\
<http://ex/a> <http://ex/p> <http://ex/b> .
<http://ex/b> <http://ex/p> <http://ex/c> .
<http://ex/a> <http://ex/name> \"Alice\" .
<http://ex/c> <http://ex/p> <http://ex/a> <http://ex/g1> .
";

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

type Rows = Vec<Vec<Option<String>>>;

fn content(db: &mut Database) -> (Rows, Rows) {
    let mut t = db
        .select("SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
        .unwrap()
        .rows;
    let mut q = db
        .select("SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }")
        .unwrap()
        .rows;
    t.sort();
    q.sort();
    (t, q)
}

/// The updates the crash test acknowledges one by one.
fn updates() -> Vec<String> {
    let mut u = vec![
        "INSERT DATA { <http://ex/n1> <http://ex/p> <http://ex/n2> }".to_string(),
        "INSERT DATA { <http://ex/n2> <http://ex/fresh> \"new literal\" }".to_string(),
        "DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }".to_string(),
        "INSERT DATA { GRAPH <http://ex/g2> { <http://ex/n1> <http://ex/q> <http://ex/n3> } }"
            .to_string(),
        "DELETE { ?s <http://ex/name> ?n } INSERT { ?s <http://ex/name> \"Alicia\" } \
         WHERE { ?s <http://ex/name> ?n }"
            .to_string(),
        "CLEAR GRAPH <http://ex/g1>".to_string(),
        "DELETE WHERE { <http://ex/n1> ?p ?o }".to_string(),
    ];
    for i in 0..5 {
        u.push(format!(
            "INSERT DATA {{ <http://ex/m{i}> <http://ex/r{i}> \"v{i}\" . <http://ex/b> <http://ex/r{i}> <http://ex/m{i}> }}"
        ));
    }
    u.push("CLEAR DEFAULT".to_string());
    u.push("INSERT DATA { <http://ex/after> <http://ex/p> <http://ex/clear> }".to_string());
    u
}

#[test]
fn a_crash_after_the_ith_acknowledged_update_keeps_exactly_i_updates() {
    let root = tmp("ith");
    let dir = build(&root);
    // The oracle starts from the same data (built from the same file).
    let mut oracle = Database::build_from_files("o", &[root.join("base.nq")]).unwrap();
    for (i, u) in updates().iter().enumerate() {
        let mode = if i % 3 == 2 {
            DiskQueryMode::Materialize
        } else {
            DiskQueryMode::Stream
        };
        let mut db = open(&dir, mode);
        assert_eq!(content(&mut db), content(&mut oracle), "before update {i}");
        db.query(u).unwrap_or_else(|e| panic!("{u}: {e}"));
        oracle.query(u).unwrap();
        // Acknowledged: now crash.
        std::mem::forget(db);
        let mut db = open(&dir, DiskQueryMode::Stream);
        assert_eq!(
            content(&mut db),
            content(&mut oracle),
            "after the crash following update {i} ({mode}): {u}"
        );
        drop(db);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_uncommitted_transaction_is_absent_after_a_crash() {
    let root = tmp("txn");
    let dir = build(&root);
    let mut db = open(&dir, DiskQueryMode::Stream);
    let before = content(&mut db);
    db.query("INSERT DATA { <http://ex/committed> <http://ex/p> <http://ex/x> }")
        .unwrap();
    let committed = content(&mut db);
    assert_ne!(committed, before);
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/in-txn> <http://ex/new-pred> \"new term\" }")
        .unwrap();
    db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    db.query(
        "INSERT DATA { GRAPH <http://ex/g9> { <http://ex/in-txn> <http://ex/p> <http://ex/x> } }",
    )
    .unwrap();
    // A commit point while the transaction is open writes none of it.
    db.sync().unwrap();
    std::mem::forget(db);
    for mode in [DiskQueryMode::Stream, DiskQueryMode::Materialize] {
        let mut db = open(&dir, mode);
        assert_eq!(content(&mut db), committed, "{mode}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_crash_between_a_term_and_its_triple_leaves_only_an_orphan_term() {
    let root = tmp("orphan");
    let dir = build(&root);
    let mut db = open(&dir, DiskQueryMode::Stream);
    let before = content(&mut db);
    let entities = db.entity_num();
    // The first half of an insert: its new term reaches the store (and a
    // commit point) — then the process dies before the triple is written.
    let backing = db
        .dict()
        .backing()
        .expect("an out-of-core dictionary")
        .clone();
    let orphan = backing.intern(Space::Entity, "<http://ex/orphan>").unwrap();
    assert_eq!(orphan as usize, entities);
    db.backend()
        .as_disk()
        .unwrap()
        .disk_store()
        .unwrap()
        .commit()
        .unwrap();
    std::mem::forget(db);
    drop(backing);

    let mut db = open(&dir, DiskQueryMode::Stream);
    assert_eq!(content(&mut db), before, "no triple came with the term");
    assert_eq!(db.entity_num(), entities + 1, "the orphan term stays");
    assert_eq!(
        db.dict().id_of(Space::Entity, "<http://ex/orphan>"),
        Some(orphan)
    );
    // The orphan is reused by its key, and the next new term gets the next id.
    db.query("INSERT DATA { <http://ex/orphan> <http://ex/p> <http://ex/brand-new> }")
        .unwrap();
    assert_eq!(
        db.dict().id_of(Space::Entity, "<http://ex/brand-new>"),
        Some(orphan + 1)
    );
    drop(db);
    let mut db = open(&dir, DiskQueryMode::Materialize);
    assert_eq!(db.triple_num(), before.0.len() as u64 + 1);
    assert_eq!(
        db.select("SELECT ?o WHERE { <http://ex/orphan> <http://ex/p> ?o }")
            .unwrap()
            .rows,
        vec![vec![Some("<http://ex/brand-new>".to_string())]]
    );
    drop(db);

    // The same through the ordinary write path: the triple's write fails
    // after its terms were interned (the store breaks), then a crash.
    let mut db = open(&dir, DiskQueryMode::Stream);
    let before = content(&mut db);
    db.backend().as_disk().unwrap().debug_fail_next_write();
    assert!(db
        .query("INSERT DATA { <http://ex/lost> <http://ex/lost-pred> \"lost\" }")
        .is_err());
    std::mem::forget(db);
    let mut db = open(&dir, DiskQueryMode::Stream);
    assert_eq!(content(&mut db), before);
    db.query("INSERT DATA { <http://ex/lost> <http://ex/lost-pred> \"found\" }")
        .unwrap();
    assert_eq!(
        db.select("SELECT ?o WHERE { <http://ex/lost> <http://ex/lost-pred> ?o }")
            .unwrap()
            .rows,
        vec![vec![Some("\"found\"".to_string())]]
    );
    drop(db);
    let _ = fs::remove_dir_all(&root);
}
