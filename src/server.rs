//! A minimal HTTP SPARQL endpoint over a [`Database`] — the analogue of gStore's
//! `src/Server` / `ghttp`.
//!
//! This is a dependency-free, blocking HTTP/1.1 server (one request per
//! connection, each connection handled on its own worker thread) implementing a
//! useful subset of the SPARQL 1.1 Protocol:
//!
//! | route | does |
//! |-------|------|
//! | `GET /sparql?query=…` / `POST /sparql` | run a query (SELECT/ASK/CONSTRUCT/DESCRIBE) |
//! | `POST /update` | run a SPARQL UPDATE |
//! | `GET /status` | report [`DbStats`](crate::db::DbStats) |
//!
//! SELECT/ASK results are content-negotiated (see [`crate::sparql_results`]):
//! JSON (default), XML, CSV, or TSV via the `Accept` header or a `format=`
//! query parameter; CONSTRUCT/DESCRIBE return N-Triples. Connections run
//! concurrently (a worker thread each), but the [`Database`] is shared behind a
//! [`Mutex`] so the actual query/update execution is serialized (correctness
//! over throughput); everything up to and after that — parsing, auth,
//! serialization, response compression — runs off the lock.
//!
//! Beyond the base protocol this endpoint adds:
//!
//! * **Durability** — a server that knows its database directory
//!   ([`Server::with_db_dir`]) saves after every successful update (gStore's
//!   `check_point` policy, on by default; [`Server::with_check_point`] turns it
//!   off) and saves any remaining change on graceful shutdown.
//! * **Graceful shutdown** — [`Server::shutdown`] / a [`ShutdownHandle`] (and,
//!   on unix, `SIGINT`/`SIGTERM` via [`shutdown_on_signals`]) stops accepting,
//!   lets in-flight requests finish (bounded by a grace period), saves, and makes
//!   `serve_forever` return a [`ShutdownReport`]. The same machinery drives
//!   [`ApiServer`](crate::http_api::ApiServer).
//! * **Panic isolation** — each request is routed under `catch_unwind`: a
//!   panicking handler answers `500` with a JSON error, and the poisoned
//!   database lock is recovered so later requests keep working.
//! * **HTTP Basic auth** — optional, via [`Server::with_basic_auth`]. When
//!   configured, every request must carry a valid `Authorization: Basic` header
//!   or it is answered `401` with `WWW-Authenticate: Basic`. With no credentials
//!   configured the server stays open (unchanged behavior).
//! * **Content negotiation** — four SPARQL Results serializations (above).
//! * **Streaming** — a SELECT with `?stream=true` is sent with
//!   `Transfer-Encoding: chunked`, emitting rows without buffering the whole
//!   body. All other responses keep `Content-Length`, so the chunked-unaware
//!   [`crate::http_client`] (and the `SERVICE` federation path) are unaffected.
//! * **Compression** (feature `gzip`) — buffered responses honour a client
//!   `Accept-Encoding: gzip`/`deflate`, setting `Content-Encoding` accordingly.
//!   Without the feature the default build stays dependency-free and sends
//!   bodies verbatim. Streamed (`chunked`) responses are never compressed.
//!
//! **HTTPS:** with the optional `tls` feature, [`Server::serve_forever_tls`]
//! (and its [`ApiServer`](crate::http_api::ApiServer) counterpart) terminate
//! TLS in-process via `rustls` — build a [`rustls::ServerConfig`] from a
//! cert+key PEM with [`load_tls_config`]. The default (zero-dependency) build
//! omits TLS: terminate HTTPS in front of the endpoint with a reverse proxy
//! (nginx, Caddy, stunnel, …) instead, and do not send credentials over plain
//! `http://` in production without such termination.
//!
//! Still deferred (see `docs/REFACTOR_BACKLOG.md` F): gRPC and
//! clustering/sharding.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::db::Database;
use crate::query::{QueryResult, ResultSet};
use crate::sparql_results::{self, json_str, ResultFormat};

const SPARQL_JSON: &str = "application/sparql-results+json";

/// Maximum accepted request-body size (`Content-Length`). Bounds the pre-auth
/// allocation in [`Request::parse`] so a single header can't make the server
/// reserve arbitrary memory or panic on capacity overflow. Shared by the simple
/// server and the multi-DB `http_api` server (which reuses `Request::parse`).
const MAX_REQUEST_BODY: usize = 256 * 1024 * 1024; // 256 MiB

