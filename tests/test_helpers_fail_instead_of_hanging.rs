//! The shared test helpers (`tests/common`) fail a test instead of hanging
//! it, and leave nothing behind (review tests-a#8, tests-b#11, tests-a#16):
//!
//! * a request to a peer that never answers — silent, or trickling bytes
//!   forever — panics within its deadline, naming the request;
//! * a server guard whose server does not stop panics after its stop wait
//!   instead of blocking the test (a deliberately stuck server), and a real
//!   `ApiServer`'s guard stops and joins it when dropped (its listener is
//!   closed afterwards);
//! * temporary paths are unique per process and test, and removed by their
//!   guards — also when the test panics.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::http;
use common::server::{Running, Serve};
use common::tmp::{self, TempDir};
use gstore::http_api::ApiServer;
use gstore::server::ShutdownReport;

/// The text of a caught panic.
fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// A peer that accepts connections (the kernel completes the handshake from
/// the backlog) and never answers: the request fails within its deadline and
/// the panic names it.
#[test]
fn a_request_to_a_silent_server_fails_within_its_deadline() {
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = silent.local_addr().unwrap();
    let started = Instant::now();
    let err = catch_unwind(|| {
        http::request_with_timeout(
            &addr,
            "GET",
            "/never-answered",
            &[],
            b"",
            Duration::from_millis(300),
        )
    })
    .expect_err("a request nobody answers must fail");
    let text = panic_text(err);
    assert!(
        text.contains("GET /never-answered") && text.contains("no complete response"),
        "the failure names the request: {text}"
    );
    // A hang guard, not a measurement: generous for a loaded machine.
    assert!(started.elapsed() < Duration::from_secs(60));
    drop(silent);
}

/// A peer that sends a byte every few milliseconds and never finishes: each
/// read returns in time, so only an overall deadline (not a per-read
/// timeout) ends the request.
#[test]
fn a_request_to_a_trickling_server_fails_within_its_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let trickle = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\n");
            // Until the client gives up (the write fails) or the test ends.
            while !stop.load(Ordering::SeqCst) {
                if s.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let err = catch_unwind(|| {
        http::request_with_timeout(
            &addr,
            "POST",
            "/trickle",
            &[],
            b"body",
            Duration::from_millis(400),
        )
    })
    .expect_err("a response that never ends must fail");
    let text = panic_text(err);
    assert!(
        text.contains("POST /trickle") && text.contains("no complete response"),
        "{text}"
    );
    stop.store(true, Ordering::SeqCst);
    trickle.join().unwrap();
}

/// A complete answer is returned and parsed: status, header fields, body.
#[test]
fn a_complete_response_is_parsed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "the request ended early");
            req.extend_from_slice(&buf[..n]);
        }
        s.write_all(b"HTTP/1.1 201 Created\r\nX-Test: yes\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
        String::from_utf8(req).unwrap()
    });
    let r = http::request(&addr, "PUT", "/x", &[("X-Mine", "1")], b"");
    assert_eq!(r.status, 201);
    assert_eq!(r.header("x-test"), Some("yes"));
    assert_eq!(r.text(), "ok");
    let req = peer.join().unwrap();
    assert!(req.starts_with("PUT /x HTTP/1.1\r\n"), "{req}");
    assert!(req.contains("Host: 127.0.0.1\r\nX-Mine: 1\r\nContent-Length: 0\r\n"));
    assert!(req.contains("Connection: close\r\n\r\n"), "{req}");
}

/// A server whose shutdown never makes `serve` return.
struct Stuck {
    addr: SocketAddr,
    /// `serve` blocks until this is sent to (or dropped).
    release: Mutex<mpsc::Receiver<()>>,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

impl Serve for Stuck {
    fn serve(&self) -> ShutdownReport {
        let _ = self.release.lock().unwrap().recv();
        ShutdownReport::default()
    }
    fn stop(&self) {
        // Deliberately ignored: the server is stuck.
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Dropping the guard of a server that does not stop panics after the stop
/// wait (the test fails) instead of blocking forever.
#[test]
fn a_stuck_server_fails_the_test_instead_of_hanging_it() {
    let (release, rx) = mpsc::channel();
    let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let guard = Running::start(Stuck {
        addr: "127.0.0.1:9".parse().unwrap(),
        release: Mutex::new(rx),
        stops: Arc::clone(&stops),
    })
    .with_stop_wait(Duration::from_millis(200));
    let err = catch_unwind(AssertUnwindSafe(move || drop(guard)))
        .expect_err("a server that does not stop must fail the test");
    let text = panic_text(err);
    assert!(text.contains("did not stop within"), "{text}");
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "the guard asked it to stop"
    );
    // Let the abandoned thread end.
    release.send(()).unwrap();
}

/// A real server's guard stops and joins it when dropped: afterwards nothing
/// listens on its address, and its root can be removed.
#[test]
fn dropping_an_api_server_guard_stops_it() {
    let root = TempDir::new("helpers_api");
    let srv = Running::start(ApiServer::bind(root.path(), "root", "127.0.0.1:0").unwrap());
    let addr = srv.addr();
    let r = http::request(
        &addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        br#"{"operation": "check"}"#,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    // `stop` is what dropping does, with the report returned.
    let report = srv.stop();
    assert!(report.drained, "{report:?}");
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(5)).is_err(),
        "the listener is closed once the guard returned"
    );
    let srv = Running::start(ApiServer::bind(root.path(), "root", "127.0.0.1:0").unwrap());
    let addr = srv.addr();
    drop(srv);
    assert!(TcpStream::connect_timeout(&addr, Duration::from_secs(5)).is_err());
}

/// Unique names: distinct within the process, and carrying the pid so two
/// processes sharing the temp directory never meet.
#[test]
fn temporary_paths_are_unique_and_removed() {
    let pid = std::process::id().to_string();
    let a = TempDir::new("helpers_tmp");
    let b = TempDir::new("helpers_tmp");
    assert_ne!(a.path(), b.path());
    for d in [&a, &b] {
        let name = d.path().file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("gstore_helpers_tmp_"), "{name}");
        assert!(name.contains(&format!("_{pid}_")), "{name}");
        assert!(d.path().is_dir());
    }
    std::fs::write(a.join("f"), b"x").unwrap();
    let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
    drop(a);
    drop(b);
    assert!(!pa.exists() && !pb.exists(), "removed when dropped");
    let reserved = TempDir::reserve("helpers_reserved");
    assert!(!reserved.path().exists(), "reserved, not created");
    std::fs::create_dir_all(reserved.join("db")).unwrap();
    let pr = reserved.path().to_path_buf();
    drop(reserved);
    assert!(!pr.exists());
}

/// A guard removes its directory while a failing test unwinds, and the
/// thread registry removes its paths when the test's thread ends — also
/// after a panic.
#[test]
fn temporary_paths_are_removed_when_the_test_fails() {
    let (tx, rx) = mpsc::channel();
    let failing = thread::spawn(move || {
        let dir = TempDir::new("helpers_panic");
        let reg = tmp::thread_dir("helpers_panic_reg");
        std::fs::write(reg.join("f"), b"x").unwrap();
        tx.send((dir.path().to_path_buf(), reg)).unwrap();
        panic!("a failing test");
    });
    assert!(failing.join().is_err());
    let (dir, reg) = rx.recv().unwrap();
    assert!(!dir.exists(), "the guard removed {}", dir.display());
    assert!(!reg.exists(), "the registry removed {}", reg.display());
}
