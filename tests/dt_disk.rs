//! Data test: the on-disk B+ tree KVstore — build to disk, reopen, query, and
//! verify results match the in-memory engine (including at LUBM scale).

use std::path::PathBuf;

use gstore::kvstore::DiskStore;
use gstore::{Database, QueryResult};

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn small_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/small/small.nt")
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_disk_{tag}.db"));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn disk_build_reopen_query_small() {
    let dir = tmp("small");
    Database::build_disk(&dir, &[small_nt()]).unwrap();
    assert!(Database::is_disk(&dir));

    // A *separate* open proves persistence + reopen from disk.
    let mut db = Database::load_disk(&dir).unwrap();
    assert_eq!(db.triple_num(), 25);

    let rs = db
        .select("SELECT ?o WHERE { <root> <contain> ?o }")
        .unwrap();
    assert_eq!(rs.row_count(), 5);

    let rs = db
        .select("SELECT ?pt WHERE { <root> <contain> ?n . ?n <own> ?pt }")
        .unwrap();
    assert_eq!(rs.row_count(), 8);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disk_streaming_query_off_disk() {
    // DiskStore::query streams matches straight from the on-disk indexes — only
    // the dictionary is loaded into memory, the triples stay on disk.
    let path = std::env::temp_dir().join("gstore_dt_stream.kv");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("kv.wal"));
    let ds = DiskStore::build_files(&path, 256, &[small_nt()]).unwrap();

    match ds.query("SELECT ?o WHERE { <root> <contain> ?o }").unwrap() {
        QueryResult::Select(rs) => assert_eq!(rs.row_count(), 5),
        other => panic!("expected Select, got {other:?}"),
    }
    // A two-hop join, streamed (optimizer + executor run against the disk store).
    match ds
        .query("SELECT ?pt WHERE { <root> <contain> ?n . ?n <own> ?pt }")
        .unwrap()
    {
        QueryResult::Select(rs) => assert_eq!(rs.row_count(), 8),
        other => panic!("expected Select, got {other:?}"),
    }
    match ds.query("ASK { <root> <contain> ?o }").unwrap() {
        QueryResult::Ask(b) => assert!(b),
        other => panic!("expected Ask, got {other:?}"),
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(path.with_extension("kv.wal")).ok();
}

#[test]
fn disk_lubm_matches_memory() {
    let dir = tmp("lubm");
    // Build the full LUBM dataset onto disk via the B+ tree KVstore.
    Database::build_disk(&dir, &[lubm_nt()]).unwrap();

    // Disk-built database (reopened from disk).
    let mut disk = Database::load_disk(&dir).unwrap();
    assert_eq!(disk.triple_num(), 100_543);

    // In-memory reference.
    let mut mem = Database::build_from_files("lubm", &[lubm_nt()]).unwrap();

    // Every standard LUBM query returns the same count from disk and memory.
    let expected: &[(&str, usize)] = &[
        ("lubm_q1.rq", 4),
        ("lubm_q4.rq", 10),
        ("lubm_q5.rq", 678),
        ("lubm_q6.rq", 7790),
        ("lubm_q9.rq", 102),
        ("lubm_q13.rq", 8330),
        ("lubm_q14.rq", 5916),
    ];
    let qdir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    for (file, want) in expected {
        let q = std::fs::read_to_string(qdir.join(file)).unwrap();
        let d = disk.select(&q).unwrap().row_count();
        let m = mem.select(&q).unwrap().row_count();
        assert_eq!(d, *want, "{file} disk count");
        assert_eq!(d, m, "{file} disk vs memory");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
