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

/// A scratch path unique to this test process, so concurrent runs of the
/// suite (e.g. from two checkouts sharing `/tmp`) never clobber each other's
/// on-disk databases.
fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_disk_{tag}_{}.db", std::process::id()));
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
    let path = std::env::temp_dir().join(format!("gstore_dt_stream_{}.kv", std::process::id()));
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

/// The on-disk reads come back in the same order as the in-memory store's
/// (the C1 ordering contract), so plans and solution order do not depend on
/// the backend.
#[test]
fn disk_reads_follow_the_memory_order_at_lubm_scale() {
    use gstore::TripleSource;
    let path = std::env::temp_dir().join(format!("gstore_dt_order_{}.kv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("kv.wal"));
    let ds = DiskStore::build_files(&path, 512, &[lubm_nt()]).unwrap();
    let (_dict, mem) = ds.to_memory().unwrap();
    assert_eq!(ds.try_iter_all().unwrap(), TripleSource::iter_all(&mem));
    assert_eq!(
        ds.try_subject_keys().unwrap(),
        TripleSource::subject_keys(&mem)
    );
    assert_eq!(
        ds.try_object_keys().unwrap(),
        TripleSource::object_keys(&mem)
    );
    for p in 0..ds.predicate_num() as u32 {
        assert_eq!(ds.try_so_by_p(p).unwrap(), TripleSource::so_by_p(&mem, p));
        assert_eq!(
            ds.try_subs_by_p(p).unwrap(),
            TripleSource::subs_by_p(&mem, p)
        );
        assert_eq!(
            ds.try_objs_by_p(p).unwrap(),
            TripleSource::objs_by_p(&mem, p)
        );
        assert_eq!(
            ds.count_so_by_p(p),
            TripleSource::so_by_p(&mem, p).len() as u64
        );
    }
    // Every 7th subject and object id, with their two-id patterns.
    for s in ds.try_subject_keys().unwrap().into_iter().step_by(7) {
        let po = TripleSource::po_by_s(&mem, s);
        assert_eq!(ds.try_po_by_s(s).unwrap(), po);
        assert_eq!(ds.count_po_by_s(s), po.len() as u64);
        for &(p, o) in po.iter().step_by(3) {
            assert_eq!(
                ds.try_o_by_sp(s, p).unwrap(),
                TripleSource::o_by_sp(&mem, s, p)
            );
            assert_eq!(
                ds.try_p_by_so(s, o).unwrap(),
                TripleSource::p_by_so(&mem, s, o)
            );
        }
    }
    for o in ds.try_object_keys().unwrap().into_iter().step_by(7) {
        let ps = TripleSource::ps_by_o(&mem, o);
        assert_eq!(ds.try_ps_by_o(o).unwrap(), ps);
        assert_eq!(ds.count_ps_by_o(o), ps.len() as u64);
        for &(p, _) in ps.iter().step_by(3) {
            assert_eq!(
                ds.try_s_by_po(p, o).unwrap(),
                TripleSource::s_by_po(&mem, p, o)
            );
        }
    }
    assert_eq!(
        TripleSource::distinct_subjects(&ds),
        TripleSource::distinct_subjects(&mem)
    );
    assert_eq!(
        TripleSource::num_predicates(&ds),
        TripleSource::num_predicates(&mem)
    );
    drop(ds);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(path.with_extension("kv.wal")).ok();
}

/// The streaming `DiskStore::query` answers all 14 LUBM queries with the
/// same rows as the in-memory database (as multisets).
#[test]
fn disk_streaming_query_matches_memory_rows_on_lubm() {
    let path =
        std::env::temp_dir().join(format!("gstore_dt_stream_lubm_{}.kv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("kv.wal"));
    let ds = DiskStore::build_files(&path, 1024, &[lubm_nt()]).unwrap();
    let mut mem = Database::build_from_files("lubm", &[lubm_nt()]).unwrap();
    let qdir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    for i in 1..=14 {
        let q = std::fs::read_to_string(qdir.join(format!("lubm_q{i}.rq"))).unwrap();
        let mut disk_rows = match ds.query(&q).unwrap() {
            QueryResult::Select(rs) => rs.rows,
            other => panic!("Q{i}: expected Select, got {other:?}"),
        };
        let mut mem_rows = mem.select(&q).unwrap().rows;
        disk_rows.sort();
        mem_rows.sort();
        assert_eq!(disk_rows, mem_rows, "Q{i} rows");
    }
    drop(ds);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(path.with_extension("kv.wal")).ok();
}
