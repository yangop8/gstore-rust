//! Query time and answer-size limits (review sec#10, robust#5, M7) and the
//! query log of failed `/api` queries (review gapi#8).
//!
//! * `query_timeout`: a runaway statement — a three-way cross product with
//!   an expensive FILTER — is stopped once its time is up and answered as
//!   timed out (REST `503`, `/api` `1005`), releasing its database: an
//!   UPDATE queued behind it then completes. The timeout is tiny and the
//!   statement takes far longer, so the outcome does not depend on timing.
//! * `max_result_bytes`: an answer whose serialization passes the budget is
//!   refused (REST and single-database `503`, `/api` `1005`); a streamed one
//!   is cut off without its closing chunk.
//! * every failed `/api` query is in the query log with its status code.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::Server;
use gstore::Database;

const HANG_GUARD: Duration = Duration::from_secs(120);

/// A three-way cross product whose FILTER never holds: N³ rows to test.
const CROSS: &str = "SELECT ?a ?d ?g WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . \
                     FILTER(STR(?c) = CONCAT(STR(?f), STR(?i), \"x\")) }";

const INSERT: &str = "INSERT DATA { <http://ex/new> <http://ex/p> \"new\" }";

/// `n` triples with long-ish terms.
fn data(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "<http://example.org/a/rather/long/subject/{i}> <http://example.org/p> \
                 \"value number {i} of the data\" .\n"
            )
        })
        .collect()
}

// ---- harness ------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_query_limits_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Reply {
    status: u16,
    body: String,
    raw: Vec<u8>,
}

fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    // A cut-off stream ends with a reset or an early EOF: keep what came.
    let _ = s.read_to_end(&mut resp);
    let text = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("header end");
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    Reply {
        status,
        body: body.to_string(),
        raw: resp,
    }
}

/// Run `f` on a thread; its result, or a failure after the hang guard.
fn guarded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

const ROOT_BASIC: &str = "Basic cm9vdDpyb290"; // root:root

fn api_server(dir: &TmpDir, config: GStoreConfig, db: &str, n: usize) -> SocketAddr {
    let server = Arc::new(
        ApiServer::bind(dir.0.join("home"), "root", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let r = send(
        addr,
        "POST",
        &format!("/build?db_name={db}"),
        &[("Authorization", ROOT_BASIC)],
        &data(n),
    );
    assert_eq!(r.status, 200, "build: {}", r.body);
    addr
}

fn rest_query(addr: SocketAddr, db: &str, q: &str) -> Reply {
    send(
        addr,
        "GET",
        &format!("/sparql?db_name={db}&query={}", pct(q)),
        &[("Authorization", ROOT_BASIC)],
        "",
    )
}

fn rest_update(addr: SocketAddr, db: &str, u: &str) -> Reply {
    send(
        addr,
        "POST",
        &format!("/update?db_name={db}"),
        &[
            ("Authorization", ROOT_BASIC),
            ("Content-Type", "application/sparql-update"),
        ],
        u,
    )
}

fn api(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
        .collect();
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        &format!("{{{}}}", items.join(", ")),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    json::parse(&r.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", r.body))
}

fn root_api(addr: SocketAddr, extra: &[(&str, &str)]) -> Json {
    let mut pairs = vec![("username", "root"), ("password", "root")];
    pairs.extend_from_slice(extra);
    api(addr, &pairs)
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .expect("StatusCode")
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Today's `/api` query-log records (newest first).
fn query_log(addr: SocketAddr) -> Vec<Json> {
    let j = root_api(addr, &[("operation", "querylogdate")]);
    let Some(today) = j
        .get("list")
        .and_then(Json::as_array)
        .and_then(|l| l.first())
        .and_then(Json::as_str)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    let j = root_api(
        addr,
        &[
            ("operation", "querylog"),
            ("date", &today),
            ("pageNo", "1"),
            ("pageSize", "100"),
        ],
    );
    j.get("list")
        .and_then(Json::as_array)
        .map(<[Json]>::to_vec)
        .unwrap_or_default()
}

fn timeout_config(timeout_ms: u64) -> GStoreConfig {
    let mut c = GStoreConfig::default();
    c.query_timeout = Duration::from_millis(timeout_ms);
    c
}

// ---- query_timeout --------------------------------------------------------------

#[test]
fn a_runaway_rest_query_times_out_and_a_queued_writer_then_completes() {
    let dir = TmpDir::new("rest_timeout");
    let addr = api_server(&dir, timeout_config(50), "cross", 100);
    let read = guarded(move || rest_query(addr, "cross", CROSS));
    let write = guarded(move || rest_update(addr, "cross", INSERT));
    let r = read
        .recv_timeout(HANG_GUARD)
        .expect("the query was never answered");
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("timed out"), "{}", r.body);
    let w = write
        .recv_timeout(HANG_GUARD)
        .expect("the writer never ran");
    assert_eq!(w.status, 200, "{}", w.body);
    // The database serves on.
    let r = rest_query(
        addr,
        "cross",
        "ASK { <http://ex/new> <http://ex/p> \"new\" }",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("true"), "{}", r.body);
}

#[test]
fn a_runaway_api_query_times_out_with_1005_and_is_logged() {
    let dir = TmpDir::new("api_timeout");
    let addr = api_server(&dir, timeout_config(50), "crossapi", 100);
    let j = root_api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "crossapi"),
            ("sparql", CROSS),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("timed out"), "{j}");
    let log = query_log(addr);
    assert!(
        log.iter()
            .any(|r| r.get("statusCode").and_then(Json::as_i64) == Some(1005)
                && r.get("statusMsg")
                    .and_then(Json::as_str)
                    .is_some_and(|m| m.contains("timed out"))),
        "{log:?}"
    );
}

#[test]
fn a_runaway_query_on_the_single_database_server_times_out() {
    let db = Database::build_from_str("cross", &data(100)).unwrap();
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_query_timeout(Duration::from_millis(50)),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let read = guarded(move || {
        send(
            addr,
            "GET",
            &format!("/sparql?query={}", pct(CROSS)),
            &[],
            "",
        )
    });
    let write = guarded(move || {
        send(
            addr,
            "POST",
            "/update",
            &[("Content-Type", "application/sparql-update")],
            INSERT,
        )
    });
    let r = read
        .recv_timeout(HANG_GUARD)
        .expect("the query was never answered");
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("timed out"), "{}", r.body);
    let w = write
        .recv_timeout(HANG_GUARD)
        .expect("the writer never ran");
    assert_eq!(w.status, 200, "{}", w.body);
    server.shutdown();
}