/// A minimal, self-contained (inline CSS/JS, no external assets) single-page
/// web console served at `GET /` and `GET /console`: a textarea to run
/// SPARQL against this endpoint's single database via `/sparql`, plus a short
/// reference of the available routes. See [`crate::http_api::CONSOLE_HTML`]
/// for the richer multi-database console served by [`crate::http_api`].
pub const CONSOLE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>gStore console</title>
<style>
  body { font-family: -apple-system, "Segoe UI", sans-serif; margin: 2rem; background: #f7f7f9; color: #1a1a1a; }
  h1 { font-size: 1.3rem; }
  h2 { font-size: 1.05rem; margin-top: 2rem; }
  textarea { width: 100%; height: 8rem; font-family: monospace; font-size: 0.9rem; box-sizing: border-box; padding: 0.5rem; }
  button { margin-top: 0.5rem; padding: 0.4rem 1rem; font-size: 0.9rem; cursor: pointer; }
  pre { background: #1e1e1e; color: #e6e6e6; padding: 1rem; overflow: auto; max-height: 30rem; white-space: pre-wrap; word-break: break-word; }
  .row { display: flex; gap: 0.5rem; align-items: center; }
  code { background: #eee; padding: 0 0.25rem; }
  .endpoints { font-size: 0.85rem; line-height: 1.6; }
</style>
</head>
<body>
<h1>gStore console</h1>
<p>SPARQL query:</p>
<textarea id="q">SELECT * WHERE { ?s ?p ?o } LIMIT 20</textarea>
<div class="row">
  <button id="run">Run</button>
  <span id="runStatus"></span>
</div>
<pre id="out">(results appear here)</pre>
<h2>Endpoints</h2>
<p class="endpoints">
  <code>GET|POST /sparql?query=...</code> — run a SPARQL query
  (SELECT/ASK/CONSTRUCT/DESCRIBE)<br>
  <code>POST /update</code> — run a SPARQL UPDATE<br>
  <code>GET /status</code> — database stats (JSON)<br>
  If this server was started with HTTP Basic auth, your browser will prompt
  for credentials on the first request below.
</p>
<script>
document.getElementById('run').addEventListener('click', function () {
  var q = document.getElementById('q').value;
  var out = document.getElementById('out');
  var runStatus = document.getElementById('runStatus');
  runStatus.textContent = 'running...';
  fetch('/sparql', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: 'query=' + encodeURIComponent(q)
  }).then(function (r) {
    runStatus.textContent = 'HTTP ' + r.status;
    return r.text();
  }).then(function (text) {
    try {
      out.textContent = JSON.stringify(JSON.parse(text), null, 2);
    } catch (e) {
      out.textContent = text;
    }
  }).catch(function (e) {
    runStatus.textContent = 'error';
    out.textContent = String(e);
  });
});
</script>
</body>
</html>
"##;

/// An HTTP SPARQL server wrapping a [`Database`].
///
/// **Durability.** When the server knows its database directory (see
/// [`with_db_dir`](Self::with_db_dir)), every successful UPDATE that changed
/// data is saved back to that directory straight away (the gStore
/// `check_point` policy, on by default — turn it off with
/// [`with_check_point(false)`](Self::with_check_point)), and any still-unsaved
/// change is saved during a graceful [`shutdown`](Self::shutdown). A server
/// built with plain [`bind`](Self::bind) has no directory and stays purely
/// in-memory, exactly as before.
pub struct Server {
    /// The shared, thread-safe part handed to each connection's worker thread.
    inner: Arc<ServerInner>,
    /// The listening socket; taken (closed) once the server has shut down.
    listener: Mutex<Option<TcpListener>>,
    addr: SocketAddr,
    life: Arc<Lifecycle>,
    /// Upper bound on how long a graceful shutdown waits for in-flight requests.
    grace: Duration,
}

/// The part of a [`Server`] shared across connection worker threads (behind an
/// `Arc`): the [`Database`] (serialized by its `Mutex`) and the optional Basic
/// credentials. Kept separate from the non-`Send` bookkeeping so a worker only
/// needs to clone this `Arc`.
struct ServerInner {
    db: Mutex<Database>,
    /// Set (under the `db` lock) when the in-memory database has changes that
    /// are not yet saved to [`dir`](Self::dir); cleared by a successful save.
    dirty: AtomicBool,
    /// Optional HTTP Basic credentials. `None` ⇒ the server is open.
    auth: Option<(String, String)>,
    /// The directory the database was loaded from / is persisted to. `None` ⇒
    /// in-memory only (nothing is ever saved).
    dir: Option<PathBuf>,
    /// gStore's `check_point` policy: save after every successful update.
    check_point: bool,
}

impl Server {
    /// Bind to `addr` (e.g. `"127.0.0.1:7000"` or `"127.0.0.1:0"` for an
    /// ephemeral port), taking ownership of `db`. The server is in-memory only
    /// until a directory is attached with [`with_db_dir`](Self::with_db_dir).
    pub fn bind<A: ToSocketAddrs>(db: Database, addr: A) -> std::io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        Ok(Server {
            inner: Arc::new(ServerInner {
                db: Mutex::new(db),
                dirty: AtomicBool::new(false),
                auth: None,
                dir: None,
                check_point: true,
            }),
            listener: Mutex::new(Some(listener)),
            addr,
            life: Lifecycle::new(addr),
            grace: DEFAULT_SHUTDOWN_GRACE,
        })
    }

    /// Persist the database to `dir` (normally the directory it was loaded
    /// from): saved after each successful update while the `check_point` policy
    /// is on (the default), and on graceful shutdown in any case.
    ///
    /// ```no_run
    /// # use gstore::{Database, server::Server};
    /// let db = Database::load("lubm.db").unwrap();
    /// let server = Server::bind(db, "127.0.0.1:0").unwrap().with_db_dir("lubm.db");
    /// ```
    pub fn with_db_dir(mut self, dir: impl Into<PathBuf>) -> Server {
        // Builders run right after `bind`, before the inner `Arc` is shared with
        // any worker thread, so `get_mut` succeeds (see `with_basic_auth`).
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.dir = Some(dir.into());
        }
        self
    }

    /// Turn gStore's `check_point` policy on (the default) or off. When off,
    /// updates stay in memory and are saved only on graceful shutdown.
    pub fn with_check_point(mut self, on: bool) -> Server {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.check_point = on;
        }
        self
    }

    /// Bound the time a graceful shutdown waits for in-flight requests to
    /// finish before saving and returning (default [`DEFAULT_SHUTDOWN_GRACE`]).
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Server {
        self.grace = grace;
        self
    }

    /// Require HTTP Basic auth with these credentials on every request.
    ///
    /// ```no_run
    /// # use gstore::{Database, server::Server};
    /// # let db = Database::build_from_str("x", "").unwrap();
    /// let server = Server::bind(db, "127.0.0.1:0").unwrap().with_basic_auth("admin", "s3cret");
    /// ```
    pub fn with_basic_auth(mut self, user: &str, pass: &str) -> Server {
        // This builder runs right after `bind`, before the inner `Arc` is
        // shared with any worker thread, so it is uniquely owned and `get_mut`
        // succeeds. (If it were somehow already shared we'd silently keep the
        // open config, but the builder contract prevents that.)
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.auth = Some((user.to_string(), pass.to_string()));
        }
        self
    }

    /// The actual bound address (useful after binding to port 0).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.addr)
    }

    /// The directory the database is persisted to, if any.
    pub fn db_dir(&self) -> Option<&Path> {
        self.inner.dir.as_deref()
    }

    /// Whether the check_point policy (save after every update) is on.
    pub fn check_point(&self) -> bool {
        self.inner.check_point
    }

    /// A cloneable handle that can stop this server from any thread (or from a
    /// signal watcher, see [`shutdown_on_signals`]).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle::new(Arc::clone(&self.life))
    }

    /// Request a graceful shutdown: stop accepting connections, let in-flight
    /// requests finish (bounded by the grace period), save the database if it
    /// has unsaved changes, and return from [`serve_forever`](Self::serve_forever).
    /// Returns immediately; join the serving thread to wait for completion.
    pub fn shutdown(&self) {
        self.life.request_stop();
    }

    /// Accept connections until [`shutdown`](Self::shutdown), handling each on
    /// its own worker thread so a slow query on one connection does not block
    /// the others (the [`Database`] itself is still serialized behind its
    /// `Mutex`). On shutdown the listener is closed, in-flight requests are given
    /// up to the grace period to finish, unsaved changes are saved, and a
    /// [`ShutdownReport`] is returned.
    pub fn serve_forever(&self) -> ShutdownReport {
        let drained = run_accept_loop(&self.listener, &self.life, self.grace, |stream, guard| {
            let inner = Arc::clone(&self.inner);
            spawn_worker(move || {
                let _in_flight = guard;
                let mut stream = stream;
                let _ = catch_unwind(AssertUnwindSafe(|| inner.handle(&mut stream)));
            });
        });
        self.finish_shutdown(drained)
    }

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS
    /// in-process, wrapping every accepted connection in a `rustls` stream built
    /// from `config` (see [`load_tls_config`]). Requires the `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        let drained = run_accept_loop(&self.listener, &self.life, self.grace, |stream, guard| {
            let inner = Arc::clone(&self.inner);
            let config = Arc::clone(&config);
            spawn_worker(move || {
                let _in_flight = guard;
                if let Ok(conn) = rustls::ServerConnection::new(config) {
                    let mut tls = rustls::StreamOwned::new(conn, stream);
                    let _ = catch_unwind(AssertUnwindSafe(|| inner.handle(&mut tls)));
                }
            });
        });
        self.finish_shutdown(drained)
    }

    /// The tail of a graceful shutdown: save unsaved changes (if a directory is
    /// attached) and report what happened.
    fn finish_shutdown(&self, drained: bool) -> ShutdownReport {
        let mut report = ShutdownReport {
            drained,
            ..ShutdownReport::default()
        };
        if self.inner.dirty.load(Ordering::SeqCst) {
            let db = locked(&self.inner.db);
            match self.inner.save_locked(&db) {
                Ok(Some(dir)) => report.saved.push(dir),
                Ok(None) => {}
                Err((dir, e)) => report.failed.push((dir, e)),
            }
        }
        report
    }
}

