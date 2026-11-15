//! RocksDB under the servers' `check_point` policy (save after every
//! acknowledged write): a save at home is a commit point — a WAL fsync —
//! not a memtable flush, so small writes leave no table file (and no open
//! descriptor) behind each; RocksDB opens with a bound on open files and
//! old info logs (review H-12 / gap-rocksdb-exec#2, #8).

#![cfg(feature = "rocksdb")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gstore::config::GStoreConfig;
use gstore::server::Server;
use gstore::{Database, QueryResult};

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d =
            std::env::temp_dir().join(format!("gstore_rocks_ckpt_{tag}_{}", std::process::id()));
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

/// The table files of the store in `dir`.
fn sst_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("rocksdb"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sst"))
        .count()
}

/// The descriptors this process holds open on table files of `dir`'s
/// store (Linux; `None` elsewhere).
fn open_sst_descriptors(dir: &Path) -> Option<usize> {
    let fds = std::fs::read_dir("/proc/self/fd").ok()?;
    let store = std::fs::canonicalize(dir.join("rocksdb")).ok()?;
    Some(
        fds.filter_map(Result::ok)
            .filter_map(|e| std::fs::read_link(e.path()).ok())
            .filter(|t| t.starts_with(&store) && t.to_string_lossy().ends_with(".sst"))
            .count(),
    )
}

/// POST `body` to `/update` and return the status line and body.
fn update(addr: SocketAddr, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST /update HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/sparql-update\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

const WRITES: usize = 120;

/// 120 single-triple updates through a single-database server with
/// check_point on: the store gains no table file per write (it used to gain
/// about three: every save flushed all seven column families) and holds no
/// descriptor per write; every write survives the shutdown and a reopen.
#[test]
fn checkpointed_single_triple_writes_leave_no_table_file_per_write() {
    let tmp = TempDir::new("server");
    let dir = tmp.0.join("w.db");
    let mut base = String::new();
    for i in 0..100 {
        base.push_str(&format!("<http://ex/b{i}> <http://ex/p> \"b{i}\" .\n"));
    }
    drop(Database::build_rocksdb_from_str("w", &dir, &base).unwrap());
    let db = Database::open(&dir).unwrap();
    let ssts_before = sst_files(&dir);
    let fds_before = open_sst_descriptors(&dir);
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_db_dir(&dir)
            .with_check_point(true),
    );
    assert!(server.check_point());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = std::thread::spawn(move || s2.serve_forever());
    // Stops the server also when an assertion below fails.
    struct Stop(Arc<Server>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.shutdown();
        }
    }
    let stop = Stop(Arc::clone(&server));
    for i in 0..WRITES {
        let (status, body) = update(
            addr,
            &format!("INSERT DATA {{ <http://ex/w{i}> <http://ex/q> \"w{i}\" }}"),
        );
        assert_eq!(status, 200, "write {i}: {body}");
        assert!(!body.contains("failed"), "write {i}: {body}");
    }
    let ssts = sst_files(&dir);
    assert!(
        ssts <= ssts_before + 7,
        "{WRITES} checkpointed writes grew the store from {ssts_before} to {ssts} table files"
    );
    if let (Some(before), Some(now)) = (fds_before, open_sst_descriptors(&dir)) {
        assert!(
            now <= before + 7,
            "{WRITES} checkpointed writes grew the open table-file descriptors from \
             {before} to {now}"
        );
    }
    drop(stop);
    serving.join().unwrap();
    drop(server);
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), 100 + WRITES as u64);
    match db
        .query("SELECT (COUNT(*) AS ?n) WHERE { ?s <http://ex/q> ?o }")
        .unwrap()
    {
        QueryResult::Select(rs) => {
            let n = rs.rows[0][0].clone().unwrap();
            assert!(n.starts_with(&format!("\"{WRITES}\"")), "{n}");
        }
        other => panic!("{other:?}"),
    }
}

/// The same through the library: `save` at home after each write (what
/// check_point does) adds no table files; `checkpoint` flushes the
/// memtables, and so does closing the database.
#[test]
fn save_at_home_is_a_commit_point_and_checkpoint_flushes() {
    let tmp = TempDir::new("library");
    let dir = tmp.0.join("l.db");
    drop(
        Database::build_rocksdb_from_str("l", &dir, "<http://ex/a> <http://ex/p> \"a\" .\n")
            .unwrap(),
    );
    let mut db = Database::open(&dir).unwrap();
    let before = sst_files(&dir);
    for i in 0..60 {
        db.query(&format!(
            "INSERT DATA {{ <http://ex/n{i}> <http://ex/p> \"n{i}\" }}"
        ))
        .unwrap();
        db.save(&dir).unwrap();
    }
    assert!(
        sst_files(&dir) <= before + 7,
        "60 saves at home: {before} -> {} table files",
        sst_files(&dir)
    );
    db.checkpoint().unwrap();
    let flushed = sst_files(&dir);
    assert!(flushed > before, "checkpoint flushes the memtables");
    db.query("INSERT DATA { <http://ex/last> <http://ex/p> \"last\" }")
        .unwrap();
    db.sync().unwrap();
    drop(db);
    assert!(sst_files(&dir) > flushed, "closing flushes the memtables");
    let db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), 62);
}

/// RocksDB is opened with a bounded number of open files (`[storage]
/// rocksdb_max_open_files`, default 4096), a few old info logs and a bound
/// on the write-ahead logs: the options file RocksDB writes records them.
#[test]
fn rocksdb_opens_with_bounded_files_and_logs() {
    let tmp = TempDir::new("options");
    let dir = tmp.0.join("o.db");
    drop(
        Database::build_rocksdb_from_str("o", &dir, "<http://ex/a> <http://ex/p> \"a\" .\n")
            .unwrap(),
    );
    drop(Database::open(&dir).unwrap());
    let options = latest_options_file(&dir);
    for want in [
        "max_open_files=4096",
        "keep_log_file_num=4",
        "max_total_wal_size=268435456",
    ] {
        assert!(
            options.lines().any(|l| l.trim() == want),
            "the store's options lack {want}"
        );
    }
}

/// `[storage] rocksdb_max_open_files`: its default, its range, and its
/// plumbing into the RocksDB tuning (`0` = unlimited).
#[test]
fn the_max_open_files_key_reaches_the_rocksdb_tuning() {
    let mut cfg = GStoreConfig::default();
    assert_eq!(cfg.rocksdb_max_open_files, 4096);
    assert_eq!(cfg.rocks_tuning().max_open_files, 4096);
    assert_eq!(cfg.value("rocksdb_max_open_files").as_deref(), Some("4096"));
    cfg.set("rocksdb_max_open_files", "0").unwrap();
    assert_eq!(cfg.rocks_tuning().max_open_files, -1, "0 = unlimited");
    cfg.set("rocksdb_max_open_files", "512").unwrap();
    assert_eq!(cfg.rocks_tuning().max_open_files, 512);
    for bad in ["10", "-1", "many", "2000000"] {
        assert!(
            cfg.set("rocksdb_max_open_files", bad).is_err(),
            "rocksdb_max_open_files={bad} was taken"
        );
    }
    assert_eq!(cfg.rocks_tuning().max_open_files, 512);
}

/// The text of the newest `OPTIONS-*` file of `dir`'s store.
fn latest_options_file(dir: &Path) -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("rocksdb"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("OPTIONS-"))
        })
        .collect();
    files.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_string_lossy()[8..].parse::<u64>().ok())
            .unwrap_or(0)
    });
    std::fs::read_to_string(files.last().expect("an OPTIONS file")).unwrap()
}
