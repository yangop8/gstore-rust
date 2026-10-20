//! Data test: regressions from the Phase 0 data-safety review.
//!
//! * A restore validates the backup — of any layout — before the target is
//!   touched, and swaps the copy in as a whole directory: a corrupt or
//!   truncated backup leaves the existing database intact (library,
//!   `grestore`, HTTP `/restore`), and a failed HTTP restore leaves the loaded
//!   database served, unsaved changes included.
//! * (`rocksdb` feature) A RocksDB transaction is crash-atomic: uncommitted
//!   writes are in neither a crash's leftovers nor a checkpoint copy.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use gstore::http_api::ApiServer;
use gstore::{Database, QueryResult};

const DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n:a :name \"Alice\" .\n";

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gstore_review_it_{tag}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn ask(db: &mut Database, q: &str) -> bool {
    db.query(q).unwrap() == QueryResult::Ask(true)
}

/// A snapshot database `t.db` under `base` with [`DATA`].
fn snapshot_target(base: &Path) -> PathBuf {
    let target = base.join("t.db");
    Database::build_from_str("t", DATA)
        .unwrap()
        .save(&target)
        .unwrap();
    target
}

fn assert_original_intact(target: &Path) {
    let mut db = Database::open(target).expect("the original database still opens");
    assert_eq!(db.triple_num(), 2);
    assert!(ask(&mut db, "ASK { <http://ex/a> <http://ex/p> <http://ex/b> }"));
    let staged = PathBuf::from(format!("{}.restore-tmp", target.display()));
    assert!(!staged.exists(), "the staged copy is cleaned up");
}

/// A directory that looks like an on-disk backup (`kvstore.kv`) but holds
/// garbage.
fn garbage_disk_backup(base: &Path) -> PathBuf {
    let bad = base.join("bad_backup");
    std::fs::create_dir_all(&bad).unwrap();
    let junk: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    std::fs::write(bad.join("kvstore.kv"), junk).unwrap();
    bad
}

#[test]
fn restoring_a_corrupt_disk_backup_leaves_the_target_intact() {
    let base = tmp_dir("restore_garbage");
    let target = snapshot_target(&base);
    let bad = garbage_disk_backup(&base);
    assert!(Database::restore_into(&bad, &target).is_err());
    assert_original_intact(&target);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn restoring_a_truncated_disk_backup_leaves_the_target_intact() {
    let base = tmp_dir("restore_truncated");
    let target = snapshot_target(&base);
    // A real on-disk backup, then cut in half (a partial copy).
    let nt = base.join("data.nt");
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!("<http://ex/s{i}> <http://ex/p> \"v{i}\" .\n"));
    }
    std::fs::write(&nt, text).unwrap();
    let bak = base.join("disk_backup");
    Database::build_disk(&bak, &[&nt]).unwrap();
    let kv = bak.join("kvstore.kv");
    let len = std::fs::metadata(&kv).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&kv)
        .unwrap()
        .set_len(len / 2)
        .unwrap();
    assert!(Database::restore_into(&bak, &target).is_err());
    assert_original_intact(&target);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn grestore_of_a_corrupt_backup_fails_and_keeps_the_database() {
    let base = tmp_dir("grestore_garbage");
    let target = snapshot_target(&base);
    let bad = garbage_disk_backup(&base);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_grestore"))
        .arg(&bad)
        .arg(base.join("t"))
        .output()
        .unwrap();
    assert!(!out.status.success(), "grestore must fail: {out:?}");
    assert_original_intact(&target);
    let _ = std::fs::remove_dir_all(&base);
}

/// A restore interrupted after the old directory was moved aside but before
/// the restored copy was moved in is completed by the next open.
#[test]
fn an_interrupted_restore_swap_is_completed_on_open() {
    let base = tmp_dir("restore_resume");
    let target = base.join("t.db");
    let restored = PathBuf::from(format!("{}.restore-new", target.display()));
    let old = PathBuf::from(format!("{}.restore-old", target.display()));
    Database::build_from_str("t", "<http://ex/new> <http://ex/p> <http://ex/o> .")
        .unwrap()
        .save(&restored)
        .unwrap();
    Database::build_from_str("t", DATA).unwrap().save(&old).unwrap();
    let mut db = Database::open(&target).unwrap();
    assert!(ask(&mut db, "ASK { <http://ex/new> <http://ex/p> <http://ex/o> }"));
    assert!(!restored.exists() && !old.exists());
    let _ = std::fs::remove_dir_all(&base);
}

// ---- HTTP /restore -------------------------------------------------------

fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    // root:root
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Basic cm9vdDpyb290\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn start_api(root: &Path, check_point: bool) -> (Arc<ApiServer>, SocketAddr, thread::JoinHandle<gstore::server::ShutdownReport>) {
    let server = Arc::new(
        ApiServer::bind(root, "root", "127.0.0.1:0")
            .unwrap()
            .with_check_point(check_point),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    (server, addr, serving)
}

#[test]
fn api_restore_of_a_corrupt_backup_keeps_the_loaded_database_and_its_changes() {
    let base = tmp_dir("api_restore_bad");
    let root = base.join("root");
    // check_point off: the update below stays unsaved until shutdown.
    let (server, addr, serving) = start_api(&root, false);
    let (s, b) = request(&addr, "POST", "/build?db_name=t", DATA);
    assert_eq!(s, 200, "build: {b}");
    let (s, b) = request(
        &addr,
        "POST",
        "/update?db_name=t",
        "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
    );
    assert_eq!(s, 200, "update: {b}");

    for bad in [garbage_disk_backup(&base), {
        let d = base.join("bad_snapshot");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("meta.bin"), [0xFFu8; 4096]).unwrap();
        d
    }] {
        let (s, b) = request(
            &addr,
            "POST",
            &format!("/restore?db_name=t&path={}", bad.display()),
            "",
        );
        assert_eq!(s, 500, "a corrupt backup is refused: {b}");
        let (_, show) = request(&addr, "GET", "/show", "");
        assert!(
            show.contains("\"name\":\"t\",\"loaded\":true") && show.contains("\"dirty\":true"),
            "the database stays loaded with its unsaved change: {show}"
        );
        let (s, b) = request(
            &addr,
            "POST",
            "/sparql?db_name=t",
            "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }",
        );
        assert!(s == 200 && b.contains("true"), "still served: {s} {b}");
    }
    // The original snapshot on disk is untouched, and shutdown saves the change.
    let mut on_disk = Database::open(root.join("t.db")).unwrap();
    assert_eq!(on_disk.triple_num(), 2);
    drop(on_disk);
    server.shutdown();
    let report = serving.join().unwrap();
    assert_eq!(report.saved, vec![root.join("t.db")], "{report:?}");
    on_disk = Database::open(root.join("t.db")).unwrap();
    assert!(ask(&mut on_disk, "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }"));
    let _ = std::fs::remove_dir_all(&base);
}

// ---- RocksDB transactions --------------------------------------------------

#[cfg(feature = "rocksdb")]
mod rocks {
    use super::*;

    const TXN_WRITES: &str = "INSERT DATA { <http://x> <http://p> <http://y> . \
                              GRAPH <http://g> { <http://gx> <http://gp> <http://gy> } }";

    fn rocks_db(tag: &str) -> PathBuf {
        let dir = tmp_dir(tag).join("r.db");
        drop(Database::build_rocksdb_from_str("r", &dir, "<http://a> <http://p> <http://b> .").unwrap());
        dir
    }

    fn assert_only_committed(dir: &Path) {
        let mut db = Database::open(dir).unwrap();
        assert!(!db.in_transaction());
        assert_eq!(db.triple_num(), 1, "only the committed triple");
        assert!(ask(&mut db, "ASK { <http://a> <http://p> <http://b> }"));
        assert!(!ask(&mut db, "ASK { <http://x> <http://p> <http://y> }"));
        assert!(!ask(&mut db, "ASK { GRAPH <http://g> { ?s ?p ?o } }"));
    }

    /// A checkpoint copy taken while a transaction is open is crash-consistent:
    /// opening it yields the committed state only.
    #[test]
    fn a_checkpoint_during_a_transaction_holds_only_committed_data() {
        let dir = rocks_db("rocks_txn_checkpoint");
        let mut db = Database::open(&dir).unwrap();
        db.begin().unwrap();
        db.query(TXN_WRITES).unwrap();
        let copy = dir.with_file_name("copy.db");
        db.save(&copy).unwrap();
        assert_only_committed(&copy);
        db.commit().unwrap();
        drop(db);
        let mut db = Database::open(&dir).unwrap();
        assert!(ask(&mut db, "ASK { <http://x> <http://p> <http://y> }"), "committed");
        assert!(ask(&mut db, "ASK { GRAPH <http://g> { <http://gx> ?p ?o } }"));
    }

    /// Child half of the crash test: open a transaction, write, and die
    /// without committing (no destructors run).
    #[test]
    #[ignore = "run as a child process by uncommitted_rocks_writes_do_not_survive_a_crash"]
    fn child_crashes_inside_a_rocks_transaction() {
        let Ok(dir) = std::env::var("GSTORE_CRASH_DIR") else {
            return;
        };
        let mut db = Database::open(&dir).unwrap();
        db.begin().unwrap();
        db.query(TXN_WRITES).unwrap();
        std::process::abort();
    }

    #[test]
    fn uncommitted_rocks_writes_do_not_survive_a_crash() {
        let dir = rocks_db("rocks_txn_crash");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "rocks::child_crashes_inside_a_rocks_transaction",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env("GSTORE_CRASH_DIR", &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "the child must have crashed");
        assert_only_committed(&dir);
    }
}