/// The default upper bound on how long a graceful shutdown waits for in-flight
/// requests before it saves and returns anyway.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// What a graceful shutdown did; returned by `serve_forever` on both
/// [`Server`] and [`ApiServer`](crate::http_api::ApiServer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// `true` when every in-flight request finished within the grace period.
    pub drained: bool,
    /// Database directories saved during shutdown (only databases with unsaved
    /// changes are written).
    pub saved: Vec<PathBuf>,
    /// Saves that failed, as `(directory, error message)`.
    pub failed: Vec<(PathBuf, String)>,
    /// Open HTTP transactions rolled back because the server stopped
    /// ([`ApiServer`](crate::http_api::ApiServer) only).
    pub rolled_back: usize,
}

/// A cloneable, thread-safe handle that gracefully stops a running [`Server`]
/// or [`ApiServer`](crate::http_api::ApiServer) — from a test, another thread,
/// or a signal watcher ([`shutdown_on_signals`]).
#[derive(Clone)]
pub struct ShutdownHandle {
    life: Arc<Lifecycle>,
}

impl ShutdownHandle {
    pub(crate) fn new(life: Arc<Lifecycle>) -> ShutdownHandle {
        ShutdownHandle { life }
    }

    /// Request a graceful shutdown (idempotent, non-blocking).
    pub fn shutdown(&self) {
        self.life.request_stop();
    }

    /// Whether a shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.life.stopping()
    }
}

impl std::fmt::Debug for ShutdownHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownHandle")
            .field("addr", &self.life.wake)
            .field("shutdown", &self.is_shutdown())
            .finish()
    }
}

/// Stop/in-flight bookkeeping shared by a server's accept loop, its workers,
/// and every [`ShutdownHandle`].
pub(crate) struct Lifecycle {
    stop: AtomicBool,
    in_flight: AtomicUsize,
    /// A connectable form of the bound address, used to wake a blocked
    /// `accept()` when a shutdown is requested.
    wake: SocketAddr,
}

