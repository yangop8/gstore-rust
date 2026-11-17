//! `serve_forever` joins the server's background threads before it returns
//! (review conc#14, round-2 review): the idle-transaction reaper (whose join
//! the round-2 integration added without a test that fails without it), and
//! the asynchronous operations' runner and callback threads (never joined
//! before the round-2 review fix). A program that stops a server and starts
//! another in the same process keeps none of the old one's threads.
//!
//! Deterministic: each thread is parked right before it exits by a test hook
//! (`gstore::exit_park`), and the shutdown must announce that it joins it
//! (`wait_joining`) while it is parked — a thread left detached is never
//! joined, and still parked when `serve_forever` returns. Timeouts are hang
//! guards only. A thread counts as left over while it has not begun to exit
//! (see [`threads_named`]: a joined thread's kernel task can still be
//! listed for a moment after the join returns).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use gstore::exit_park;
use gstore::gapi::tasks::TaskConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;

const HANG_GUARD: Duration = Duration::from_secs(60);
const ROOT_BASIC: &str = "Basic cm9vdDpyb290"; // root:root

/// The exit parks are process-wide: the tests of this file run one at a
/// time.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_thread_joins_{tag}_{}_{}",
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

/// The kernel's `PF_EXITING` task flag (`include/linux/sched.h`): the
/// thread has begun to exit and never runs user code again.
const PF_EXITING: u64 = 0x4;

/// The live threads of this process whose name (`comm`, 15 bytes) starts
/// with `prefix`.
///
/// A thread that has begun to exit is not counted. `JoinHandle::join`
/// (`pthread_join`) returns once the exiting thread's kernel task has
/// cleared its thread id — in the middle of its exit, after `PF_EXITING`
/// is set and before the task is removed from `/proc/self/task` — so a
/// joined thread can still be listed there for a moment (a loaded machine
/// may preempt it in between). Such a thread has finished; a thread still
/// running, or parked by `exit_park`, has no `PF_EXITING` and counts.
fn threads_named(prefix: &str) -> usize {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    tasks
        .flatten()
        .filter(|t| {
            std::fs::read_to_string(t.path().join("comm"))
                .is_ok_and(|name| name.trim_end().starts_with(prefix))
        })
        .filter(|t| {
            // Gone meanwhile (unreadable): exited.
            std::fs::read_to_string(t.path().join("stat")).is_ok_and(|stat| !exiting(&stat))
        })
        .count()
}

/// Whether `/proc/<pid>/task/<tid>/stat` text shows `PF_EXITING`: the
/// `flags` field, the 9th — the 7th after the parenthesised `comm`, which
/// may itself hold spaces and parentheses.
fn exiting(stat: &str) -> bool {
    let flags = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().nth(6))
        .and_then(|f| f.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("no flags field in {stat:?}"));
    flags & PF_EXITING != 0
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

/// A serving `ApiServer` (root password `root`) with database `db`.
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
            .with_shutdown_grace(Duration::from_millis(100))
            .with_task_config(TaskConfig {
                callback_allow_local: true,
                ..TaskConfig::default()
            });
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        let (st, body) = request(
            addr,
            "POST",
            &format!("/build?db_name={db}"),
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
        );
        assert_eq!(st, 200, "build: {body}");
        Srv {
            server,
            addr,
            serving,
            _root: root,
        }
    }

    /// Stop the server while `park` holds a thread of it at its exit: the
    /// shutdown must join that thread (announce it while the thread is
    /// parked), and `serve_forever` returns once it is let go.
    fn stop_joining(&mut self, park: &exit_park::ExitPark, what: &str) {
        let server = Arc::clone(&self.server);
        let serving = self.serving.take().unwrap();
        let (tx, stopped) = mpsc::channel();
        thread::spawn(move || {
            server.shutdown();
            let _ = tx.send(serving.join().is_ok());
        });
        let joined = park.wait_joining(HANG_GUARD);
        park.release();
        assert!(
            stopped
                .recv_timeout(HANG_GUARD)
                .expect("serve_forever never returned"),
            "serve_forever panicked"
        );
        assert!(joined, "the shutdown never joined the {what} thread");
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

/// A callback endpoint that answers every delivery `200` at once.
fn callback_endpoint() -> (String, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/cb", listener.local_addr().unwrap());
    let (tx, delivered) = mpsc::channel();
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
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let _ = conn
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send(());
        }
    });
    (url, delivered)
}

/// Review conc#14 (round-2 integration fix 51245bb): the idle-transaction
/// reaper is joined.
#[test]
fn the_idle_transaction_reaper_is_joined() {
    let _serial = serial();
    let mut srv = Srv::start("reaper", "reaped");
    assert!(threads_named("gstore-txn-reap") > 0, "the reaper runs");
    let park = exit_park::arm("gstore-txn-reaper");
    srv.stop_joining(&park, "reaper");
    assert_eq!(threads_named("gstore-txn-reap"), 0);
}

/// An asynchronous operation's runner thread is joined.
#[test]
fn the_asynchronous_operation_runner_is_joined() {
    let _serial = serial();
    let mut srv = Srv::start("runner", "runs");
    let park = exit_park::arm("gstore-task");
    let j = api(
        srv.addr,
        &[
            ("operation", "query"),
            ("db_name", "runs"),
            ("sparql", "ASK { ?s ?p ?o }"),
            ("async", "true"),
        ],
    );
    assert_eq!(j.get("StatusCode").and_then(Json::as_i64), Some(0), "{j}");
    assert!(
        park.wait_parked(HANG_GUARD),
        "the runner ran the operation and left its pool"
    );
    srv.stop_joining(&park, "runner");
    assert_eq!(threads_named("gstore-task"), 0);
}

/// An asynchronous operation's callback thread is joined.
#[test]
fn the_callback_thread_is_joined() {
    let _serial = serial();
    let mut srv = Srv::start("callback", "calls");
    let (url, delivered) = callback_endpoint();
    let park = exit_park::arm("gstore-callback");
    let j = api(
        srv.addr,
        &[
            ("operation", "query"),
            ("db_name", "calls"),
            ("sparql", "ASK { ?s ?p ?o }"),
            ("async", "true"),
            ("callback", &url),
        ],
    );
    assert_eq!(j.get("StatusCode").and_then(Json::as_i64), Some(0), "{j}");
    delivered
        .recv_timeout(HANG_GUARD)
        .expect("the callback was delivered");
    assert!(
        park.wait_parked(HANG_GUARD),
        "the callback thread delivered and left its pool"
    );
    srv.stop_joining(&park, "callback");
    assert_eq!(threads_named("gstore-callback"), 0);
}
