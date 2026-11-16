//! The lifecycle of HTTP transactions (REST `/begin` … `/commit`, and the
//! upstream `/api` protocol's `begin` / `tquery` / `commit` / `rollback`)
//! at its edges — review gap-txn-lifecycle#1-#3, #5, #6, http#18, db#7 and
//! gap-rocksdb-exec#3:
//!
//! * a commit whose commit point fails is answered as rolled back and is
//!   never visible nor durable afterwards — or, when it can no longer be
//!   undone, answered as applied but not durable — and the transaction log
//!   records the outcome only once the commit returned (M2, http#18);
//! * an idle transaction is rolled back after `txn_idle_timeout` by each
//!   reaping home — a new `begin`, any request, the periodic reaper of an
//!   idle server — and the reapers never wait for a busy database; a
//!   running statement is activity; the owner (whatever their current
//!   privileges) and every superuser can end a transaction; a deleted
//!   user's transaction does not block the database (M3);
//! * a stale transaction id (an earlier load's, or an earlier process's,
//!   in the `<begin>_<tid>` form of the transaction log) never ends the
//!   transaction that has the number now, and transactions a killed server
//!   left `RUNNING` in the log become `ABORTED` at the next start (M4);
//! * an empty rollback keeps the caches (db#7);
//! * a RocksDB `begin` copies the database for its read snapshot without
//!   blocking the database's readers, and copies again when a write slips
//!   in before it registers (gap-rocksdb-exec#3).
//!
//! Synchronisation is deterministic: the server's transaction clock is
//! moved by a test hook (`debug_advance_txn_clock`), the periodic reaper is
//! woken by one (`debug_wake_txn_reaper`), steps are parked by latches
//! (`debug_park_txn`) or a SPARQL `SERVICE` endpoint the test holds, and
//! storage failures come from the operating system (`RLIMIT_FSIZE` on a
//! `gserver` child). Timeouts are hang guards only.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::db::LoadPolicy;
use gstore::http_api::{self, ApiServer, TxnParkPoint};
use gstore::json::{self, Json};
use gstore::{Database, QueryResult};

/// A hang guard for every blocking step (never an assertion).
const HANG_GUARD: Duration = Duration::from_secs(60);
const ROOT: &str = "Basic cm9vdDpwdw==";
const ALICE: &str = "Basic YWxpY2U6cHc=";
const BOB: &str = "Basic Ym9iOnB3";
const CAROL: &str = "Basic Y2Fyb2w6cHc=";
/// The default `txn_idle_timeout`, seconds.
const TIMEOUT: u64 = 600;
/// Past [`TIMEOUT`].
const IDLE: Duration = Duration::from_secs(TIMEOUT + 1);

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n";

// ---- helpers ---------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_txn_lifecycle_{tag}_{}_{n}",
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