impl Lifecycle {
    pub(crate) fn new(bound: SocketAddr) -> Arc<Lifecycle> {
        // An unspecified bind address (0.0.0.0 / ::) is not connectable on every
        // platform; wake through the loopback address of the same family.
        let ip = match bound.ip() {
            IpAddr::V4(v) if v.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(v) if v.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        Arc::new(Lifecycle {
            stop: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            wake: SocketAddr::new(ip, bound.port()),
        })
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Flag the shutdown and wake the accept loop with a throwaway
    /// self-connection (the loop re-checks the flag after every `accept`).
    pub(crate) fn request_stop(&self) {
        if !self.stop.swap(true, Ordering::SeqCst) {
            let _ = TcpStream::connect_timeout(&self.wake, Duration::from_secs(1));
        }
    }

    /// Count one in-flight connection until the returned guard drops.
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(Arc::clone(self))
    }

    /// Wait until no connection is in flight, at most `grace`. Returns whether
    /// the server drained.
    pub(crate) fn wait_idle(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        loop {
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Marks one connection as in flight; dropping it (normally, on a panic, or
/// when a worker thread could not be spawned) releases the count.
pub(crate) struct InFlightGuard(Arc<Lifecycle>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The shared accept loop: hand every accepted connection (with its read
/// timeout set and an [`InFlightGuard`]) to `on_conn` until a shutdown is
/// requested, then close the listener and wait — at most `grace` — for the
/// in-flight connections to finish. Returns whether they all did.
pub(crate) fn run_accept_loop(
    listener: &Mutex<Option<TcpListener>>,
    life: &Arc<Lifecycle>,
    grace: Duration,
    mut on_conn: impl FnMut(TcpStream, InFlightGuard),
) -> bool {
    // Accept on a clone of the socket so the `Mutex` is not held while blocked.
    let sock = locked(listener).as_ref().and_then(|l| l.try_clone().ok());
    if let Some(sock) = sock {
        while !life.stopping() {
            match sock.accept() {
                Ok((stream, _)) => {
                    if life.stopping() {
                        break; // the wake-up self-connection (or a late client)
                    }
                    let _ = stream.set_read_timeout(Some(CONN_READ_TIMEOUT));
                    on_conn(stream, life.enter());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // E.g. EMFILE under a connection flood: back off instead of
                // spinning on the failing accept.
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    // Close both descriptors so new connections are refused from now on.
    locked(listener).take();
    life.wait_idle(grace)
}

/// Arrange for `SIGINT`/`SIGTERM` to trigger `handle.shutdown()` (unix only;
/// returns `false` — and does nothing — elsewhere or if the handlers could not
/// be installed). A tiny watcher thread polls the flag the async-signal-safe
/// handler sets. After the first signal the default disposition is restored,
/// so a second Ctrl-C terminates the process immediately.
pub fn shutdown_on_signals(handle: ShutdownHandle) -> bool {
    #[cfg(unix)]
    {
        if !signals::install() {
            return false;
        }
        thread::Builder::new()
            .name("gstore-signals".into())
            .spawn(move || loop {
                if signals::received() {
                    signals::restore_default();
                    handle.shutdown();
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            })
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = handle;
        false
    }
}

/// Minimal `signal(2)` binding (no extra crates): the handler only stores to an
/// `AtomicBool`, which is async-signal-safe.
#[cfg(unix)]
mod signals {
    use std::os::raw::c_int;
    use std::sync::atomic::{AtomicBool, Ordering};

    static RECEIVED: AtomicBool = AtomicBool::new(false);

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    /// `SIG_DFL` / `SIG_ERR` as the pointer-sized `sighandler_t` values.
    const SIG_DFL: usize = 0;
    const SIG_ERR: usize = usize::MAX;

    extern "C" {
        /// `sighandler_t signal(int signum, sighandler_t handler)` from libc;
        /// `sighandler_t` is passed as a pointer-sized integer so that
        /// `SIG_DFL` can be expressed.
        fn signal(signum: c_int, handler: usize) -> usize;
    }

    extern "C" fn on_signal(_signum: c_int) {
        RECEIVED.store(true, Ordering::SeqCst);
    }

    pub(super) fn install() -> bool {
        let handler = on_signal as extern "C" fn(c_int) as usize;
        // SAFETY: `signal` is the C library's; `on_signal` has the C handler
        // signature and only performs a lock-free atomic store.
        unsafe { signal(SIGINT, handler) != SIG_ERR && signal(SIGTERM, handler) != SIG_ERR }
    }

    pub(super) fn restore_default() {
        // SAFETY: restoring the default disposition is always valid.
        unsafe {
            signal(SIGINT, SIG_DFL);
            signal(SIGTERM, SIG_DFL);
        }
    }

    pub(super) fn received() -> bool {
        RECEIVED.load(Ordering::SeqCst)
    }
}

/// Idle read timeout applied to every accepted connection: a client that opens a
/// socket and then sends nothing (a slowloris) times out in `Request::parse`
/// instead of pinning its worker thread and fd forever.
pub(crate) const CONN_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn a per-connection worker that can never take down the accept loop: a
/// failed spawn (e.g. the OS thread limit under a connection flood) drops just
/// this connection instead of panicking the accepting thread and exiting the
/// process.
pub(crate) fn spawn_worker<F: FnOnce() + Send + 'static>(f: F) {
    let _ = thread::Builder::new().spawn(f);
}

/// Lock a mutex, tolerating poisoning: if a previous worker panicked while
/// holding it, recover the guard rather than cascading the panic (which would
/// otherwise zombify the whole server — accepting connections but answering
/// none). Reads/writes after a recovered panic may see partial state, but the
/// server stays available.
pub(crate) fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The message returned (as HTTP 500) when a request handler panics; the panic
/// itself is caught per request so the worker, the locks and the server survive.
pub(crate) const PANIC_MSG: &str = "internal server error: the request handler panicked";

/// A response computed by [`ServerInner::route`], written after the (panic-
/// isolated) routing step.
enum Reply {
    /// A buffered body. `compress` allows gzip/deflate (feature `gzip`).
    Buffered {
        status: u16,
        ctype: String,
        body: Vec<u8>,
        compress: bool,
    },
    /// A SELECT streamed with `Transfer-Encoding: chunked`.
    Stream(ResultFormat, ResultSet),
}

impl Reply {
    fn buffered(status: u16, ctype: &str, body: Vec<u8>) -> Reply {
        Reply::Buffered {
            status,
            ctype: ctype.to_string(),
            body,
            compress: true,
        }
    }

    fn plain(status: u16, ctype: &str, body: &[u8]) -> Reply {
        Reply::Buffered {
            status,
            ctype: ctype.to_string(),
            body: body.to_vec(),
            compress: false,
        }
    }
}

impl ServerInner {
    /// Handle one connection (one request). Generic over the transport so the
    /// same code path serves plain [`TcpStream`]s and `rustls` TLS streams.
    /// Routing runs under `catch_unwind`: a panicking handler answers `500`
    /// and leaves the server (and its recovered locks) serving.
    fn handle<S: Read + Write>(&self, stream: &mut S) -> std::io::Result<()> {
        let req = match Request::parse(stream) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()), // connection closed with no request
            Err(_) => return write_response(stream, 400, "text/plain", b"bad request"),
        };
        // Auth gate: when credentials are configured every request must present
        // a matching `Authorization: Basic` header.
        if let Some((user, pass)) = &self.auth {
            if !req.authorized(user, pass) {
                return write_response_ext(
                    stream,
                    401,
                    "text/plain",
                    &[("WWW-Authenticate", "Basic realm=\"gStore\"")],
                    b"unauthorized",
                );
            }
        }
        let reply = catch_unwind(AssertUnwindSafe(|| self.route(&req)))
            .unwrap_or_else(|_| Reply::plain(500, "application/json", &err_json(PANIC_MSG)));
        // The client's `Accept-Encoding` (used to optionally compress buffered
        // bodies under the `gzip` feature; ignored without it).
        let enc = req.header("accept-encoding");
        match reply {
            Reply::Buffered {
                status,
                ctype,
                body,
                compress: true,
            } => write_response_maybe_gzip(stream, status, &ctype, &[], &body, enc),
            Reply::Buffered {
                status, ctype, body, ..
            } => write_response(stream, status, &ctype, &body),
            Reply::Stream(fmt, rs) => stream_select(stream, fmt, &rs),
        }
    }

    /// Route a parsed, authorized request to its handler.
    fn route(&self, req: &Request) -> Reply {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET" | "POST", "/sparql") => self.handle_sparql(req),
            ("POST", "/update") => self.run_update(&req.body_string()),
            ("GET", "/status") => self.status_json(),
            ("GET", "/" | "/console") => Reply::buffered(
                200,
                "text/html; charset=utf-8",
                CONSOLE_HTML.as_bytes().to_vec(),
            ),
            // Test-only trigger for the panic-isolation tests: panics while
            // holding the database lock (poisoning it).
            #[cfg(test)]
            ("GET", "/__test/panic") => {
                let _db = locked(&self.db);
                panic!("test-triggered handler panic");
            }
            ("GET" | "POST", _) => Reply::plain(404, "text/plain", b"not found"),
            _ => Reply::plain(405, "text/plain", b"method not allowed"),
        }
    }

    /// Run a `/sparql` request: negotiate a result format, then answer with a
    /// buffered (`Content-Length`) body, or a chunked stream when `?stream=true`.
    /// The [`Database`] lock is released before any serialization or network
    /// write, so it is held only for the query itself.
    fn handle_sparql(&self, req: &Request) -> Reply {
        let q = match req.query_string() {
            Some(q) => q,
            None => return Reply::plain(400, SPARQL_JSON, &err_json("missing query")),
        };
        let fmt = ResultFormat::negotiate(req.format_param().as_deref(), req.header("accept"));
        let (result, warning) = self.execute(&q);
        match result {
            Ok(QueryResult::Select(rs)) => {
                if req.wants_stream() {
                    Reply::Stream(fmt, rs)
                } else {
                    let mut buf = Vec::new();
                    let _ = sparql_results::write_select(fmt, &rs, &mut buf);
                    Reply::buffered(200, fmt.content_type(), buf)
                }
            }
            Ok(QueryResult::Ask(b)) => {
                let mut buf = Vec::new();
                let _ = sparql_results::write_ask(fmt, b, &mut buf);
                Reply::buffered(200, fmt.content_type(), buf)
            }
            Ok(QueryResult::Construct(ts)) => {
                Reply::buffered(200, "application/n-triples", ntriples_body(&ts))
            }
            Ok(QueryResult::Update { changed }) => {
                Reply::buffered(200, "application/json", changed_json(changed, warning))
            }
            Err(e) => Reply::plain(400, SPARQL_JSON, &err_json(&e.to_string())),
        }
    }

    fn run_update(&self, u: &str) -> Reply {
        let (result, warning) = self.execute(u);
        match result {
            Ok(QueryResult::Update { changed }) => {
                Reply::buffered(200, "application/json", changed_json(changed, warning))
            }
            Ok(_) => Reply::buffered(
                400,
                "application/json",
                err_json("/update expects a SPARQL UPDATE request"),
            ),
            Err(e) => Reply::buffered(400, "application/json", err_json(&e.to_string())),
        }
    }

    /// Run `sparql` under the database lock. An UPDATE that changed data marks
    /// the database dirty and — under the check_point policy — saves it before
    /// the lock is released, so the on-disk copy never lags an acknowledged
    /// write. A failed save does not undo the (applied) update: it is reported
    /// as a warning and the database stays dirty, to be retried on shutdown.
    fn execute(&self, sparql: &str) -> (crate::Result<QueryResult>, Option<String>) {
        let mut db = locked(&self.db);
        let result = db.query(sparql);
        let mut warning = None;
        if let Ok(QueryResult::Update { changed }) = &result {
            if *changed > 0 {
                self.dirty.store(true, Ordering::SeqCst);
                if self.check_point {
                    if let Err((dir, e)) = self.save_locked(&db) {
                        warning = Some(format!(
                            "update applied in memory but saving to '{}' failed: {e}",
                            dir.display()
                        ));
                    }
                }
            }
        }
        (result, warning)
    }

    /// Save `db` (whose lock the caller holds) to the attached directory and
    /// clear the dirty flag. `Ok(None)` when no directory is attached.
    fn save_locked(&self, db: &Database) -> Result<Option<PathBuf>, (PathBuf, String)> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        match db.save(dir) {
            Ok(()) => {
                self.dirty.store(false, Ordering::SeqCst);
                Ok(Some(dir.clone()))
            }
            Err(e) => Err((dir.clone(), e.to_string())),
        }
    }

    fn status_json(&self) -> Reply {
        let db = locked(&self.db);
        let s = db.stats();
        let dir = match &self.dir {
            Some(d) => json_str(&d.to_string_lossy()),
            None => "null".to_string(),
        };
        let body = format!(
            "{{\"name\":{},\"triples\":{},\"entities\":{},\"literals\":{},\"predicates\":{},\"index_valid\":{},\"in_transaction\":{},\"db_dir\":{dir},\"check_point\":{},\"dirty\":{}}}",
            json_str(&s.name),
            s.triple_num,
            s.entity_num,
            s.literal_num,
            s.predicate_num,
            s.index_valid,
            s.in_transaction,
            self.check_point,
            self.dirty.load(Ordering::SeqCst),
        );
        Reply::buffered(200, "application/json", body.into_bytes())
    }
}

/// `{"changed":N}` plus an optional `"warning"` (e.g. a failed check_point save).
fn changed_json(changed: usize, warning: Option<String>) -> Vec<u8> {
    match warning {
        Some(w) => format!("{{\"changed\":{changed},\"warning\":{}}}", json_str(&w)).into_bytes(),
        None => format!("{{\"changed\":{changed}}}").into_bytes(),
    }
}

/// A parsed HTTP request (method, path, query string, headers, body).
///
/// `pub(crate)` so the richer [`crate::http_api`] endpoint can reuse the same
/// parser and accessors rather than re-implementing HTTP request parsing.
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: Option<String>,
    /// Header names lowercased for case-insensitive lookup.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    pub(crate) fn parse<R: Read>(stream: &mut R) -> std::io::Result<Option<Request>> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (target, None),
        };

        let mut content_length = 0usize;
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 {
                break;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break; // end of headers
            }
            if let Some((k, v)) = h.split_once(':') {
                let k = k.trim();
                let v = v.trim();
                if k.eq_ignore_ascii_case("content-length") {
                    content_length = v.parse().unwrap_or(0);
                }
                headers.push((k.to_ascii_lowercase(), v.to_string()));
            }
        }

        // Refuse an oversized (or absurd, e.g. u64::MAX) Content-Length BEFORE
        // allocating, so a client cannot make the server reserve arbitrary memory
        // (or panic on capacity overflow) with a single header — this runs before
        // authentication, so the cap is the only guard. Large RDF ingests should
        // use file-based tools (gbuild/gadd) or the server-side `/load` path.
        if content_length > MAX_REQUEST_BODY {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request body exceeds the maximum size",
            ));
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        Ok(Some(Request {
            method,
            path,
            query,
            headers,
            body,
        }))
    }

    /// Look up a header by (case-insensitive) name.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// True iff the request carries valid `Authorization: Basic` credentials
    /// equal to `user:pass`.
    fn authorized(&self, user: &str, pass: &str) -> bool {
        let token = match self
            .header("authorization")
            .and_then(|h| strip_basic_prefix(h))
        {
            Some(t) => t.trim(),
            None => return false,
        };
        let decoded = match base64_decode(token) {
            Some(d) => d,
            None => return false,
        };
        let expected = format!("{user}:{pass}");
        ct_eq(&decoded, expected.as_bytes())
    }

    /// The negotiated result format token from `?format=` (URL or form body).
    pub(crate) fn format_param(&self) -> Option<String> {
        if let Some(q) = &self.query {
            if let Some(v) = form_get(q, "format") {
                return Some(v);
            }
        }
        let b = String::from_utf8_lossy(&self.body);
        form_get(&b, "format")
    }

    /// True when the URL query string requests a chunked stream
    /// (`?stream=true`). Deliberately ignores the body so the chunked-unaware
    /// `SERVICE` POST path (no query string) is never streamed.
    pub(crate) fn wants_stream(&self) -> bool {
        self.query
            .as_deref()
            .and_then(|q| form_get(q, "stream"))
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    }

    /// Look up a parameter by name in the URL query string first, then in a
    /// form-encoded body. Used by [`crate::http_api`] for its `db=`/`name=`/
    /// `txn=`/`username=` … parameters. Returns `None` when the body is raw
    /// (non-form) content.
    pub(crate) fn param(&self, key: &str) -> Option<String> {
        if let Some(q) = &self.query {
            if let Some(v) = form_get(q, key) {
                return Some(v);
            }
        }
        let b = String::from_utf8_lossy(&self.body);
        form_get(&b, key)
    }

    /// The raw request body as a UTF-8 (lossy) string.
    pub(crate) fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Extract a session/bearer token from `Authorization: Bearer …`, the
    /// `X-Session-Token` header, or a `session=`/`token=` parameter.
    pub(crate) fn bearer_or_session_token(&self) -> Option<String> {
        if let Some(h) = self.header("authorization") {
            let h = h.trim_start();
            if h.len() >= 7 && h[..7].eq_ignore_ascii_case("bearer ") {
                return Some(h[7..].trim().to_string());
            }
        }
        if let Some(h) = self.header("x-session-token") {
            return Some(h.trim().to_string());
        }
        self.param("session").or_else(|| self.param("token"))
    }

    /// The query text: from the URL `?query=`, a form-encoded `query=` body, or
    /// (for POST with a `application/sparql-query` body) the raw body.
    pub(crate) fn query_string(&self) -> Option<String> {
        if let Some(q) = &self.query {
            if let Some(v) = form_get(q, "query") {
                return Some(v);
            }
        }
        let b = String::from_utf8_lossy(&self.body);
        if let Some(v) = form_get(&b, "query") {
            return Some(v);
        }
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        None
    }

    /// The update text: a form-encoded `update=` body, else the raw body.
    fn body_string(&self) -> String {
        let b = String::from_utf8_lossy(&self.body);
        form_get(&b, "update").unwrap_or_else(|| b.into_owned())
    }
}

// --- response helpers --------------------------------------------------------

pub(crate) fn write_response<W: Write>(
    stream: &mut W,
    status: u16,
    ctype: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_response_ext(stream, status, ctype, &[], body)
}

/// Write a buffered response, optionally gzip/deflate-compressing the body when
/// the client's `Accept-Encoding` permits it and the `gzip` feature is enabled.
/// Sets `Content-Encoding` (and `Vary: Accept-Encoding`) on the compressed path.
/// Without the feature `accept_encoding` is ignored and the body is sent
/// verbatim, so the default build stays dependency-free.
pub(crate) fn write_response_maybe_gzip<W: Write>(
    stream: &mut W,
    status: u16,
    ctype: &str,
    extra: &[(&str, &str)],
    body: &[u8],
    accept_encoding: Option<&str>,
) -> std::io::Result<()> {
    #[cfg(feature = "gzip")]
    {
        if let Some(enc) = pick_encoding(accept_encoding) {
            if let Some(compressed) = compress(body, enc) {
                let mut hdrs: Vec<(&str, &str)> = extra.to_vec();
                hdrs.push(("Content-Encoding", enc));
                hdrs.push(("Vary", "Accept-Encoding"));
                return write_response_ext(stream, status, ctype, &hdrs, &compressed);
            }
        }
    }
    #[cfg(not(feature = "gzip"))]
    let _ = accept_encoding;
    write_response_ext(stream, status, ctype, extra, body)
}

/// Like [`write_response`], but with additional response headers (e.g.
/// `WWW-Authenticate` on a `401`).
pub(crate) fn write_response_ext<W: Write>(
    stream: &mut W,
    status: u16,
    ctype: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for &(k, v) in extra {
        header.push_str(&format!("{k}: {v}\r\n"));
    }
    header.push_str("Connection: close\r\n\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Stream a SELECT result with `Transfer-Encoding: chunked`, framing each
/// serializer `write_all` (one per row) as its own HTTP chunk.
fn stream_select<W: Write>(
    stream: &mut W,
    fmt: ResultFormat,
    rs: &ResultSet,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        fmt.content_type()
    );
    stream.write_all(header.as_bytes())?;
    let mut cw = ChunkedWriter { inner: stream };
    sparql_results::write_select(fmt, rs, &mut cw)?;
    cw.finish()
}

/// A [`Write`] adapter that frames each write as one HTTP/1.1 transfer chunk.
struct ChunkedWriter<'a, W: Write> {
    inner: &'a mut W,
}

impl<W: Write> ChunkedWriter<'_, W> {
    /// Emit the terminating zero-length chunk and flush.
    fn finish(&mut self) -> io::Result<()> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()
    }
}

