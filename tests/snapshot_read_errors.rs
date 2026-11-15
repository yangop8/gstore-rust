//! A snapshot database whose `named.bin` cannot be read must not open as a
//! database without named graphs (review db#1 / robust#4 / durability#3, and
//! the named.bin part of the test gap db#24).
//!
//! Before the fix every read error of `named.bin` — truncation, a decode
//! error, EIO, EACCES, EMFILE — became an empty map: the database opened
//! without its named graphs, and the next save or `gcompact --reclaim`
//! overwrote `named.bin` with an empty map and freed the terms only those
//! quads used. Only a *missing* `named.bin` (a database written before named
//! graphs existed) means "no named graphs".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::{Database, QueryResult};

const BASE: &str = "\
<http://ex/a> <http://ex/p> <http://ex/b> .
<http://ex/a> <http://ex/name> \"alpha\" .
<http://ex/b> <http://ex/name> \"beta\" .
";

const NAMED_INSERT: &str = "INSERT DATA { GRAPH <http://ex/g> { \
     <http://ex/only-in-g> <http://ex/q> \"named literal\" . \
     <http://ex/a> <http://ex/q> <http://ex/also-only-in-g> } }";

const GRAPH_COUNT: &str = "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }";

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_snapshot_read_errors_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // Restore permissions a test may have taken away, so removal works.
        if let Ok(entries) = fs::read_dir(&self.0) {
            for e in entries.flatten() {
                restore_perms(&e.path());
                if let Ok(inner) = fs::read_dir(e.path()) {
                    for f in inner.flatten() {
                        restore_perms(&f.path());
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn restore_perms(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = fs::symlink_metadata(p) {
        if !m.file_type().is_symlink() {
            let mode = if m.is_dir() { 0o755 } else { 0o644 };
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
        }
    }
}

#[cfg(not(unix))]
fn restore_perms(_: &Path) {}

/// The single integer of a one-cell SELECT.
fn count(db: &mut Database, q: &str) -> u64 {
    match db.query(q).unwrap() {
        QueryResult::Select(rs) => {
            let cell = rs.rows[0][0].clone().expect("bound count");
            let digits: String = cell
                .trim_start_matches('"')
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits
                .parse()
                .unwrap_or_else(|_| panic!("not a count: {cell}"))
        }
        other => panic!("expected Select, got {other:?}"),
    }
}

/// A snapshot database in `dir` holding three default-graph triples and two
/// quads in `<http://ex/g>` whose terms no default-graph triple uses.
fn build_with_named_graph(dir: &Path) {
    let mut db = Database::build_from_str("named", BASE).unwrap();
    db.query(NAMED_INSERT).unwrap();
    assert_eq!(count(&mut db, GRAPH_COUNT), 2);
    db.save(dir).unwrap();
    assert!(dir.join("named.bin").is_file());
}

/// `Database::open` and `Database::read_copy` of `dir` both fail with an
/// error naming `named.bin`.
fn assert_open_fails_naming_named_bin(dir: &Path, what: &str) {
    let err = match Database::open(dir) {
        Ok(mut db) => panic!(
            "{what}: open succeeded with {} named-graph quad(s)",
            count(&mut db, GRAPH_COUNT)
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("named.bin"),
        "{what}: open error does not name the file: {err}"
    );
    let err = match Database::read_copy(dir) {
        Ok(_) => panic!("{what}: read_copy succeeded"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("named.bin"),
        "{what}: read_copy error does not name the file: {err}"
    );
}

/// Run `gcompact <name> --reclaim` in `cwd` (where `<name>.db` lives).
fn gcompact_reclaim(cwd: &Path, name: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gcompact"))
        .args([name, "--reclaim"])
        .current_dir(cwd)
        .output()
        .expect("run gcompact")
}

#[test]
fn a_truncated_named_bin_fails_the_open_and_gcompact_reclaim_refuses() {
    let tmp = TmpDir::new("truncated");
    let dir = tmp.0.join("named.db");
    build_with_named_graph(&dir);
    let named = dir.join("named.bin");
    let full = fs::metadata(&named).unwrap().len();
    assert!(full > 10, "named.bin holds the graph ({full} bytes)");
    fs::OpenOptions::new()
        .write(true)
        .open(&named)
        .unwrap()
        .set_len(10)
        .unwrap();
    let damaged = fs::read(&named).unwrap();

    assert_open_fails_naming_named_bin(&dir, "truncated named.bin");

    // gcompact --reclaim must not free the named graph's terms nor rewrite
    // named.bin as an empty map.
    let out = gcompact_reclaim(&tmp.0, "named");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "gcompact --reclaim succeeded on a damaged named.bin: {}{stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stderr.contains("named.bin"), "gcompact error: {stderr}");
    assert_eq!(
        fs::read(&named).unwrap(),
        damaged,
        "named.bin was rewritten"
    );
}

#[test]
fn an_undecodable_named_bin_fails_the_open() {
    let tmp = TmpDir::new("garbage");
    let dir = tmp.0.join("named.db");
    build_with_named_graph(&dir);
    let named = dir.join("named.bin");
    // Same length, every byte flipped: the map's length prefix now claims far
    // more entries than the file holds.
    let flipped: Vec<u8> = fs::read(&named).unwrap().iter().map(|b| !b).collect();
    fs::write(&named, &flipped).unwrap();
    assert_open_fails_naming_named_bin(&dir, "undecodable named.bin");
    assert_eq!(fs::read(&named).unwrap(), flipped);
}

#[test]
fn an_unreadable_named_bin_fails_the_open() {
    // A directory in place of the file: open(2) succeeds, read(2) fails with
    // EISDIR — an I/O error other than NotFound, whoever runs the test.
    let tmp = TmpDir::new("eisdir");
    let dir = tmp.0.join("named.db");
    build_with_named_graph(&dir);
    let named = dir.join("named.bin");
    fs::remove_file(&named).unwrap();
    fs::create_dir(&named).unwrap();
    assert_open_fails_naming_named_bin(&dir, "named.bin is a directory");
}

#[cfg(unix)]
#[test]
fn a_named_bin_without_read_permission_fails_the_open() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TmpDir::new("eacces");
    let dir = tmp.0.join("named.db");
    build_with_named_graph(&dir);
    let named = dir.join("named.bin");
    fs::set_permissions(&named, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&named).is_ok() {
        // Running with CAP_DAC_OVERRIDE (root): permissions cannot make the
        // file unreadable; the EISDIR test covers the I/O error path.
        eprintln!("skipped: the file stays readable for this user");
        return;
    }
    assert_open_fails_naming_named_bin(&dir, "named.bin without read permission");
}

#[cfg(unix)]
#[test]
fn a_dangling_named_bin_symlink_is_not_a_missing_file() {
    let tmp = TmpDir::new("dangling");
    let dir = tmp.0.join("named.db");
    build_with_named_graph(&dir);
    let named = dir.join("named.bin");
    fs::remove_file(&named).unwrap();
    std::os::unix::fs::symlink(tmp.0.join("volume-not-mounted/named.bin"), &named).unwrap();
    assert_open_fails_naming_named_bin(&dir, "dangling named.bin symlink");
}

#[test]
fn a_missing_named_bin_still_opens_without_named_graphs() {
    // A database written before named graphs existed has no named.bin.
    let tmp = TmpDir::new("missing");
    let dir = tmp.0.join("old.db");
    Database::build_from_str("old", BASE)
        .unwrap()
        .save(&dir)
        .unwrap();
    fs::remove_file(dir.join("named.bin")).unwrap();
    let mut db = Database::open(&dir).expect("a database without named.bin opens");
    assert_eq!(db.triple_num(), 3);
    assert_eq!(count(&mut db, GRAPH_COUNT), 0);
    drop(db);
    let mut copy = Database::read_copy(&dir).expect("read_copy without named.bin");
    assert_eq!(copy.triple_num(), 3);
    assert_eq!(count(&mut copy, GRAPH_COUNT), 0);
}

#[test]
fn an_intact_named_bin_round_trips_through_open_and_gcompact() {
    let tmp = TmpDir::new("intact");
    let dir = tmp.0.join("named.db");
    build_with_named_graph(&dir);
    let out = gcompact_reclaim(&tmp.0, "named");
    assert!(
        out.status.success(),
        "gcompact --reclaim: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(count(&mut db, GRAPH_COUNT), 2);
    let rows = match db
        .query("SELECT ?o WHERE { GRAPH <http://ex/g> { <http://ex/only-in-g> ?p ?o } }")
        .unwrap()
    {
        QueryResult::Select(rs) => rs.rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some("\"named literal\""));
}

/// The VS-tree of a reopened snapshot is valid and answers like the store —
/// both from a saved `vstree.bin` and when `vstree.bin` is damaged (it is an
/// index derived from the store, so it is rebuilt rather than failing the
/// open).
#[test]
fn the_vstree_of_a_reopened_snapshot_is_valid_also_after_vstree_bin_damage() {
    let tmp = TmpDir::new("vstree");
    let dir = tmp.0.join("vs.db");
    build_with_named_graph(&dir);
    let q = "SELECT ?s WHERE { ?s <http://ex/name> \"beta\" }";
    let expect = |db: &mut Database, what: &str| {
        assert!(
            db.stats().index_valid,
            "{what}: VS-tree not valid after open"
        );
        match db.query(q).unwrap() {
            QueryResult::Select(rs) => {
                let got: Vec<_> = rs.rows.iter().map(|r| r[0].clone()).collect();
                assert_eq!(got, [Some("<http://ex/b>".to_string())], "{what}");
            }
            other => panic!("{what}: {other:?}"),
        }
        assert_eq!(count(db, GRAPH_COUNT), 2, "{what}");
    };
    let mut db = Database::open(&dir).unwrap();
    expect(&mut db, "saved vstree.bin");
    drop(db);
    let vs = dir.join("vstree.bin");
    assert!(vs.is_file());
    let len = fs::metadata(&vs).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&vs)
        .unwrap()
        .set_len(len / 2)
        .unwrap();
    let mut db = Database::open(&dir).unwrap();
    expect(&mut db, "truncated vstree.bin");
}

/// A legacy RocksDB layout (dictionary and named graphs still in snapshot
/// files) with an unreadable named.bin is not migrated: the open fails and
/// nothing is retired, so the files stay recoverable as they are.
#[cfg(feature = "rocksdb")]
#[test]
fn a_legacy_rocksdb_migration_refuses_an_unreadable_named_bin() {
    use gstore::store::{MutableStore, TripleSource};
    let tmp = TmpDir::new("rocks_legacy");
    let snap = tmp.0.join("snap.db");
    build_with_named_graph(&snap);
    let mem = Database::open(&snap).unwrap();
    let dir = tmp.0.join("legacy.db");
    fs::create_dir_all(&dir).unwrap();
    for f in ["dict.bin", "meta.bin", "named.bin"] {
        fs::copy(snap.join(f), dir.join(f)).unwrap();
    }
    {
        let mut r = gstore::RocksStore::open(dir.join("rocksdb")).unwrap();
        r.bulk_load(mem.backend().iter_all());
        r.flush().unwrap();
    }
    drop(mem);
    fs::OpenOptions::new()
        .write(true)
        .open(dir.join("named.bin"))
        .unwrap()
        .set_len(10)
        .unwrap();
    let err = match Database::open(&dir) {
        Ok(_) => panic!("a legacy database with a truncated named.bin was migrated"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("named.bin"), "{err}");
    for f in ["dict.bin", "meta.bin", "named.bin"] {
        assert!(
            dir.join(f).is_file(),
            "{f} was retired by a failed migration"
        );
    }
}
