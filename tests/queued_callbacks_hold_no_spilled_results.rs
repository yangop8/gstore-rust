//! Callbacks waiting for delivery are bounded (round-2 review of gapi#12):
//!
//! * a result spilled to the task directory waits for its callback as its
//!   file's name and is read when it is delivered — before the fix every
//!   queued callback kept a copy of its whole result in memory, so one user's
//!   `query` results to a callback host that never answers grew the
//!   server's memory by the size of all of them;
//! * one user's callbacks may not fill the queue: beyond
//!   `callback_pending_per_user` that user's next callbacks are recorded as
//!   failed, and other users' are still queued.
//!
//! The callback host is a stub that accepts every connection and answers
//! nothing until the test lets it go; the callback timeout is far longer
//! than the test. One runner runs the operations in turn, so once a last
//! operation without a callback has finished, every earlier operation's
//! callback has been queued (or refused). Timeouts are hang guards only.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::gapi::tasks::{TaskConfig, TaskManager, TaskSpec, TaskState};
use gstore::json::Json;

const HANG_GUARD: Duration = Duration::from_secs(60);
/// The size of each result.
const RESULT_BYTES: usize = 1 << 20;

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_queued_callbacks_{tag}_{}_{n}",
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

/// A callback host that accepts every connection and holds it, answering
/// nothing, until it is dropped.
struct Stub {
    url: String,
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start() -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/cb");
        let stop = Arc::new(AtomicBool::new(false));
        let halt = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(conn) = conn {
                    held.push(conn);
                }
            }
            drop(held);
        });
        Stub {
            url,
            addr,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn manager(per_user: usize) -> TaskManager {
    TaskManager::new(TaskConfig {
        workers: 1,
        callback_workers: 1,
        callback_allow_local: true,
        callback_timeout: Duration::from_secs(600),
        callback_pending_per_user: per_user,
        ..TaskConfig::default()
    })
}

/// Submit a `query` operation of `user` whose result is about
/// [`RESULT_BYTES`], with a callback to `url`, its result spilled to `dir`.
fn submit(m: &TaskManager, user: &str, url: &str, dir: &TmpDir) -> u64 {
    m.submit(
        TaskSpec {
            operation: "query".into(),
            user: user.into(),
            callback: Some(url.to_string()),
            result_dir: Some(dir.0.clone()),
            ..TaskSpec::default()
        },
        Box::new(|_| {
            Json::obj()
                .with("StatusCode", 0)
                .with("payload", "x".repeat(RESULT_BYTES))
        }),
    )
    .unwrap()
}

/// Wait until every operation submitted so far has run and queued (or
/// refused) its callback: the one runner runs this one after them.
fn settle(m: &TaskManager) {
    let id = m
        .submit(
            TaskSpec {
                operation: "settle".into(),
                ..TaskSpec::default()
            },
            Box::new(|_| Json::obj().with("StatusCode", 0)),
        )
        .unwrap();
    assert_eq!(m.wait_finished(id, HANG_GUARD), Some(TaskState::Succeeded));
}

#[test]
fn queued_callbacks_of_spilled_results_hold_no_result_in_memory() {
    let dir = TmpDir::new("spilled");
    let stub = Stub::start();
    let m = manager(64);
    let ids: Vec<u64> = (0..12)
        .map(|_| submit(&m, "alice", &stub.url, &dir))
        .collect();
    settle(&m);
    for &id in &ids {
        assert_eq!(m.wait_finished(id, HANG_GUARD), Some(TaskState::Succeeded));
    }
    let (queued, bytes) = m.queued_callbacks();
    assert!(
        queued >= 11,
        "one delivery in flight, the rest queued: {queued}"
    );
    assert!(
        bytes < RESULT_BYTES,
        "{queued} queued callbacks hold {bytes} bytes of results in memory"
    );
    drop(stub);
    assert!(m.wait_async_idle(HANG_GUARD), "the deliveries ended");
}

#[test]
fn one_users_callbacks_do_not_fill_the_queue_for_everyone() {
    let dir = TmpDir::new("per_user");
    let stub = Stub::start();
    let m = manager(4);
    // At most one in flight and four queued: the rest are refused.
    let alice: Vec<u64> = (0..8)
        .map(|_| submit(&m, "alice", &stub.url, &dir))
        .collect();
    settle(&m);
    for &id in &alice {
        assert_eq!(m.wait_finished(id, HANG_GUARD), Some(TaskState::Succeeded));
    }
    let refused = alice
        .iter()
        .filter(|id| {
            m.get(**id)
                .and_then(|t| t.callback_status)
                .is_some_and(|s| s.contains("this user's callbacks"))
        })
        .count();
    let statuses: Vec<_> = alice
        .iter()
        .map(|id| m.get(*id).and_then(|t| t.callback_status))
        .collect();
    assert!(
        (3..=4).contains(&refused),
        "alice's surplus callbacks are refused ({refused}): {statuses:?}"
    );
    // Bob's callback still waits for delivery.
    let bob = submit(&m, "bob", &stub.url, &dir);
    settle(&m);
    assert_eq!(m.wait_finished(bob, HANG_GUARD), Some(TaskState::Succeeded));
    let status = m.get(bob).and_then(|t| t.callback_status);
    assert!(
        !status.as_deref().is_some_and(|s| s.starts_with("failed")),
        "bob's callback is queued, not refused: {status:?}"
    );
    drop(stub);
    assert!(m.wait_async_idle(HANG_GUARD), "the deliveries ended");
}
