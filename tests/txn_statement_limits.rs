//! A statement inside an HTTP transaction (REST `/tquery`, `/api` `tquery`)
//! is bounded like every other statement (review sec#10, robust#5; the
//! round-2 integration of r2-limits-auth's limits with r2-txn's
//! transaction path):
//!
//! * `query_timeout`: a runaway read inside a transaction is stopped once its
//!   time is up and answered as timed out (REST `503`, `/api` `1005`); the
//!   transaction stays open and commits afterwards.
//! * `max_result_bytes`: an answer whose serialization passes the budget is
//!   refused (REST `503`, `/api` `1005`), and a small one still fits.
//!
//! The timeout is tiny and the statement takes far longer (a three-way cross
//! product with a FILTER that never holds), so the outcome does not depend
//! on timing.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

/// Bounds a socket read, so a hung server fails the test instead of
/// blocking it forever (never the assertion).
const HANG_GUARD: Duration = Duration::from_secs(120);

/// `Authorization` for root (password `pw`).
const ROOT: &str = "Basic cm9vdDpwdw==";

/// A three-way cross product whose FILTER never holds: N³ rows to test.
const CROSS: &str = "SELECT ?a ?d ?g WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . \
                     FILTER(STR(?c) = CONCAT(STR(?f), STR(?i), \"x\")) }";

const ALL: &str = "SELECT * WHERE { ?s ?p ?o }";

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

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_txn_statement_limits_{tag}_{}_{n}",
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

/// An in-process multi-database server (root password `pw`) holding database
/// `db` with `n` triples; shut down and joined when dropped, before its
/// directory is removed.
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
    fn start(tag: &str, config: GStoreConfig, db: &str, n: usize) -> Api {
        let tmp = TmpDir::new(tag);
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
        let (st, body) = api.rest("POST", &format!("/build?db_name={db}"), &data(n));
        assert_eq!(st, 200, "build: {body}");
        api
    }

    fn rest(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        send(self.addr, method, target, Some(ROOT), body)
    }

    /// `POST /api` as root with string parameters: the envelope.
    fn api(&self, pairs: &[(&str, &str)]) -> Json {
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
            self.addr,
            "POST",
            "/api",
            None,
            &format!("{{{}}}", items.join(", ")),
        );
        assert_eq!(st, 200, "/api always answers HTTP 200: {body}");
        parse(&body)
    }

    /// REST `/begin` on `db`: the TID.
    fn begin(&self, db: &str) -> String {
        let (st, body) = self.rest("POST", &format!("/begin?db_name={db}"), "");
        assert_eq!(st, 200, "begin: {body}");
        text(&parse(&body), "TID")
    }

    fn tquery(&self, db: &str, tid: &str, sparql: &str) -> (u16, String) {
        self.rest("POST", &format!("/tquery?db_name={db}&tid={tid}"), sparql)
    }

    fn api_tquery(&self, db: &str, tid: &str, sparql: &str) -> Json {
        self.api(&[
            ("operation", "tquery"),
            ("db_name", db),
            ("tid", tid),
            ("sparql", sparql),
        ])
    }
}

fn timeout_config(timeout_ms: u64) -> GStoreConfig {
    let mut c = GStoreConfig::default();
    c.ip_deny_path = None;
    c.query_timeout = Duration::from_millis(timeout_ms);
    c
}

fn budget_config(bytes: u64) -> GStoreConfig {
    let mut c = GStoreConfig::default();
    c.ip_deny_path = None;
    c.max_result_bytes = bytes;
    c
}

#[test]
fn a_runaway_rest_tquery_times_out_and_the_transaction_stays_open() {
    let api = Api::start("rest_timeout", timeout_config(50), "cross", 100);
    let tid = api.begin("cross");
    let (st, body) = api.tquery("cross", &tid, CROSS);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains("timed out"), "{body}");
    // The transaction takes statements and commits afterwards.
    let (st, body) = api.tquery(
        "cross",
        &tid,
        "INSERT DATA { <http://ex/new> <http://ex/p> \"new\" }",
    );
    assert_eq!(st, 200, "{body}");
    let (st, body) = api.rest("POST", &format!("/commit?db_name=cross&tid={tid}"), "");
    assert_eq!(st, 200, "{body}");
    let (st, body) = api.rest(
        "POST",
        "/sparql?db_name=cross",
        "ASK { <http://ex/new> <http://ex/p> \"new\" }",
    );
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("true"), "{body}");
}

#[test]
fn a_runaway_api_tquery_times_out_with_1005() {
    let api = Api::start("api_timeout", timeout_config(50), "crossapi", 100);
    let tid = api.begin("crossapi");
    let j = api.api_tquery("crossapi", &tid, CROSS);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("timed out"), "{j}");
    let j = api.api_tquery("crossapi", &tid, "ASK { ?s ?p ?o }");
    assert_eq!(code(&j), 0, "the transaction stays open: {j}");
}

#[test]
fn a_rest_tquery_answer_over_the_byte_budget_is_refused() {
    let api = Api::start("rest_budget", budget_config(2000), "big", 100);
    let tid = api.begin("big");
    let (st, body) = api.tquery("big", &tid, ALL);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains("result too large"), "{body}");
    assert!(body.len() < 2000, "no part of the answer is sent");
    let (st, body) = api.tquery("big", &tid, "SELECT * WHERE { ?s ?p ?o } LIMIT 3");
    assert_eq!(st, 200, "a small answer fits: {body}");
}

#[test]
fn an_api_tquery_answer_over_the_byte_budget_is_refused() {
    let api = Api::start("api_budget", budget_config(2000), "bigapi", 100);
    let tid = api.begin("bigapi");
    let j = api.api_tquery("bigapi", &tid, ALL);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("result too large"), "{j}");
    let j = api.api_tquery("bigapi", &tid, "SELECT * WHERE { ?s ?p ?o } LIMIT 3");
    assert_eq!(code(&j), 0, "a small answer fits: {j}");
}