/// `POST /api` as `user` (password `pw`) with string parameters: the
/// envelope (always HTTP 200).
fn api_at(addr: SocketAddr, user: &str, pairs: &[(&str, &str)]) -> Json {
    let mut items = vec![
        format!("{}: {}", json::quote("username"), json::quote(user)),
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
    parse(&body)
}

fn parse(body: &str) -> Json {
    json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn text(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
        .to_string()
}

/// The records of the transaction log (`<root>/logs/transaction.json`),
/// last state per (database, `TID`), read from the file — no request.
fn txnlog(root: &Path) -> Vec<Json> {
    let text = fs::read_to_string(root.join("logs").join("transaction.json")).unwrap_or_default();
    let mut out: Vec<Json> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let rec = parse(line);
        let key = (text_of(&rec, "db_name"), text_of(&rec, "TID"));
        out.retain(|r| (text_of(r, "db_name"), text_of(r, "TID")) != key);
        out.push(rec);
    }
    out
}

fn text_of(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `<begin>_<tid>` id and state the transaction log shows for
/// transaction `tid` of `db` (the newest one with that number).
fn logged(root: &Path, db: &str, tid: &str) -> (String, String) {
    txnlog(root)
        .iter()
        .rev()
        .find(|r| text_of(r, "db_name") == db && text_of(r, "TID").ends_with(&format!("_{tid}")))
        .map(|r| (text_of(r, "TID"), text_of(r, "state")))
        .unwrap_or_else(|| panic!("transaction {tid} of {db} in {:?}", txnlog(root)))
}

/// An in-process multi-database server (root password `pw`) whose
/// `SERVICE` may reach this host; shut down when dropped.
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
    _tmp: TmpDir,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

impl Api {
    fn start(tag: &str, txn_idle_timeout: u64) -> Api {
        let tmp = TmpDir::new(tag);
        let root = tmp.0.join("root");
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        config.txn_idle_timeout = txn_idle_timeout;
        let server = ApiServer::bind(&root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_load_policy(LoadPolicy::permissive());
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        let api = Api {
            server,
            addr,
            root,
            _tmp: tmp,
        };
        // An answer proves the server serves (its transaction housekeeping
        // starts before it accepts).
        assert_eq!(api.rest("GET", "/status", ROOT, "").0, 200);
        api
    }

    fn rest(&self, method: &str, target: &str, auth: &str, body: &str) -> (u16, String) {
        send(self.addr, method, target, Some(auth), body)
    }

    fn api(&self, user: &str, pairs: &[(&str, &str)]) -> Json {
        api_at(self.addr, user, pairs)
    }

    fn build(&self, db: &str) {
        let (st, body) = self.rest("POST", &format!("/build?db_name={db}"), ROOT, BASE);
        assert_eq!(st, 200, "build: {body}");
    }

    /// Add user `name` (password `pw`) with privilege codes `privileges` on
    /// `db`.
    fn add_user(&self, name: &str, db: &str, privileges: &str) {
        let j = self.api(
            "root",
            &[
                ("operation", "usermanage"),
                ("type", "1"),
                ("op_username", name),
                ("op_password", "pw"),
            ],
        );
        assert_eq!(code(&j), 0, "{j}");
        let j = self.api(
            "root",
            &[
                ("operation", "userprivilegemanage"),
                ("type", "1"),
                ("op_username", name),
                ("db_name", db),
                ("privileges", privileges),
            ],
        );
        assert_eq!(code(&j), 0, "{j}");
    }

    /// `/api begin` on `db` as `user`: the TID.
    fn begin(&self, user: &str, db: &str) -> String {
        let j = self.api(user, &[("operation", "begin"), ("db_name", db)]);
        assert_eq!(code(&j), 0, "begin: {j}");
        text(&j, "TID")
    }

    fn tquery(&self, user: &str, db: &str, tid: &str, sparql: &str) -> Json {
        self.api(
            user,
            &[
                ("operation", "tquery"),
                ("db_name", db),
                ("tid", tid),
                ("sparql", sparql),
            ],
        )
    }

    fn end(&self, user: &str, op: &str, db: &str, tid: &str) -> Json {
        self.api(user, &[("operation", op), ("db_name", db), ("tid", tid)])
    }

    /// A non-transactional write by root: whether it was applied.
    fn write(&self, db: &str, subject: &str) -> Json {
        self.api(
            "root",
            &[
                ("operation", "query"),
                ("db_name", db),
                (
                    "sparql",
                    &format!("INSERT DATA {{ <http://ex/{subject}> <http://ex/p> <http://ex/o> }}"),
                ),
            ],
        )
    }

    fn ask(&self, db: &str, subject: &str) -> bool {
        let j = self.api(
            "root",
            &[
                ("operation", "query"),
                ("db_name", db),
                ("sparql", &format!("ASK {{ <http://ex/{subject}> ?p ?o }}")),
            ],
        );
        assert_eq!(code(&j), 0, "{j}");
        j.to_string().contains("true")
    }

    /// Whether transaction `tid` of `db` takes statements.
    fn is_open(&self, db: &str, tid: &str) -> bool {
        code(&self.tquery("root", db, tid, "ASK { ?s ?p ?o }")) == 0
    }

    /// The transaction REST `/show` reports open on `db`, if any (`TID`).
    fn open_tid(&self, db: &str) -> Option<String> {
        let (st, body) = self.rest("GET", "/show", ROOT, "");
        assert_eq!(st, 200, "{body}");
        let j = parse(&body);
        let dbs = j.get("databases").and_then(Json::as_array).unwrap();
        let item = dbs.iter().find(|d| text_of(d, "name") == db)?;
        item.get("transaction").map(|t| text(t, "TID"))
    }

    fn advance_clock(&self, by: Duration) {
        assert!(http_api::debug_advance_txn_clock(&self.root, by));
    }
}

/// A SPARQL endpoint that answers each request (an empty result) only once
/// the test releases it: `arrived` gets a message when a request is in.
struct HeldEndpoint {
    addr: SocketAddr,
    arrived: Receiver<()>,
    release: Option<Sender<()>>,
}

impl HeldEndpoint {
    fn start() -> HeldEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (arrived_tx, arrived) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let _ = conn.set_read_timeout(Some(HANG_GUARD));
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let l = line.to_ascii_lowercase();
                    if let Some(v) = l.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                let _ = arrived_tx.send(());
                let _ = release_rx.recv_timeout(HANG_GUARD);
                let answer = r#"{"head":{"vars":["s"]},"results":{"bindings":[]}}"#;
                let _ = conn.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                        answer.len()
                    )
                    .as_bytes(),
                );
            }
        });
        HeldEndpoint {
            addr,
            arrived,
            release: Some(release),
        }
    }

    fn release(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for HeldEndpoint {
    fn drop(&mut self) {
        self.release();
    }
}

// ---- db#7 ----------------------------------------------------------------------

/// Review db#7: rolling back a transaction that changed nothing — only
/// reads, or a write that changed no triple — leaves the mutation count,
/// the cached query results and the CSR as they were.
#[test]
fn an_empty_rollback_keeps_the_caches() {
    let mut db = Database::build_from_str("cache", BASE).unwrap();
    let q = "SELECT ?s WHERE { ?s <http://ex/p> ?o }";
    db.query(q).unwrap();
    db.preload_csr().unwrap();
    let (count, cached) = (db.mutation_count(), db.query_cache_len());
    assert!(cached > 0 && db.csr_is_cached());
    db.begin().unwrap();
    db.query(q).unwrap();
    db.rollback().unwrap();
    assert_eq!(db.mutation_count(), count, "a read-only rollback");
    assert_eq!(db.query_cache_len(), cached, "a read-only rollback");
    assert!(db.csr_is_cached(), "a read-only rollback");
    db.begin().unwrap();
    let r = db
        .query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(r, QueryResult::Update { changed: 0 });
    db.rollback().unwrap();
    assert_eq!(db.mutation_count(), count, "a no-op write's rollback");
    assert!(db.csr_is_cached(), "a no-op write's rollback");
    // A rollback that undid something still invalidates.
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }")
        .unwrap();
    db.rollback().unwrap();
    assert!(db.mutation_count() > count);
    assert!(!db.csr_is_cached());
}

