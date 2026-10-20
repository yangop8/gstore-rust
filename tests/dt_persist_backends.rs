//! Data test: persistence correctness of every storage layout.
//!
//! * On-disk (`kvstore.kv`) databases: updates made through `Database` (API,
//!   SPARQL, transactions, the `gadd`/`gsub`/`gquery` binaries) — default graph
//!   and named graphs — survive a reopen. (Regression: they used to be written
//!   to a shadow `store.bin` that was never read back.)
//! * `Database::open` layout detection, atomic snapshot saves, file-level
//!   backup/restore, `gbuild --disk` input-format dispatch.
//! * RocksDB (feature `rocksdb`): update → reopen with and without an explicit
//!   save, backup → restore, named graphs, `Snapshot::from_db`, legacy layouts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gstore::db::StorageKind;
use gstore::{Database, Term, Triple};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_pb_{tag}"));
    let _ = fs::remove_dir_all(&d);
    d
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Sorted first-column values of a SELECT.
fn col0(db: &mut Database, q: &str) -> Vec<String> {
    let mut v: Vec<String> = db
        .select(q)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

fn iri(s: &str) -> Term {
    Term::iri(format!("http://ex/{s}"))
}

fn triple(s: &str, p: &str, o: &str) -> Triple {
    Triple::new(iri(s), iri(p), iri(o))
}

const BASE_NT: &str = "\
<http://ex/a> <http://ex/p> <http://ex/b> .
<http://ex/b> <http://ex/p> <http://ex/c> .
<http://ex/a> <http://ex/name> \"Alice\" .
";

/// A disk database built from `BASE_NT` in `dir`.
fn disk_db(dir: &Path) {
    let nt = dir.with_extension("nt");
    write(&nt, BASE_NT);
    Database::build_disk(dir, &[&nt]).unwrap();
    let _ = fs::remove_file(&nt);
}

// ---- on-disk databases ------------------------------------------------------

#[test]
fn disk_api_insert_delete_survive_reopen() {
    let dir = tmp("disk_api");
    disk_db(&dir);
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.storage(), StorageKind::Disk);
        assert_eq!(db.triple_num(), 3);
        // Brand-new terms (subject, predicate, literal object) and a delete.
        assert!(db.insert_triple(&triple("x", "q", "y")));
        assert!(db.insert_triple(&Triple::new(
            iri("x"),
            iri("label"),
            Term::plain_literal("fresh literal")
        )));
        assert!(db.remove_triple(&triple("a", "p", "b")));
        db.save(&dir).unwrap();
        assert_eq!(db.triple_num(), 4);
    }
    // No shadow snapshot is written next to the store.
    assert!(!dir.join("store.bin").exists(), "save() must not write store.bin");
    assert!(!dir.join("dict.bin").exists());

    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), 4);
    assert_eq!(col0(&mut db, "SELECT ?o WHERE { <http://ex/x> <http://ex/q> ?o }"), ["<http://ex/y>"]);
    assert_eq!(
        col0(&mut db, "SELECT ?l WHERE { <http://ex/x> <http://ex/label> ?l }"),
        ["\"fresh literal\""]
    );
    assert!(col0(&mut db, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }").is_empty());
    // The VS-tree-filtered path (constant object) sees post-build data too: the
    // stale out-of-core index must have been rebuilt, not trusted.
    assert_eq!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/q> <http://ex/y> }"), ["<http://ex/x>"]);
    assert_eq!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/c> }"), ["<http://ex/b>"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_sparql_update_is_durable_without_save() {
    // Each auto-commit UPDATE is a commit point: no explicit save needed (the
    // server path).
    let dir = tmp("disk_sparql");
    disk_db(&dir);
    {
        let mut db = Database::open(&dir).unwrap();
        db.query("INSERT DATA { <http://ex/n1> <http://ex/p> <http://ex/n2> }").unwrap();
        db.query("DELETE DATA { <http://ex/b> <http://ex/p> <http://ex/c> }").unwrap();
        db.query("DELETE { ?s <http://ex/name> ?n } INSERT { ?s <http://ex/name> \"Alicia\" } WHERE { ?s <http://ex/name> ?n }")
            .unwrap();
        // Dropped without save().
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"),
        ["<http://ex/a>", "<http://ex/n1>"]
    );
    assert_eq!(col0(&mut db, "SELECT ?n WHERE { ?s <http://ex/name> ?n }"), ["\"Alicia\""]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_commit_points_survive_a_crash() {
    // `mem::forget` skips every destructor — the closest in-process stand-in
    // for a crash: only what reached a commit point may be on disk.
    let dir = tmp("disk_crash");
    disk_db(&dir);
    let mut db = Database::open(&dir).unwrap();
    db.query("INSERT DATA { <http://ex/by_update> <http://ex/p> <http://ex/x> }").unwrap();
    db.insert_triple(&triple("by_api_synced", "p", "x"));
    db.sync().unwrap();
    db.begin().unwrap();
    db.insert_triple(&triple("in_open_txn", "p", "x"));
    std::mem::forget(db);

    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/x> }"),
        ["<http://ex/by_api_synced>", "<http://ex/by_update>"]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_named_graphs_survive_reopen() {
    let dir = tmp("disk_named");
    disk_db(&dir);
    {
        let mut db = Database::open(&dir).unwrap();
        db.query(
            "INSERT DATA { GRAPH <http://ex/g1> { <http://ex/s1> <http://ex/p> <http://ex/o1> . \
                                                <http://ex/s2> <http://ex/p> \"v\" } \
                           GRAPH <http://ex/g2> { <http://ex/s3> <http://ex/p> <http://ex/o3> } }",
        )
        .unwrap();
        db.query("DELETE DATA { GRAPH <http://ex/g1> { <http://ex/s2> <http://ex/p> \"v\" } }")
            .unwrap();
    }
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.named_graphs().len(), 2);
        assert_eq!(
            col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g1> { ?s ?p ?o } }"),
            ["<http://ex/s1>"]
        );
        assert_eq!(
            col0(&mut db, "SELECT ?g WHERE { GRAPH ?g { <http://ex/s3> ?p ?o } }"),
            ["<http://ex/g2>"]
        );
        // The default graph is untouched by the quad updates.
        assert_eq!(db.triple_num(), 3);
        db.query("DROP GRAPH <http://ex/g2>").unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.named_graphs().len(), 1, "DROP GRAPH persisted");
    assert!(col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g2> { ?s ?p ?o } }").is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_transactions_persist_only_committed_work() {
    let dir = tmp("disk_txn");
    disk_db(&dir);
    {
        let mut db = Database::open(&dir).unwrap();
        db.begin().unwrap();
        db.insert_triple(&triple("ghost", "p", "x"));
        db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }").unwrap();
        db.rollback().unwrap();

        db.begin().unwrap();
        db.insert_triple(&triple("kept", "p", "x"));
        db.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/kept> <http://ex/p> <http://ex/x> } }")
            .unwrap();
        db.commit().unwrap();

        // An open, uncommitted transaction at drop time is discarded.
        db.begin().unwrap();
        db.insert_triple(&triple("uncommitted", "p", "x"));
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"),
        ["<http://ex/a>", "<http://ex/b>", "<http://ex/kept>"]
    );
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"),
        ["<http://ex/kept>"]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_clear_and_reasoning_are_written_through() {
    let dir = tmp("disk_clear");
    disk_db(&dir);
    {
        let mut db = Database::open(&dir).unwrap();
        db.query("CLEAR DEFAULT").unwrap();
        assert_eq!(db.triple_num(), 0);
        db.query(
            "INSERT DATA { <http://ex/Cat> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://ex/Animal> . \
                           <http://ex/tom> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Cat> }",
        )
        .unwrap();
        assert!(db.materialize_rdfs() > 0);
        db.checkpoint().unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        col0(
            &mut db,
            "SELECT ?c WHERE { <http://ex/tom> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ?c }"
        ),
        ["<http://ex/Animal>", "<http://ex/Cat>"],
        "inferred triples persisted"
    );
    assert!(col0(&mut db, "SELECT ?o WHERE { <http://ex/a> ?p ?o }").is_empty(), "CLEAR persisted");
    let _ = fs::remove_dir_all(&dir);
}