impl<W: Write> Write for ChunkedWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let size_line = format!("{:x}\r\n", buf.len());
        self.inner.write_all(size_line.as_bytes())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// --- response compression (feature `gzip`) -----------------------------------

/// Choose a supported `Content-Encoding` from the client's `Accept-Encoding`.
/// Prefers `gzip`, then `deflate`. Honors an explicit `q=0` (the client refusing
/// a coding); other q-values are treated as "acceptable". Returns `None` when
/// neither is offered (or both are refused).
#[cfg(feature = "gzip")]
fn pick_encoding(accept_encoding: Option<&str>) -> Option<&'static str> {
    let ae = accept_encoding?.to_ascii_lowercase();
    let acceptable = |name: &str| {
        ae.split(',').any(|tok| {
            let mut parts = tok.trim().split(';');
            if parts.next().map(str::trim) != Some(name) {
                return false;
            }
            // A `q=0` weight means the client explicitly refuses this coding.
            !parts.any(|p| {
                p.trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f64>().ok())
                    .is_some_and(|v| v <= 0.0)
            })
        })
    };
    if acceptable("gzip") {
        Some("gzip")
    } else if acceptable("deflate") {
        Some("deflate")
    } else {
        None
    }
}

/// Compress `body` with the chosen encoding. `"deflate"` uses the zlib wrapper
/// (RFC 1950), which is what HTTP's `deflate` content-coding specifies.
#[cfg(feature = "gzip")]
fn compress(body: &[u8], enc: &str) -> Option<Vec<u8>> {
    use flate2::write::{GzEncoder, ZlibEncoder};
    use flate2::Compression;
    let mut buf = Vec::new();
    match enc {
        "gzip" => {
            let mut e = GzEncoder::new(&mut buf, Compression::default());
            e.write_all(body).ok()?;
            e.finish().ok()?;
        }
        "deflate" => {
            let mut e = ZlibEncoder::new(&mut buf, Compression::default());
            e.write_all(body).ok()?;
            e.finish().ok()?;
        }
        _ => return None,
    }
    Some(buf)
}