// ---- M3: idle transactions, owners, deleted users ------------------------------

/// Reaping home 1, a new `begin` (review gap-txn-lifecycle#1, #5): the
/// transaction of a deleted user does not block its database — the next
/// `begin` rolls it back (also with `txn_idle_timeout` 0, when nothing
/// else reaps), logged as aborted.
#[test]
fn a_new_begin_rolls_back_the_transaction_of_a_deleted_user() {
    let api = Api::start("orphan", 0);
    api.build("db1");
    api.add_user("alice", "db1", "1,4");
    let tid = api.begin("alice", "db1");
    let j = api.tquery(
        "alice",
        "db1",
        &tid,
        "INSERT DATA { <http://ex/gone> <http://ex/p> <http://ex/o> }",
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api.api(
        "root",
        &[
            ("operation", "usermanage"),
            ("type", "2"),
            ("op_username", "alice"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // Before: "1008 ... has an open transaction" until root found the TID.
    let next = api.begin("root", "db1");
    assert_eq!(next, "2");
    let (_, state) = logged(&api.root, "db1", &tid);
    assert_eq!(state, "ABORTED");
    assert!(!api.ask("db1", "gone"), "its write was rolled back");
    let j = api.end("root", "commit", "db1", &tid);
    assert!(
        msg(&j).contains("rolled back because its owner was deleted"),
        "{j}"
    );
    assert_eq!(code(&api.end("root", "commit", "db1", &next)), 0);
}

/// Reaping home 2, any request (review gap-txn-lifecycle#1): once the
/// transaction is idle past `txn_idle_timeout`, the next request — here a
/// write it would have refused — rolls it back first.
#[test]
fn any_request_rolls_back_an_idle_transaction() {
    let api = Api::start("request", TIMEOUT);
    api.build("db1");
    api.add_user("alice", "db1", "1,4");
    let tid = api.begin("alice", "db1");
    let j = api.tquery(
        "alice",
        "db1",
        &tid,
        "INSERT DATA { <http://ex/idle> <http://ex/p> <http://ex/o> }",
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api.write("db1", "early");
    assert_eq!(code(&j), 1004, "the open transaction refuses: {j}");
    api.advance_clock(Duration::from_secs(TIMEOUT - 1));
    assert_eq!(
        code(&api.write("db1", "early")),
        1004,
        "not idle long enough"
    );
    api.advance_clock(Duration::from_secs(2));
    let j = api.write("db1", "late");
    assert_eq!(code(&j), 0, "the idle transaction was rolled back: {j}");
    assert!(!api.ask("db1", "idle"));
    assert_eq!(logged(&api.root, "db1", &tid).1, "ABORTED");
    let j = api.end("alice", "commit", "db1", &tid);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(
        msg(&j).contains("not running (already rolled back after txn_idle_timeout)"),
        "{j}"
    );
    assert_eq!(api.open_tid("db1"), None);
}

/// Reaping home 3, the periodic reaper of an idle server (review
/// gap-txn-lifecycle#1, gap-rocksdb-exec#3: it also frees a RocksDB
/// database's copy): no request at all after the transaction went idle.
#[test]
fn the_periodic_reaper_rolls_back_an_idle_transaction_on_an_idle_server() {
    let api = Api::start("periodic", TIMEOUT);
    api.build("db1");
    let tid = api.begin("root", "db1");
    assert_eq!(logged(&api.root, "db1", &tid).1, "RUNNING");
    api.advance_clock(IDLE);
    let wake = http_api::debug_wake_txn_reaper(&api.root).unwrap();
    // A pass that started after the wake-up (one in flight may have read
    // the clock before it moved).
    assert!(
        http_api::debug_wait_txn_reaper_pass(&api.root, wake, HANG_GUARD),
        "the periodic reaper made a pass"
    );
    // Observed in the log file, with no request in between.
    assert_eq!(logged(&api.root, "db1", &tid).1, "ABORTED");
    assert_eq!(api.open_tid("db1"), None);
    assert_eq!(api.begin("root", "db1"), "2");
}

/// The ORCHESTRATOR note of the plan, and the second-pass amendment: a
/// statement in flight is activity — its transaction is not answered as
/// ended nor reaped while it runs, however long — and the idle clock
/// restarts when it ends.
#[test]
fn a_running_statement_keeps_its_transaction_alive() {
    let api = Api::start("running", TIMEOUT);
    api.build("db1");
    let tid = api.begin("root", "db1");
    let mut endpoint = HeldEndpoint::start();
    let read = format!(
        "SELECT ?s WHERE {{ SERVICE <http://{}/sparql> {{ ?s ?p ?o }} }}",
        endpoint.addr
    );
    let addr = api.addr;
    let tid2 = tid.clone();
    let parked = thread::spawn(move || {
        api_at(
            addr,
            "root",
            &[
                ("operation", "tquery"),
                ("db_name", "db1"),
                ("tid", &tid2),
                ("sparql", &read),
            ],
        )
    });
    endpoint
        .arrived
        .recv_timeout(HANG_GUARD)
        .expect("the statement reached its SERVICE endpoint");
    api.advance_clock(IDLE);
    // Requests run reaping passes: the transaction stays.
    assert_eq!(api.open_tid("db1").as_deref(), Some(tid.as_str()));
    let j = api.tquery("root", "db1", &tid, "ASK { ?s ?p ?o }");
    assert_eq!(code(&j), 0, "a running statement keeps it live: {j}");
    endpoint.release();
    let j = parked.join().unwrap();
    assert_eq!(code(&j), 0, "{j}");
    // Its end restarted the idle clock.
    assert_eq!(api.open_tid("db1").as_deref(), Some(tid.as_str()));
    api.advance_clock(IDLE);
    assert_eq!(api.open_tid("db1"), None, "idle again: reaped");
    assert_eq!(logged(&api.root, "db1", &tid).1, "ABORTED");
}

/// The second-pass amendment: the request-path reaper and the periodic one
/// never wait for a database guard. A transaction idle past the timeout
/// whose database's guard is held (its last statement parked after it
/// ended) is skipped: `POST /cancel` of that statement and an unrelated
/// request answer at once; once the guard is free, the next request reaps
/// it, and the registry and the database agree throughout.
#[test]
fn the_reapers_skip_a_busy_database_instead_of_waiting_for_it() {
    let api = Api::start("busy", TIMEOUT);
    api.build("db1");
    api.build("db2");
    let tid = api.begin("root", "db1");
    let park = http_api::debug_park_txn("db1", TxnParkPoint::StatementEnded);
    let addr = api.addr;
    let tid2 = tid.clone();
    let parked = thread::spawn(move || {
        send(
            addr,
            "POST",
            &format!("/tquery?db_name=db1&tid={tid2}"),
            Some(ROOT),
            "SELECT ?s WHERE { ?s ?p ?o }",
        )
    });
    assert!(park.wait_parked(HANG_GUARD), "the statement parked");
    api.advance_clock(IDLE);
    // An unrelated request and the periodic reaper skip db1 (busy).
    let j = api.write("db2", "other");
    assert_eq!(code(&j), 0, "{j}");
    let wake = http_api::debug_wake_txn_reaper(&api.root).unwrap();
    assert!(
        http_api::debug_wait_txn_reaper_pass(&api.root, wake, HANG_GUARD),
        "a periodic pass that did not wait for db1"
    );
    // /cancel of the parked statement answers at once.
    let (st, body) = api.rest("GET", "/tasklist", ROOT, "");
    assert_eq!(st, 200, "{body}");
    let task = parse(&body)
        .get("tasks")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .find(|t| text_of(t, "db") == "db1")
        .map(|t| text(t, "opt_id"))
        .unwrap_or_else(|| panic!("the parked statement in {body}"));
    let (st, body) = api.rest("POST", &format!("/cancel?id={task}"), ROOT, "");
    assert_eq!(st, 200, "{body}");
    // Not reaped: still open, in the registry and the database alike.
    assert_eq!(api.open_tid("db1").as_deref(), Some(tid.as_str()));
    assert_eq!(logged(&api.root, "db1", &tid).1, "RUNNING");
    park.release();
    let (st, _) = parked.join().unwrap();
    assert!(st == 200 || st == 400, "{st}");
    // Free now: the next request reaps it (the statement's end was stamped
    // before the clock moved).
    assert_eq!(code(&api.write("db1", "after")), 0);
    assert_eq!(api.open_tid("db1"), None);
    assert_eq!(logged(&api.root, "db1", &tid).1, "ABORTED");
    assert_eq!(api.begin("root", "db1"), "2");
}

/// Review gap-txn-lifecycle#5: the owner may roll back their transaction
/// whatever their current privileges, any superuser (not only the literal
/// `root`) may end any transaction, and a user holding all seven database
/// privileges still may not end another user's.
#[test]
fn the_owner_and_superusers_can_end_a_transaction() {
    let api = Api::start("owners", TIMEOUT);
    api.build("db1");
    api.add_user("alice", "db1", "1,4");
    api.add_user("bob", "db1", "1,2,3,4,5,6,7");
    api.add_user("carol", "db1", "1");
    let (st, body) = api.rest(
        "POST",
        "/user/grant?op_username=carol&privileges=admin",
        ROOT,
        "",
    );
    assert_eq!(st, 200, "{body}");

    // bob: every privilege, not the owner.
    let tid = api.begin("alice", "db1");
    let j = api.end("bob", "rollback", "db1", &tid);
    assert_eq!(code(&j), 1004, "{j}");
    assert!(msg(&j).contains("belongs to another user"), "{j}");
    let (st, _) = api.rest("POST", &format!("/rollback?db_name=db1&tid={tid}"), BOB, "");
    assert_eq!(st, 403);
    // carol (admin): over REST and /api.
    let (st, body) = api.rest(
        "POST",
        &format!("/rollback?db_name=db1&tid={tid}"),
        CAROL,
        "",
    );
    assert_eq!(st, 200, "{body}");
    let tid = api.begin("alice", "db1");
    let j = api.end("carol", "rollback", "db1", &tid);
    assert_eq!(code(&j), 0, "{j}");

    // alice, after her update privilege was revoked, rolls back her own.
    let tid = api.begin("alice", "db1");
    let j = api.api(
        "root",
        &[
            ("operation", "userprivilegemanage"),
            ("type", "2"),
            ("op_username", "alice"),
            ("db_name", "db1"),
            ("privileges", "4"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api.end("alice", "commit", "db1", &tid);
    assert_eq!(code(&j), 1004, "committing still needs the privilege: {j}");
    let (st, body) = api.rest(
        "POST",
        &format!("/rollback?db_name=db1&tid={tid}"),
        ALICE,
        "",
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(api.open_tid("db1"), None);
}

/// Monitoring shows the open transaction — id (also in the transaction
/// log's form), owner, idle time — so an operator can find the one that
/// blocks a database (review gap-txn-lifecycle#1).
#[test]
fn show_and_monitor_expose_the_open_transaction() {
    let api = Api::start("monitor", TIMEOUT);
    api.build("db1");
    api.add_user("alice", "db1", "1,4");
    let tid = api.begin("alice", "db1");
    api.advance_clock(Duration::from_secs(42));
    let j = api.api("root", &[("operation", "show")]);
    let item = j
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .find(|d| text_of(d, "database") == "db1")
        .cloned()
        .unwrap();
    let t = item.get("transaction").unwrap_or_else(|| panic!("{item}"));
    assert_eq!(text(t, "TID"), tid);
    assert_eq!(text(t, "user"), "alice");
    assert!(
        t.get("idle_seconds").and_then(Json::as_i64).unwrap() >= 42,
        "{t}"
    );
    let (long, _) = logged(&api.root, "db1", &tid);
    assert_eq!(text(t, "txnlog_TID"), long);
    let j = api.api("root", &[("operation", "monitor"), ("db_name", "db1")]);
    assert_eq!(text(j.get("transaction").unwrap(), "user"), "alice", "{j}");
    let (st, body) = api.rest("GET", "/monitor", ROOT, "");
    assert_eq!(st, 200);
    assert!(body.contains("\"transaction\""), "{body}");
}

// ---- M4: stale transaction ids -------------------------------------------------

/// Review gap-txn-lifecycle#2 (variant b): after an unload and a load the
/// numbering restarts, but the `<begin>_<tid>` id of the earlier load's
/// transaction 1 names that one — already committed — never the new
/// transaction 1, and how it ended is still known.
#[test]
fn a_stale_tid_of_an_earlier_load_never_ends_the_new_transaction() {
    let api = Api::start("reload", TIMEOUT);
    api.build("db1");
    let old = api.begin("root", "db1");
    assert_eq!(old, "1");
    let (old_long, _) = logged(&api.root, "db1", &old);
    assert_eq!(code(&api.end("root", "commit", "db1", &old)), 0);
    let (st, body) = api.rest("POST", "/unload?db_name=db1", ROOT, "");
    assert_eq!(st, 200, "{body}");
    let (st, body) = api.rest("POST", "/load?db_name=db1", ROOT, "");
    assert_eq!(st, 200, "{body}");
    let new = api.begin("root", "db1");
    assert_eq!(new, "1", "ids restart per load, as upstream");
    let j = api.tquery("root", "db1", &new, "DELETE WHERE { <http://ex/a> ?p ?o }");
    assert_eq!(code(&j), 0, "{j}");
    // A client retrying the old commit in the transaction log's form.
    let j = api.end("root", "commit", "db1", &old_long);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("already committed"), "{j}");
    let (st, body) = api.rest(
        "POST",
        &format!("/commit?db_name=db1&tid={old_long}"),
        ROOT,
        "",
    );
    assert_eq!(st, 409, "{body}");
    // The new transaction is untouched: still open, its delete uncommitted.
    assert!(api.is_open("db1", &new));
    assert!(
        api.ask("db1", "a"),
        "the half-done delete was not committed"
    );
    // A made-up begin time is no transaction at all.
    let j = api.end("root", "rollback", "db1", "1700000000_1");
    assert!(msg(&j).contains("Wrong transaction id"), "{j}");
    assert!(api.is_open("db1", &new));
    let (new_long, _) = logged(&api.root, "db1", &new);
    assert_ne!(new_long, old_long);
    assert_eq!(code(&api.end("root", "rollback", "db1", &new_long)), 0);
    assert!(api.ask("db1", "a"));
}

/// The REST endpoints parse transaction ids as `/api` does (review
/// gap-txn-lifecycle#2): no sign, no other separator.
#[test]
fn rest_transaction_ids_are_parsed_like_the_api() {
    let api = Api::start("parse", TIMEOUT);
    api.build("db1");
    let tid = api.begin("root", "db1");
    assert_eq!(tid, "1");
    for bad in ["%2B1", "1_%2B1", "x_1"] {
        let (st, body) = api.rest(
            "POST",
            &format!("/rollback?db_name=db1&tid={bad}"),
            ROOT,
            "",
        );
        assert_eq!(st, 400, "{bad}: {body}");
    }
    assert!(api.is_open("db1", &tid));
}

// ---- gserver children: a killed server, injected storage failures -------------

#[cfg(target_os = "linux")]
mod child {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }

    extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        fn signal(signum: i32, handler: usize) -> usize;
    }

    const RLIMIT_FSIZE: i32 = 1;
    const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;

    /// A `gserver --api` child (root password `pw`), killed (`SIGKILL`) and
    /// reaped when dropped.
    pub(super) struct Gserver {
        child: Child,
        pub(super) addr: SocketAddr,
    }

    impl Gserver {
        /// Serve `root` from `cwd` (no `conf/conf.ini` there), in disk query
        /// mode `mode`, with `RLIMIT_FSIZE` = `fsize` when given.
        pub(super) fn spawn(cwd: &Path, root: &Path, mode: &str, fsize: Option<u64>) -> Gserver {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_gserver"));
            cmd.arg(root)
                .args(["--api", "--port", "0", "--password", "pw"])
                .env("GSTORE_DISK_QUERY_MODE", mode)
                .current_dir(cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            if let Some(limit) = fsize {
                let rl = RLimit {
                    cur: limit,
                    max: limit,
                };
                // SAFETY: setrlimit(2) and signal(2) are async-signal-safe;
                // the closure only calls them on values it owns.
                unsafe {
                    cmd.pre_exec(move || {
                        if setrlimit(RLIMIT_FSIZE, &rl) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        // A write past the limit fails with EFBIG instead of
                        // killing the process.
                        signal(SIGXFSZ, SIG_IGN);
                        Ok(())
                    });
                }
            }
            let mut child = cmd.spawn().expect("spawn gserver");
            let stdout = child.stdout.take().unwrap();
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(rest) = line.split("http://").nth(1) {
                        let addr = rest.split_whitespace().next().unwrap_or("").to_string();
                        let _ = tx.send(addr);
                    }
                }
            });
            let addr = match rx.recv_timeout(HANG_GUARD) {
                Ok(a) => a.parse().expect("socket address"),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("gserver did not print its address");
                }
            };
            Gserver { child, addr }
        }

        pub(super) fn kill(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }

        pub(super) fn rest(&self, method: &str, target: &str, body: &str) -> (u16, String) {
            send(self.addr, method, target, Some(ROOT), body)
        }

        pub(super) fn api(&self, user: &str, pairs: &[(&str, &str)]) -> Json {
            api_at(self.addr, user, pairs)
        }
    }

    impl Drop for Gserver {
        fn drop(&mut self) {
            self.kill();
        }
    }
}

/// Review gap-txn-lifecycle#2 (variant a, the Workbench path): a server is
/// killed with a transaction open; at the next start the transaction log
/// shows it `ABORTED` (not `RUNNING` forever), and rolling back its stale
/// `<begin>_1` id does not end the new transaction 1 of the same user.
#[cfg(target_os = "linux")]
#[test]
fn a_stale_tid_of_a_killed_server_never_ends_the_new_transaction() {
    use child::Gserver;
    let tmp = TmpDir::new("killed");
    let root = tmp.0.join("root");
    fs::create_dir_all(&root).unwrap();
    let mut first = Gserver::spawn(&tmp.0, &root, "memory", None);
    let (st, body) = first.rest("POST", "/build?db_name=db1", BASE);
    assert_eq!(st, 200, "{body}");
    let j = first.api("root", &[("operation", "begin"), ("db_name", "db1")]);
    let tid = text(&j, "TID");
    assert_eq!(tid, "1");
    let (stale, state) = logged(&root, "db1", &tid);
    assert_eq!(state, "RUNNING");
    first.kill();

    let second = Gserver::spawn(&tmp.0, &root, "memory", None);
    let (st, body) = second.rest("POST", "/load?db_name=db1", "");
    assert_eq!(st, 200, "{body}");
    let rec = txnlog(&root)
        .into_iter()
        .find(|r| text_of(r, "TID") == stale)
        .unwrap();
    assert_eq!(text_of(&rec, "state"), "ABORTED", "{rec}");
    let j = second.api("root", &[("operation", "begin"), ("db_name", "db1")]);
    assert_eq!(text(&j, "TID"), "1");
    let j = second.api(
        "root",
        &[
            ("operation", "tquery"),
            ("db_name", "db1"),
            ("tid", "1"),
            (
                "sparql",
                "INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/o> }",
            ),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // Root cleans up the stale row the way the Workbench does.
    let j = second.api(
        "root",
        &[
            ("operation", "rollback"),
            ("db_name", "db1"),
            ("tid", &stale),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
    // The live transaction is still running.
    let j = second.api(
        "root",
        &[
            ("operation", "tquery"),
            ("db_name", "db1"),
            ("tid", "1"),
            ("sparql", "ASK { <http://ex/new> ?p ?o }"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    assert!(j.to_string().contains("true"), "{j}");
}

/// The size of `kvstore.kv` of the on-disk database in `dir`.
#[cfg(target_os = "linux")]
fn kv_size(dir: &Path) -> u64 {
    fs::metadata(dir.join("kvstore.kv")).unwrap().len()
}

/// `INSERT DATA` of `n` new triples about `tag`.
#[cfg(target_os = "linux")]
fn bulk(tag: &str, n: usize) -> String {
    let mut s = String::from("INSERT DATA {\n");
    for i in 0..n {
        s.push_str(&format!(
            "<http://ex/{tag}/{i}> <http://ex/value> \"{tag} literal number {i} with padding\" .\n"
        ));
    }
    s.push('}');
    s
}

/// Which of `tags` the on-disk database at `dir` holds (reopened: its
/// write-ahead log replayed), in query mode `mode`.
#[cfg(target_os = "linux")]
fn durable(dir: &Path, tags: &[&str]) -> Vec<String> {
    let db = Database::open(dir).unwrap_or_else(|e| panic!("reopen {}: {e}", dir.display()));
    tags.iter()
        .filter(|t| {
            db.query_read(
                &format!("ASK {{ <http://ex/{t}/0> <http://ex/value> ?v }}"),
                None,
                None,
            )
            .unwrap()
                == QueryResult::Ask(true)
        })
        .map(|t| t.to_string())
        .collect()
}

/// Review gap-txn-lifecycle#3 (M2) and http#18, through the real server:
/// the disk fills up (`RLIMIT_FSIZE` below the size of the transaction's
/// write-ahead log, so the commit point fails before anything reaches the
/// file) while an on-disk database commits a transaction of 600 triples.
/// The commit is answered as rolled back, no reader sees the transaction
/// (Materialize mode; a Stream-mode store refuses reads until it is
/// reopened), a retried commit is told so (not "already committed"), the
/// transaction log says `ABORTED`, and neither a reload nor a restart
/// brings it back. Before: answered "commit failed", yet visible to every
/// reader, retried as "already committed", logged as committed — and made
/// durable by the next successful save.
#[cfg(target_os = "linux")]
#[test]
fn a_commit_whose_commit_point_fails_is_rolled_back_and_never_visible() {
    use child::Gserver;
    for (mode, via_api) in [("memory", false), ("stream", true)] {
        let tmp = TmpDir::new(&format!("rolled_{mode}"));
        let root = tmp.0.join("root");
        fs::create_dir_all(&root).unwrap();
        // Build the database (no limit), then serve it with one.
        let mut setup = Gserver::spawn(&tmp.0, &root, mode, None);
        let (st, body) = setup.rest("POST", "/build?db_name=db1&layout=disk", BASE);
        assert_eq!(st, 200, "{body}");
        setup.kill();
        let dir = root.join("db1.db");
        let limit = 64 * 1024;
        assert!(kv_size(&dir) <= limit, "{}", kv_size(&dir));
        let mut srv = Gserver::spawn(&tmp.0, &root, mode, Some(limit));
        let (st, body) = srv.rest("POST", "/load?db_name=db1", "");
        assert_eq!(st, 200, "{body}");
        let j = srv.api("root", &[("operation", "begin"), ("db_name", "db1")]);
        let tid = text(&j, "TID");
        let j = srv.api(
            "root",
            &[
                ("operation", "tquery"),
                ("db_name", "db1"),
                ("tid", &tid),
                ("sparql", &bulk("t", 600)),
            ],
        );
        assert_eq!(code(&j), 0, "{mode}: {j}");
        if via_api {
            let j = srv.api(
                "root",
                &[("operation", "commit"), ("db_name", "db1"), ("tid", &tid)],
            );
            assert_eq!(code(&j), 1005, "{mode}: {j}");
            assert!(msg(&j).contains("was rolled back"), "{mode}: {j}");
        } else {
            let (st, body) = srv.rest("POST", &format!("/commit?db_name=db1&tid={tid}"), "");
            assert_eq!(st, 500, "{mode}: {body}");
            assert!(body.contains("was rolled back"), "{mode}: {body}");
            // What every reader sees: the state before the transaction.
            let (st, body) = srv.rest(
                "POST",
                "/sparql?db_name=db1",
                "ASK { <http://ex/t/0> ?p ?o }",
            );
            assert_eq!(st, 200, "{mode}: {body}");
            assert!(body.contains("false"), "{mode}: visible: {body}");
        }
        let j = srv.api(
            "root",
            &[("operation", "commit"), ("db_name", "db1"), ("tid", &tid)],
        );
        assert!(
            msg(&j).contains("already rolled back after its commit failed"),
            "{mode}: {j}"
        );
        assert_eq!(logged(&root, "db1", &tid).1, "ABORTED", "{mode}");
        // The store failed: a reload (no commit point) recovers it.
        let (st, body) = srv.rest("POST", "/unload?db_name=db1", "");
        assert_eq!(st, 200, "{mode}: {body}");
        let (st, body) = srv.rest("POST", "/load?db_name=db1", "");
        assert_eq!(st, 200, "{mode}: {body}");
        let (st, body) = srv.rest(
            "POST",
            "/sparql?db_name=db1",
            "ASK { <http://ex/t/0> ?p ?o }",
        );
        assert_eq!(st, 200, "{mode}: {body}");
        assert!(body.contains("false"), "{mode}: after the reload: {body}");
        srv.kill();
        assert!(durable(&dir, &["t"]).is_empty(), "{mode}: became durable");
    }
}

/// Review gap-txn-lifecycle#3: when the commit point fails after the
/// transaction's write-ahead log was complete (`RLIMIT_FSIZE` = the size of
/// `kvstore.kv`: the log is written, growing the file fails), the
/// transaction can no longer be undone — the next open replays the log.
/// It is answered as applied but not durable (the database needs a
/// reload), stays visible, and the reopened database holds it.
#[cfg(target_os = "linux")]
#[test]
fn a_commit_that_can_no_longer_be_undone_is_answered_as_not_durable() {
    use child::Gserver;
    for mode in ["memory", "stream"] {
        let tmp = TmpDir::new(&format!("notdurable_{mode}"));
        let root = tmp.0.join("root");
        fs::create_dir_all(&root).unwrap();
        let mut setup = Gserver::spawn(&tmp.0, &root, mode, None);
        let (st, body) = setup.rest("POST", "/build?db_name=db1&layout=disk", BASE);
        assert_eq!(st, 200, "{body}");
        let (st, body) = setup.rest("POST", "/load?db_name=db1", "");
        assert_eq!(st, 200, "{body}");
        let (st, body) = setup.rest("POST", "/update?db_name=db1", &bulk("base", 3000));
        assert_eq!(st, 200, "{body}");
        let (st, body) = setup.rest("POST", "/unload?db_name=db1", "");
        assert_eq!(st, 200, "{body}");
        setup.kill();
        let dir = root.join("db1.db");
        let mut srv = Gserver::spawn(&tmp.0, &root, mode, Some(kv_size(&dir)));
        let (st, body) = srv.rest("POST", "/load?db_name=db1", "");
        assert_eq!(st, 200, "{body}");
        let (st, body) = srv.rest("POST", "/begin?db_name=db1", "");
        assert_eq!(st, 200, "{body}");
        let tid = text(&parse(&body), "TID");
        let (st, body) = srv.rest(
            "POST",
            &format!("/tquery?db_name=db1&tid={tid}"),
            &bulk("t", 300),
        );
        assert_eq!(st, 200, "{mode}: {body}");
        let (st, body) = srv.rest("POST", &format!("/commit?db_name=db1&tid={tid}"), "");
        assert_eq!(st, 500, "{mode}: {body}");
        assert!(
            body.contains("applied in memory but not durable"),
            "{mode}: {body}"
        );
        assert!(body.contains("needs a reload"), "{mode}: {body}");
        if mode == "memory" {
            let (st, body) = srv.rest(
                "POST",
                "/sparql?db_name=db1",
                "ASK { <http://ex/t/0> ?p ?o }",
            );
            assert_eq!(st, 200, "{body}");
            assert!(body.contains("true"), "applied, as answered: {body}");
        }
        assert_eq!(logged(&root, "db1", &tid).1, "ABORTED", "{mode}");
        srv.kill();
        assert_eq!(durable(&dir, &["t"]), ["t"], "{mode}: the log is replayed");
    }
}

// ---- gap-rocksdb-exec#3: the RocksDB begin copy ---------------------------------

/// Review gap-rocksdb-exec#3: a RocksDB `begin` copies the database for its
/// read snapshot under the shared guard — a read of the same database
/// completes while the copy is parked (before: the copy held the exclusive
/// guard, and the read waited for it) — and copies again when a write
/// slips in between the copy and the registration, so the transaction's
/// snapshot, which every other client reads, holds that write.
#[cfg(feature = "rocksdb")]
#[test]
fn a_rocksdb_begin_copies_without_blocking_readers_and_recopies_after_a_write() {
    let api = Api::start("rocks", TIMEOUT);
    let (st, body) = api.rest("POST", "/build?db_name=rkdb&layout=rocksdb", ROOT, BASE);
    assert_eq!(st, 200, "{body}");
    let addr = api.addr;

    // A read completes while the copy is parked.
    let park = http_api::debug_park_txn("rkdb", TxnParkPoint::BeginCapture);
    let begin =
        thread::spawn(move || api_at(addr, "root", &[("operation", "begin"), ("db_name", "rkdb")]));
    assert!(park.wait_parked(HANG_GUARD), "the copy parked");
    let (st, body) = api.rest(
        "POST",
        "/sparql?db_name=rkdb",
        ROOT,
        "ASK { <http://ex/a> ?p ?o }",
    );
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("true"), "{body}");
    park.release();
    let j = begin.join().unwrap();
    assert_eq!(code(&j), 0, "{j}");
    let tid = text(&j, "TID");
    assert_eq!(code(&api.end("root", "rollback", "rkdb", &tid)), 0);
    drop(park);

    // A write between the copy and the registration forces a new copy.
    let park = http_api::debug_park_txn("rkdb", TxnParkPoint::BeginCaptured);
    let begin =
        thread::spawn(move || api_at(addr, "root", &[("operation", "begin"), ("db_name", "rkdb")]));
    assert!(
        park.wait_parked(HANG_GUARD),
        "the begin parked after its copy"
    );
    assert_eq!(code(&api.write("rkdb", "slipped")), 0);
    park.release();
    let j = begin.join().unwrap();
    assert_eq!(code(&j), 0, "{j}");
    let tid = text(&j, "TID");
    // Other clients read the begin-time snapshot: it has the write.
    assert!(api.ask("rkdb", "slipped"), "the snapshot predates the write");
    assert_eq!(code(&api.end("root", "rollback", "rkdb", &tid)), 0);
}
