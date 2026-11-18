//! Loading a database that is already loaded and healthy is a no-op that
//! answers at once — also while a statement runs on it — and leaves the
//! database free for a concurrent `begin` (commit 2007ec7; review of round 1:
//! the reopen-in-place path of r1-storage-failstate reserved the database
//! and waited for its guard first, so `/api load` and REST `/load` waited
//! behind any running statement while `begin` / `unload` / `drop` / `build`
//! were refused as busy).
//!
//! A statement is parked deterministically: an UPDATE whose WHERE clause
//! calls a SPARQL `SERVICE` endpoint run by the test, which holds its answer
//! until the test releases it. The UPDATE holds the database's exclusive
//! guard all that time. The loads must answer while it is parked; socket
//! timeouts are hang guards only (a load that waits for the statement would
//! wait for a release that comes only after it answered).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::db::LoadPolicy;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

/// A hang guard for every blocking step (never an assertion).
const HANG_GUARD: Duration = Duration::from_secs(60);
/// Basic `root:pw`.
const ROOT: (&str, &str) = ("Authorization", "Basic cm9vdDpwdw==");

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("gstore_load_busy_{tag}_{}_{n}", std::process::id()));
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

/// A multi-database server whose `SERVICE` may reach this host; shut down
/// when dropped.
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<()>>,
    _root: TmpDir,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        // `shutdown` only asks: the serving thread then rolls back open
        // transactions and saves the databases, writing below the root.
        // Wait for it, so the temp directory is removed after its last
        // write (a server still shutting down could recreate it).
        if let Some(h) = self.serving.take() {
            let _ = h.join();
        }
    }
}

fn start_api() -> Api {
    let root = TmpDir::new("api");
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let server = ApiServer::bind(root.0.join("root"), "pw", "127.0.0.1:0")
        .unwrap()
        .with_config(config)
        .with_load_policy(LoadPolicy::permissive());
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = Some(thread::spawn(move || {
        s2.serve_forever();
    }));
    Api {
        server,
        addr,
        serving,
        _root: root,
    }
}

/// One request (`Connection: close`): `(status, body)`.
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

/// `POST /api` as root with the string parameters `pairs`: the envelope.
fn api_call(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
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

fn status_code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

fn status_msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A SPARQL endpoint that answers each request (an empty result) only once
/// the test releases it: `arrived` gets a message when a request is in,
/// `release` (dropped or sent to) lets it answer.
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
                // Read the request head (and its body, if announced).
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
                // Held until released (or the test is gone).
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

#[test]
fn loading_a_healthy_database_answers_while_a_statement_holds_it() {
    let api = start_api();
    let (st, body) = send(
        api.addr,
        "POST",
        "/build?db_name=dba",
        &[ROOT],
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    assert_eq!(st, 200, "build: {body}");

    // Park an UPDATE on `dba`: it holds the database's exclusive guard until
    // the endpoint answers.
    let mut endpoint = HeldEndpoint::start();
    let update = format!(
        "INSERT {{ <http://ex/held> <http://ex/p> ?s }} WHERE {{ SERVICE <http://{}/sparql> {{ ?s ?p ?o }} }}",
        endpoint.addr
    );
    let addr = api.addr;
    let parked = thread::spawn(move || {
        send(
            addr,
            "POST",
            "/update?db_name=dba",
            &[ROOT, ("Content-Type", "application/sparql-update")],
            &update,
        )
    });
    endpoint
        .arrived
        .recv_timeout(HANG_GUARD)
        .expect("the UPDATE reached its SERVICE endpoint");

    // Both loads answer while the statement is parked: already loaded.
    let j = api_call(addr, &[("operation", "load"), ("db_name", "dba")]);
    assert_eq!(status_code(&j), 0, "{j}");
    assert!(status_msg(&j).contains("already load"), "{j}");
    let (st, body) = send(addr, "POST", "/load?db_name=dba", &[ROOT], "");
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("already loaded"), "{body}");
    assert!(
        !parked.is_finished(),
        "the statement is still parked (the loads did not wait for it)"
    );

    // The loads left the database free: a `begin` is not refused as busy
    // (it takes the database once the statement is done).
    let begin =
        thread::spawn(move || api_call(addr, &[("operation", "begin"), ("db_name", "dba")]));
    endpoint.release();
    let (st, body) = parked.join().unwrap();
    assert_eq!(st, 200, "the parked UPDATE: {body}");
    let j = begin.join().unwrap();
    assert_eq!(status_code(&j), 0, "begin after the loads: {j}");
    let tid = j
        .get("TID")
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no TID in {j}"))
        .to_string();
    let j = api_call(
        addr,
        &[("operation", "rollback"), ("db_name", "dba"), ("tid", &tid)],
    );
    assert_eq!(status_code(&j), 0, "{j}");
}