/// The reported repro: `gbuild --disk t base.nt; gadd t add.nt` said "2 now
/// total" but `gquery` returned one row. Also covers `gsub` and a `gquery`
/// UPDATE against the disk database.
#[test]
fn disk_cli_gadd_gsub_gquery_roundtrip() {
    let base = tmp("disk_cli");
    fs::create_dir_all(&base).unwrap();
    let name = base.join("t");
    let base_nt = base.join("base.nt");
    let add_nt = base.join("add.nt");
    write(&base_nt, "<http://ex/s1> <http://ex/p> <http://ex/o> .\n");
    write(&add_nt, "<http://ex/s2> <http://ex/p> <http://ex/o> .\n");

    let run = |bin: &str, args: &[&str]| {
        let out = Command::new(bin).args(args).output().unwrap();
        assert!(out.status.success(), "{bin} {args:?} failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let name_s = name.to_str().unwrap();
    run(env!("CARGO_BIN_EXE_gbuild"), &["--disk", name_s, base_nt.to_str().unwrap()]);
    let added = run(env!("CARGO_BIN_EXE_gadd"), &[name_s, add_nt.to_str().unwrap()]);
    assert!(added.contains("2 now total"), "gadd: {added}");

    let q = "SELECT ?s WHERE { ?s <http://ex/p> ?o }";
    let out = run(env!("CARGO_BIN_EXE_gquery"), &[name_s, "-e", q]);
    assert!(out.contains("<http://ex/s1>") && out.contains("<http://ex/s2>"), "gquery after gadd:\n{out}");

    run(env!("CARGO_BIN_EXE_gsub"), &[name_s, base_nt.to_str().unwrap()]);
    let out = run(env!("CARGO_BIN_EXE_gquery"), &[name_s, "-e", q]);
    assert!(!out.contains("<http://ex/s1>") && out.contains("<http://ex/s2>"), "gquery after gsub:\n{out}");

    run(
        env!("CARGO_BIN_EXE_gquery"),
        &[name_s, "-e", "INSERT DATA { <http://ex/s3> <http://ex/p> <http://ex/o> }"],
    );
    let out = run(env!("CARGO_BIN_EXE_gquery"), &[name_s, "-e", q]);
    assert!(out.contains("<http://ex/s3>"), "gquery UPDATE persisted:\n{out}");

    // The tool reports the real backend and the database stayed on disk.
    let shown = run(env!("CARGO_BIN_EXE_gshow"), &[name_s]);
    assert!(shown.contains("on-disk") && shown.contains("triples   : 2"), "gshow:\n{shown}");
    assert!(Database::is_disk(base.join("t.db")));
    assert!(!base.join("t.db/store.bin").exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn gbuild_disk_dispatches_on_file_extension() {
    let base = tmp("disk_formats");
    fs::create_dir_all(&base).unwrap();
    let ttl = base.join("data.ttl");
    write(
        &ttl,
        "@prefix ex: <http://ex/> .\nex:a ex:knows ex:b , ex:c ;\n     ex:name \"A\" .\n",
    );
    let nq = base.join("more.nq");
    write(&nq, "<http://ex/d> <http://ex/knows> <http://ex/e> <http://ex/g> .\n");
    let name = base.join("fmt");

    let out = Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .args(["--disk", name.to_str().unwrap(), ttl.to_str().unwrap(), nq.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "gbuild --disk failed: {out:?}");
    let mut db = Database::open(base.join("fmt.db")).unwrap();
    assert_eq!(db.storage(), StorageKind::Disk);
    assert_eq!(db.triple_num(), 4, "3 Turtle triples + 1 flattened N-Quad");
    assert_eq!(
        col0(&mut db, "SELECT ?o WHERE { ?s <http://ex/knows> ?o }"),
        ["<http://ex/b>", "<http://ex/c>", "<http://ex/e>"]
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn legacy_shadow_snapshot_is_reconciled_into_kvstore() {
    // A pre-fix build saved updates of a disk database as a shadow snapshot
    // next to the untouched kvstore.kv. The newer shadow wins once, is merged
    // into kvstore.kv, and is retired as *.legacy.
    let dir = tmp("legacy_shadow");
    disk_db(&dir);
    let snap_dir = tmp("legacy_shadow_snap");
    {
        let mut mem = Database::open(&dir).unwrap().into_memory().unwrap();
        mem.insert_triple(&triple("added", "p", "x"));
        mem.remove_triple(&triple("a", "p", "b"));
        mem.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/q> <http://ex/p> <http://ex/r> } }")
            .unwrap();
        mem.save(&snap_dir).unwrap();
    }
    for f in ["dict.bin", "store.bin", "named.bin", "meta.bin", "vstree.bin"] {
        fs::copy(snap_dir.join(f), dir.join(f)).unwrap();
    }
    assert_eq!(Database::storage_kind(&dir), Some(StorageKind::Disk), "kvstore.kv takes precedence");

    for round in 0..2 {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.storage(), StorageKind::Disk);
        assert_eq!(
            col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"),
            ["<http://ex/added>", "<http://ex/b>"],
            "round {round}"
        );
        assert_eq!(col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"), ["<http://ex/q>"]);
    }
    assert!(!dir.join("store.bin").exists());
    assert!(dir.join("store.bin.legacy").is_file(), "shadow kept for manual recovery");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&snap_dir);
}

#[test]
fn older_shadow_snapshot_is_ignored() {
    let dir = tmp("legacy_old_shadow");
    let snap_dir = tmp("legacy_old_shadow_snap");
    let mut mem = Database::build_from_str("old", "<http://ex/stale> <http://ex/p> <http://ex/x> .").unwrap();
    mem.insert_triple(&triple("stale2", "p", "x"));
    mem.save(&snap_dir).unwrap();
    fs::create_dir_all(&dir).unwrap();
    for f in ["dict.bin", "store.bin", "named.bin", "meta.bin", "vstree.bin"] {
        fs::copy(snap_dir.join(f), dir.join(f)).unwrap();
    }
    // A disk build written *after* the shadow — the shadow is older.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let nt = dir.with_extension("nt");
    write(&nt, BASE_NT);
    // Build the store files directly so the (older) shadow is left in place.
    {
        let kv = dir.join("kvstore.kv");
        let ds = gstore::kvstore::DiskStore::build_files(&kv, 64, &[&nt]).unwrap();
        drop(ds);
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), 3, "older shadow content is not applied");
    assert!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/x> }").is_empty());
    assert!(dir.join("store.bin.legacy").is_file());
    let _ = fs::remove_file(&nt);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&snap_dir);
}

// ---- layout detection -------------------------------------------------------

#[test]
fn open_detects_every_layout() {
    // Snapshot.
    let snap = tmp("detect_snap");
    Database::build_from_str("snap", BASE_NT).unwrap().save(&snap).unwrap();
    assert_eq!(Database::storage_kind(&snap), Some(StorageKind::Snapshot));
    let db = Database::open(&snap).unwrap();
    assert_eq!((db.storage(), db.triple_num()), (StorageKind::Snapshot, 3));
    assert_eq!(db.home(), Some(snap.as_path()));

    // On-disk.
    let disk = tmp("detect_disk");
    disk_db(&disk);
    assert_eq!(Database::storage_kind(&disk), Some(StorageKind::Disk));
    assert_eq!(Database::open(&disk).unwrap().storage(), StorageKind::Disk);
    // `load` auto-detects too (it used to fail on a disk directory).
    assert_eq!(Database::load(&disk).unwrap().triple_num(), 3);

    // Missing and empty directories give clear errors.
    let missing = tmp("detect_missing");
    assert!(Database::open(&missing).unwrap_err().to_string().contains("does not exist"));
    fs::create_dir_all(&missing).unwrap();
    assert_eq!(Database::storage_kind(&missing), None);
    assert!(Database::open(&missing)
        .unwrap_err()
        .to_string()
        .contains("does not contain a gStore database"));

    // A RocksDB directory is recognized even without the feature.
    let rocks = tmp("detect_rocks");
    fs::create_dir_all(rocks.join("rocksdb")).unwrap();
    assert_eq!(Database::storage_kind(&rocks), Some(StorageKind::RocksDb));
    assert!(Database::is_rocksdb(&rocks));
    #[cfg(not(feature = "rocksdb"))]
    {
        let err = Database::open(&rocks).unwrap_err().to_string();
        assert!(err.contains("RocksDB") && err.contains("--features rocksdb"), "{err}");
    }

    for d in [&snap, &disk, &missing, &rocks] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn saving_another_layout_replaces_the_old_one() {
    let dir = tmp("relayout");
    disk_db(&dir);
    // Detach into memory and save back: the directory becomes a snapshot.
    let mut mem = Database::open(&dir).unwrap().into_memory().unwrap();
    mem.insert_triple(&triple("m", "p", "x"));
    mem.save(&dir).unwrap();
    assert_eq!(Database::storage_kind(&dir), Some(StorageKind::Snapshot));
    assert!(!dir.join("kvstore.kv").exists());
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), 4);
    assert_eq!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/x> }"), ["<http://ex/m>"]);

    // A disk build over it replaces the snapshot.
    disk_db(&dir);
    assert_eq!(Database::storage_kind(&dir), Some(StorageKind::Disk));
    assert!(!dir.join("store.bin").exists());
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 3);
    let _ = fs::remove_dir_all(&dir);
}

