//! `query_timeout` bounds a statement's evaluation, not its wait for the
//! database (round-2 review of sec#10/robust#5): a trivial statement queued
//! behind another holder of its database's guard — a transaction statement,
//! a long write, a save — is not answered as timed out once it finally runs.
//! Before the fix the deadline was armed when the request arrived, so it
//! fired while the statement waited for the guard, and the statement was
//! stopped at its first cancellation check (REST and single-database `503`,
//! `/api` `1005`, "query timed out").
//!
//! Synchronisation is deterministic: the holder is parked on its guard and
//! the queued statement's arrival at the guard is observed through test
//! hooks (`debug_park_txn`). The one wait (`HOLD`) is the scenario itself —
//! the holder keeps the guard longer than `query_timeout` — not a
//! synchronisation: the queued statement passes whatever the wait is.

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
use gstore::server::Server;
use gstore::Database;

/// Bounds every blocking step (a hang guard, never the assertion).
const HANG_GUARD: Duration = Duration::from_secs(120);

/// The `query_timeout` of every server here: far longer than a trivial
/// statement's evaluation.
const TIMEOUT: Duration = Duration::from_millis(500);

/// How long the holder keeps the guard while the statement waits: past
/// [`TIMEOUT`].
const HOLD: Duration = Duration::from_millis(1500);

/// `Authorization` for root (password `pw`).
const ROOT: &str = "Basic cm9vdDpwdw==";

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n";

const ASK: &str = "ASK { <http://ex/a> <http://ex/p> <http://ex/b> }";

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_timeout_evaluation_only_{tag}_{}_{n}",
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

/// One HTTP request (`Connection: close`): `(status, body)`.
fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
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

fn rest(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    send(addr, method, target, &[("Authorization", ROOT)], body)
}

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
        &[("Content-Type", "application/json")],
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

/// Run `f` on a thread; join with [`HANG_GUARD`] as the bound.
struct Bg<T>(std::sync::mpsc::Receiver<T>);

impl<T: Send + 'static> Bg<T> {
    fn spawn(f: impl FnOnce() -> T + Send + 'static) -> Bg<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        Bg(rx)
    }

    fn join(self, what: &str) -> T {
        self.0
            .recv_timeout(HANG_GUARD)
            .unwrap_or_else(|_| panic!("{what} was never answered"))
    }
}

/// An in-process multi-database server (root password `pw`) holding
/// database `db`; shut down and joined when dropped.
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
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
    fn start(db: &str) -> Api {
        let tmp = TmpDir::new(db);
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        config.query_timeout = TIMEOUT;
        let server = ApiServer::bind(tmp.0.join("root"), "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config);
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        let api = Api {
            server,
            addr,
            serving,
            _tmp: tmp,
        };
        let (st, body) = rest(addr, "POST", &format!("/build?db_name={db}"), DATA);
        assert_eq!(st, 200, "build: {body}");
        api
    }

    fn begin(&self, db: &str) -> String {
        let (st, body) = rest(self.addr, "POST", &format!("/begin?db_name={db}"), "");
        assert_eq!(st, 200, "begin: {body}");
        json::parse(&body)
            .unwrap()
            .get("TID")
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("no TID: {body}"))
            .to_string()
    }

    /// Park a `/tquery` INSERT of transaction `tid` on `db` after its
    /// statement, holding the exclusive guard: the parked request.
    fn hold_with_tquery(&self, db: &str, tid: &str) -> (http_api::TxnPark, Bg<(u16, String)>) {
        let park = http_api::debug_park_txn(db, TxnParkPoint::StatementEnded);
        let addr = self.addr;
        let target = format!("/tquery?db_name={db}&tid={tid}");
        let held = Bg::spawn(move || {
            rest(
                addr,
                "POST",
                &target,
                "INSERT DATA { <http://ex/held> <http://ex/p> \"held\" }",
            )
        });
        assert!(park.wait_parked(HANG_GUARD), "the holder parked");
        (park, held)
    }
}

