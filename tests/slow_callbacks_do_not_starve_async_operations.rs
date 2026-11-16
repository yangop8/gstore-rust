//! A callback host that never answers holds a callback thread, not a task
//! runner (review gapi#12): with one runner, an asynchronous operation
//! submitted after one whose callback is stuck still runs and finishes.
//!
//! The callback endpoint is a stub that accepts the connection and answers
//! nothing until a latch is released; the callback timeout is longer than
//! the test's hang guard, so before the fix the second operation waits for
//! the stuck delivery and the guard fails the test.

use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gstore::gapi::tasks::{TaskConfig, TaskManager, TaskSpec, TaskState};
use gstore::json::Json;

const HANG_GUARD: Duration = Duration::from_secs(60);

#[test]
fn a_stuck_callback_does_not_hold_the_only_runner() {
    let manager = TaskManager::new(TaskConfig {
        workers: 1,
        callback_allow_local: true,
        callback_timeout: Duration::from_secs(600),
        ..TaskConfig::default()
    });
    // The stub: accepts, then holds the connection (no answer) until the
    // latch is released.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/cb", listener.local_addr().unwrap());
    let (release, latch) = mpsc::channel::<()>();
    let (accepted_tx, accepted) = mpsc::channel::<()>();
    let stub = thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let _ = accepted_tx.send(());
        let _ = latch.recv();
        drop(conn);
    });

    let first = manager
        .submit(
            TaskSpec {
                operation: "first".into(),
                callback: Some(url),
                ..TaskSpec::default()
            },
            Box::new(|_| Json::obj().with("StatusCode", 0)),
        )
        .unwrap();
    // The first operation finished and its callback is being delivered.
    accepted
        .recv_timeout(HANG_GUARD)
        .expect("the callback never connected");
    assert_eq!(
        manager.wait_finished(first, HANG_GUARD),
        Some(TaskState::Succeeded)
    );

    let second = manager
        .submit(
            TaskSpec {
                operation: "second".into(),
                ..TaskSpec::default()
            },
            Box::new(|_| Json::obj().with("StatusCode", 0)),
        )
        .unwrap();
    let state = manager.wait_finished(second, HANG_GUARD);
    // Release the stub before asserting, so a failure leaves nothing stuck.
    let _ = release.send(());
    stub.join().unwrap();
    assert_eq!(
        state,
        Some(TaskState::Succeeded),
        "the second operation waited for the first one's stuck callback"
    );
    assert!(manager.wait_async_idle(HANG_GUARD), "the delivery ended");
    let status = manager.get(first).and_then(|t| t.callback_status);
    assert!(status.is_some(), "the delivery outcome is recorded");
}