// ---- atomic snapshot writes -------------------------------------------------

#[test]
fn failed_save_leaves_previous_snapshot_intact() {
    let dir = tmp("atomic_save");
    let mut db = Database::build_from_str("atomic", BASE_NT).unwrap();
    db.save(&dir).unwrap();

    // Make the save fail midway: the staging file for store.bin cannot be
    // created (a directory sits at its path) — after dict.bin was staged.
    let blocker = dir.join("store.bin.tmp");
    fs::create_dir_all(&blocker).unwrap();
    db.insert_triple(&triple("new", "p", "x"));
    assert!(db.save(&dir).is_err(), "the blocked save must fail");
    assert!(!dir.join("dict.bin.tmp").exists(), "aborted save cleans its temp files");

    // The previous, complete snapshot still loads.
    let mut old = Database::open(&dir).unwrap();
    assert_eq!(old.triple_num(), 3);
    assert!(col0(&mut old, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/x> }").is_empty());

    // Once the obstacle is gone the save goes through.
    fs::remove_dir(&blocker).unwrap();
    db.save(&dir).unwrap();
    let mut new = Database::open(&dir).unwrap();
    assert_eq!(new.triple_num(), 4);
    assert_eq!(col0(&mut new, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/x> }"), ["<http://ex/new>"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn interrupted_snapshot_commit_is_rolled_forward_on_open() {
    let dir = tmp("atomic_rollfwd");
    Database::build_from_str("v1", BASE_NT).unwrap().save(&dir).unwrap();
    // Stage a complete v2 set by hand and leave a commit record, as a crash
    // right after the record was written would.
    let v2_dir = tmp("atomic_rollfwd_v2");
    let mut v2 = Database::build_from_str("v2", BASE_NT).unwrap();
    v2.insert_triple(&triple("v2", "p", "x"));
    v2.save(&v2_dir).unwrap();
    let names = ["dict.bin", "store.bin", "named.bin", "vstree.bin", "meta.bin"];
    for f in names {
        fs::copy(v2_dir.join(f), dir.join(format!("{f}.tmp"))).unwrap();
    }
    fs::write(dir.join("SNAPSHOT.commit"), names.join("\n")).unwrap();
    // Half the renames happened before the "crash".
    fs::rename(dir.join("dict.bin.tmp"), dir.join("dict.bin")).unwrap();

    let db = Database::open(&dir).unwrap();
    assert_eq!((db.name(), db.triple_num()), ("v2", 4), "the committed set wins");
    assert!(!dir.join("SNAPSHOT.commit").exists());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&v2_dir);
}

// ---- backup / restore -------------------------------------------------------

#[test]
fn backup_dir_copies_wals_logs_and_rocksdb_dir() {
    let src = tmp("bdir_src");
    let dst = tmp("bdir_dst");
    disk_db(&src);
    {
        let mut db = Database::open(&src).unwrap();
        db.enable_redo_log_in(&src).unwrap();
        db.enable_update_log_in(&src).unwrap();
        db.query("INSERT DATA { <http://ex/u> <http://ex/p> <http://ex/v> }").unwrap();
    }
    // A pending (here: torn, hence ignored on open) write-ahead log and a
    // RocksDB store directory must be carried along by a file-level copy.
    fs::write(src.join("vstree.kv.wal"), b"torn").unwrap();
    write(&src.join("rocksdb/CURRENT"), "MANIFEST-000001\n");
    write(&src.join("rocksdb/sub/000001.sst"), "sst");
    // A stale file of the destination that the source lacks is removed.
    write(&dst.join("store.bin"), "stale");

    Database::backup_dir(&src, &dst).unwrap();
    for f in ["kvstore.kv", "vstree.kv", "vstree.kv.wal", "redo.log", "update.log", "rocksdb/CURRENT", "rocksdb/sub/000001.sst"] {
        assert!(dst.join(f).is_file(), "{f} copied");
    }
    assert!(!dst.join("store.bin").exists(), "dst mirrors src");

    // Without the fake rocksdb dir the copy opens as the same disk database.
    fs::remove_dir_all(dst.join("rocksdb")).unwrap();
    let mut db = Database::open(&dst).unwrap();
    assert_eq!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/v> }"), ["<http://ex/u>"]);
    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&dst);
}