/// Let the statement `start` sends reach its database's guard (observed at
/// the `StatementQueued` hook of `db`), keep the guard held for [`HOLD`],
/// then release `holder`: the statement's answer.
fn queue_behind<T: Send + 'static>(
    db: &str,
    holder: &http_api::TxnPark,
    start: impl FnOnce() -> T + Send + 'static,
) -> T {
    let queued = http_api::debug_park_txn(db, TxnParkPoint::StatementQueued);
    let statement = Bg::spawn(start);
    assert!(
        queued.wait_parked(HANG_GUARD),
        "the statement reached its database"
    );
    // It now waits for the guard the holder keeps.
    queued.release();
    thread::sleep(HOLD);
    holder.release();
    statement.join("the queued statement")
}

#[test]
fn a_rest_read_queued_behind_a_transaction_statement_is_not_timed_out() {
    let db = "tq_rest_read";
    let api = Api::start(db);
    let tid = api.begin(db);
    let (holder, held) = api.hold_with_tquery(db, &tid);
    let addr = api.addr;
    let target = format!("/sparql?db_name={db}");
    let (st, body) = queue_behind(db, &holder, move || rest(addr, "POST", &target, ASK));
    assert_eq!(st, 200, "the wait is not the read's time: {body}");
    assert!(body.contains("true"), "{body}");
    assert_eq!(held.join("the holder").0, 200);
}

#[test]
fn an_api_query_queued_behind_a_transaction_statement_is_not_timed_out() {
    let db = "tq_api_query";
    let api = Api::start(db);
    let tid = api.begin(db);
    let (holder, held) = api.hold_with_tquery(db, &tid);
    let addr = api.addr;
    let j = queue_behind(db, &holder, move || {
        api_at(
            addr,
            &[("operation", "query"), ("db_name", db), ("sparql", ASK)],
        )
    });
    assert_eq!(code(&j), 0, "the wait is not the query's time: {j}");
    assert_eq!(held.join("the holder").0, 200);
}

#[test]
fn a_transaction_statement_queued_behind_another_is_not_timed_out() {
    let db = "tq_txn_statement";
    let api = Api::start(db);
    let tid = api.begin(db);
    let (holder, held) = api.hold_with_tquery(db, &tid);
    let addr = api.addr;
    let target = format!("/tquery?db_name={db}&tid={tid}");
    let (st, body) = queue_behind(db, &holder, move || {
        rest(
            addr,
            "POST",
            &target,
            "DELETE { ?s <http://ex/p> ?o } INSERT { ?s <http://ex/q> ?o } \
             WHERE { ?s <http://ex/p> ?o FILTER(?s = <http://ex/a>) }",
        )
    });
    assert_eq!(st, 200, "the wait is not the statement's time: {body}");
    assert_eq!(held.join("the holder").0, 200);
    let (st, body) = rest(addr, "POST", &format!("/commit?db_name={db}&tid={tid}"), "");
    assert_eq!(st, 200, "{body}");
}

#[test]
fn a_single_database_read_queued_behind_a_write_is_not_timed_out() {
    let db = "tq_single_read";
    let server = Arc::new(
        Server::bind(Database::build_from_str(db, DATA).unwrap(), "127.0.0.1:0")
            .unwrap()
            .with_query_timeout(TIMEOUT),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    // The holder: a write parked on the exclusive lock (its own outcome
    // does not matter here: it may be stopped by its deadline once let go).
    let holder = http_api::debug_park_txn(db, TxnParkPoint::StatementHolding);
    let write = Bg::spawn(move || {
        send(
            addr,
            "POST",
            "/update",
            &[("Content-Type", "application/sparql-update")],
            "INSERT DATA { <http://ex/held> <http://ex/p> \"held\" }",
        )
    });
    assert!(holder.wait_parked(HANG_GUARD), "the write holds the lock");
    let (st, body) = queue_behind(db, &holder, move || {
        send(
            addr,
            "POST",
            "/sparql",
            &[("Content-Type", "application/sparql-query")],
            ASK,
        )
    });
    assert_eq!(st, 200, "the wait is not the read's time: {body}");
    assert!(body.contains("true"), "{body}");
    let _ = write.join("the holder");
    server.shutdown();
    let _ = serving.join();
}