// ---- max_result_bytes ------------------------------------------------------------

fn budget_config(bytes: u64) -> GStoreConfig {
    let mut c = GStoreConfig::default();
    c.max_result_bytes = bytes;
    c
}

const ALL: &str = "SELECT * WHERE { ?s ?p ?o }";

#[test]
fn a_rest_answer_over_the_byte_budget_is_refused() {
    let dir = TmpDir::new("rest_budget");
    let addr = api_server(&dir, budget_config(2000), "big", 100);
    let r = rest_query(addr, "big", ALL);
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("result too large"), "{}", r.body);
    assert!(r.body.len() < 2000, "no part of the answer is sent");
    let r = rest_query(addr, "big", "SELECT * WHERE { ?s ?p ?o } LIMIT 3");
    assert_eq!(r.status, 200, "a small answer fits: {}", r.body);
}

#[test]
fn an_api_answer_over_the_byte_budget_is_refused_and_logged() {
    let dir = TmpDir::new("api_budget");
    let addr = api_server(&dir, budget_config(2000), "bigapi", 100);
    let j = root_api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "bigapi"),
            ("sparql", ALL),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("result too large"), "{j}");
    let j = root_api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "bigapi"),
            ("sparql", "SELECT * WHERE { ?s ?p ?o } LIMIT 3"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let log = query_log(addr);
    assert!(
        log.iter()
            .any(|r| r.get("statusCode").and_then(Json::as_i64) == Some(1005)),
        "{log:?}"
    );
}

#[test]
fn the_single_database_server_refuses_or_cuts_an_answer_over_the_budget() {
    let db = Database::build_from_str("big", &data(100)).unwrap();
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_max_result_bytes(2000),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let r = send(addr, "GET", &format!("/sparql?query={}", pct(ALL)), &[], "");
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("result too large"), "{}", r.body);
    let construct = "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }";
    let r = send(
        addr,
        "GET",
        &format!("/sparql?query={}", pct(construct)),
        &[],
        "",
    );
    assert_eq!(r.status, 503, "{}", r.body);
    // Streamed: the status is already sent, so the stream is cut off at the
    // budget, without its closing chunk.
    let r = send(
        addr,
        "GET",
        &format!("/sparql?stream=true&query={}", pct(ALL)),
        &[],
        "",
    );
    assert_eq!(r.status, 200);
    assert!(!r.raw.ends_with(b"0\r\n\r\n"), "the stream must be cut off");
    assert!(r.raw.len() < 4000, "{} bytes", r.raw.len());
    let small = "SELECT * WHERE { ?s ?p ?o } LIMIT 2";
    let r = send(
        addr,
        "GET",
        &format!("/sparql?stream=true&query={}", pct(small)),
        &[],
        "",
    );
    assert!(r.raw.ends_with(b"0\r\n\r\n"), "a small stream completes");
    server.shutdown();
}

// ---- the query log of failed /api queries ------------------------------------------

#[test]
fn failed_api_queries_are_in_the_query_log_with_their_status() {
    let dir = TmpDir::new("qlog");
    let addr = api_server(&dir, GStoreConfig::default(), "logged", 3);
    let ok = root_api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "logged"),
            ("sparql", ALL),
        ],
    );
    assert_eq!(code(&ok), 0, "{ok}");
    let parse = root_api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "logged"),
            ("sparql", "SELEC broken"),
        ],
    );
    assert_eq!(code(&parse), 1005, "{parse}");
    let format = root_api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "logged"),
            ("sparql", ALL),
            ("format", "html"),
        ],
    );
    assert_eq!(code(&format), 1005, "{format}");
    let log = query_log(addr);
    assert_eq!(log.len(), 3, "{log:?}");
    let status = |sparql: &str, fmt: &str| {
        log.iter()
            .find(|r| {
                r.get("sparql").and_then(Json::as_str) == Some(sparql)
                    && r.get("format").and_then(Json::as_str) == Some(fmt)
            })
            .and_then(|r| r.get("statusCode").and_then(Json::as_i64))
    };
    assert_eq!(status(ALL, "json"), Some(0), "{log:?}");
    assert_eq!(status("SELEC broken", "json"), Some(1005), "{log:?}");
    assert_eq!(status(ALL, "html"), Some(1005), "{log:?}");
}