// --- TLS (feature `tls`) -----------------------------------------------------

/// Build a [`rustls::ServerConfig`] (wrapped in an `Arc`, ready for
/// [`Server::serve_forever_tls`] / [`ApiServer::serve_forever_tls`](crate::http_api::ApiServer::serve_forever_tls))
/// from a PEM certificate chain and PKCS#8/SEC1/PKCS#1 private key on disk. Uses
/// the pure-`ring` crypto backend (no `aws-lc-rs`/bindgen). Requires the `tls`
/// feature.
///
/// ```no_run
/// # #[cfg(feature = "tls")] {
/// use gstore::{Database, server::{Server, load_tls_config}};
/// let db = Database::build_from_str("x", "").unwrap();
/// let server = Server::bind(db, "127.0.0.1:8443").unwrap();
/// let config = load_tls_config("cert.pem", "key.pem").unwrap();
/// // server.serve_forever_tls(config); // serves HTTPS
/// # let _ = (server, config);
/// # }
/// ```
#[cfg(feature = "tls")]
pub fn load_tls_config(
    cert_pem: impl AsRef<std::path::Path>,
    key_pem: impl AsRef<std::path::Path>,
) -> std::io::Result<Arc<rustls::ServerConfig>> {
    use std::fs::File;
    let mut cr = BufReader::new(File::open(cert_pem)?);
    let certs = rustls_pemfile::certs(&mut cr).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no certificates found in the certificate PEM",
        ));
    }
    let mut kr = BufReader::new(File::open(key_pem)?);
    let key = rustls_pemfile::private_key(&mut kr)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no private key found in the key PEM")
    })?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Arc::new(config))
}

pub(crate) fn ntriples_body(triples: &[crate::model::Triple]) -> Vec<u8> {
    let mut s = String::new();
    for t in triples {
        s.push_str(&format!("{} {} {} .\n", t.subject, t.predicate, t.object));
    }
    s.into_bytes()
}

pub(crate) fn err_json(msg: &str) -> Vec<u8> {
    format!("{{\"error\":{}}}", json_str(msg)).into_bytes()
}

