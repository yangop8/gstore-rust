//! Concurrent `begin`s of one RocksDB database copy it one at a time (round-2
//! review of gap-rocksdb-exec#3): a RocksDB `begin` copies the whole store
//! for its transaction's read snapshot, under the shared guard; a second
//! `begin` that arrives meanwhile waits for the first to register its
//! transaction, then finds it open and is refused without copying. Before
//! the fix every concurrent `begin` passed the "no transaction open" check
//! and made its own whole-store copy at once (all but one thrown away),
//! multiplying the memory a begin needs.
//!
//! Synchronisation is deterministic: the first copy is parked
//! (`BeginCapture`), and the later begins are observed queuing behind it
//! (`BeginQueued`). Timeouts are hang guards only.
#![cfg(feature = "rocksdb")]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::{self, ApiServer, TxnParkPoint};
use gstore::json::{self, Json};

const HANG_GUARD: Duration = Duration::from_secs(60);
const ROOT: &str = "Basic cm9vdDpwdw==";
const LOSERS: u64 = 3;

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n";

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_rocks_begins_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    auth: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    if let Some(a) = auth {
        raw.push_str(&format!("Authorization: {a}\r\n"));
    }
    if target.starts_with("/api") {
        raw.push_str("Content-Type: application/json\r\n");
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp)
        .unwrap_or_else(|e| panic!("{method} {target}: no answer ({e})"));
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn api_begin(addr: SocketAddr, db: &str) -> Json {
    let body = format!(
        "{{\"username\": \"root\", \"password\": \"pw\", \"operation\": \"begin\", \
         \"db_name\": {}}}",
        json::quote(db)
    );
    let (st, body) = send(addr, "POST", "/api", None, &body);
    assert_eq!(st, 200, "{body}");
    json::parse(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

#[test]
fn concurrent_rocksdb_begins_make_one_copy() {
    let db = "rkbegins";
    let tmp = TmpDir::new("one_copy");
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let server = Arc::new(
        ApiServer::bind(tmp.0.join("root"), "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (st, body) = send(
        addr,
        "POST",
        &format!("/build?db_name={db}&layout=rocksdb"),
        Some(ROOT),
        BASE,
    );
    assert_eq!(st, 200, "{body}");

    let copying = http_api::debug_park_txn(db, TxnParkPoint::BeginCapture);
    let queued = http_api::debug_park_txn(db, TxnParkPoint::BeginQueued);
    let first = thread::spawn(move || api_begin(addr, db));
    assert!(copying.wait_parked(HANG_GUARD), "the first begin copies");
    let losers: Vec<_> = (0..LOSERS)
        .map(|_| thread::spawn(move || api_begin(addr, db)))
        .collect();
    assert!(
        queued.wait_parked_n(LOSERS, HANG_GUARD),
        "the later begins queue behind the copy (copies made: {})",
        copying.parked()
    );
    assert_eq!(copying.parked(), 1, "only one begin copies");
    queued.release();
    copying.release();

    let first = first.join().unwrap();
    assert_eq!(code(&first), 0, "{first}");
    for loser in losers {
        let j = loser.join().unwrap();
        assert_ne!(code(&j), 0, "one transaction per database: {j}");
        assert!(j.to_string().contains("already open"), "{j}");
    }
    assert_eq!(
        copying.parked(),
        1,
        "the refused begins copied nothing (they found the transaction open)"
    );
    drop(queued);
    drop(copying);
    server.shutdown();
    let _ = serving.join();
}