#[test]
fn disk_backup_and_restore_into_roundtrip() {
    let dir = tmp("disk_bk");
    let bak = tmp("disk_bk_backup");
    let target = tmp("disk_bk_target");
    disk_db(&dir);
    {
        let mut db = Database::open(&dir).unwrap();
        db.query("INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/m> . GRAPH <http://ex/g> { <http://ex/n> <http://ex/p> <http://ex/m> } }")
            .unwrap();
        db.backup(&bak).unwrap();
        // Changes after the backup are not in it.
        db.insert_triple(&triple("later", "p", "x"));
    }
    assert_eq!(Database::storage_kind(&bak), Some(StorageKind::Disk));
    Database::restore_into(&bak, &target).unwrap();
    let mut db = Database::open(&target).unwrap();
    assert_eq!(db.storage(), StorageKind::Disk);
    assert_eq!(db.triple_num(), 4);
    assert_eq!(col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"), ["<http://ex/n>"]);
    assert!(col0(&mut db, "SELECT ?s WHERE { ?s ?p <http://ex/x> }").is_empty());
    // The restored copy is independent of the original.
    db.insert_triple(&triple("only_target", "p", "x"));
    db.save(&target).unwrap();
    assert!(col0(&mut Database::open(&dir).unwrap(), "SELECT ?s WHERE { ?s ?p <http://ex/x> }")
        .iter()
        .all(|s| s != "<http://ex/only_target>"));
    for d in [&dir, &bak, &target] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn cli_backup_restore_of_a_disk_database() {
    let base = tmp("cli_bk");
    fs::create_dir_all(&base).unwrap();
    let nt = base.join("d.nt");
    write(&nt, BASE_NT);
    let name = base.join("live");
    let bak = base.join("bak");
    let ok = |bin: &str, args: &[&str]| {
        let out = Command::new(bin).args(args).output().unwrap();
        assert!(out.status.success(), "{bin} {args:?}: {out:?}");
    };
    ok(env!("CARGO_BIN_EXE_gbuild"), &["--disk", name.to_str().unwrap(), nt.to_str().unwrap()]);
    ok(env!("CARGO_BIN_EXE_gquery"), &[name.to_str().unwrap(), "-e", "INSERT DATA { <http://ex/z> <http://ex/p> <http://ex/zz> }"]);
    ok(env!("CARGO_BIN_EXE_gbackup"), &[name.to_str().unwrap(), bak.to_str().unwrap()]);
    let restored = base.join("restored");
    ok(env!("CARGO_BIN_EXE_grestore"), &[bak.to_str().unwrap(), restored.to_str().unwrap()]);
    let mut db = Database::open(base.join("restored.db")).unwrap();
    assert_eq!(db.storage(), StorageKind::Disk);
    assert_eq!(col0(&mut db, "SELECT ?o WHERE { <http://ex/z> <http://ex/p> ?o }"), ["<http://ex/zz>"]);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn concurrent_db_over_a_disk_database() {
    // ConcurrentDb snapshots the working set; updates still write through.
    let dir = tmp("disk_concurrent");
    disk_db(&dir);
    {
        let cdb = gstore::ConcurrentDb::new(Database::open(&dir).unwrap());
        cdb.update("INSERT DATA { <http://ex/c1> <http://ex/p> <http://ex/c2> }").unwrap();
        match cdb.query("ASK { <http://ex/c1> <http://ex/p> <http://ex/c2> }").unwrap() {
            gstore::QueryResult::Ask(b) => assert!(b),
            other => panic!("{other:?}"),
        }
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(col0(&mut db, "SELECT ?o WHERE { <http://ex/c1> <http://ex/p> ?o }"), ["<http://ex/c2>"]);
    let _ = fs::remove_dir_all(&dir);
}

// ---- RocksDB ------------------------------------------------------------------

#[cfg(feature = "rocksdb")]
mod rocks {
    use super::*;

    fn rocks_db(dir: &Path) -> Database {
        Database::build_rocksdb_from_str("rk", dir, BASE_NT).unwrap()
    }

    #[test]
    fn rocks_update_save_reopen() {
        let dir = tmp("rocks_upd_save");
        {
            let mut db = rocks_db(&dir);
            assert_eq!(db.storage(), StorageKind::RocksDb);
            db.query("INSERT DATA { <http://ex/new> <http://ex/newp> \"brand new\" }").unwrap();
            db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }").unwrap();
            db.save(&dir).unwrap();
        }
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.name(), "rk");
        assert_eq!(db.triple_num(), 3);
        assert_eq!(
            col0(&mut db, "SELECT ?o WHERE { <http://ex/new> <http://ex/newp> ?o }"),
            ["\"brand new\""]
        );
        assert!(col0(&mut db, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rocks_updates_are_durable_without_save() {
        // The `gserver --rocksdb` path never saved: dictionary and counts must
        // still reopen consistently (this used to refuse to open).
        let dir = tmp("rocks_nosave");
        {
            let mut db = rocks_db(&dir);
            db.insert_triple(&triple("t1", "tp", "t2"));
            db.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/t3> <http://ex/tp> \"lit\" } }")
                .unwrap();
            db.remove_triple(&triple("b", "p", "c"));
        }
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.triple_num(), 3);
        assert_eq!(col0(&mut db, "SELECT ?o WHERE { <http://ex/t1> <http://ex/tp> ?o }"), ["<http://ex/t2>"]);
        assert_eq!(col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"), ["<http://ex/t3>"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rocks_backup_restore_roundtrip() {
        let dir = tmp("rocks_bk");
        let bak = tmp("rocks_bk_backup");
        let target = tmp("rocks_bk_target");
        {
            let mut db = rocks_db(&dir);
            db.query("INSERT DATA { <http://ex/bk> <http://ex/p> <http://ex/v> . GRAPH <http://ex/g> { <http://ex/bk> <http://ex/p> <http://ex/v> } }")
                .unwrap();
            db.backup(&bak).unwrap();
            db.insert_triple(&triple("after", "p", "v"));
        }
        assert_eq!(Database::storage_kind(&bak), Some(StorageKind::RocksDb));
        Database::restore_into(&bak, &target).unwrap();
        let mut db = Database::open(&target).unwrap();
        assert_eq!(db.triple_num(), 4, "the backup carries the triples");
        assert_eq!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/v> }"), ["<http://ex/bk>"]);
        assert_eq!(col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"), ["<http://ex/bk>"]);
        drop(db);

        // The file-level copy of a closed store round-trips as well.
        let copy = tmp("rocks_bk_copy");
        Database::backup_dir(&dir, &copy).unwrap();
        let db = Database::open(&copy).unwrap();
        assert_eq!(db.triple_num(), 5);
        for d in [&dir, &bak, &target, &copy] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn rocks_concurrent_snapshot_does_not_panic() {
        let dir = tmp("rocks_concurrent");
        let cdb = gstore::ConcurrentDb::new(rocks_db(&dir));
        match cdb.query("SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }").unwrap() {
            gstore::QueryResult::Select(rs) => assert_eq!(rs.row_count(), 1),
            other => panic!("{other:?}"),
        }
        cdb.update("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/z> }").unwrap();
        match cdb.query("SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }").unwrap() {
            gstore::QueryResult::Select(rs) => assert_eq!(rs.row_count(), 2),
            other => panic!("{other:?}"),
        }
        drop(cdb);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rocks_reclaim_then_reopen_keeps_ids_consistent() {
        let dir = tmp("rocks_reclaim");
        {
            let mut db = rocks_db(&dir);
            db.remove_triple(&triple("b", "p", "c"));
            assert!(db.reclaim_unused() > 0, "<c> is no longer referenced");
            // A new term reuses the freed id.
            db.insert_triple(&triple("reuse", "p", "fresh"));
            db.save(&dir).unwrap();
        }
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            col0(&mut db, "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }"),
            ["<http://ex/a>", "<http://ex/reuse>"]
        );
        assert_eq!(col0(&mut db, "SELECT ?o WHERE { <http://ex/reuse> <http://ex/p> ?o }"), ["<http://ex/fresh>"]);
        // And another new term after reopen does not collide.
        db.insert_triple(&triple("again", "p", "other"));
        assert_eq!(col0(&mut db, "SELECT ?o WHERE { <http://ex/again> <http://ex/p> ?o }"), ["<http://ex/other>"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_rocks_layout_is_migrated() {
        use gstore::store::{MutableStore, TripleSource};
        // The pre-fix layout: triples in rocksdb/, dictionary in dict.bin and
        // name/counts in meta.bin.
        let dir = tmp("rocks_legacy");
        let snap = tmp("rocks_legacy_snap");
        let mem = Database::build_from_str("legacy", BASE_NT).unwrap();
        mem.save(&snap).unwrap();
        fs::create_dir_all(&dir).unwrap();
        for f in ["dict.bin", "meta.bin"] {
            fs::copy(snap.join(f), dir.join(f)).unwrap();
        }
        {
            let mut r = gstore::RocksStore::open(dir.join("rocksdb")).unwrap();
            r.bulk_load(mem.backend().iter_all());
            r.flush().unwrap();
        }
        for _ in 0..2 {
            let mut db = Database::open(&dir).unwrap();
            assert_eq!(db.name(), "legacy");
            assert_eq!(col0(&mut db, "SELECT ?n WHERE { ?s <http://ex/name> ?n }"), ["\"Alice\""]);
        }
        assert!(dir.join("dict.bin.legacy").is_file());

        // A legacy store with triples whose terms were never saved is refused.
        let bad = tmp("rocks_legacy_bad");
        fs::create_dir_all(&bad).unwrap();
        for f in ["dict.bin", "meta.bin"] {
            fs::copy(snap.join(f), bad.join(f)).unwrap();
        }
        {
            let mut r = gstore::RocksStore::open(bad.join("rocksdb")).unwrap();
            r.bulk_load(mem.backend().iter_all());
            r.insert(gstore::IdTriple::new(999, 999, 999));
            r.flush().unwrap();
        }
        let err = Database::open(&bad).unwrap_err().to_string();
        assert!(err.contains("never saved"), "{err}");
        for d in [&dir, &snap, &bad] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn gbuild_rocksdb_dispatches_on_file_extension_and_gadd_persists() {
        let base = tmp("rocks_cli");
        fs::create_dir_all(&base).unwrap();
        let ttl = base.join("d.ttl");
        write(&ttl, "@prefix ex: <http://ex/> .\nex:m ex:dir ex:n , ex:o .\n");
        let add = base.join("add.nt");
        write(&add, "<http://ex/q> <http://ex/dir> <http://ex/n> .\n");
        let name = base.join("r");
        let ok = |bin: &str, args: &[&str]| {
            let out = Command::new(bin).args(args).output().unwrap();
            assert!(out.status.success(), "{bin} {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        ok(env!("CARGO_BIN_EXE_gbuild"), &["--rocksdb", name.to_str().unwrap(), ttl.to_str().unwrap()]);
        ok(env!("CARGO_BIN_EXE_gadd"), &[name.to_str().unwrap(), add.to_str().unwrap()]);
        let out = ok(env!("CARGO_BIN_EXE_gquery"), &[name.to_str().unwrap(), "-e", "SELECT ?s ?o WHERE { ?s <http://ex/dir> ?o }"]);
        assert!(out.contains("<http://ex/q>") && out.contains("<http://ex/o>"), "{out}");
        let shown = ok(env!("CARGO_BIN_EXE_gshow"), &[name.to_str().unwrap()]);
        assert!(shown.contains("RocksDB") && shown.contains("triples   : 3"), "{shown}");
        let _ = fs::remove_dir_all(&base);
    }
}
