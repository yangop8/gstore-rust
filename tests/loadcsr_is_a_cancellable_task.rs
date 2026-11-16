//! `/api` `loadcsr` (and `load` with `csr=1`) build the CSR as a registered
//! task with a cancel flag (round-2 review of conc#5): it is listed by
//! `/tasklist`, `/cancel` stops the build at its next check (nothing is
//! cached), and a shutdown past its grace period cancels it too — so the
//! shutdown's final pass, which waits for the database, never waits for a
//! whole build. Before the fix the build got no flag: neither `/cancel` nor
//! a shutdown reached it, and `serve_forever` returned only once it ended.
//!
//! The build is parked at its first cancellation check by the CSR build
//! hook (`gstore::analytics::csr::build_hook`) until its own flag is raised
//! (a hang guard lets it go otherwise). Timeouts are hang guards only.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gstore::analytics::csr::build_hook;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;

const HANG_GUARD: Duration = Duration::from_secs(60);
/// How long a parked build waits for its flag before it goes on by itself
/// (shorter than [`HANG_GUARD`], so a build that never sees a flag shows).
const PARK_GUARD: Duration = Duration::from_secs(20);
const ROOT_BASIC: &str = "Basic cm9vdDpyb290"; // root:root

/// The build hook is process-wide: the tests of this file run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_loadcsr_task_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Parks the first check of a CSR build until the build's own flag is
/// raised (or [`PARK_GUARD`] passes); records whether it saw its flag.
/// Removed when dropped.
struct Park {
    parked: mpsc::Receiver<()>,
    saw_cancel: Arc<AtomicBool>,
    had_flag: Arc<AtomicBool>,
}

impl Park {
    fn install() -> Park {
        let (tx, parked) = mpsc::channel();
        let tx = Mutex::new(tx);
        let first = AtomicBool::new(true);
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let had_flag = Arc::new(AtomicBool::new(false));
        let (saw, had) = (Arc::clone(&saw_cancel), Arc::clone(&had_flag));
        build_hook::set(Some(Arc::new(move |cancel: Option<&AtomicBool>| {
            if !first.swap(false, Ordering::SeqCst) {
                return;
            }
            had.store(cancel.is_some(), Ordering::SeqCst);
            let _ = tx.lock().unwrap().send(());
            let until = Instant::now() + PARK_GUARD;
            while !cancel.is_some_and(|f| f.load(Ordering::SeqCst)) && Instant::now() < until {
                // The server raises the flag from another thread.
                thread::park_timeout(Duration::from_millis(10));
            }
            saw.store(
                cancel.is_some_and(|f| f.load(Ordering::SeqCst)),
                Ordering::SeqCst,
            );
        })));
        Park {
            parked,
            saw_cancel,
            had_flag,
        }
    }

    fn wait_parked(&self) {
        self.parked
            .recv_timeout(HANG_GUARD)
            .expect("the build never reached a check");
    }
}

impl Drop for Park {
    fn drop(&mut self) {
        build_hook::set(None);
    }
}

