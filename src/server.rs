//! A minimal HTTP SPARQL endpoint over a [`Database`] — the analogue of gStore's
//! `src/Server` / `ghttp`.
//!
//! This is a dependency-free, blocking HTTP/1.1 server (connections are served
//! by a bounded worker pool, with keep-alive) implementing a useful subset of
//! the SPARQL 1.1 Protocol:
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
//! concurrently (on the worker pool), but the [`Database`] is shared behind a
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
//! **Connections** (shared with [`ApiServer`](crate::http_api::ApiServer)):
//!
//! * **Bounded worker pool** — `thread_num` worker threads (default
//!   [`DEFAULT_THREAD_NUM`], gStore's `thread_num`) serve connections; accepted
//!   connections beyond that wait in a bounded queue, and when the queue is
//!   full the accept loop stops accepting (backpressure: the kernel backlog
//!   holds further clients) instead of spawning without limit.
//! * **HTTP/1.1 keep-alive** — a connection serves up to
//!   `keep_alive_max_requests` requests, idling at most `keep_alive_timeout`
//!   between them (`Connection: close`, HTTP/1.0 without `keep-alive`, a
//!   streamed response, or a shutdown end it). An idle keep-alive connection
//!   gives its worker up as soon as other connections are queued.
//! * **Request limits** — the body is capped (default 256 MiB; `gserver` uses
//!   conf.ini's upload limit) and answered `413` beyond it; header lines and
//!   counts are bounded. `Expect: 100-continue` is honoured and chunked request
//!   bodies are decoded.
//! * **CORS** — `OPTIONS` preflights are answered before auth. Responses and
//!   preflights on `/file/*` carry `Access-Control-Allow-Origin: *` (as
//!   upstream gStore); with a configured origin
//!   ([`Server::with_cors_origin`]) every response carries it.
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

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::GStoreConfig;
use crate::db::{Database, LoadPolicy};
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
///
/// **`LOAD`.** The served database gets [`LoadPolicy::restricted`]: a client's
/// SPARQL `LOAD` cannot make the server read local files (or anything else).
/// Re-allow a directory with [`with_load_policy`](Self::with_load_policy).
pub struct Server {
    /// The shared, thread-safe part handed to each connection's worker thread.
    inner: Arc<ServerInner>,
    /// The listening socket; taken (closed) once the server has shut down.
    listener: Mutex<Option<TcpListener>>,
    addr: SocketAddr,
    life: Arc<Lifecycle>,
    /// Upper bound on how long a graceful shutdown waits for in-flight requests.
    grace: Duration,
    /// Worker pool, keep-alive, body cap and CORS settings.
    http: HttpOptions,
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
    /// Set when a graceful shutdown begins: from then on every update saves
    /// itself before it is acknowledged, whatever the check_point policy — so
    /// an update still in flight when the shutdown's final save runs (or that
    /// only gets the lock afterwards) is never acknowledged and then lost.
    closing: AtomicBool,
}

impl Server {
    /// Bind to `addr` (e.g. `"127.0.0.1:7000"` or `"127.0.0.1:0"` for an
    /// ephemeral port), taking ownership of `db`. The server is in-memory only
    /// until a directory is attached with [`with_db_dir`](Self::with_db_dir).
    /// The database's `LOAD` policy is set to [`LoadPolicy::restricted`].
    pub fn bind<A: ToSocketAddrs>(mut db: Database, addr: A) -> std::io::Result<Server> {
        db.set_load_policy(LoadPolicy::restricted());
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        Ok(Server {
            inner: Arc::new(ServerInner {
                db: Mutex::new(db),
                dirty: AtomicBool::new(false),
                auth: None,
                dir: None,
                check_point: true,
                closing: AtomicBool::new(false),
            }),
            listener: Mutex::new(Some(listener)),
            addr,
            life: Lifecycle::new(addr),
            grace: DEFAULT_SHUTDOWN_GRACE,
            http: HttpOptions::default(),
        })
    }

    /// Apply the HTTP-layer settings of a `conf.ini` ([`GStoreConfig`]):
    /// `thread_num`, keep-alive, the request body cap, CORS, the shutdown
    /// grace period and `check_point`.
    pub fn with_config(self, cfg: &GStoreConfig) -> Server {
        let mut s = self.with_check_point(cfg.check_point);
        s.http = HttpOptions::from_config(cfg);
        s.grace = Duration::from_secs(cfg.shutdown_grace);
        s
    }

    /// Size the worker pool serving connections (default
    /// [`DEFAULT_THREAD_NUM`]; at least 1). Connections beyond it queue.
    pub fn with_thread_num(mut self, n: usize) -> Server {
        self.http.threads = n.max(1);
        self
    }

    /// HTTP/1.1 keep-alive: keep an idle connection open for `idle` between
    /// requests and serve at most `max_requests` on it. `None` (or
    /// `max_requests <= 1`) closes every connection after one response.
    pub fn with_keep_alive(mut self, idle: Option<Duration>, max_requests: usize) -> Server {
        self.http.keep_alive = idle.filter(|_| max_requests > 1);
        self.http.keep_alive_max = max_requests.max(1);
        self
    }

    /// Cap request bodies at `bytes` (default 256 MiB); larger ones are
    /// answered `413 Payload Too Large`.
    pub fn with_max_body_size(mut self, bytes: usize) -> Server {
        self.http.max_body = bytes;
        self
    }

