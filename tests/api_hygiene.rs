//! Request-path hygiene of the database server (review round 4,
//! `r4-api-hygiene`):
//!
//! * Creating a served database's handle — loading, building, uploading
//!   into a new name, restoring, on the REST endpoints and through `/api` —
//!   locks its directory and measures its size (a walk of every file) with
//!   the server-wide state lock released, so `/status`, `/cancel` and every
//!   other request go on meanwhile (review http#13, conc#7).
//!
//! Every server here is in-process on `127.0.0.1:0` in a temp directory
//! removed when the test ends (also when it fails). Synchronisation is by
//! test hooks that park a step; a timeout only guards against a hang.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::http_api::{debug_park_dir_size, ApiServer};
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;

/// Bounds every wait (a hang guard, never what a test asserts).
const HANG_GUARD: Duration = Duration::from_secs(30);
const ROOT: (&str, &str) = ("root", "root");
const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/a> <http://ex/q> \"x\" .\n";

// ---- plumbing ---------------------------------------------------------------

/// A temp directory removed on drop (also while a failing test unwinds).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_api_hygiene_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
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

fn base64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Json {
        json::parse(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

/// One request on a fresh connection; the reply is awaited at most
/// `HANG_GUARD` (a read past it panics: the request hung).
fn send(addr: SocketAddr, method: &str, target: &str, ctype: Option<&str>, body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let auth = base64(&format!("{}:{}", ROOT.0, ROOT.1));
    let ctype = ctype
        .map(|c| format!("Content-Type: {c}\r\n"))
        .unwrap_or_default();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\n\
         {ctype}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)
        .unwrap_or_else(|e| panic!("{method} {target}: no answer within the hang guard: {e}"));
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    Reply {
        status,
        body: body.to_string(),
    }
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
    root: TmpDir,
}

impl Srv {
    fn start(tag: &str) -> Srv {
        let root = TmpDir::new(tag);
        let server = ApiServer::bind(&root.0, ROOT.1, "127.0.0.1:0")
            .expect("bind")
            .with_shutdown_grace(Duration::from_millis(100));
        let server = Arc::new(server);
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

    fn rest(&self, method: &str, target: &str, body: &str) -> Reply {
        send(self.addr, method, target, None, body)
    }

    /// A REST request that must succeed.
    fn ok(&self, method: &str, target: &str, body: &str) -> Reply {
        let r = self.rest(method, target, body);
        assert_eq!(r.status, 200, "{method} {target}: {}", r.body);
        r
    }

    fn api(&self, pairs: &[(&str, &str)]) -> Json {
        let mut obj = Json::obj()
            .with("username", ROOT.0)
            .with("password", ROOT.1);
        for (k, v) in pairs {
            obj.set(*k, *v);
        }
        let r = send(self.addr, "POST", "/api", None, &obj.to_string());
        assert_eq!(r.status, 200, "/api answers HTTP 200: {}", r.body);
        r.json()
    }

    /// An `/api` operation that must succeed.
    fn api_ok(&self, pairs: &[(&str, &str)]) -> Json {
        let j = self.api(pairs);
        assert_eq!(code(&j), 0, "{pairs:?}: {j}");
        j
    }

    fn db_dir(&self, db: &str) -> PathBuf {
        self.root.0.join(format!("{db}.db"))
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(h) = self.serving.take() {
            let _ = h.join();
        }
    }
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

// ---- handle creation off the state lock (http#13, conc#7) -------------------

/// Run `op` (which creates the handle of the database at `dir`) on its own
/// thread with the measurement of `dir` parked; once it is parked there,
/// `/status` and an `/api` `show` must be answered — they wait for the
/// state lock — and then `op` finishes when the walk is let go.
fn handle_creation_does_not_block_the_server<T: Send + 'static>(
    srv: &Srv,
    dir: &Path,
    what: &str,
    op: impl FnOnce(SocketAddr) -> T + Send + 'static,
) -> T {
    let park = debug_park_dir_size(dir);
    let addr = srv.addr;
    let worker = thread::spawn(move || op(addr));
    assert!(
        park.wait_parked(HANG_GUARD),
        "{what}: the directory of the new handle was never measured"
    );
    // The state lock is free while the walk is parked: before the fix
    // these waited for the walk (and ran into the hang guard).
    let status = srv.rest("GET", "/status", "");
    assert_eq!(status.status, 200, "{what}: /status: {}", status.body);
    let show = srv.api(&[("operation", "show")]);
    assert_eq!(code(&show), 0, "{what}: /api show: {show}");
    park.release();
    worker.join().expect("the request thread")
}

#[test]
fn rest_load_measures_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("restload");
    srv.ok("POST", "/build?db_name=big", DATA);
    srv.ok("POST", "/unload?db_name=big", "");
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/load", |a| {
        send(a, "POST", "/load?db_name=big", None, "")
    });
    assert_eq!(r.status, 200, "/load: {}", r.body);
    // The size measured off the lock is the one monitoring reports.
    let j = srv.api_ok(&[("operation", "monitor"), ("db_name", "big")]);
    assert!(j.get("diskUsed").is_some(), "{j}");
}

#[test]
fn api_load_measures_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("apiload");
    srv.ok("POST", "/build?db_name=big", DATA);
    srv.ok("POST", "/unload?db_name=big", "");
    let body = Json::obj()
        .with("username", ROOT.0)
        .with("password", ROOT.1)
        .with("operation", "load")
        .with("db_name", "big")
        .to_string();
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/api load", {
        move |a| send(a, "POST", "/api", None, &body)
    });
    assert_eq!(code(&r.json()), 0, "/api load: {}", r.body);
}

#[test]
fn rest_build_measures_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("restbuild");
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/build", |a| {
        send(a, "POST", "/build?db_name=big", None, DATA)
    });
    assert_eq!(r.status, 200, "/build: {}", r.body);
}

#[test]
fn rest_upload_into_an_unloaded_database_measures_it_with_the_state_lock_released() {
    let srv = Srv::start("upload");
    srv.ok("POST", "/build?db_name=big", DATA);
    srv.ok("POST", "/unload?db_name=big", "");
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/upload", |a| {
        send(
            a,
            "POST",
            "/upload?db_name=big&format=ntriples",
            None,
            "<http://ex/c> <http://ex/p> <http://ex/d> .\n",
        )
    });
    assert_eq!(r.status, 200, "/upload: {}", r.body);
}

#[test]
fn restores_measure_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("restore");
    srv.ok("POST", "/build?db_name=big", DATA);
    let j = srv.api_ok(&[("operation", "backup"), ("db_name", "big")]);
    let shown = j.get("backupfilepath").and_then(Json::as_str).unwrap();
    let backup = srv
        .root
        .0
        .join("backups")
        .join(Path::new(shown).file_name().unwrap());
    let target = format!(
        "/restore?db_name=big&path={}",
        enc(&backup.to_string_lossy())
    );
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/restore", {
        move |a| send(a, "POST", &target, None, "")
    });
    assert_eq!(r.status, 200, "/restore: {}", r.body);
    // `/api` `restore` over the loaded database serves the restored copy.
    let body = Json::obj()
        .with("username", ROOT.0)
        .with("password", ROOT.1)
        .with("operation", "restore")
        .with("db_name", "big")
        .with("backup_path", backup.to_string_lossy().as_ref())
        .to_string();
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/api restore", {
        move |a| send(a, "POST", "/api", None, &body)
    });
    assert_eq!(code(&r.json()), 0, "/api restore: {}", r.body);
}