fn request(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {ROOT_BASIC}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

fn api(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let mut items = vec![
        format!("{}: {}", json::quote("username"), json::quote("root")),
        format!("{}: {}", json::quote("password"), json::quote("root")),
    ];
    items.extend(
        pairs
            .iter()
            .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v))),
    );
    let (st, body) = request(addr, "POST", "/api", &format!("{{{}}}", items.join(", ")));
    assert_eq!(st, 200, "{body}");
    json::parse(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

/// A directed chain with literal labels (the CSR keeps the entity edges).
fn chain(len: usize) -> String {
    (0..len)
        .map(|i| {
            format!(
                "<http://ex/n{i}> <http://ex/p> <http://ex/n{}> .\n\
                 <http://ex/n{i}> <http://ex/label> \"node {i}\" .\n",
                i + 1
            )
        })
        .collect()
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
    _root: TmpDir,
}

impl Srv {
    fn start(tag: &str, db: &str) -> Srv {
        let root = TmpDir::new(tag);
        let server = ApiServer::bind(&root.0, "root", "127.0.0.1:0")
            .expect("bind")
            .with_shutdown_grace(Duration::from_millis(100));
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        let (st, body) = request(addr, "POST", &format!("/build?db_name={db}"), &chain(200));
        assert_eq!(st, 200, "build: {body}");
        Srv {
            server,
            addr,
            serving,
            _root: root,
        }
    }

    /// The id of the running `loadcsr` task of `db`.
    fn loadcsr_task(&self, db: &str) -> u64 {
        let (s, list) = request(self.addr, "GET", "/tasklist", "");
        assert_eq!(s, 200, "{list}");
        let entry = list
            .split("{\"id\":")
            .skip(1)
            .find(|e| e.contains(&format!("\"db\":\"{db}\"")) && e.contains("loadcsr"))
            .unwrap_or_else(|| panic!("no running loadcsr task of {db}: {list}"));
        entry
            .split([',', '}'])
            .next()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{list}"))
    }

    fn csr_built(&self, db: &str) -> bool {
        let j = api(self.addr, &[("operation", "show")]);
        let list = j.get("ResponseBody").and_then(Json::as_array).unwrap();
        let item = list
            .iter()
            .find(|d| d.get("database").and_then(Json::as_str) == Some(db))
            .unwrap_or_else(|| panic!("{db} not shown: {j}"));
        match item.get("csr") {
            Some(Json::Bool(b)) => *b,
            Some(v) => v.as_str() == Some("1") || v.as_str() == Some("true"),
            None => panic!("no csr field: {item}"),
        }
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            self.server.shutdown();
            let _ = serving.join();
        }
    }
}

#[test]
fn cancel_stops_a_loadcsr_build_and_caches_nothing() {
    let _serial = serial();
    let srv = Srv::start("cancel", "csrc");
    let park = Park::install();
    let addr = srv.addr;
    let load = thread::spawn(move || api(addr, &[("operation", "loadcsr"), ("db_name", "csrc")]));
    park.wait_parked();
    let id = srv.loadcsr_task("csrc");
    let (s, b) = request(srv.addr, "POST", &format!("/cancel?id={id}"), "");
    assert_eq!(s, 200, "cancel: {b}");
    let j = load.join().unwrap();
    assert!(
        park.saw_cancel.load(Ordering::SeqCst),
        "the build saw /cancel"
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert!(j.to_string().contains("cancel"), "{j}");
    drop(park);
    assert!(!srv.csr_built("csrc"), "a cancelled build is not cached");
    // Without a cancel it builds.
    let j = api(srv.addr, &[("operation", "loadcsr"), ("db_name", "csrc")]);
    assert_eq!(code(&j), 0, "{j}");
    assert!(srv.csr_built("csrc"));
}

#[test]
fn a_shutdown_stops_a_parked_loadcsr_build() {
    let _serial = serial();
    let mut srv = Srv::start("shutdown", "csrs");
    let park = Park::install();
    let addr = srv.addr;
    let load = thread::spawn(move || {
        let _ = std::panic::catch_unwind(|| {
            api(
                addr,
                &[("operation", "load"), ("db_name", "csrs"), ("csr", "1")],
            )
        });
    });
    park.wait_parked();
    assert!(
        park.had_flag.load(Ordering::SeqCst),
        "the build got the request's cancel flag"
    );
    let server = Arc::clone(&srv.server);
    let serving = srv.serving.take().unwrap();
    let (tx, stopped) = mpsc::channel();
    thread::spawn(move || {
        server.shutdown();
        let _ = tx.send(serving.join().is_ok());
    });
    assert!(
        stopped
            .recv_timeout(HANG_GUARD)
            .expect("serve_forever never returned"),
        "serve_forever panicked"
    );
    assert!(
        park.saw_cancel.load(Ordering::SeqCst),
        "the shutdown's cancel reached the parked build (it did not run on until its guard)"
    );
    let _ = load.join();
}