    /// Send `Access-Control-Allow-Origin: origin` on every response and answer
    /// CORS preflights on every path (default: only `/file/*`, as upstream).
    pub fn with_cors_origin(mut self, origin: Option<&str>) -> Server {
        self.http.cors_origin = origin.map(str::to_string);
        self
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

    /// Replace the SPARQL `LOAD` policy of the served database (the default is
    /// [`LoadPolicy::restricted`]), e.g. to let `LOAD` read one directory with
    /// `LoadPolicy::restricted().with_allowed_dir(dir)`.
    pub fn with_load_policy(self, policy: LoadPolicy) -> Server {
        locked(&self.inner.db).set_load_policy(policy);
        self
    }

    /// Turn gStore's `check_point` policy on (the default) or off. When off,
    /// updates stay in memory and are saved only on graceful shutdown (an
    /// on-disk / RocksDB database still commits each update it writes through).
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
        let inner = Arc::clone(&self.inner);
        let drained = serve_pooled(
            &self.listener,
            &self.life,
            self.grace,
            &self.http,
            Some,
            move |s: &mut TcpStream, req: &Request, conn: &ConnHeaders| {
                inner.handle_request(s, req, conn)
            },
        );
        self.finish_shutdown(drained)
    }

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS
    /// in-process, wrapping every accepted connection in a `rustls` stream built
    /// from `config` (see [`load_tls_config`]). Requires the `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        let inner = Arc::clone(&self.inner);
        let drained = serve_pooled(
            &self.listener,
            &self.life,
            self.grace,
            &self.http,
            move |stream| tls_wrap(&config, stream),
            move |s: &mut TlsConn, req: &Request, conn: &ConnHeaders| {
                inner.handle_request(s, req, conn)
            },
        );
        self.finish_shutdown(drained)
    }

    /// The tail of a graceful shutdown: save unsaved changes (if a directory is
    /// attached) and report what happened.
    ///
    /// The `closing` flag is raised first. An update that applies its change
    /// after that saves it itself (see [`ServerInner::execute`]); one that
    /// applied it before has set `dirty` first. Both are sequentially
    /// consistent, so at least one side sees the other: either this save or
    /// the update's own covers every change — while a shutdown with nothing
    /// unsaved still never waits for the database lock.
    fn finish_shutdown(&self, drained: bool) -> ShutdownReport {
        let mut report = ShutdownReport {
            drained,
            restart: self.life.restart_requested(),
            ..ShutdownReport::default()
        };
        self.inner.closing.store(true, Ordering::SeqCst);
        if self.inner.dirty.load(Ordering::SeqCst) {
            let db = locked(&self.inner.db);
            // Re-checked under the lock: the update may have saved itself.
            if self.inner.dirty.load(Ordering::SeqCst) {
                match self.inner.save_locked(&db) {
                    Ok(Some(dir)) => report.saved.push(dir),
                    Ok(None) => {}
                    Err((dir, e)) => report.failed.push((dir, e)),
                }
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
    /// The shutdown was a restart request ([`ShutdownHandle::restart`], e.g.
    /// the `/restart` route): the embedding process should start the server
    /// again (`gserver` re-executes itself).
    pub restart: bool,
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

    /// Request a graceful shutdown that the embedder should follow with a
    /// fresh start: the [`ShutdownReport`] carries `restart: true`.
    pub fn restart(&self) {
        self.life.request_restart();
    }

    /// Whether a restart (rather than a plain shutdown) was requested.
    pub fn restart_requested(&self) -> bool {
        self.life.restart_requested()
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
    /// Set (before `stop`) when the shutdown is a restart request.
    restart: AtomicBool,
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
            restart: AtomicBool::new(false),
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

    /// Flag a restart, then request the (graceful) stop.
    pub(crate) fn request_restart(&self) {
        self.restart.store(true, Ordering::SeqCst);
        self.request_stop();
    }

    pub(crate) fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::SeqCst)
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
    on_termination_signal(move || handle.shutdown())
}

/// Run `f` (once, on a watcher thread) when the process receives `SIGINT` or
/// `SIGTERM` (unix only; returns `false` — and does nothing — elsewhere or if
/// the handlers could not be installed). After the first signal the default
/// disposition is restored, so a second one terminates the process. This is
/// the building block of [`shutdown_on_signals`]; `gserver`'s supervisor uses
/// it to forward a stop to the server it supervises.
pub fn on_termination_signal(f: impl FnOnce() + Send + 'static) -> bool {
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
                    f();
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            })
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = f;
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

/// Read timeout applied to every accepted connection while a request is being
/// read: a client that opens a socket and then sends nothing (a slowloris)
/// times out instead of pinning its pool worker and fd forever.
pub(crate) const CONN_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn a detached background thread that can never take down its caller: a
/// failed spawn (e.g. the OS thread limit) just drops the work. Connections are
/// served by the bounded [`WorkerPool`]; this remains for background jobs.
#[allow(dead_code)]
pub(crate) fn spawn_worker<F: FnOnce() + Send + 'static>(f: F) {
    let _ = thread::Builder::new().spawn(f);
}

// --- connection handling: worker pool, keep-alive, CORS -----------------------

/// Default size of the connection worker pool (gStore's `thread_num` default).
pub const DEFAULT_THREAD_NUM: usize = 30;

/// Default idle time a keep-alive connection is kept open between requests.
pub const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Default number of requests served on one keep-alive connection.
pub const DEFAULT_KEEP_ALIVE_MAX: usize = 100;

/// Longest accepted request-line or header line (a GET may carry a long,
/// URL-encoded SPARQL query).
const MAX_HEADER_LINE: u64 = 1 << 20;

/// Most header lines accepted in one request.
const MAX_HEADERS: usize = 256;

/// How often an idle keep-alive connection re-checks for a shutdown or for
/// queued connections that need its worker.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// HTTP-layer settings shared by [`Server`] and
/// [`ApiServer`](crate::http_api::ApiServer).
#[derive(Debug, Clone)]
pub(crate) struct HttpOptions {
    /// Worker threads serving connections.
    pub(crate) threads: usize,
    /// Keep-alive idle timeout; `None` disables keep-alive.
    pub(crate) keep_alive: Option<Duration>,
    /// Requests served per connection.
    pub(crate) keep_alive_max: usize,
    /// Largest accepted request body.
    pub(crate) max_body: usize,
    /// `Access-Control-Allow-Origin` for every response (else `/file/*` only).
    pub(crate) cors_origin: Option<String>,
}

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            threads: DEFAULT_THREAD_NUM,
            keep_alive: Some(DEFAULT_KEEP_ALIVE),
            keep_alive_max: DEFAULT_KEEP_ALIVE_MAX,
            max_body: MAX_REQUEST_BODY,
            cors_origin: None,
        }
    }
}

impl HttpOptions {
    pub(crate) fn from_config(cfg: &GStoreConfig) -> HttpOptions {
        HttpOptions {
            threads: cfg.thread_num.max(1),
            keep_alive: cfg.keep_alive(),
            keep_alive_max: cfg.keep_alive_max_requests.max(1),
            max_body: cfg.request_body_limit(),
            cors_origin: cfg.cors_allow_origin.clone(),
        }
    }

    /// Connections allowed to wait for a worker before accepting pauses.
    fn queue_capacity(&self) -> usize {
        (self.threads * 4).max(16)
    }
}

/// A connection transport: plain TCP or TLS over TCP. Exposes the socket so
/// the connection loop can adjust read timeouts and learn the peer address.
pub(crate) trait Conn: Read + Write {
    fn tcp(&self) -> &TcpStream;
}

impl Conn for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

/// A server-side TLS connection (feature `tls`).
#[cfg(feature = "tls")]
pub(crate) type TlsConn = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

#[cfg(feature = "tls")]
impl Conn for TlsConn {
    fn tcp(&self) -> &TcpStream {
        &self.sock
    }
}

/// Wrap an accepted socket in a server-side TLS session.
#[cfg(feature = "tls")]
pub(crate) fn tls_wrap(config: &Arc<rustls::ServerConfig>, stream: TcpStream) -> Option<TlsConn> {
    rustls::ServerConnection::new(Arc::clone(config))
        .ok()
        .map(|c| rustls::StreamOwned::new(c, stream))
}

/// The connection-level headers of one response: `Connection` (with
/// `Keep-Alive` when the connection stays open) and the CORS origin. Built by
/// the connection loop; a handler appends [`headers`](Self::headers) to its
/// own and reports [`keeps_alive`](Self::keeps_alive) back.
pub(crate) struct ConnHeaders {
    /// `Some("timeout=…, max=…")` when the connection may serve another request.
    keep_alive: Option<String>,
    cors: Option<String>,
    life: Arc<Lifecycle>,
}

