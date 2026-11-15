//! A damaged RocksDB store is reported, never a panic, and `gcompact --check`
//! finds what is wrong with it (review disk#11, gap-rocksdb-exec#6 and #7).
//!
//! The damage is written straight into the column families with the
//! `rocksdb` crate, as a partial copy, a foreign tool or bit rot would leave
//! it: a key of the wrong length in a triple ordering or in the statistics,
//! a missing or wrong statistics counter, an ordering that lacks a triple.

#![cfg(feature = "rocksdb")]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;

use gstore::db::OpenOptions;
use gstore::{Database, QueryResult};

const CFS: [&str; 7] = ["spo", "pos", "osp", "stats", "dict", "gspo", "meta"];

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d =
            std::env::temp_dir().join(format!("gstore_rocks_damaged_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const DATA: &str = "<http://ex/s0> <http://ex/p> \"o0\" .\n\
                    <http://ex/s1> <http://ex/p> \"o1\" .\n\
                    <http://ex/s2> <http://ex/q> <http://ex/s0> .\n";

/// A RocksDB database of `DATA` at `<tmp>/d.db`, closed.
fn build(tmp: &TempDir) -> PathBuf {
    let dir = tmp.0.join("d.db");
    drop(Database::build_rocksdb_from_str("d", &dir, DATA).unwrap());
    dir
}

/// Change the closed store of `dir` with `f` (its RocksDB handle).
fn damage(dir: &Path, f: impl FnOnce(&rocksdb::DB)) {
    let db = rocksdb::DB::open_cf(&rocksdb::Options::default(), dir.join("rocksdb"), CFS).unwrap();
    f(&db);
    db.flush().unwrap();
}

fn put(db: &rocksdb::DB, cf: &str, key: &[u8], value: &[u8]) {
    db.put_cf(db.cf_handle(cf).unwrap(), key, value).unwrap();
}

fn delete(db: &rocksdb::DB, cf: &str, key: &[u8]) {
    db.delete_cf(db.cf_handle(cf).unwrap(), key).unwrap();
}

/// Run `f`, failing the test with `what` if it panics.
fn no_panic<T>(what: &str, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| panic!("{what} panicked"))
}

/// `tool args…` exits with failure, reporting an error rather than a panic.
fn tool_fails(tool: &str, args: &[&str]) -> String {
    let out = Command::new(tool).args(args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !out.status.success(),
        "{tool} {args:?} succeeded on a damaged store: {stdout}{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "{tool} {args:?} panicked: {stderr}"
    );
    format!("{stdout}{stderr}")
}

fn full_scan(db: &mut Database) -> gstore::Result<QueryResult> {
    db.query("SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
}

/// A short key in SPO outside every subject's range: a full scan, the
/// integrity check, `gcompact --check` and `gexport` report it.
#[test]
fn a_short_spo_key_fails_scans_checks_and_exports_without_a_panic() {
    let tmp = TempDir::new("short_spo");
    let dir = build(&tmp);
    damage(&dir, |db| put(db, "spo", &[0xff, 0xff, 0xff], b""));
    let path = dir.to_str().unwrap();
    {
        let mut db = Database::open(&dir).unwrap();
        let res = no_panic("a full scan", || full_scan(&mut db));
        assert!(res.is_err(), "a full scan over a damaged key: {res:?}");
        let check = no_panic("verify_integrity", || db.verify_integrity());
        let e = check.expect_err("the integrity check passed a damaged store");
        assert!(e.to_string().contains("corrupt rocksdb key"), "{e}");
    }
    let out = tool_fails(env!("CARGO_BIN_EXE_gcompact"), &[path, "--check"]);
    assert!(out.contains("corrupt rocksdb key"), "{out}");
    let export = tmp.0.join("out.nt");
    tool_fails(
        env!("CARGO_BIN_EXE_gexport"),
        &[path, export.to_str().unwrap()],
    );
}

/// A short key inside a subject's SPO range: a subject-bound query and an
/// open that builds the VS-tree (`vstree_on_open`) report it.
#[test]
fn a_short_key_inside_a_subject_range_fails_reads_and_vstree_builds_without_a_panic() {
    let tmp = TempDir::new("short_in_range");
    let dir = build(&tmp);
    // Subject id 0 is the first subject term: `<http://ex/s0>`.
    damage(&dir, |db| put(db, "spo", &[0, 0, 0, 0, 0, 0], b""));
    let opts = OpenOptions {
        vstree_on_open: true,
        ..OpenOptions::default()
    };
    let opened = no_panic("an open with vstree_on_open", || {
        Database::open_with(&dir, &opts)
    });
    if let Ok(mut db) = opened {
        let res = no_panic("a subject-bound query", || {
            db.query("SELECT ?p ?o WHERE { <http://ex/s0> ?p ?o }")
        });
        assert!(
            res.is_err(),
            "a subject-bound read over a damaged key: {res:?}"
        );
    }
    let mut db = Database::open(&dir).unwrap();
    let res = no_panic("a subject-bound query", || {
        db.query("SELECT ?p ?o WHERE { <http://ex/s0> ?p ?o }")
    });
    assert!(res.is_err(), "{res:?}");
    assert!(no_panic("verify_integrity", || db.verify_integrity()).is_err());
}

/// Malformed statistics keys (a subject counter without its id, a pair
/// counter cut short) and a malformed POS key are reported too.
#[test]
fn malformed_stats_and_pos_keys_fail_reads_and_checks_without_a_panic() {
    for (cf, key) in [
        ("stats", &b"s\x00\x00"[..]),
        ("stats", &b"x\x00\x00\x00\x00\x00"[..]),
        ("pos", &[0u8, 0, 0, 0, 7][..]),
        ("osp", &[1u8][..]),
    ] {
        let tmp = TempDir::new("malformed_misc");
        let dir = build(&tmp);
        damage(&dir, |db| put(db, cf, key, &7u64.to_be_bytes()));
        let mut db = Database::open(&dir).unwrap();
        let check = no_panic("verify_integrity", || db.verify_integrity());
        assert!(check.is_err(), "{cf} {key:?}: the integrity check passed");
        for q in [
            "SELECT ?s WHERE { ?s ?p ?o }",
            "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }",
            "SELECT ?s ?p WHERE { ?s ?p <http://ex/s0> }",
            "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
        ] {
            // A read may or may not reach the damaged key; it never panics.
            let _ = no_panic(q, || db.query(q));
        }
        drop(db);
        tool_fails(
            env!("CARGO_BIN_EXE_gcompact"),
            &[dir.to_str().unwrap(), "--check"],
        );
    }
}

/// `gcompact --check` reports a statistics counter that is missing or
/// wrong, and an ordering that lacks a triple (it used to say "integrity
/// ok" — and `--reclaim` then freed a term the data still used).
#[test]
fn gcompact_check_reports_wrong_rocksdb_statistics() {
    // `s|1`: the subject counter of <http://ex/s1>, a subject that is never
    // an object; `o|0`'s value; a POS key; a stray counter.
    let s1 = [b's', 0, 0, 0, 1];
    type Damage = Box<dyn Fn(&rocksdb::DB)>;
    let cases: Vec<(&str, Damage)> = vec![
        (
            "missing counter",
            Box::new(move |db| delete(db, "stats", &s1)),
        ),
        (
            "wrong counter",
            Box::new(move |db| put(db, "stats", &s1, &5u64.to_be_bytes())),
        ),
        (
            "stray counter",
            Box::new(|db| put(db, "stats", &[b'o', 0, 0, 0, 99], &1u64.to_be_bytes())),
        ),
        (
            "global counter",
            Box::new(|db| put(db, "stats", b"T", &9u64.to_be_bytes())),
        ),
        (
            "pos lacks a triple",
            Box::new(|db| {
                let pos = db.cf_handle("pos").unwrap();
                let first = db
                    .iterator_cf(pos, rocksdb::IteratorMode::Start)
                    .next()
                    .unwrap()
                    .unwrap()
                    .0;
                db.delete_cf(pos, first).unwrap();
            }),
        ),
    ];
    for (what, f) in cases {
        let tmp = TempDir::new("stats");
        let dir = build(&tmp);
        {
            let db = Database::open(&dir).unwrap();
            db.verify_integrity().unwrap();
        }
        damage(&dir, f);
        let db = Database::open(&dir).unwrap();
        let e = db
            .verify_integrity()
            .expect_err(&format!("{what}: verify_integrity passed"));
        assert!(e.to_string().contains("statistics"), "{what}: {e}");
        drop(db);
        let out = tool_fails(
            env!("CARGO_BIN_EXE_gcompact"),
            &[dir.to_str().unwrap(), "--check"],
        );
        assert!(out.contains("statistics"), "{what}: {out}");
    }
}

/// The streamed check (`verify_stats`, what `gcompact --check` runs) and the
/// in-memory reference check (`check_stats`) agree, on healthy stores after
/// churn and on damaged ones.
#[test]
fn the_streamed_statistics_check_agrees_with_the_reference_check() {
    let tmp = TempDir::new("agree");
    let dir = build(&tmp);
    {
        let mut db = Database::open(&dir).unwrap();
        for i in 0..200u32 {
            let q = if i % 3 == 2 {
                format!(
                    "DELETE DATA {{ <http://ex/n{}> <http://ex/p{}> \"v{}\" }}",
                    i - 1,
                    (i - 1) % 5,
                    (i - 1) % 11
                )
            } else {
                format!("INSERT DATA {{ <http://ex/n{i}> <http://ex/p{}> \"v{}\" . <http://ex/n{i}> <http://ex/link> <http://ex/n{}> }}", i % 5, i % 11, i / 2)
            };
            db.query(&q).unwrap();
            if i % 40 == 0 {
                let rocks = db.backend().as_rocks().unwrap();
                assert_eq!(rocks.verify_stats(), Ok(()), "step {i}");
                assert_eq!(rocks.check_stats(), Ok(()), "step {i}");
            }
        }
        let rocks = db.backend().as_rocks().unwrap();
        assert_eq!(rocks.verify_stats(), Ok(()));
        assert_eq!(rocks.check_stats(), Ok(()));
    }
    let keys: Vec<(&str, Vec<u8>, Option<u64>)> = vec![
        ("stats", vec![b'c', 0, 0, 0, 0], Some(77)),
        ("stats", vec![b'u', 0, 0, 0, 0], None),
        ("stats", vec![b'v', 0, 0, 0, 1], Some(42)),
        ("stats", vec![b'x', 0, 0, 0, 0, 0, 0, 0, 1], Some(3)),
        ("stats", vec![b'y', 0, 0, 0, 200, 0, 0, 0, 200], Some(1)),
        ("stats", b"S".to_vec(), Some(1)),
        ("osp", vec![0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0], Some(0)),
    ];
    for (cf, key, value) in keys {
        let copy = tmp.0.join("copy.db");
        let _ = std::fs::remove_dir_all(&copy);
        copy_dir(&dir, &copy);
        damage(&copy, |db| match value {
            Some(v) if cf == "stats" => put(db, cf, &key, &v.to_be_bytes()),
            Some(_) => put(db, cf, &key, b""),
            None => delete(db, cf, &key),
        });
        let db = Database::open(&copy).unwrap();
        let rocks = db.backend().as_rocks().unwrap();
        let (streamed, reference) = (rocks.verify_stats(), rocks.check_stats());
        assert!(
            streamed.is_err() && reference.is_err(),
            "{cf} {key:?} = {value:?}: streamed {streamed:?}, reference {reference:?}"
        );
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else if entry.file_name() != "LOCK" {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}
