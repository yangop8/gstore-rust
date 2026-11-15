//! Recovering a served on-disk database whose store broke, without
//! restarting the server (review M5: gap-txn-lifecycle#4).
//!
//! Once a commit of an on-disk database fails, no commit point can succeed
//! any more (the store is broken: in Stream mode every statement is refused,
//! in Materialize mode nothing more is committed). `unload` used to save —
//! a commit point — first and keep the database loaded when that failed, and
//! `load` answered "already loaded", so only a restart of the whole server
//! (every database offline) recovered it. Now `/api unload` and REST
//! `/unload` retire a broken database *without* a commit point and name the
//! discarded changes in a `warning` (the `/api` answer stays HTTP 200 with
//! StatusCode 0); `load` of a loaded broken database reopens it in place;
//! a REST `/build` over it no longer fails on the save.
//!
//! The commit is made to fail by occupying the name its write-ahead log is
//! staged under (`kvstore.kv.wal.tmp`) with a directory: the log cannot be
//! written, exactly as when the disk is full before the log got its name, so
//! the failed update never reaches the file (a failure *after* the log was
//! written is replayed by the reopen instead; see
//! tests/storage_fault_injection.rs).

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;
use gstore::{Database, OpenOptions};

// ---- harness ----------------------------------------------------------------------

const BASE: usize = 40;

/// A temporary directory, removed when dropped (also when the test fails,
/// even before the server over it started).
struct TmpRoot(PathBuf);

impl Drop for TmpRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An in-process API server (root password `root`) over a temporary root
/// holding one on-disk database `dbx`; stops the server and removes the root
/// when dropped (also when the test fails).
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
    /// Declared last: removed after the server stopped (fields drop after
    /// `Drop::drop`, in declaration order).
    root: TmpRoot,
}