impl ConnHeaders {
    /// Whether the connection stays open after this response. Re-checks the
    /// shutdown flag, so a response written after a shutdown began (e.g. by the
    /// `/shutdown` route itself) closes its connection.
    pub(crate) fn keeps_alive(&self) -> bool {
        self.keep_alive.is_some() && !self.life.stopping()
    }

    /// The headers to add to a response that honours [`keeps_alive`](Self::keeps_alive).
    pub(crate) fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = Vec::with_capacity(3);
        match &self.keep_alive {
            Some(ka) if !self.life.stopping() => {
                h.push(("Connection", "keep-alive".to_string()));
                h.push(("Keep-Alive", ka.clone()));
            }
            _ => h.push(("Connection", "close".to_string())),
        }
        if let Some(o) = &self.cors {
            h.push(("Access-Control-Allow-Origin", o.clone()));
        }
        h
    }

    /// The headers of a response after which the connection closes anyway
    /// (a streamed body, an error mid-request): `Connection: close` + CORS.
    pub(crate) fn closing_headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![("Connection", "close".to_string())];
        if let Some(o) = &self.cors {
            h.push(("Access-Control-Allow-Origin", o.clone()));
        }
        h
    }
}

/// Borrow `(name, owned value)` header pairs as `(&str, &str)`, after `extra`.
pub(crate) fn merge_headers<'a>(
    extra: &[(&'a str, &'a str)],
    conn: &'a [(&'static str, String)],
) -> Vec<(&'a str, &'a str)> {
    let mut all = extra.to_vec();
    all.extend(conn.iter().map(|(k, v)| (*k, v.as_str())));
    all
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A pool of at most `max_threads` worker threads fed from a bounded FIFO
/// queue. Threads are started on demand — when a job arrives and no worker is
/// idle — so an idle server holds none beyond those it once needed.
pub(crate) struct WorkerPool {
    shared: Arc<PoolShared>,
}

struct PoolShared {
    state: Mutex<PoolState>,
    /// Signalled when a job is queued or the pool closes.
    work: Condvar,
    /// Signalled when a queued job is taken (room in the queue).
    room: Condvar,
    capacity: usize,
    max_threads: usize,
}

struct PoolState {
    jobs: VecDeque<Job>,
    closed: bool,
    /// Worker threads started (and not exited).
    workers: usize,
    /// Started workers that have not yet looked at the queue.
    starting: usize,
    /// Workers waiting for work.
    idle: usize,
    /// Wake-ups handed to idle workers and not yet consumed: `idle - wakeups`
    /// workers are free for a new job.
    wakeups: usize,
}

impl WorkerPool {
    pub(crate) fn new(max_threads: usize, capacity: usize) -> WorkerPool {
        WorkerPool {
            shared: Arc::new(PoolShared {
                state: Mutex::new(PoolState {
                    jobs: VecDeque::new(),
                    closed: false,
                    workers: 0,
                    starting: 0,
                    idle: 0,
                    wakeups: 0,
                }),
                work: Condvar::new(),
                room: Condvar::new(),
                capacity: capacity.max(1),
                max_threads: max_threads.max(1),
            }),
        }
    }

    /// Queue `job`, waiting while the queue is full (backpressure). Gives up —
    /// dropping the job — once `give_up()` turns true while waiting. Returns
    /// whether the job was queued (or run).
    pub(crate) fn submit(&self, job: Job, give_up: impl Fn() -> bool) -> bool {
        let sh = &self.shared;
        let mut st = locked(&sh.state);
        while st.jobs.len() >= sh.capacity && !st.closed {
            if give_up() {
                return false;
            }
            st = match sh.room.wait_timeout(st, Duration::from_millis(50)) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        if st.closed {
            return false;
        }
        st.jobs.push_back(job);
        if st.idle > st.wakeups {
            st.wakeups += 1;
            drop(st);
            sh.work.notify_one();
            return true;
        }
        if st.workers >= sh.max_threads {
            return true; // every worker is busy: the job waits its turn
        }
        st.workers += 1;
        st.starting += 1;
        let n = st.workers;
        drop(st);
        let worker = Arc::clone(sh);
        let spawned = thread::Builder::new()
            .name(format!("gstore-http-{n}"))
            .spawn(move || worker_loop(&worker));
        if spawned.is_err() {
            // E.g. the OS thread limit. With no worker at all, run the queue
            // here rather than strand it.
            let mut st = locked(&sh.state);
            st.workers -= 1;
            st.starting -= 1;
            if st.workers == 0 {
                let jobs: Vec<Job> = st.jobs.drain(..).collect();
                drop(st);
                sh.room.notify_all();
                for j in jobs {
                    let _ = catch_unwind(AssertUnwindSafe(j));
                }
            }
        }
        true
    }

    /// A cheap handle reporting how many jobs wait for a worker.
    pub(crate) fn queue_probe(&self) -> QueueProbe {
        QueueProbe(Arc::clone(&self.shared))
    }

    /// Stop taking jobs; workers exit once the queue is drained.
    pub(crate) fn close(&self) {
        locked(&self.shared.state).closed = true;
        self.shared.work.notify_all();
        self.shared.room.notify_all();
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reports the number of connections waiting for a pool worker.
#[derive(Clone)]
pub(crate) struct QueueProbe(Arc<PoolShared>);

impl QueueProbe {
    /// Queued jobs no worker is on its way to take.
    pub(crate) fn queued(&self) -> usize {
        let st = locked(&self.0.state);
        st.jobs.len().saturating_sub(st.wakeups + st.starting)
    }
}

fn worker_loop(shared: &PoolShared) {
    let mut st = locked(&shared.state);
    st.starting -= 1;
    loop {
        if let Some(job) = st.jobs.pop_front() {
            drop(st);
            shared.room.notify_one();
            // A panicking connection must not take its worker down.
            let _ = catch_unwind(AssertUnwindSafe(job));
            st = locked(&shared.state);
            continue;
        }
        if st.closed {
            st.workers -= 1;
            return;
        }
        st.idle += 1;
        while st.wakeups == 0 && !st.closed {
            st = shared.work.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.idle -= 1;
        if st.wakeups > 0 {
            st.wakeups -= 1;
        }
    }
}

/// Accept connections until a shutdown, serving each on the bounded worker
/// pool (sized by `http.threads`) with [`serve_connection`]: `wrap` turns the
/// socket into the transport (identity, or a TLS session) and `handler`
/// answers one parsed request. Returns whether in-flight connections drained
/// within `grace` (see [`run_accept_loop`]).
pub(crate) fn serve_pooled<S, W, H>(
    listener: &Mutex<Option<TcpListener>>,
    life: &Arc<Lifecycle>,
    grace: Duration,
    http: &HttpOptions,
    wrap: W,
    handler: H,
) -> bool
where
    S: Conn + 'static,
    W: Fn(TcpStream) -> Option<S> + Send + Sync + 'static,
    H: Fn(&mut S, &Request, &ConnHeaders) -> io::Result<bool> + Send + Sync + 'static,
{
    let pool = WorkerPool::new(http.threads, http.queue_capacity());
    let probe = pool.queue_probe();
    let http = Arc::new(http.clone());
    let wrap = Arc::new(wrap);
    let handler = Arc::new(handler);
    let drained = run_accept_loop(listener, life, grace, |stream, guard| {
        let (life2, http, wrap, handler, probe) = (
            Arc::clone(life),
            Arc::clone(&http),
            Arc::clone(&wrap),
            Arc::clone(&handler),
            probe.clone(),
        );
        let job: Job = Box::new(move || {
            let _in_flight = guard;
            if let Some(conn) = wrap(stream) {
                serve_connection(conn, &http, &life2, &|| probe.queued(), &*handler);
            }
        });
        // Dropping the job (when the stop arrives while the queue is full)
        // closes the connection and releases its in-flight guard.
        pool.submit(job, || life.stopping());
    });
    pool.close();
    drained
}

/// Serve one connection: read requests one after another (HTTP/1.1
/// keep-alive), answer `OPTIONS` preflights, and hand every other request to
/// `handler`, which writes the response and returns whether the connection may
/// stay open. Malformed or oversized requests are answered `400` / `413` and
/// end the connection.
pub(crate) fn serve_connection<S: Conn>(
    stream: S,
    http: &HttpOptions,
    life: &Arc<Lifecycle>,
    queued: &dyn Fn() -> usize,
    handler: &dyn Fn(&mut S, &Request, &ConnHeaders) -> io::Result<bool>,
) {
    let peer = stream.tcp().peer_addr().ok();
    let _ = stream.tcp().set_read_timeout(Some(CONN_READ_TIMEOUT));
    let mut reader = BufReader::new(stream);
    let mut served = 0usize;
    loop {
        if served > 0 {
            let idle = http.keep_alive.unwrap_or(Duration::ZERO);
            if !await_next_request(&mut reader, idle, life, queued) {
                return;
            }
        }
        let mut req = match Request::read_head(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(_) => {
                let _ = write_response(reader.get_mut(), 400, "text/plain", b"bad request");
                return;
            }
        };
        req.peer = peer;
        let cors = cors_origin_for(http, &req.path);
        let cors_hdr: Vec<(&str, &str)> = cors
            .as_deref()
            .map(|o| vec![("Access-Control-Allow-Origin", o)])
            .unwrap_or_default();
        if req.declared_body_len().is_some_and(|n| n > http.max_body) {
            let msg = format!(
                "request body exceeds the maximum size ({} bytes)",
                http.max_body
            );
            let _ = write_response_ext(reader.get_mut(), 413, "text/plain", &cors_hdr, msg.as_bytes());
            return;
        }
        if req.expects_continue() {
            let w = reader.get_mut();
            if w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .and_then(|_| w.flush())
                .is_err()
            {
                return;
            }
        }
        if let Err(e) = req.read_body(&mut reader, http.max_body) {
            let (status, msg) = if e.kind() == io::ErrorKind::InvalidData {
                (413, e.to_string())
            } else {
                (400, "bad request".to_string())
            };
            let _ = write_response_ext(reader.get_mut(), status, "text/plain", &cors_hdr, msg.as_bytes());
            return;
        }
        served += 1;
        let keep_alive = (http.keep_alive.is_some()
            && served < http.keep_alive_max
            && req.wants_keep_alive()
            && !life.stopping())
        .then(|| {
            format!(
                "timeout={}, max={}",
                http.keep_alive.unwrap_or_default().as_secs(),
                http.keep_alive_max - served
            )
        });
        let conn = ConnHeaders {
            keep_alive,
            cors,
            life: Arc::clone(life),
        };
        let keep = if req.method == "OPTIONS" {
            write_preflight(reader.get_mut(), &req, http, &conn).map(|_| conn.keeps_alive())
        } else {
            handler(reader.get_mut(), &req, &conn)
        };
        if !matches!(keep, Ok(true)) {
            return;
        }
    }
}

/// The CORS origin for a response on `path`: the configured one, else `*` on
/// upstream's `/file/*` routes.
fn cors_origin_for(http: &HttpOptions, path: &str) -> Option<String> {
    http.cors_origin
        .clone()
        .or_else(|| path.starts_with("/file/").then(|| "*".to_string()))
}

/// Answer an `OPTIONS` request: a CORS preflight when CORS applies to the path
/// (upstream answers `/file/upload` and `/file/download/*` with
/// `Access-Control-Allow-Origin: *` and body `ok`), else a plain `204` with
/// `Allow`. Runs before authentication — preflights carry no credentials.
fn write_preflight<W: Write>(
    w: &mut W,
    req: &Request,
    http: &HttpOptions,
    conn: &ConnHeaders,
) -> io::Result<()> {
    let hdrs = conn.headers();
    if conn.cors.is_none() {
        let extra = merge_headers(&[("Allow", "GET, POST, OPTIONS")], &hdrs);
        return write_response_ext(w, 204, "text/plain", &extra, b"");
    }
    let methods = if http.cors_origin.is_some() {
        "GET, POST, OPTIONS"
    } else if req.path.starts_with("/file/download") {
        "GET"
    } else {
        "POST"
    };
    let allow_headers = req
        .header("access-control-request-headers")
        .unwrap_or("Content-Type, Authorization")
        .to_string();
    let extra = merge_headers(
        &[
            ("Access-Control-Allow-Methods", methods),
            ("Access-Control-Allow-Headers", &allow_headers),
            ("Access-Control-Max-Age", "86400"),
        ],
        &hdrs,
    );
    write_response_ext(w, 200, "text/plain", &extra, b"ok")
}

/// Wait (at most `idle`) for the first byte of the next request on a
/// keep-alive connection. Gives up — so the connection closes and its worker
/// is freed — on EOF, on a shutdown, or as soon as other connections are
/// queued for a worker.
fn await_next_request<S: Conn>(
    reader: &mut BufReader<S>,
    idle: Duration,
    life: &Lifecycle,
    queued: &dyn Fn() -> usize,
) -> bool {
    if !reader.buffer().is_empty() {
        return true; // a pipelined request is already buffered
    }
    let deadline = Instant::now() + idle;
    let ready = loop {
        if life.stopping() || queued() > 0 {
            break false;
        }
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        let slice = (deadline - now).min(IDLE_POLL);
        if reader.get_ref().tcp().set_read_timeout(Some(slice)).is_err() {
            break false;
        }
        match reader.fill_buf() {
            Ok(buf) => break !buf.is_empty(),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => break false,
        }
    };
    let _ = reader.get_ref().tcp().set_read_timeout(Some(CONN_READ_TIMEOUT));
    ready
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
    /// Answer one parsed request on its connection. Generic over the
    /// transport so the same code path serves plain [`TcpStream`]s and
    /// `rustls` TLS streams. Routing runs under `catch_unwind`: a panicking
    /// handler answers `500` and leaves the server (and its recovered locks)
    /// serving. Returns whether the connection may serve another request.
    fn handle_request<W: Write>(
        &self,
        stream: &mut W,
        req: &Request,
        conn: &ConnHeaders,
    ) -> std::io::Result<bool> {
        let conn_hdrs = conn.headers();
        // Auth gate: when credentials are configured every request must present
        // a matching `Authorization: Basic` header.
        if let Some((user, pass)) = &self.auth {
            if !req.authorized(user, pass) {
                let extra = merge_headers(
                    &[("WWW-Authenticate", "Basic realm=\"gStore\"")],
                    &conn_hdrs,
                );
                write_response_ext(stream, 401, "text/plain", &extra, b"unauthorized")?;
                return Ok(conn.keeps_alive());
            }
        }
        let reply = catch_unwind(AssertUnwindSafe(|| self.route(req)))
            .unwrap_or_else(|_| Reply::plain(500, "application/json", &err_json(PANIC_MSG)));
        // The client's `Accept-Encoding` (used to optionally compress buffered
        // bodies under the `gzip` feature; ignored without it).
        let enc = req.header("accept-encoding");
        let extra = merge_headers(&[], &conn_hdrs);
        match reply {
            Reply::Buffered {
                status,
                ctype,
                body,
                compress,
            } => {
                let enc = if compress { enc } else { None };
                write_response_maybe_gzip(stream, status, &ctype, &extra, &body, enc)?;
                Ok(conn.keeps_alive())
            }
            Reply::Stream(fmt, rs) => {
                let closing = conn.closing_headers();
                stream_select(stream, fmt, &rs, &merge_headers(&[], &closing))?;
                Ok(false)
            }
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
    /// the database dirty and — under the check_point policy, or once a
    /// shutdown has begun — saves it before the lock is released, so the
    /// on-disk copy never lags an acknowledged write. That includes an UPDATE
    /// that failed part-way: its operations before the failing one stay
    /// applied (and visible), so they are saved too. A failed save does not
    /// undo the (applied) update: it is reported as a warning and the database
    /// stays dirty, to be retried on shutdown.
    fn execute(&self, sparql: &str) -> (crate::Result<QueryResult>, Option<String>) {
        let mut db = locked(&self.db);
        let before = db.mutation_count();
        let mut result = db.query(sparql);
        let mut warning = None;
        if db.mutation_count() != before {
            self.dirty.store(true, Ordering::SeqCst);
            if self.check_point || self.closing.load(Ordering::SeqCst) {
                if let Err((dir, e)) = self.save_locked(&db) {
                    warning = Some(format!(
                        "update applied in memory but saving to '{}' failed: {e}",
                        dir.display()
                    ));
                }
            }
            if let Err(e) = &result {
                let note = match &warning {
                    Some(w) => format!("operations before the failing one were applied; {w}"),
                    None => "operations before the failing one were applied".to_string(),
                };
                result = Err(crate::GStoreError::Query(format!("{e} ({note})")));
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
    /// The protocol of the request line (`HTTP/1.1`, `HTTP/1.0`, …).
    pub(crate) version: String,
    /// The client's address (set by the connection loop).
    pub(crate) peer: Option<SocketAddr>,
}

impl Request {
    /// Read one whole request (head and body, capped at the default 256 MiB)
    /// from a stream that carries nothing else.
    #[cfg(test)]
    pub(crate) fn parse<R: Read>(stream: &mut R) -> std::io::Result<Option<Request>> {
        let mut reader = BufReader::new(stream);
        let Some(mut req) = Request::read_head(&mut reader)? else {
            return Ok(None);
        };
        req.read_body(&mut reader, MAX_REQUEST_BODY)?;
        Ok(Some(req))
    }

    /// Read the request line and headers (not the body: see
    /// [`read_body`](Self::read_body)). `Ok(None)` when the connection closed
    /// before a request started. Over-long lines and too many headers are
    /// `InvalidData` errors.
    pub(crate) fn read_head<B: BufRead>(reader: &mut B) -> std::io::Result<Option<Request>> {
        let mut line = String::new();
        // Tolerate empty lines before the request line (RFC 9112 §2.2).
        loop {
            line.clear();
            if read_limited_line(reader, &mut line)? == 0 {
                return Ok(None);
            }
            if !line.trim().is_empty() {
                break;
            }
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let version = parts.next().unwrap_or("HTTP/1.0").to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (target, None),
        };

        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if read_limited_line(reader, &mut h)? == 0 {
                break;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break; // end of headers
            }
            if headers.len() >= MAX_HEADERS {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "too many headers"));
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        Ok(Some(Request {
            method,
            path,
            query,
            headers,
            body: Vec::new(),
            version,
            peer: None,
        }))
    }

    /// Whether the body is sent with `Transfer-Encoding: chunked`.
    fn is_chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    }

    /// The body length announced by `Content-Length` (`None` for a chunked
    /// body, whose length is only known once read).
    pub(crate) fn declared_body_len(&self) -> Option<usize> {
        if self.is_chunked() {
            return None;
        }
        Some(
            self.header("content-length")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0),
        )
    }

    /// Read the body announced by the head: `Content-Length` bytes, or a
    /// `chunked` body (decoded). A body larger than `max` is refused with an
    /// `InvalidData` error *before* it is buffered, so a client cannot make the
    /// server reserve arbitrary memory (this runs before authentication). The
    /// buffer grows with the bytes actually received, not the announced length.
    pub(crate) fn read_body<B: BufRead>(&mut self, reader: &mut B, max: usize) -> io::Result<()> {
        let too_large = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request body exceeds the maximum size ({max} bytes)"),
            )
        };
        let mut body = Vec::new();
        if self.is_chunked() {
            let mut line = String::new();
            loop {
                line.clear();
                if read_limited_line(reader, &mut line)? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                let size_field = line.trim().split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_field, 16).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "bad chunk size")
                })?;
                if size == 0 {
                    // Trailer fields until the blank line.
                    loop {
                        line.clear();
                        if read_limited_line(reader, &mut line)? == 0 || line.trim().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                if body.len().saturating_add(size) > max {
                    return Err(too_large());
                }
                let got = reader.by_ref().take(size as u64).read_to_end(&mut body)?;
                if got < size {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                line.clear();
                read_limited_line(reader, &mut line)?;
                if !line.trim().is_empty() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad chunk framing"));
                }
            }
        } else {
            let len = match self.header("content-length") {
                Some(v) => v.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "bad Content-Length")
                })?,
                None => 0,
            };
            if len > max {
                return Err(too_large());
            }
            body.reserve(len.min(1 << 20));
            let got = reader.by_ref().take(len as u64).read_to_end(&mut body)?;
            if got < len {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        self.body = body;
        Ok(())
    }

    /// Whether the client waits for `100 Continue` before sending its body.
    pub(crate) fn expects_continue(&self) -> bool {
        self.version != "HTTP/1.0"
            && self
                .header("expect")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("100-continue"))
            && (self.is_chunked() || self.declared_body_len().unwrap_or(0) > 0)
    }

    /// Whether the client asks to keep the connection open: HTTP/1.1 unless
    /// `Connection: close`; HTTP/1.0 only with `Connection: keep-alive`.
    pub(crate) fn wants_keep_alive(&self) -> bool {
        let tokens: Vec<String> = self
            .header("connection")
            .map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()).collect())
            .unwrap_or_default();
        if tokens.iter().any(|t| t == "close") {
            return false;
        }
        if self.version.eq_ignore_ascii_case("HTTP/1.1") || self.version.starts_with("HTTP/2") {
            return true;
        }
        tokens.iter().any(|t| t == "keep-alive")
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
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for &(k, v) in extra {
        header.push_str(&format!("{k}: {v}\r\n"));
    }
    // Close after the response unless the caller (the keep-alive connection
    // loop, via `ConnHeaders`) chose the `Connection` header itself.
    if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("connection")) {
        header.push_str("Connection: close\r\n");
    }
    header.push_str("\r\n");
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
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\n",
        fmt.content_type()
    );
    for &(k, v) in extra {
        header.push_str(&format!("{k}: {v}\r\n"));
    }
    if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("connection")) {
        header.push_str("Connection: close\r\n");
    }
    header.push_str("\r\n");
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
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Read one line (at most [`MAX_HEADER_LINE`] bytes) into `buf`; a longer
/// line is an `InvalidData` error rather than unbounded buffering.
fn read_limited_line<B: BufRead>(reader: &mut B, buf: &mut String) -> io::Result<usize> {
    let n = reader.by_ref().take(MAX_HEADER_LINE).read_line(buf)?;
    if n as u64 == MAX_HEADER_LINE && !buf.ends_with('\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "header line too long"));
    }
    Ok(n)
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

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gstore_server_unit_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn ask(dir: &std::path::Path, q: &str) -> bool {
        Database::open(dir).unwrap().query(q).unwrap() == QueryResult::Ask(true)
    }

    /// An UPDATE that fails part-way leaves its earlier operations applied, so
    /// the database is dirty and saved under check_point like after any write.
    #[test]
    fn a_partially_applied_update_is_saved() {
        let dir = tmp_dir("partial");
        let db = Database::build_from_str("srv", DATA).unwrap();
        db.save(&dir).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap().with_db_dir(&dir));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());
        let resp = post(
            &addr,
            "/update",
            "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> } ; LOAD <file:///nonexistent/x.ttl>",
        );
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        assert!(resp.contains("were applied"), "the client is told: {resp}");
        assert!(
            ask(&dir, "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }"),
            "the applied part was saved"
        );
        server.shutdown();
        let report = serving.join().unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With check_point off, an update still in flight when the graceful
    /// shutdown's final save ran (here: its body arrives only afterwards) is
    /// saved before it is acknowledged — never acknowledged and then lost.
    #[test]
    fn an_update_finishing_after_shutdown_is_saved_before_it_is_acked() {
        let dir = tmp_dir("late_update");
        let db = Database::build_from_str("srv", DATA).unwrap();
        db.save(&dir).unwrap();
        let server = Arc::new(
            Server::bind(db, "127.0.0.1:0")
                .unwrap()
                .with_db_dir(&dir)
                .with_check_point(false)
                .with_shutdown_grace(Duration::from_millis(50)),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());

        let update = "INSERT DATA { <http://ex/late> <http://ex/p> <http://ex/x> }";
        let mut late = TcpStream::connect(addr).unwrap();
        late.write_all(
            format!(
                "POST /update HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                update.len()
            )
            .as_bytes(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        server.shutdown();
        let report = serving.join().unwrap();
        assert!(!report.drained && report.saved.is_empty(), "{report:?}");

        late.write_all(update.as_bytes()).unwrap();
        let mut resp = String::new();
        late.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(
            ask(&dir, "ASK { <http://ex/late> <http://ex/p> <http://ex/x> }"),
            "an acknowledged update is durable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handler_panic_returns_500_and_the_server_keeps_serving() {
        let (_srv, addr) = start();
        // The test-only route panics while holding the database lock.
        let resp = http(&addr, "GET /__test/panic HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp.contains("500 Internal Server Error"), "{resp}");
        assert!(resp.contains("application/json"), "{resp}");
        assert!(resp.contains("\"error\"") && resp.contains("panicked"), "{resp}");
        // The poisoned database lock is recovered: reads and writes still work.
        let ok = post(&addr, "/sparql", SELECT_O);
        assert!(ok.contains("200 OK") && ok.contains("http://ex/b"), "{ok}");
        let upd = post(&addr, "/update", "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }");
        assert!(upd.contains("\"changed\":1"), "{upd}");
        let status = http(&addr, "GET /status HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(status.contains("\"triples\":3"), "{status}");
    }

    #[test]
    fn shutdown_returns_from_serve_forever_and_closes_the_listener() {
        let db = Database::build_from_str("sd", DATA).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());
        assert!(post(&addr, "/sparql", SELECT_O).contains("200 OK"));
        let handle = server.shutdown_handle();
        assert!(!handle.is_shutdown());
        handle.shutdown();
        handle.shutdown(); // idempotent
        assert!(handle.is_shutdown());
        let report = serving.join().unwrap();
        assert!(report.drained);
        assert!(std::net::TcpStream::connect(addr).is_err());
        // serve_forever after a shutdown returns immediately.
        assert_eq!(server.serve_forever(), ShutdownReport { drained: true, ..Default::default() });
    }

    #[test]
    fn wake_address_maps_unspecified_to_loopback() {
        let v4: SocketAddr = "0.0.0.0:7000".parse().unwrap();
        assert_eq!(Lifecycle::new(v4).wake, "127.0.0.1:7000".parse().unwrap());
        let v6: SocketAddr = "[::]:7000".parse().unwrap();
        assert_eq!(Lifecycle::new(v6).wake, "[::1]:7000".parse().unwrap());
        let specific: SocketAddr = "10.1.2.3:80".parse().unwrap();
        assert_eq!(Lifecycle::new(specific).wake, specific);
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

    // ---- connection handling: parsing limits, keep-alive, pool, CORS --------

    /// Read exactly one `Content-Length`-framed response from a keep-alive
    /// connection: `(head, body)`.
    fn read_one<R: BufRead>(r: &mut R) -> (String, String) {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            assert!(r.read_line(&mut line).unwrap() > 0, "EOF in head: {head}");
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let len: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).unwrap();
        (head, String::from_utf8(body).unwrap())
    }

    fn start_with(f: impl FnOnce(Server) -> Server) -> (Arc<Server>, SocketAddr) {
        let db = Database::build_from_str("srv", DATA).unwrap();
        let server = Arc::new(f(Server::bind(db, "127.0.0.1:0").unwrap()));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        (server, addr)
    }

    #[test]
    fn request_parsing_decodes_chunked_bodies_and_bounds_heads() {
        let raw = "POST /update HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                   5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nTrailer: x\r\n\r\n";
        let req = Request::parse(&mut raw.as_bytes()).unwrap().unwrap();
        assert_eq!(req.body, b"hello world");
        assert_eq!(req.version, "HTTP/1.1");
        assert!(req.wants_keep_alive());

        let mut r = std::io::BufReader::new(raw.as_bytes());
        let mut req = Request::read_head(&mut r).unwrap().unwrap();
        let err = req.read_body(&mut r, 8).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "chunked body over the cap");

        let raw = "POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n0123456789";
        let mut r = std::io::BufReader::new(raw.as_bytes());
        let mut req = Request::read_head(&mut r).unwrap().unwrap();
        assert_eq!(req.declared_body_len(), Some(10));
        assert_eq!(req.read_body(&mut r, 9).unwrap_err().kind(), io::ErrorKind::InvalidData);

        let bad = "POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n";
        assert!(Request::parse(&mut bad.as_bytes()).is_err());
        let short = "POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc";
        assert!(Request::parse(&mut short.as_bytes()).is_err());

        let long = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEADER_LINE as usize));
        let err = Request::parse(&mut long.as_bytes()).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let many = format!("GET / HTTP/1.1\r\n{}\r\n", "X-A: 1\r\n".repeat(MAX_HEADERS + 1));
        assert!(Request::parse(&mut many.as_bytes()).is_err());

        // Leading blank lines are skipped; HTTP/1.0 needs an explicit keep-alive.
        let req = Request::parse(&mut "\r\nGET /status HTTP/1.0\r\n\r\n".as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(req.path, "/status");
        assert!(!req.wants_keep_alive());
        let req = Request::parse(&mut "GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n".as_bytes())
            .unwrap()
            .unwrap();
        assert!(req.wants_keep_alive());
        let req = Request::parse(&mut "GET / HTTP/1.1\r\nConnection: close\r\n\r\n".as_bytes())
            .unwrap()
            .unwrap();
        assert!(!req.wants_keep_alive());
    }

    #[test]
    fn worker_pool_grows_on_demand_up_to_its_bound_and_applies_backpressure() {
        use std::sync::mpsc;
        let pool = WorkerPool::new(3, 100);
        assert_eq!(locked(&pool.shared.state).workers, 0, "no thread before work");
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        for _ in 0..12 {
            let (r, p, tx) = (Arc::clone(&running), Arc::clone(&peak), tx.clone());
            assert!(pool.submit(
                Box::new(move || {
                    let n = r.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(n, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(20));
                    r.fetch_sub(1, Ordering::SeqCst);
                    tx.send(()).unwrap();
                }),
                || false
            ));
        }
        for _ in 0..12 {
            rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 3, "at most max_threads at once");
        assert_eq!(locked(&pool.shared.state).workers, 3);
        // A panicking job does not cost the pool its worker.
        let (tx2, rx2) = mpsc::channel();
        pool.submit(Box::new(|| panic!("job panic")), || false);
        pool.submit(Box::new(move || tx2.send(()).unwrap()), || false);
        rx2.recv_timeout(Duration::from_secs(10)).unwrap();
        pool.close();

        // Backpressure: one worker, a queue of one. With the worker blocked
        // and the queue full, a submission waits — and gives up when told to.
        let pool = WorkerPool::new(1, 1);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel();
        pool.submit(
            Box::new(move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
            }),
            || false,
        );
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(pool.submit(Box::new(|| {}), || false), "fills the queue");
        assert_eq!(pool.queue_probe().queued(), 1);
        let t0 = Instant::now();
        let gave_up = !pool.submit(Box::new(|| {}), || t0.elapsed() > Duration::from_millis(200));
        assert!(gave_up && t0.elapsed() >= Duration::from_millis(200));
        release_tx.send(()).unwrap();
        pool.close();
    }

    #[test]
    fn keep_alive_serves_several_requests_on_one_connection() {
        let (server, addr) = start_with(|s| s.with_keep_alive(Some(Duration::from_secs(5)), 3));
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        // Two requests written back to back (pipelined) plus a third later.
        let q = format!(
            "POST /sparql HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{SELECT_O}",
            SELECT_O.len()
        );
        w.write_all(format!("{q}{q}").as_bytes()).unwrap();
        for _ in 0..2 {
            let (head, body) = read_one(&mut r);
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            assert!(head.contains("Connection: keep-alive"), "{head}");
            assert!(head.contains("Keep-Alive: timeout=5"), "{head}");
            assert!(body.contains("http://ex/b"), "{body}");
        }
        // The third (= keep_alive_max) response closes the connection.
        w.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let (head, body) = read_one(&mut r);
        assert!(head.contains("Connection: close"), "{head}");
        assert!(body.contains("\"triples\""), "{body}");
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "", "the server closed the connection");
        server.shutdown();
    }

    #[test]
    fn keep_alive_off_and_http10_close_after_one_response() {
        let (_server, addr) = start_with(|s| s.with_keep_alive(None, 100));
        let resp = http(&addr, "GET /status HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(resp.contains("Connection: close"), "{resp}");
        let (_server, addr) = start();
        let resp = http(&addr, "GET /status HTTP/1.0\r\n\r\n");
        assert!(resp.contains("Connection: close"), "HTTP/1.0: {resp}");
    }

    #[test]
    fn shutdown_closes_idle_keep_alive_connections_promptly() {
        let db = Database::build_from_str("srv", DATA).unwrap();
        let server = Arc::new(
            Server::bind(db, "127.0.0.1:0")
                .unwrap()
                .with_keep_alive(Some(Duration::from_secs(60)), 100),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("keep-alive"), "{head}");
        // The connection idles; a shutdown must neither wait out its 60 s
        // timeout nor report the idle connection as an undrained request.
        let t0 = Instant::now();
        server.shutdown();
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "");
        let report = serving.join().unwrap();
        assert!(report.drained, "{report:?}");
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    }

    #[test]
    fn an_idle_keep_alive_connection_yields_its_worker_to_queued_ones() {
        // One worker. Client A finishes a request and idles on keep-alive;
        // client B must not wait out A's 60 s idle timeout.
        let (server, addr) = start_with(|s| {
            s.with_thread_num(1)
                .with_keep_alive(Some(Duration::from_secs(60)), 100)
        });
        let a = TcpStream::connect(addr).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut aw = a.try_clone().unwrap();
        let mut ar = BufReader::new(a);
        aw.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let (head, _) = read_one(&mut ar);
        assert!(head.contains("keep-alive"), "{head}");
        let t0 = Instant::now();
        let resp = http(&addr, "GET /status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        server.shutdown();
    }

    #[test]
    fn the_worker_pool_bounds_concurrent_connections() {
        // Two workers, both pinned by requests whose bodies have not arrived.
        let (server, addr) = start_with(|s| s.with_thread_num(2));
        let head = format!(
            "POST /sparql HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            SELECT_O.len()
        );
        let mut slow: Vec<TcpStream> = (0..2)
            .map(|_| {
                let mut s = TcpStream::connect(addr).unwrap();
                s.write_all(head.as_bytes()).unwrap();
                s
            })
            .collect();
        thread::sleep(Duration::from_millis(300));
        // A third connection (N + 1) is queued, not served.
        let mut third = TcpStream::connect(addr).unwrap();
        third
            .write_all(b"GET /status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        third.set_read_timeout(Some(Duration::from_millis(700))).unwrap();
        let mut buf = [0u8; 1];
        let early = third.read(&mut buf);
        assert!(
            matches!(&early, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "the third connection must wait for a worker: {early:?}"
        );
        // Finish one slow request: its worker frees up and serves the third.
        slow[0].write_all(SELECT_O.as_bytes()).unwrap();
        let mut done = String::new();
        slow[0].read_to_string(&mut done).unwrap();
        assert!(done.contains("http://ex/b"), "{done}");
        third.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut resp = String::new();
        third.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        slow[1].write_all(SELECT_O.as_bytes()).unwrap();
        server.shutdown();
    }

    #[test]
    fn many_more_connections_than_workers_are_all_served() {
        let (server, addr) = start_with(|s| s.with_thread_num(2));
        let clients: Vec<_> = (0..24)
            .map(|_| {
                thread::spawn(move || {
                    post(&addr, "/sparql", SELECT_O)
                })
            })
            .collect();
        for c in clients {
            let resp = c.join().unwrap();
            assert!(resp.contains("http://ex/b"), "{resp}");
        }
        server.shutdown();
    }

    #[test]
    fn oversized_bodies_are_413_and_expect_continue_is_honoured() {
        let (server, addr) = start_with(|s| s.with_max_body_size(16));
        let resp = http(
            &addr,
            "POST /update HTTP/1.1\r\nHost: x\r\nContent-Length: 17\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
        server.shutdown();

        let (server, addr) = start();
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(
            format!(
                "POST /sparql HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                SELECT_O.len()
            )
            .as_bytes(),
        )
        .unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "HTTP/1.1 100 Continue\r\n");
        r.read_line(&mut line).unwrap();
        s.write_all(SELECT_O.as_bytes()).unwrap();
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert!(rest.starts_with("HTTP/1.1 200"), "{rest}");
        assert!(rest.contains("http://ex/b"), "{rest}");
        server.shutdown();
    }

    #[test]
    fn options_preflight_and_cors_headers() {
        let (_server, addr) = start();
        // Upstream answers the /file/* preflights with a wildcard origin.
        let resp = http(
            &addr,
            "OPTIONS /file/upload HTTP/1.1\r\nHost: x\r\nOrigin: http://w\r\n\
             Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("Access-Control-Allow-Origin: *"), "{resp}");
        assert!(resp.contains("Access-Control-Allow-Methods: POST"), "{resp}");
        assert!(resp.ends_with("\r\n\r\nok"), "{resp}");
        let resp = http(&addr, "OPTIONS /file/download/x.nt HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp.contains("Access-Control-Allow-Methods: GET"), "{resp}");
        // Elsewhere, without a configured origin: a plain 204 + Allow, no CORS.
        let resp = http(&addr, "OPTIONS /sparql HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert!(resp.contains("Allow: GET, POST, OPTIONS"), "{resp}");
        assert!(!resp.contains("Access-Control-Allow-Origin"), "{resp}");
        let resp = post(&addr, "/sparql", SELECT_O);
        assert!(!resp.contains("Access-Control-Allow-Origin"), "{resp}");

        // With a configured origin every response and preflight carries it —
        // and a preflight needs no credentials even on an auth-gated server.
        let (_server, addr) = start_with(|s| {
            s.with_cors_origin(Some("https://app.example"))
                .with_basic_auth("u", "p")
        });
        let resp = http(
            &addr,
            "OPTIONS /sparql HTTP/1.1\r\nAccess-Control-Request-Headers: content-type, x-a\r\n\
             Connection: close\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("Access-Control-Allow-Origin: https://app.example"), "{resp}");
        assert!(resp.contains("Access-Control-Allow-Headers: content-type, x-a"), "{resp}");
        let resp = post(&addr, "/sparql", SELECT_O);
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        assert!(resp.contains("Access-Control-Allow-Origin: https://app.example"), "{resp}");
    }

    #[test]
    fn with_config_applies_the_http_settings() {
        let cfg = GStoreConfig::from_ini_str(
            "[server]\nthread_num=3\nkeep_alive_timeout=0\nmax_body_size=8\n\
             cors_allow_origin=*\nshutdown_grace=1\n[system]\ncheck_point=off\n",
        );
        let db = Database::build_from_str("srv", DATA).unwrap();
        let s = Server::bind(db, "127.0.0.1:0").unwrap().with_config(&cfg);
        assert_eq!(s.http.threads, 3);
        assert_eq!(s.http.keep_alive, None);
        assert_eq!(s.http.max_body, 8);
        assert_eq!(s.http.cors_origin.as_deref(), Some("*"));
        assert_eq!(s.grace, Duration::from_secs(1));
        assert!(!s.check_point());
    }

    #[test]
    fn restart_requests_are_reported() {
        let (server, _addr) = start_with(|s| s);
        let handle = server.shutdown_handle();
        assert!(!handle.restart_requested());
        handle.restart();
        assert!(handle.is_shutdown() && handle.restart_requested());
        let s2 = Arc::clone(&server);
        let report = thread::spawn(move || s2.serve_forever()).join().unwrap();
        assert!(report.restart, "{report:?}");
    }

    #[cfg(feature = "tls")]
    #[test]
    fn tls_keep_alive_serves_two_requests() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("gstore_tls_ka_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, CERT_PEM).unwrap();
        std::fs::write(&key, KEY_PEM).unwrap();
        let config = load_tls_config(&cert, &key).expect("load_tls_config");
        let db = Database::build_from_str("tls", DATA).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever_tls(config));

        let mut roots = rustls::RootCertStore::empty();
        let cert_der = rustls_pemfile::certs(&mut std::io::BufReader::new(CERT_PEM.as_bytes()))
            .next()
            .unwrap()
            .unwrap();
        roots.add(cert_der).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(client_config), name).unwrap();
        let sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut tls = BufReader::new(rustls::StreamOwned::new(conn, sock));
        for _ in 0..2 {
            tls.get_mut()
                .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            tls.get_mut().flush().unwrap();
            let (head, body) = read_one(&mut tls);
            assert!(head.contains("Connection: keep-alive"), "{head}");
            assert!(body.contains("\"triples\""), "{body}");
        }
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
