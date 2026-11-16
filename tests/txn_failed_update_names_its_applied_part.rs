//! A multi-operation UPDATE that fails part-way inside an HTTP transaction
//! (REST `/tquery`, `/api` `tquery`) keeps the operations before the
//! failing one applied in the transaction — as the same UPDATE does
//! outside one — and the answer says so ("operations before the failing
//! one were applied"), also when `query_timeout` stopped it. Before the
//! round-2 review fix the answer named only the failure (a timeout, or the
//! refused `LOAD`), so a client could commit a half-applied statement
//! without knowing.
//!
//! The timeout is tiny and the failing operation takes far longer (a
//! three-way cross product whose FILTER never holds), so the outcome does
//! not depend on timing; the `LOAD` case fails at once (local files are
//! refused by the default load policy).

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

/// Bounds a socket read (a hang guard, never the assertion).
const HANG_GUARD: Duration = Duration::from_secs(120);

/// `Authorization` for root (password `pw`).
const ROOT: &str = "Basic cm9vdDpwdw==";

/// An UPDATE whose first operation applies at once and whose second runs
/// a three-way cross product with a FILTER that never holds.
const PARTIAL_TIMEOUT: &str = "INSERT DATA { <http://ex/partial> <http://ex/p> \"partial\" } ; \
     INSERT { ?a <http://ex/q> ?d } WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . \
     FILTER(STR(?c) = CONCAT(STR(?f), STR(?i), \"x\")) }";

const ASK_PARTIAL: &str = "ASK { <http://ex/partial> <http://ex/p> \"partial\" }";

/// An UPDATE whose second operation is refused at once (a local `LOAD`).
fn partial_load(tag: &str) -> String {
    format!(
        "INSERT DATA {{ <http://ex/{tag}> <http://ex/p> \"{tag}\" }} ; \
         LOAD <file:///nonexistent/gstore-review-{tag}.nt>"
    )
}

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
            "gstore_txn_failed_update_{tag}_{}_{n}",
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

/// An in-process multi-database server (root password `pw`) holding
/// database `db` with `n` triples; shut down and joined when dropped.
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
    fn start(tag: &str, timeout_ms: u64, db: &str) -> Api {
        let tmp = TmpDir::new(tag);
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        config.query_timeout = Duration::from_millis(timeout_ms);
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
        let (st, body) = api.rest("POST", &format!("/build?db_name={db}"), &data(100));
        assert_eq!(st, 200, "build: {body}");
        api
    }

    fn rest(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        send(self.addr, method, target, Some(ROOT), body)
    }

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

    fn begin(&self, db: &str) -> String {
        let (st, body) = self.rest("POST", &format!("/begin?db_name={db}"), "");
        assert_eq!(st, 200, "begin: {body}");
        parse(&body)
            .get("TID")
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("no TID: {body}"))
            .to_string()
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

const APPLIED: &str = "operations before the failing one were applied";

#[test]
fn a_timed_out_rest_tquery_update_names_the_operations_it_applied() {
    let api = Api::start("rest_timeout", 50, "partial");
    let tid = api.begin("partial");
    let (st, body) = api.tquery("partial", &tid, PARTIAL_TIMEOUT);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains("timed out"), "{body}");
    assert!(
        body.contains(APPLIED),
        "the answer must say that the first operation stays applied: {body}"
    );
    // It does stay applied in the transaction: rolling back discards it.
    let (st, body) = api.tquery("partial", &tid, ASK_PARTIAL);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("true"), "{body}");
    let (st, body) = api.rest("POST", &format!("/rollback?db_name=partial&tid={tid}"), "");
    assert_eq!(st, 200, "{body}");
    let (st, body) = api.rest("POST", "/sparql?db_name=partial", ASK_PARTIAL);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("false"), "rolled back: {body}");
}

#[test]
fn a_timed_out_api_tquery_update_names_the_operations_it_applied() {
    let api = Api::start("api_timeout", 50, "partialapi");
    let tid = api.begin("partialapi");
    let j = api.api_tquery("partialapi", &tid, PARTIAL_TIMEOUT);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("timed out"), "{j}");
    assert!(msg(&j).contains(APPLIED), "{j}");
}

#[test]
fn a_failed_tquery_update_names_the_operations_it_applied_as_outside_a_transaction() {
    let api = Api::start("load", 0, "partialload");
    // Outside a transaction (the reference behaviour).
    let (st, body) = api.rest(
        "POST",
        "/update?db_name=partialload",
        &partial_load("outside"),
    );
    assert_eq!(st, 400, "{body}");
    assert!(body.contains(APPLIED), "{body}");
    // Inside one: the same wording, REST and /api.
    let tid = api.begin("partialload");
    let (st, body) = api.tquery("partialload", &tid, &partial_load("rest"));
    assert_eq!(st, 400, "{body}");
    assert!(body.contains("LOAD"), "{body}");
    assert!(body.contains(APPLIED), "{body}");
    let j = api.api_tquery("partialload", &tid, &partial_load("api"));
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains(APPLIED), "{j}");
    // A failing single-operation UPDATE applied nothing and does not claim to.
    let (st, body) = api.tquery(
        "partialload",
        &tid,
        "LOAD <file:///nonexistent/gstore-review-single.nt>",
    );
    assert_eq!(st, 400, "{body}");
    assert!(!body.contains(APPLIED), "{body}");
    let (st, body) = api.rest(
        "POST",
        &format!("/rollback?db_name=partialload&tid={tid}"),
        "",
    );
    assert_eq!(st, 200, "{body}");
}