/// Parse HTTP Basic `Authorization` credentials into `(username, password)`.
/// Shared with [`crate::http_api`], which validates them against its user store
/// (the simpler [`Server`] compares against a single fixed credential instead).
pub(crate) fn parse_basic_auth(req: &Request) -> Option<(String, String)> {
    let token = req
        .header("authorization")
        .and_then(strip_basic_prefix)?
        .trim();
    let decoded = base64_decode(token)?;
    let s = String::from_utf8(decoded).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

// --- HTTP Basic auth ---------------------------------------------------------

/// Strip a case-insensitive `Basic ` scheme prefix from an `Authorization`
/// header value, returning the credentials token.
fn strip_basic_prefix(h: &str) -> Option<&str> {
    let h = h.trim_start();
    if h.len() >= 6 && h[..6].eq_ignore_ascii_case("basic ") {
        Some(&h[6..])
    } else {
        None
    }
}

/// Decode standard (RFC 4648) base64, ignoring ASCII whitespace and stopping at
/// padding. Returns `None` on an invalid alphabet character.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Length-checked, byte-by-byte equality that does not short-circuit on the
/// first differing byte (mitigates timing side channels on the credential
/// compare; the length itself is not hidden).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// --- URL form decoding -------------------------------------------------------

/// Look up `key` in an `application/x-www-form-urlencoded` string.
fn form_get(qs: &str, key: &str) -> Option<String> {
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if url_decode(k) == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::thread;

    #[cfg(feature = "gzip")]
    #[test]
    fn pick_encoding_honors_q_zero() {
        assert_eq!(pick_encoding(Some("gzip")), Some("gzip"));
        assert_eq!(pick_encoding(Some("gzip, deflate")), Some("gzip"));
        // An explicit q=0 refuses that coding.
        assert_eq!(pick_encoding(Some("gzip;q=0")), None);
        assert_eq!(pick_encoding(Some("gzip;q=0, deflate")), Some("deflate"));
        assert_eq!(pick_encoding(Some("gzip;q=0, deflate;q=0")), None);
        // A non-zero weight is acceptable.
        assert_eq!(pick_encoding(Some("gzip;q=0.5")), Some("gzip"));
        assert_eq!(pick_encoding(None), None);
    }

    #[test]
    fn locked_recovers_from_a_poisoned_mutex() {
        // A worker that panics while holding a lock must not brick the server:
        // locked() recovers the guard instead of cascading the panic.
        let m = Mutex::new(5);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
            panic!("poison the mutex");
        }));
        assert!(m.is_poisoned());
        assert_eq!(*locked(&m), 5, "locked() recovers a poisoned guard");
    }

    const DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n:a :name \"Alice\" .\n";

    fn start() -> (Arc<Server>, SocketAddr) {
        let db = Database::build_from_str("srv", DATA).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        (server, addr)
    }

    fn http(addr: &SocketAddr, raw: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    }

    fn post(addr: &SocketAddr, path: &str, body: &str) -> String {
        let raw = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        http(addr, &raw)
    }

    #[test]
    fn select_returns_sparql_json() {
        let (_srv, addr) = start();
        let resp = post(&addr, "/sparql", "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }");
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("application/sparql-results+json"));
        assert!(resp.contains("\"vars\":[\"o\"]"));
        assert!(resp.contains("\"type\":\"uri\""));
        assert!(resp.contains("http://ex/b"));
    }

    #[test]
    fn update_then_select_reflects_change() {
        let (_srv, addr) = start();
        let resp = post(
            &addr,
            "/update",
            "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        );
        assert!(resp.contains("\"changed\":1"));
        let resp2 = post(&addr, "/sparql", "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/d> }");
        assert!(resp2.contains("http://ex/c"));
    }

    #[test]
    fn ask_get_with_url_query_param() {
        let (_srv, addr) = start();
        // ASK { <http://ex/a> <http://ex/p> ?o } url-encoded
        let q = "ASK%20%7B%20%3Chttp%3A%2F%2Fex%2Fa%3E%20%3Chttp%3A%2F%2Fex%2Fp%3E%20%3Fo%20%7D";
        let raw = format!("GET /sparql?query={q} HTTP/1.1\r\nConnection: close\r\n\r\n");
        let resp = http(&addr, &raw);
        assert!(resp.contains("\"boolean\":true"));
    }

    #[test]
    fn console_served_at_root_and_console_path() {
        let (_srv, addr) = start();
        let resp = http(&addr, "GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp.contains("200 OK"), "{resp}");
        assert!(resp.contains("text/html"), "{resp}");
        assert!(resp.contains("gStore console"), "{resp}");
        assert!(resp.contains("/sparql"), "endpoint note present: {resp}");

        let resp2 = http(&addr, "GET /console HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp2.contains("200 OK"));
        assert!(resp2.contains("gStore console"));
    }

    #[test]
    fn status_reports_counts_and_unknown_path_404() {
        let (_srv, addr) = start();
        let resp = http(&addr, "GET /status HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("\"triples\":2"));
        let r404 = http(&addr, "GET /nope HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(r404.contains("404"));
    }

    #[test]
    fn malformed_query_is_400() {
        let (_srv, addr) = start();
        let resp = post(&addr, "/sparql", "SELECT this is not sparql");
        assert!(resp.contains("400"));
        assert!(resp.contains("\"error\""));
    }

    // --- helpers for the auth / negotiation / streaming tests ----------------

    const SELECT_O: &str = "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }";

    /// Like [`start`], but the server requires HTTP Basic `user:pass`.
    fn start_auth() -> (Arc<Server>, SocketAddr) {
        let db = Database::build_from_str("srv_auth", DATA).unwrap();
        let server =
            Arc::new(Server::bind(db, "127.0.0.1:0").unwrap().with_basic_auth("user", "pass"));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        (server, addr)
    }

    /// Standard (RFC 4648) base64 encode — the client side of [`base64_decode`].
    fn base64_encode(input: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    fn post_auth(addr: &SocketAddr, path: &str, body: &str, user: &str, pass: &str) -> String {
        let token = base64_encode(format!("{user}:{pass}").as_bytes());
        let raw = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Basic {token}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        http(addr, &raw)
    }

    fn post_accept(addr: &SocketAddr, path: &str, body: &str, accept: &str) -> String {
        let raw = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nAccept: {accept}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        http(addr, &raw)
    }

    /// Split a raw HTTP response into (headers, body).
    fn split_resp(resp: &str) -> (String, String) {
        let (h, b) = resp.split_once("\r\n\r\n").expect("header/body delimiter");
        (h.to_string(), b.to_string())
    }

    /// Decode an HTTP/1.1 chunked transfer body (ASCII content).
    fn decode_chunked(body: &str) -> String {
        let mut out = String::new();
        let mut rest = body;
        loop {
            let nl = rest.find("\r\n").expect("chunk-size line");
            let size = usize::from_str_radix(rest[..nl].trim(), 16).expect("hex chunk size");
            rest = &rest[nl + 2..];
            if size == 0 {
                break;
            }
            out.push_str(&rest[..size]);
            rest = &rest[size + 2..]; // skip chunk data + trailing CRLF
        }
        out
    }

    #[test]
    fn base64_roundtrips_and_rejects_garbage() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "user:pass"] {
            assert_eq!(
                base64_decode(&base64_encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
        assert!(base64_decode("not*base64").is_none());
    }

    #[test]
    fn auth_401_then_200_with_good_creds() {
        let (_srv, addr) = start_auth();

        // Missing credentials -> 401 with a Basic challenge.
        let none = post(&addr, "/sparql", SELECT_O);
        assert!(none.contains("401 Unauthorized"));
        assert!(none.contains("WWW-Authenticate: Basic"));

        // Wrong password -> 401.
        let bad = post_auth(&addr, "/sparql", SELECT_O, "user", "nope");
        assert!(bad.contains("401 Unauthorized"));

        // Correct credentials -> 200 with the answer.
        let ok = post_auth(&addr, "/sparql", SELECT_O, "user", "pass");
        assert!(ok.contains("200 OK"));
        assert!(ok.contains("http://ex/b"));
    }

    #[test]
    fn open_server_ignores_authorization_header() {
        // With no credentials configured the server stays open even if a client
        // happens to send (bogus) auth.
        let (_srv, addr) = start();
        let ok = post_auth(&addr, "/sparql", SELECT_O, "x", "y");
        assert!(ok.contains("200 OK"));
        assert!(ok.contains("http://ex/b"));
    }

    #[test]
    fn accept_xml_returns_parsable_xml() {
        let (_srv, addr) = start();
        let resp = post_accept(&addr, "/sparql", SELECT_O, "application/sparql-results+xml");
        let (head, body) = split_resp(&resp);
        assert!(head.contains("application/sparql-results+xml"));
        assert!(body.starts_with("<?xml version=\"1.0\"?>"));
        assert!(body.contains("<variable name=\"o\"/>"));
        assert!(body.contains("<binding name=\"o\"><uri>http://ex/b</uri></binding>"));
        assert!(body.trim_end().ends_with("</sparql>"));
    }

    #[test]
    fn accept_csv_returns_parsable_csv() {
        let (_srv, addr) = start();
        let resp = post_accept(&addr, "/sparql", SELECT_O, "text/csv");
        let (head, body) = split_resp(&resp);
        assert!(head.contains("text/csv"));
        let mut lines = body.split("\r\n");
        assert_eq!(lines.next().unwrap(), "o"); // header = bare var name
        assert_eq!(lines.next().unwrap(), "http://ex/b"); // IRI without <>
    }

    #[test]
    fn accept_tsv_returns_parsable_tsv() {
        let (_srv, addr) = start();
        let resp = post_accept(&addr, "/sparql", SELECT_O, "text/tab-separated-values");
        let (head, body) = split_resp(&resp);
        assert!(head.contains("text/tab-separated-values"));
        let mut lines = body.split('\n');
        assert_eq!(lines.next().unwrap(), "?o"); // header = ?var
        assert_eq!(lines.next().unwrap(), "<http://ex/b>"); // Turtle term
    }

    #[test]
    fn format_param_overrides_accept_and_default_is_json() {
        let (_srv, addr) = start();
        // `format=` wins over an Accept that asks for XML.
        let csv = post_accept(&addr, "/sparql?format=csv", SELECT_O, "application/sparql-results+xml");
        assert!(csv.contains("text/csv"));
        // `*/*` (and no format param) falls back to JSON.
        let json = post_accept(&addr, "/sparql", SELECT_O, "*/*");
        assert!(json.contains("application/sparql-results+json"));
        assert!(json.contains("\"type\":\"uri\""));
    }

    #[test]
    fn streaming_chunked_select_decodes() {
        let (_srv, addr) = start();
        // Two rows: (<p>,<b>) and (<name>,"Alice").
        let resp = post(
            &addr,
            "/sparql?stream=true&format=csv",
            "SELECT ?p ?o WHERE { <http://ex/a> ?p ?o }",
        );
        let (head, body) = split_resp(&resp);
        assert!(head.contains("Transfer-Encoding: chunked"));
        assert!(!head.contains("Content-Length"));
        assert!(head.contains("text/csv"));

        let decoded = decode_chunked(&body);
        let mut lines = decoded.lines();
        assert_eq!(lines.next().unwrap(), "p,o");
        let rows: Vec<&str> = lines.collect();
        assert!(rows.iter().any(|r| r.contains("http://ex/b")));
        assert!(rows.iter().any(|r| r.contains("Alice")));
    }

    // --- multi-threading: a slow connection must not block a second one ------

    #[test]
    fn concurrent_connections_are_not_serialized() {
        // Two clients connect at once; both must be served. This mainly proves
        // the accept loop spawns a worker per connection rather than handling
        // them strictly one-at-a-time on the accept thread.
        let (_srv, addr) = start();
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(thread::spawn(move || {
                let resp = post(&addr, "/sparql", SELECT_O);
                assert!(resp.contains("200 OK"), "{resp}");
                assert!(resp.contains("http://ex/b"), "{resp}");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    // --- gzip response compression (feature `gzip`) --------------------------

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_compresses_when_accepted() {
        use std::io::Read as _;
        let (_srv, addr) = start();
        let mut s = TcpStream::connect(addr).unwrap();
        let raw = format!(
            "POST /sparql HTTP/1.1\r\nHost: x\r\nAccept-Encoding: gzip\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            SELECT_O.len(),
            SELECT_O
        );
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).unwrap();
        let sep = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header/body separator");
        let head = String::from_utf8_lossy(&resp[..sep]).to_string();
        let body = &resp[sep + 4..];
        assert!(head.contains("Content-Encoding: gzip"), "headers: {head}");
        assert!(head.contains("Vary: Accept-Encoding"), "headers: {head}");
        // The body is real gzip: decode it and check the payload survived.
        let mut d = flate2::read::GzDecoder::new(body);
        let mut out = String::new();
        d.read_to_string(&mut out).unwrap();
        assert!(out.contains("http://ex/b"), "decompressed body: {out}");
    }

    #[cfg(not(feature = "gzip"))]
    #[test]
    fn without_gzip_feature_body_is_uncompressed() {
        // The default (dependency-free) build must ignore Accept-Encoding.
        let (_srv, addr) = start();
        let raw = format!(
            "POST /sparql HTTP/1.1\r\nHost: x\r\nAccept-Encoding: gzip\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            SELECT_O.len(),
            SELECT_O
        );
        let resp = http(&addr, &raw);
        assert!(!resp.contains("Content-Encoding"), "must not compress: {resp}");
        assert!(resp.contains("http://ex/b"), "{resp}");
    }

    // --- TLS (feature `tls`) -------------------------------------------------
    //
    // A self-signed EC end-entity cert (CN=localhost, SAN DNS:localhost + IP
    // 127.0.0.1, CA:FALSE, EKU serverAuth) generated with openssl. Being a leaf
    // (not a CA) it is valid as the server's presented cert, and the client
    // trusts it by pinning the same cert as a root.

    #[cfg(feature = "tls")]
    const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBvTCCAWOgAwIBAgIUKHclqAxNOvVOQ3kWyfFT+rCJUHwwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwMjIwMjkyMFoXDTM2MDYyOTIw
MjkyMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEYHEfrA2tZnM3sdpdua9aH0oSv8G2EmRrc10JlY7xgVO8+6VoyIJqtvH5
AxyPvu6UfMZMa+/UFLAmk+WCYEKCi6OBkjCBjzAdBgNVHQ4EFgQUJOZ8AYKC5S5i
QJhbdtMpHgmjlaowHwYDVR0jBBgwFoAUJOZ8AYKC5S5iQJhbdtMpHgmjlaowGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/
BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAoGCCqGSM49BAMCA0gAMEUCIQCo
ndoh3KosHMemvMMbjxtaNbbXmxGjfsgme3rBkLuziQIgFeUx/cU+Fpw/Dl5McuKP
cCWfronEM/ASghi4/9ldMUI=
-----END CERTIFICATE-----
";

    #[cfg(feature = "tls")]
    const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgUBOdoa9/azwl851s
NPlNYkOXVnBaUKf3fk8J38AOaYehRANCAARgcR+sDa1mczex2l25r1ofShK/wbYS
ZGtzXQmVjvGBU7z7pWjIgmq28fkDHI++7pR8xkxr79QUsCaT5YJgQoKL
-----END PRIVATE KEY-----
";

    #[cfg(feature = "tls")]
    #[test]
    fn tls_https_round_trip() {
        use std::io::{Read as _, Write as _};
        // Write the embedded cert+key to temp files and load them through the
        // crate's real `load_tls_config` path.
        let dir = std::env::temp_dir().join(format!("gstore_tls_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, CERT_PEM).unwrap();
        std::fs::write(&key, KEY_PEM).unwrap();
        let config = load_tls_config(&cert, &key).expect("load_tls_config");

        // Serve HTTPS on an ephemeral port.
        let db = Database::build_from_str("tls", DATA).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let cfg = Arc::clone(&config);
        thread::spawn(move || s2.serve_forever_tls(cfg));

        // A rustls client that trusts the self-signed cert (added as a root).
        let mut roots = rustls::RootCertStore::empty();
        let cert_der = rustls_pemfile::certs(&mut std::io::BufReader::new(CERT_PEM.as_bytes()))
            .next()
            .unwrap()
            .unwrap();
        roots.add(cert_der).unwrap();
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        let sock = TcpStream::connect(addr).unwrap();
        let mut tls = rustls::StreamOwned::new(conn, sock);

        let raw = format!(
            "POST /sparql HTTP/1.1\r\nHost: localhost\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            SELECT_O.len(),
            SELECT_O
        );
        tls.write_all(raw.as_bytes()).unwrap();
        tls.flush().unwrap();
        let mut resp = Vec::new();
        // The server drops the connection without a TLS close_notify, which
        // rustls reports as an EOF error *after* delivering all application
        // data; ignore it and assert on what was decrypted.
        let _ = tls.read_to_end(&mut resp);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200 OK"), "TLS response: {text}");
        assert!(text.contains("http://ex/b"), "TLS response: {text}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