impl Srv {
    fn start(tag: &str, options: OpenOptions) -> Srv {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gstore_broken_store_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let root = TmpRoot(root);
        let nt = root.0.join("base.nt");
        let data: String = (0..BASE)
            .map(|i| format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"))
            .collect();
        fs::write(&nt, data).unwrap();
        Database::build_disk(root.0.join("dbx.db"), &[&nt]).unwrap();
        fs::remove_file(&nt).unwrap();
        let server = Arc::new(
            ApiServer::bind(&root.0, "root", "127.0.0.1:0")
                .unwrap()
                .with_open_options(options),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        Srv {
            server,
            addr,
            serving,
            root,
        }
    }

    fn db_dir(&self) -> PathBuf {
        self.root.0.join("dbx.db")
    }

    /// Make every commit of `dbx` fail before its log gets a name.
    fn block_commits(&self) {
        fs::create_dir(staged_log(&self.db_dir())).unwrap();
    }

    fn unblock_commits(&self) {
        fs::remove_dir(staged_log(&self.db_dir())).unwrap();
    }

    fn send(&self, method: &str, target: &str, auth: bool, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        if auth {
            // root:root
            raw.push_str("Authorization: Basic cm9vdDpyb290\r\n");
        } else {
            raw.push_str("Content-Type: application/json\r\n");
        }
        raw.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let status = resp
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, body.to_string())
    }

    /// `POST /api` as root with `pairs`: always HTTP 200 (upstream wire).
    fn api(&self, pairs: &[(&str, &str)]) -> Json {
        let mut body = Json::obj()
            .with("username", "root")
            .with("password", "root");
        for (k, v) in pairs {
            body.set(*k, *v);
        }
        let (status, text) = self.send("POST", "/api", false, &body.to_string());
        assert_eq!(status, 200, "{text}");
        json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
    }

    /// A REST call as root.
    fn rest(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        self.send(method, target, true, body)
    }

    /// The number of triples `dbx` answers with (`None` if the query fails).
    fn count(&self) -> Option<i64> {
        let j = self.api(&[
            ("operation", "query"),
            ("db_name", "dbx"),
            ("sparql", "SELECT * WHERE { ?s ?p ?o }"),
        ]);
        (code(&j) == 0).then(|| j.get("AnsNum").and_then(Json::as_i64).unwrap())
    }

    fn insert(&self, tag: &str) -> Json {
        let q = format!("INSERT DATA {{ <http://ex/{tag}> <http://ex/p> <http://ex/{tag}o> }}");
        self.api(&[("operation", "query"), ("db_name", "dbx"), ("sparql", &q)])
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
    }
}

fn staged_log(db_dir: &Path) -> PathBuf {
    db_dir.join("kvstore.kv.wal.tmp")
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap_or(-1)
}

fn warning(j: &Json) -> String {
    j.get("warning")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn modes() -> [(&'static str, OpenOptions); 2] {
    [
        ("stream", OpenOptions::stream()),
        ("materialize", OpenOptions::materialize()),
    ]
}

/// Load `dbx`, then break its store with a failing update.
fn broken(tag: &str, options: OpenOptions) -> Srv {
    let srv = Srv::start(tag, options);
    let j = srv.api(&[("operation", "load"), ("db_name", "dbx")]);
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(srv.count(), Some(BASE as i64));
    srv.block_commits();
    let j = srv.insert("lost");
    assert_ne!(code(&j), 0, "the update's commit must fail: {j}");
    srv.unblock_commits();
    srv
}

/// After the reopen: the committed data, without the failed update, and a
/// database that commits again.
fn assert_serves_again(srv: &Srv) {
    assert_eq!(
        srv.count(),
        Some(BASE as i64),
        "the failed update is discarded"
    );
    let j = srv.insert("after");
    assert_eq!(code(&j), 0, "it commits again: {j}");
    assert_eq!(srv.count(), Some(BASE as i64 + 1));
}

// ---- /api -------------------------------------------------------------------------

#[test]
fn api_unload_retires_a_broken_store_without_a_commit_point_and_load_reopens_it() {
    for (tag, options) in modes() {
        let srv = broken(&format!("api_unload_{tag}"), options);
        // Not even a commit point can succeed now.
        let j = srv.api(&[("operation", "checkpoint"), ("db_name", "dbx")]);
        assert_ne!(code(&j), 0, "({tag}) {j}");
        let j = srv.api(&[("operation", "unload"), ("db_name", "dbx")]);
        assert_eq!(code(&j), 0, "({tag}) unload must succeed: {j}");
        let w = warning(&j);
        assert!(
            w.contains("without a commit point") && w.contains("discarded"),
            "({tag}) the answer names the discarded changes: {j}"
        );
        assert!(w.contains("1 change(s)"), "({tag}) {w}");
        let j = srv.api(&[("operation", "show")]);
        assert!(!j.to_string().contains("\"loaded\""), "({tag}) {j}");
        let j = srv.api(&[("operation", "load"), ("db_name", "dbx")]);
        assert_eq!(code(&j), 0, "({tag}) {j}");
        assert_eq!(
            j.get("StatusMsg").and_then(Json::as_str),
            Some("Database loaded successfully."),
            "({tag}) {j}"
        );
        assert_serves_again(&srv);
    }
}

#[test]
fn api_load_reopens_a_loaded_broken_store_in_place() {
    for (tag, options) in modes() {
        let srv = broken(&format!("api_reload_{tag}"), options);
        let j = srv.api(&[("operation", "load"), ("db_name", "dbx")]);
        assert_eq!(code(&j), 0, "({tag}) {j}");
        assert_eq!(
            j.get("StatusMsg").and_then(Json::as_str),
            Some("Database loaded successfully."),
            "({tag}) reopened, not 'already load yet': {j}"
        );
        assert!(warning(&j).contains("reopened"), "({tag}) {j}");
        assert_serves_again(&srv);
        // A healthy loaded database is left alone (upstream's answer).
        let j = srv.api(&[("operation", "load"), ("db_name", "dbx")]);
        assert_eq!(
            j.get("StatusMsg").and_then(Json::as_str),
            Some("The database already load yet."),
            "({tag}) {j}"
        );
        assert!(j.get("warning").is_none(), "({tag}) {j}");
    }
}

// ---- REST -------------------------------------------------------------------------

#[test]
fn rest_unload_retires_a_broken_store_without_a_commit_point_and_load_reopens_it() {
    for (tag, options) in modes() {
        let srv = broken(&format!("rest_unload_{tag}"), options);
        let (s, body) = srv.rest("POST", "/unload?db_name=dbx", "");
        assert_eq!(s, 200, "({tag}) unload must succeed: {body}");
        assert!(
            body.contains("\"warning\"") && body.contains("discarded"),
            "({tag}) the answer names the discarded changes: {body}"
        );
        let (s, body) = srv.rest("POST", "/load?db_name=dbx", "");
        assert_eq!(s, 200, "({tag}) {body}");
        assert!(body.contains("loaded"), "({tag}) {body}");
        assert_serves_again(&srv);
    }
}

#[test]
fn rest_load_reopens_a_loaded_broken_store_in_place() {
    for (tag, options) in modes() {
        let srv = broken(&format!("rest_reload_{tag}"), options);
        let (s, body) = srv.rest("POST", "/load?db_name=dbx", "");
        assert_eq!(s, 200, "({tag}) {body}");
        assert!(
            body.contains("reopened") && body.contains("\"warning\""),
            "({tag}) {body}"
        );
        assert_serves_again(&srv);
        let (s, body) = srv.rest("POST", "/load?db_name=dbx", "");
        assert_eq!(s, 200, "({tag}) {body}");
        assert!(body.contains("already loaded"), "({tag}) {body}");
    }
}

#[test]
fn rest_build_over_a_loaded_broken_store_replaces_it() {
    for (tag, options) in modes() {
        let srv = broken(&format!("rest_build_{tag}"), options);
        let (s, body) = srv.rest(
            "POST",
            "/build?db_name=dbx&layout=disk",
            "<http://ex/n1> <http://ex/p> <http://ex/m1> .\n\
             <http://ex/n2> <http://ex/p> <http://ex/m2> .\n",
        );
        assert_eq!(s, 200, "({tag}) the build must not fail on a save: {body}");
        let (s, body) = srv.rest("POST", "/load?db_name=dbx", "");
        assert_eq!(s, 200, "({tag}) {body}");
        assert_eq!(srv.count(), Some(2), "({tag})");
    }
}
