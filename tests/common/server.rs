//! In-process servers that stop when their guard is dropped (review
//! tests-a#8).
//!
//! [`Running::start`] serves a [`Serve`] value (an [`ApiServer`], a
//! single-database [`Server`]) on a thread of its own. Dropping the guard —
//! at the end of the test, or while a failing test unwinds — requests a
//! graceful shutdown and waits for the serving thread, at most
//! [`STOP_WAIT`]: a server whose shutdown hangs fails the test with a
//! message (or, when the test is already failing, prints one and leaves the
//! thread behind) instead of keeping the test binary alive. Without it a
//! finished test left its server's threads and socket alive until the
//! process exited, while its temporary root was being removed under them.

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gstore::http_api::ApiServer;
use gstore::server::{Server, ShutdownReport};

/// How long dropping a [`Running`] waits for the server to stop: longer than
/// the default shutdown grace (`DEFAULT_SHUTDOWN_GRACE`, 10 s) plus the
/// bounded joins after it.
pub const STOP_WAIT: Duration = Duration::from_secs(60);

/// A server [`Running`] can drive.
pub trait Serve: Send + Sync + 'static {
    /// Serve until stopped (blocks).
    fn serve(&self) -> ShutdownReport;
    /// Request a graceful stop (returns at once).
    fn stop(&self);
    /// The bound address.
    fn addr(&self) -> SocketAddr;
}

impl Serve for ApiServer {
    fn serve(&self) -> ShutdownReport {
        self.serve_forever()
    }
    fn stop(&self) {
        self.shutdown();
    }
    fn addr(&self) -> SocketAddr {
        self.local_addr().expect("local_addr")
    }
}

impl Serve for Server {
    fn serve(&self) -> ShutdownReport {
        self.serve_forever()
    }
    fn stop(&self) {
        self.shutdown();
    }
    fn addr(&self) -> SocketAddr {
        self.local_addr().expect("local_addr")
    }
}

/// A server serving on a background thread; stopped and joined when dropped
/// (see the module documentation). Dereferences to the server.
pub struct Running<S: Serve> {
    server: Arc<S>,
    addr: SocketAddr,
    done: Receiver<ShutdownReport>,
    thread: Option<JoinHandle<()>>,
    stop_wait: Duration,
}

impl<S: Serve> Running<S> {
    /// Serve `server` on a new thread.
    pub fn start(server: S) -> Running<S> {
        Running::start_arc(Arc::new(server))
    }

    /// Serve a shared `server` on a new thread (the test keeps other clones).
    pub fn start_arc(server: Arc<S>) -> Running<S> {
        let addr = server.addr();
        let (tx, done) = mpsc::channel();
        let s2 = Arc::clone(&server);
        let thread = thread::Builder::new()
            .name(format!("test-server-{addr}"))
            .spawn(move || {
                let report = s2.serve();
                let _ = tx.send(report);
            })
            .expect("spawn the server thread");
        Running {
            server,
            addr,
            done,
            thread: Some(thread),
            stop_wait: STOP_WAIT,
        }
    }

    /// Wait at most `wait` for the server to stop when dropped.
    pub fn with_stop_wait(mut self, wait: Duration) -> Running<S> {
        self.stop_wait = wait;
        self
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// A clone of the shared server (for threads of the test).
    pub fn arc(&self) -> Arc<S> {
        Arc::clone(&self.server)
    }

    /// Stop the server now, wait for it (as dropping does) and return what
    /// its shutdown did.
    pub fn stop(mut self) -> ShutdownReport {
        self.stop_and_join().unwrap_or_default()
    }

    /// Request the stop and wait for the serving thread (at most
    /// `stop_wait`). `None` when it was already joined.
    fn stop_and_join(&mut self) -> Option<ShutdownReport> {
        let thread = self.thread.take()?;
        self.server.stop();
        match self.done.recv_timeout(self.stop_wait) {
            Ok(report) => {
                let _ = thread.join();
                Some(report)
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The serving thread ended without a report: it panicked.
                let panicked = thread.join().is_err();
                let msg = format!("the server at {} panicked while serving", self.addr);
                if panicked && !thread::panicking() {
                    panic!("{msg}");
                }
                eprintln!("{msg}");
                None
            }
            Err(RecvTimeoutError::Timeout) => {
                // Leave the thread behind: the test fails, it does not hang.
                let msg = format!(
                    "the server at {} did not stop within {:?} of its shutdown request \
                     (a hang in the shutdown path?)",
                    self.addr, self.stop_wait
                );
                if !thread::panicking() {
                    panic!("{msg}");
                }
                eprintln!("{msg}");
                None
            }
        }
    }
}

impl<S: Serve> std::ops::Deref for Running<S> {
    type Target = S;
    fn deref(&self) -> &S {
        &self.server
    }
}

impl<S: Serve> Drop for Running<S> {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}
