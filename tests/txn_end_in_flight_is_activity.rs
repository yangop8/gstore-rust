//! A `/commit` or `/rollback` (REST, and `/api` `commit` / `rollback`)
//! accepted while its transaction was live counts as the transaction's
//! activity until it has ended it, like a statement in flight (round-2
//! review of gap-txn-lifecycle#1/#5): when it waits for the database's
//! guard behind another holder past `txn_idle_timeout`, it still commits
//! (or rolls back) as asked, and no reaper rolls the transaction back
//! meanwhile. Before the fix such a commit was refused as "not running
//! (rolled back after txn_idle_timeout)" and its writes were lost.
//!
//! Synchronisation is deterministic: the waiting end request is parked by a
//! test hook (`debug_park_txn`, `EndQueued`), the guard holder at
//! `StatementEnded`, the transaction clock is moved by
//! `debug_advance_txn_clock`, and the periodic reaper is woken and awaited
//! by its hooks. Timeouts are hang guards only.

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
/// `txn_idle_timeout`, seconds.
const TIMEOUT: u64 = 600;
/// Past [`TIMEOUT`].
const IDLE: Duration = Duration::from_secs(TIMEOUT + 1);

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_txn_end_in_flight_{tag}_{}_{n}",
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

/// One HTTP request (`Connection: close`) to `addr`: `(status, body)`.
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

/// `POST /api` as root (password `pw`) with string parameters.
fn api_at(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let mut items = vec![
        format!("{}: {}", json::quote("username"), json::quote("root")),
        format!("{}: {}", json::quote("password"), json::quote("pw")),
    ];
    items.extend(
        pairs
            .iter()
            .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v))),
    );
    let (st, body) = send(
        addr,
        "POST",
        "/api",
        None,
        &format!("{{{}}}", items.join(", ")),
    );
    assert_eq!(st, 200, "/api always answers HTTP 200: {body}");
    json::parse(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

/// An in-process multi-database server (root password `pw`); shut down and
/// joined when dropped.
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
    serving: Option<thread::JoinHandle<gstore::server::ShutdownReport>>,
    _tmp: TmpDir,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
    }
}

impl Api {
    fn start(tag: &str, db: &str) -> Api {
        let tmp = TmpDir::new(tag);
        let root = tmp.0.join("root");
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        config.txn_idle_timeout = TIMEOUT;
        let server = ApiServer::bind(&root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config);
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        let api = Api {
            server,
            addr,
            root,
            serving,
            _tmp: tmp,
        };
        let (st, body) = api.rest("POST", &format!("/build?db_name={db}"), BASE);
        assert_eq!(st, 200, "build: {body}");
        api
    }

    fn rest(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        send(self.addr, method, target, Some(ROOT), body)
    }

    fn begin(&self, db: &str) -> String {
        let j = api_at(self.addr, &[("operation", "begin"), ("db_name", db)]);
        assert_eq!(code(&j), 0, "begin: {j}");
        j.get("TID").and_then(Json::as_str).unwrap().to_string()
    }

    fn insert(&self, db: &str, tid: &str, subject: &str) {
        let (st, body) = self.rest(
            "POST",
            &format!("/tquery?db_name={db}&tid={tid}"),
            &format!("INSERT DATA {{ <http://ex/{subject}> <http://ex/p> <http://ex/o> }}"),
        );
        assert_eq!(st, 200, "{body}");
    }

    fn ask(&self, db: &str, subject: &str) -> bool {
        let (st, body) = self.rest(
            "POST",
            &format!("/sparql?db_name={db}"),
            &format!("ASK {{ <http://ex/{subject}> ?p ?o }}"),
        );
        assert_eq!(st, 200, "{body}");
        body.contains("true")
    }

    /// Move the transaction clock past `txn_idle_timeout`, then let the
    /// periodic reaper run one full pass on it.
    fn go_idle_and_reap(&self) {
        assert!(http_api::debug_advance_txn_clock(&self.root, IDLE));
        let wake = http_api::debug_wake_txn_reaper(&self.root).expect("serving");
        assert!(
            http_api::debug_wait_txn_reaper_pass(&self.root, wake, HANG_GUARD),
            "the reaper ran a pass"
        );
    }
}

/// The realistic case: a REST `/commit` waits for the guard a statement of
/// its own transaction still holds (parked after its evaluation), and the
/// wait outlasts `txn_idle_timeout`.
#[test]
fn a_rest_commit_waiting_for_the_guard_past_the_idle_timeout_commits() {
    let db = "endwait";
    let api = Api::start("rest_commit", db);
    let tid = api.begin(db);
    api.insert(db, &tid, "first");
    // A second statement of the transaction holds the exclusive guard.
    let holder = http_api::debug_park_txn(db, TxnParkPoint::StatementEnded);
    let addr = api.addr;
    let target = format!("/tquery?db_name={db}&tid={tid}");
    let held = thread::spawn(move || {
        send(
            addr,
            "POST",
            &target,
            Some(ROOT),
            "INSERT DATA { <http://ex/second> <http://ex/p> <http://ex/o> }",
        )
    });
    assert!(
        holder.wait_parked(HANG_GUARD),
        "the statement holds the guard"
    );
    // The commit is accepted (its transaction is live) and goes to wait for
    // the guard.
    let queued = http_api::debug_park_txn(db, TxnParkPoint::EndQueued);
    let target = format!("/commit?db_name={db}&tid={tid}");
    let commit = thread::spawn(move || send(addr, "POST", &target, Some(ROOT), ""));
    assert!(queued.wait_parked(HANG_GUARD), "the commit was accepted");
    queued.release();
    // The wait outlasts txn_idle_timeout; a reaper pass runs meanwhile.
    api.go_idle_and_reap();
    holder.release();
    assert_eq!(held.join().unwrap().0, 200);
    let (st, body) = commit.join().unwrap();
    assert_eq!(st, 200, "the commit in flight was activity: {body}");
    assert!(api.ask(db, "first"), "committed");
    assert!(api.ask(db, "second"), "committed");
}

/// `/api` `commit` and `rollback`, each parked after it was accepted while
/// the transaction goes idle and a reaper pass runs.
#[test]
fn an_api_commit_or_rollback_in_flight_is_not_turned_into_an_idle_rollback() {
    let db = "endwaitapi";
    let api = Api::start("api_end", db);
    for (op, kept) in [("commit", true), ("rollback", false)] {
        let tid = api.begin(db);
        let subject = format!("by_{op}");
        api.insert(db, &tid, &subject);
        let queued = http_api::debug_park_txn(db, TxnParkPoint::EndQueued);
        let addr = api.addr;
        let tid2 = tid.clone();
        let end = thread::spawn(move || {
            api_at(addr, &[("operation", op), ("db_name", db), ("tid", &tid2)])
        });
        assert!(queued.wait_parked(HANG_GUARD), "the {op} was accepted");
        api.go_idle_and_reap();
        queued.release();
        let j = end.join().unwrap();
        assert_eq!(code(&j), 0, "the {op} in flight was activity: {j}");
        assert_eq!(api.ask(db, &subject), kept, "{op}");
        drop(queued);
    }
}
