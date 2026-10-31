//! A minimal HTTP SPARQL endpoint over a [`Database`] — the analogue of gStore's
//! `src/Server` / `ghttp`.
//!
//! This is a dependency-free, blocking HTTP/1.1 server (connections are served
//! by a bounded worker pool, with keep-alive — see *Connections* below)
//! implementing a useful subset of the SPARQL 1.1 Protocol:
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
//! **Connections** (shared with [`ApiServer`](crate::http_api::ApiServer), tuned
//! by [`HttpOptions`] — gStore's `thread_num` and friends, see
//! [`crate::config`]):
//!
//! * a **bounded worker pool** of [`HttpOptions::threads`] threads serves the
//!   accepted connections; up to [`HttpOptions::queue_len`] more wait for a free
//!   worker, and beyond that the accept loop stops accepting (backpressure into
//!   the kernel's listen backlog) instead of spawning without bound;
//! * **HTTP/1.1 keep-alive**: a connection serves up to
//!   [`HttpOptions::keep_alive_max`] requests and is closed after
//!   [`HttpOptions::keep_alive_timeout`] idle (honouring a client's
//!   `Connection: close`, HTTP/1.0 semantics and `Keep-Alive: timeout=, max=`).
//!   An idle connection gives its worker up early when other connections are
//!   waiting for one, and closes at once when a shutdown begins;
//! * **CORS**: with [`HttpOptions::cors`] (the default) every response carries
//!   `Access-Control-Allow-Origin: *` and `OPTIONS` preflights are answered
//!   directly (as upstream does for `/file/upload` and `/file/download`);
//! * a **request body cap** ([`HttpOptions::max_body`]): a larger
//!   `Content-Length` is refused with `413` before anything is allocated, and
//!   `Expect: 100-continue` is honoured.
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

use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::db::{Database, LoadPolicy};
use crate::query::{QueryResult, ResultSet};
use crate::sparql_results::{self, json_str, ResultFormat};

const SPARQL_JSON: &str = "application/sparql-results+json";

/// Default maximum accepted request-body size (`Content-Length`) — the default
/// of [`HttpOptions::max_body`]. Bounds the pre-auth allocation in request
/// parsing so a single header can't make the server reserve arbitrary memory or
/// panic on capacity overflow. Shared by the simple server and the multi-DB
/// `http_api` server. (`gserver` raises it to `conf.ini`'s
/// `upload_max_body_size`, 1 GiB by default.)
pub const MAX_REQUEST_BODY: usize = 256 * 1024 * 1024; // 256 MiB

/// Connection-handling settings shared by [`Server`] and
/// [`ApiServer`](crate::http_api::ApiServer): the worker pool, keep-alive,
/// CORS, timeouts and the request body cap. [`GStoreConfig::http_options`]
/// derives them from `conf.ini`.
///
/// [`GStoreConfig::http_options`]: crate::config::GStoreConfig::http_options
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOptions {
    /// Worker threads serving connections (gStore's `thread_num`, default
    /// 30). At least one is always started.
    pub threads: usize,
    /// Accepted connections that may wait for a free worker (default 128).
    /// When that many are waiting, the accept loop stops accepting until a
    /// worker frees up — the kernel listen backlog absorbs the rest.
    pub queue_len: usize,
    /// How long a kept-alive connection may sit idle between requests
    /// (default 60 s, upstream workflow's default). `Duration::ZERO` turns
    /// keep-alive off: every response then carries `Connection: close`.
    pub keep_alive_timeout: Duration,
    /// Maximum number of requests served on one connection (default 100).
    pub keep_alive_max: usize,
    /// Read timeout while a request is being received (default 30 s): a
    /// client that stalls mid-request (a slowloris) is dropped.
    pub read_timeout: Duration,
    /// Write timeout for responses (default 30 s): a client that stops reading
    /// cannot pin a worker forever.
    pub write_timeout: Duration,
    /// Maximum request body in bytes (default [`MAX_REQUEST_BODY`]); larger
    /// requests are answered `413` without reading the body.
    pub max_body: usize,
    /// Add `Access-Control-Allow-Origin: *` to every response and answer
    /// `OPTIONS` preflight requests (default on).
    pub cors: bool,
}

/// The default worker-pool size (gStore's `thread_num` default).
pub const DEFAULT_THREADS: usize = 30;

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            threads: DEFAULT_THREADS,
            queue_len: 128,
            keep_alive_timeout: Duration::from_secs(60),
            keep_alive_max: 100,
            read_timeout: CONN_READ_TIMEOUT,
            write_timeout: CONN_READ_TIMEOUT,
            max_body: MAX_REQUEST_BODY,
            cors: true,
        }
    }
}

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
    /// Worker pool, keep-alive, CORS and body-size settings.
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

    /// Replace the connection settings (worker-pool size, keep-alive, CORS,
    /// timeouts, body cap; default [`HttpOptions::default`]).
    pub fn with_http_options(mut self, opts: HttpOptions) -> Server {
        self.http = opts;
        self
    }

    /// The connection settings in effect.
    pub fn http_options(&self) -> &HttpOptions {
        &self.http
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
    /// `LoadPolicy::restricted().with_allowed_dir(dir)`. Its `allow_remote`
    /// also decides whether federated `SERVICE` patterns may contact remote
    /// endpoints (not by default).
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

    /// Accept connections until [`shutdown`](Self::shutdown), serving them on
    /// the bounded worker pool (see [`HttpOptions`]) so a slow query on one
    /// connection does not block the others (the [`Database`] itself is still
    /// serialized behind its `Mutex`). On shutdown the listener is closed, idle
    /// kept-alive connections are closed, in-flight requests are given up to the
    /// grace period to finish, unsaved changes are saved, and a
    /// [`ShutdownReport`] is returned.
    pub fn serve_forever(&self) -> ShutdownReport {
        let drained = run_accept_loop(&self.listener, &self.life, self.grace, &self.http, |acc| {
            let inner = Arc::clone(&self.inner);
            Box::new(move || {
                let ctl = acc.stream.try_clone().ok();
                serve_connection(acc.stream, ctl.as_ref(), Some(acc.peer), acc.guard, &acc.env, |s, req, meta| {
                    inner.handle(s, req, meta)
                });
            })
        });
        self.finish_shutdown(drained)
    }

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS
    /// in-process, wrapping every accepted connection in a `rustls` stream built
    /// from `config` (see [`load_tls_config`]). Requires the `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        let drained = run_accept_loop(&self.listener, &self.life, self.grace, &self.http, |acc| {
            let inner = Arc::clone(&self.inner);
            let config = Arc::clone(&config);
            Box::new(move || {
                let Ok(conn) = rustls::ServerConnection::new(config) else {
                    return;
                };
                let ctl = acc.stream.try_clone().ok();
                let tls = rustls::StreamOwned::new(conn, acc.stream);
                serve_connection(tls, ctl.as_ref(), Some(acc.peer), acc.guard, &acc.env, |s, req, meta| {
                    inner.handle(s, req, meta)
                });
            })
        });
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

    /// Request a graceful shutdown that the embedding program should follow
    /// with a restart (what the HTTP `/restart` route does): the server stops
    /// exactly as for [`shutdown`](Self::shutdown), and
    /// [`restart_requested`](Self::restart_requested) turns `true`. `gserver`
    /// then re-executes itself.
    pub fn restart(&self) {
        self.life.request_restart();
    }

    /// Whether the shutdown was requested as a restart (see [`restart`](Self::restart)).
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
    /// Set (before `stop`) when the shutdown is a restart.
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

    /// [`request_stop`](Self::request_stop), flagged as a restart. A shutdown
    /// already under way stays a plain shutdown.
    pub(crate) fn request_restart(&self) {
        if !self.stopping() {
            self.restart.store(true, Ordering::SeqCst);
        }
        self.request_stop();
    }

    pub(crate) fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::SeqCst)
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

/// One unit of work for the [`WorkerPool`]: serving one accepted connection.
pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

/// What the accept loop hands to a server's job factory for each accepted
/// connection.
pub(crate) struct Accepted {
    pub(crate) stream: TcpStream,
    pub(crate) peer: SocketAddr,
    /// Counts the connection's first request as in flight from the moment it
    /// was accepted (so a shutdown waits for it, bounded by the grace period).
    pub(crate) guard: InFlightGuard,
    pub(crate) env: ConnEnv,
}

/// The shared, per-`serve_forever` context every connection needs.
#[derive(Clone)]
pub(crate) struct ConnEnv {
    life: Arc<Lifecycle>,
    opts: Arc<HttpOptions>,
    /// Connections accepted but not yet picked up by a worker.
    waiting: Arc<AtomicUsize>,
}

/// Stack size of the connection worker threads. Request handling recurses
/// over client-supplied structure (a SPARQL request's syntax tree, nested
/// RDF), bounded by the parsers' limits (e.g.
/// [`MAX_NESTING_DEPTH`](crate::parser::sparql::MAX_NESTING_DEPTH)); this stack
/// covers the deepest request they accept with a wide margin — a stack
/// overflow would abort the whole server. Rust's 2 MiB thread default is
/// enough for an optimized build but not for an unoptimized one, whose frames
/// are several times larger. (8 MiB is also what C/C++ threads get by
/// default on Linux; untouched stack pages cost no memory.)
pub(crate) const WORKER_STACK_SIZE: usize = if cfg!(debug_assertions) {
    16 << 20
} else {
    8 << 20
};

/// A fixed set of worker threads fed through a bounded queue. Dropping the
/// pool closes the queue: the workers finish what is queued, then exit.
struct WorkerPool {
    /// `None` when not a single worker thread could be started — jobs then run
    /// inline on the accepting thread (degraded, but still serving).
    tx: Option<SyncSender<Job>>,
    waiting: Arc<AtomicUsize>,
}

impl WorkerPool {
    fn start(threads: usize, queue_len: usize, waiting: Arc<AtomicUsize>) -> WorkerPool {
        let (tx, rx) = mpsc::sync_channel::<Job>(queue_len);
        let rx = Arc::new(Mutex::new(rx));
        let mut started = 0;
        for i in 0..threads.max(1) {
            let rx = Arc::clone(&rx);
            let waiting = Arc::clone(&waiting);
            let spawned = thread::Builder::new()
                .name(format!("gstore-http-{i}"))
                .stack_size(WORKER_STACK_SIZE)
                .spawn(move || worker_loop(&rx, &waiting));
            if spawned.is_ok() {
                started += 1;
            }
        }
        WorkerPool {
            tx: (started > 0).then_some(tx),
            waiting,
        }
    }

    /// Queue `job` for a worker. While the queue is full the caller (the
    /// accept loop) waits here — it accepts nothing meanwhile, which is the
    /// backpressure — unless a shutdown begins, in which case the job (an
    /// unserved connection) is dropped.
    fn submit(&self, job: Job, life: &Lifecycle) {
        let Some(tx) = &self.tx else {
            job();
            return;
        };
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let mut job = job;
        loop {
            match tx.try_send(job) {
                Ok(()) => return,
                Err(TrySendError::Full(j)) if !life.stopping() => {
                    job = j;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    self.waiting.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
}

fn worker_loop(rx: &Mutex<Receiver<Job>>, waiting: &AtomicUsize) {
    loop {
        let job = locked(rx).recv();
        let Ok(job) = job else {
            return; // the pool was dropped and the queue is drained
        };
        waiting.fetch_sub(1, Ordering::SeqCst);
        // A panic is contained to this connection; the worker keeps serving.
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
}

/// The shared accept loop: turn every accepted connection (with its timeouts
/// set and an [`InFlightGuard`]) into a job with `make_job` and run it on a
/// bounded [`WorkerPool`] (sized by `opts`), until a shutdown is requested;
/// then close the listener and wait — at most `grace` — for the in-flight
/// requests to finish. Returns whether they all did.
pub(crate) fn run_accept_loop(
    listener: &Mutex<Option<TcpListener>>,
    life: &Arc<Lifecycle>,
    grace: Duration,
    opts: &HttpOptions,
    mut make_job: impl FnMut(Accepted) -> Job,
) -> bool {
    // Accept on a clone of the socket so the `Mutex` is not held while blocked.
    let sock = locked(listener).as_ref().and_then(|l| l.try_clone().ok());
    if let Some(sock) = sock {
        let waiting = Arc::new(AtomicUsize::new(0));
        let env = ConnEnv {
            life: Arc::clone(life),
            opts: Arc::new(opts.clone()),
            waiting: Arc::clone(&waiting),
        };
        let pool = WorkerPool::start(opts.threads, opts.queue_len, waiting);
        while !life.stopping() {
            match sock.accept() {
                Ok((stream, peer)) => {
                    if life.stopping() {
                        break; // the wake-up self-connection (or a late client)
                    }
                    let _ = stream.set_read_timeout(Some(opts.read_timeout));
                    let _ = stream.set_write_timeout(Some(opts.write_timeout));
                    // Responses go out as one write; don't let Nagle hold back
                    // a kept-alive connection's next response.
                    let _ = stream.set_nodelay(true);
                    let job = make_job(Accepted {
                        stream,
                        peer,
                        guard: life.enter(),
                        env: env.clone(),
                    });
                    pool.submit(job, life);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // E.g. EMFILE under a connection flood: back off instead of
                // spinning on the failing accept.
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        // Queued connections are still served (they hold in-flight guards);
        // the workers exit once the queue is drained.
        drop(pool);
    }
    // Close both descriptors so new connections are refused from now on.
    locked(listener).take();
    life.wait_idle(grace)
}

// --- per-connection loop: keep-alive, CORS, body cap -------------------------

/// How often an idle kept-alive connection re-checks for a shutdown or for
/// connections waiting for its worker.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// Longest accepted request line / header line, and most header lines.
const MAX_HEADER_LINE: u64 = 64 * 1024;
const MAX_HEADERS: usize = 256;

/// Why no request could be read from a connection (answered, then closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadRequest {
    pub(crate) status: u16,
    pub(crate) message: &'static str,
}

impl BadRequest {
    const MALFORMED: BadRequest = BadRequest {
        status: 400,
        message: "bad request",
    };
    const TOO_LARGE: BadRequest = BadRequest {
        status: 413,
        message: "request body exceeds the maximum size",
    };
}

/// Per-response connection facts a request handler must honour: the headers
/// to add to its response (`Connection`/`Keep-Alive`, CORS). The client's
/// address is available through [`current_peer`].
pub(crate) struct ConnMeta {
    headers: Vec<(&'static str, String)>,
    /// `Access-Control-Allow-Origin: *` is part of `headers`.
    cors: bool,
}

impl ConnMeta {
    fn new(keep_alive: bool, idle: Duration, remaining: usize, cors: bool) -> ConnMeta {
        let mut headers: Vec<(&'static str, String)> = Vec::new();
        if keep_alive {
            headers.push(("Connection", "keep-alive".to_string()));
            headers.push((
                "Keep-Alive",
                format!("timeout={}, max={remaining}", idle.as_secs().max(1)),
            ));
        } else {
            headers.push(("Connection", "close".to_string()));
        }
        if cors {
            headers.push(("Access-Control-Allow-Origin", "*".to_string()));
        }
        ConnMeta { headers, cors }
    }

    /// `extra` followed by this connection's headers, ready for
    /// [`write_response_ext`] / [`write_response_maybe_gzip`].
    pub(crate) fn headers<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut out = extra.to_vec();
        out.extend(self.headers.iter().map(|(k, v)| (*k, v.as_str())));
        out
    }

    /// The CORS header alone (for a streamed response, which always closes).
    fn cors_headers(&self) -> Vec<(&'static str, &'static str)> {
        if self.cors {
            vec![("Access-Control-Allow-Origin", "*")]
        } else {
            Vec::new()
        }
    }
}

thread_local! {
    /// The peer of the connection the current worker thread is serving.
    static CURRENT_PEER: Cell<Option<SocketAddr>> = const { Cell::new(None) };
}

/// The TCP peer address of the request being handled on this thread (set by
/// the connection loop for the duration of a connection; `None` outside one).
/// Lets request-handling code (e.g. the upstream-compatible API's IP checks
/// and access log) learn the client IP without threading it through every
/// signature.
#[allow(dead_code)]
pub(crate) fn current_peer() -> Option<SocketAddr> {
    CURRENT_PEER.with(Cell::get)
}

/// Serve one connection: read requests and hand each to `handle` — which
/// writes the response, adding [`ConnMeta::headers`], and returns whether it
/// honoured keep-alive (`false` e.g. after a streamed response) — until the
/// client closes, keep-alive ends (idle timeout, request limit, `Connection:
/// close`, HTTP/1.0), the request is malformed, or a shutdown begins. `ctl` is
/// the underlying socket (for timeouts; for TLS, `stream` wraps it); without it
/// keep-alive is off. `first` counts the first request as in flight; later
/// requests count only once their first byte has arrived, and a request that
/// arrives after a shutdown began is not served (the connection is closed, as a
/// new connection would be refused).
pub(crate) fn serve_connection<S: Read + Write>(
    stream: S,
    ctl: Option<&TcpStream>,
    peer: Option<SocketAddr>,
    first: InFlightGuard,
    env: &ConnEnv,
    mut handle: impl FnMut(&mut S, Result<Request, BadRequest>, &ConnMeta) -> io::Result<bool>,
) {
    struct PeerScope;
    impl Drop for PeerScope {
        fn drop(&mut self) {
            CURRENT_PEER.with(|p| p.set(None));
        }
    }
    CURRENT_PEER.with(|p| p.set(peer));
    let _scope = PeerScope;

    let opts = &*env.opts;
    let life = &env.life;
    let mut reader = BufReader::new(Timed {
        inner: stream,
        ctl,
        deadline: None,
    });
    let mut first = Some(first);
    let mut served = 0usize;
    let mut idle = opts.keep_alive_timeout;
    let mut max_requests = opts.keep_alive_max.max(1);
    loop {
        // Held until this request's response is written (end of the iteration).
        let _in_flight = match first.take() {
            Some(g) => g,
            None => {
                let Some(ctl) = ctl else { return };
                if !wait_for_request(&mut reader, ctl, life, idle, &env.waiting) {
                    return;
                }
                let g = life.enter();
                if life.stopping() {
                    return; // arrived after the shutdown began: not served
                }
                let _ = ctl.set_read_timeout(Some(opts.read_timeout));
                g
            }
        };
        let (req, info) = match read_request(&mut reader, opts.max_body, opts.read_timeout) {
            Ok(None) => return, // closed before sending anything
            Ok(Some(r)) => r,
            Err(bad) => {
                let meta = ConnMeta::new(false, idle, 0, opts.cors);
                let _ = handle(&mut reader.get_mut().inner, Err(bad), &meta);
                return;
            }
        };
        served += 1;
        // The client's own `Keep-Alive: timeout=N, max=M` hints can only
        // shorten what the server allows.
        let (hint_timeout, hint_max) = keep_alive_hints(&req);
        if let Some(t) = hint_timeout {
            idle = idle.min(t);
        }
        if let Some(m) = hint_max {
            max_requests = max_requests.min(m.max(1));
        }
        let keep = ctl.is_some()
            && !idle.is_zero()
            && served < max_requests
            && wants_keep_alive(&req, info.http10)
            && !life.stopping();
        let meta = ConnMeta::new(keep, idle, max_requests.saturating_sub(served), opts.cors);
        let stream = &mut reader.get_mut().inner;
        let kept = if opts.cors && req.method == "OPTIONS" {
            write_preflight(stream, &req, &meta).is_ok()
        } else {
            matches!(handle(stream, Ok(req), &meta), Ok(true))
        };
        if !(kept && keep) || life.stopping() {
            return;
        }
    }
}

/// The connection's transport plus an optional deadline: while one is set,
/// every read first narrows the socket's read timeout to the time left (and
/// fails once it has passed), so a request head must arrive *in full* within
/// [`HttpOptions::read_timeout`] — a client dripping one byte at a time cannot
/// hold a worker of the bounded pool indefinitely.
struct Timed<'a, S> {
    inner: S,
    ctl: Option<&'a TcpStream>,
    deadline: Option<Instant>,
}

impl<S: Read> Read for Timed<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let (Some(deadline), Some(ctl)) = (self.deadline, self.ctl) {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let _ = ctl.set_read_timeout(Some(deadline - now));
        }
        self.inner.read(buf)
    }
}

/// Wait (at most `idle`) for the next request on a kept-alive connection.
/// `false` — close the connection — on EOF, timeout, a shutdown, or when other
/// connections are waiting for a worker (an idle connection yields its worker).
fn wait_for_request<S: Read>(
    reader: &mut BufReader<S>,
    ctl: &TcpStream,
    life: &Lifecycle,
    idle: Duration,
    waiting: &AtomicUsize,
) -> bool {
    if !reader.buffer().is_empty() {
        return true; // a pipelined request is already buffered
    }
    let deadline = Instant::now() + idle;
    loop {
        if life.stopping() || waiting.load(Ordering::SeqCst) > 0 {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let _ = ctl.set_read_timeout(Some((deadline - now).min(IDLE_POLL)));
        match reader.fill_buf() {
            Ok(buf) => return !buf.is_empty(),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return false,
        }
    }
}

/// Whether the client allows the connection to stay open: HTTP/1.1 unless it
/// sent `Connection: close`; HTTP/1.0 only with `Connection: keep-alive`.
fn wants_keep_alive(req: &Request, http10: bool) -> bool {
    let tokens = |name: &str| {
        req.header("connection").is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case(name))
        })
    };
    if tokens("close") {
        return false;
    }
    !http10 || tokens("keep-alive")
}

/// The `timeout=` (seconds) and `max=` parameters of a `Keep-Alive` header.
fn keep_alive_hints(req: &Request) -> (Option<Duration>, Option<usize>) {
    let mut timeout = None;
    let mut max = None;
    if let Some(h) = req.header("keep-alive") {
        for part in h.split(',') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "timeout" => timeout = v.parse::<u64>().ok().map(Duration::from_secs),
                "max" => max = v.parse::<usize>().ok(),
                _ => {}
            }
        }
    }
    (timeout, max)
}

/// Answer a CORS preflight (`OPTIONS`) like upstream's `/file/upload` and
/// `/file/download` routes: `200 ok` with `Access-Control-Allow-*` headers.
fn write_preflight<W: Write>(stream: &mut W, req: &Request, meta: &ConnMeta) -> io::Result<()> {
    let methods = if req.path == "/file/upload" {
        "POST"
    } else if req.path == "/file/download" || req.path.starts_with("/file/download/") {
        "GET, POST"
    } else {
        "GET, POST, OPTIONS"
    };
    let allow_headers = req
        .header("access-control-request-headers")
        .unwrap_or("Content-Type, Authorization, X-Session-Token")
        .to_string();
    let extra = [
        ("Access-Control-Allow-Methods", methods),
        ("Access-Control-Allow-Headers", allow_headers.as_str()),
        ("Access-Control-Max-Age", "86400"),
    ];
    write_response_ext(stream, 200, "text/plain", &meta.headers(&extra), b"ok")
}

/// Facts about a parsed request that matter to the connection, not the handler.
struct RequestInfo {
    http10: bool,
}

/// The request line and headers of a request.
struct Head {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    content_length: usize,
    /// `Transfer-Encoding: chunked` body (then `content_length` is ignored).
    chunked: bool,
    info: RequestInfo,
    expect_continue: bool,
}

/// Read one request from a connection, answering `Expect: 100-continue`
/// before the body is read. The request line and headers must arrive within
/// `read_timeout` in total; the body only must not stall for longer than that
/// between reads (a large upload may take longer overall). `Ok(None)` when the
/// client closed the connection before sending anything.
fn read_request<S: Read + Write>(
    reader: &mut BufReader<Timed<'_, S>>,
    max_body: usize,
    read_timeout: Duration,
) -> Result<Option<(Request, RequestInfo)>, BadRequest> {
    reader.get_mut().deadline = Some(Instant::now() + read_timeout);
    let head = read_head(reader, max_body);
    reader.get_mut().deadline = None;
    if let Some(ctl) = reader.get_ref().ctl {
        let _ = ctl.set_read_timeout(Some(read_timeout));
    }
    let Some(head) = head? else {
        return Ok(None);
    };
    if head.expect_continue && (head.chunked || head.content_length > 0) {
        let w = &mut reader.get_mut().inner;
        w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .and_then(|_| w.flush())
            .map_err(|_| BadRequest::MALFORMED)?;
    }
    let body = if head.chunked {
        read_chunked_body(reader, max_body)?
    } else {
        read_body(reader, head.content_length)?
    };
    Ok(Some((
        Request {
            method: head.method,
            path: head.path,
            query: head.query,
            headers: head.headers,
            body,
        },
        head.info,
    )))
}

/// Read one bounded CRLF/LF-terminated line (without the terminator).
/// `Ok(None)` at EOF before any byte.
fn read_line_bounded<R: BufRead>(r: &mut R) -> Result<Option<String>, BadRequest> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_HEADER_LINE)
        .read_until(b'\n', &mut buf)
        .map_err(|_| BadRequest::MALFORMED)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(BadRequest::MALFORMED); // over-long line, or EOF mid-line
    }
    let line = String::from_utf8(buf).map_err(|_| BadRequest::MALFORMED)?;
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

fn read_head<R: BufRead>(r: &mut R, max_body: usize) -> Result<Option<Head>, BadRequest> {
    let Some(line) = read_line_bounded(r)? else {
        return Ok(None);
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let http10 = version.eq_ignore_ascii_case("HTTP/1.0");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target, None),
    };

    let mut content_length = 0usize;
    let mut chunked = false;
    let mut expect_continue = false;
    let mut headers = Vec::new();
    // EOF also ends the headers.
    while let Some(h) = read_line_bounded(r)? {
        if h.is_empty() {
            break; // end of headers
        }
        if headers.len() >= MAX_HEADERS {
            return Err(BadRequest::MALFORMED);
        }
        if let Some((k, v)) = h.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                // An absurd value (e.g. u64::MAX) overflows: refuse it as too
                // large; anything else unparsable is malformed.
                content_length = v.parse().map_err(|e: std::num::ParseIntError| {
                    if *e.kind() == std::num::IntErrorKind::PosOverflow {
                        BadRequest::TOO_LARGE
                    } else {
                        BadRequest::MALFORMED
                    }
                })?;
            } else if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = !v.eq_ignore_ascii_case("identity");
            } else if k.eq_ignore_ascii_case("expect") {
                expect_continue = v.eq_ignore_ascii_case("100-continue");
            }
            headers.push((k.to_ascii_lowercase(), v.to_string()));
        }
    }

    // Refuse an oversized Content-Length BEFORE allocating, so a client cannot
    // make the server reserve arbitrary memory with a single header — this runs
    // before authentication, so the cap is the only guard. Large RDF ingests
    // should use file-based tools (gbuild/gadd) or the server-side `/load` path.
    if content_length > max_body {
        return Err(BadRequest::TOO_LARGE);
    }
    Ok(Some(Head {
        method,
        path,
        query,
        headers,
        content_length,
        chunked,
        info: RequestInfo { http10 },
        expect_continue,
    }))
}

fn read_body<R: BufRead>(r: &mut R, len: usize) -> Result<Vec<u8>, BadRequest> {
    // Grow as the bytes actually arrive instead of trusting the header.
    let mut body = Vec::with_capacity(len.min(1 << 20));
    if len > 0 {
        r.by_ref()
            .take(len as u64)
            .read_to_end(&mut body)
            .map_err(|_| BadRequest::MALFORMED)?;
        if body.len() != len {
            return Err(BadRequest::MALFORMED); // truncated (or timed out)
        }
    }
    Ok(body)
}

/// Decode a `Transfer-Encoding: chunked` request body (RFC 9112 §7.1), at most
/// `max_body` bytes in total; chunk extensions and trailers are ignored.
fn read_chunked_body<R: BufRead>(r: &mut R, max_body: usize) -> Result<Vec<u8>, BadRequest> {
    let mut body = Vec::new();
    loop {
        let line = read_line_bounded(r)?.ok_or(BadRequest::MALFORMED)?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| BadRequest::MALFORMED)?;
        if size == 0 {
            break;
        }
        if body.len().saturating_add(size) > max_body {
            return Err(BadRequest::TOO_LARGE);
        }
        let start = body.len();
        r.by_ref()
            .take(size as u64)
            .read_to_end(&mut body)
            .map_err(|_| BadRequest::MALFORMED)?;
        if body.len() - start != size {
            return Err(BadRequest::MALFORMED);
        }
        // The CRLF after the chunk data.
        if read_line_bounded(r)?.is_none_or(|l| !l.is_empty()) {
            return Err(BadRequest::MALFORMED);
        }
    }
    // Trailer fields, up to the empty line that ends the message.
    for _ in 0..MAX_HEADERS {
        match read_line_bounded(r)? {
            Some(l) if !l.is_empty() => {}
            _ => return Ok(body),
        }
    }
    Err(BadRequest::MALFORMED)
}

/// Arrange for `SIGINT`/`SIGTERM` to trigger `handle.shutdown()` (unix only;
/// returns `false` — and does nothing — elsewhere or if the handlers could not
/// be installed). A tiny watcher thread polls the flag the async-signal-safe
/// handler sets. After the first signal the default disposition is restored,
/// so a second Ctrl-C terminates the process immediately.
pub fn shutdown_on_signals(handle: ShutdownHandle) -> bool {
    on_termination_signal(move || handle.shutdown())
}

/// Run `f` once, on a watcher thread, when the process first receives
/// `SIGINT` or `SIGTERM` (unix only; returns `false` — and does nothing —
/// elsewhere or if the handlers could not be installed). After that first
/// signal the default disposition is restored, so a second one terminates the
/// process immediately. [`shutdown_on_signals`] is this with a graceful
/// shutdown; `gserver`'s supervisor uses it to forward the signal to the
/// server it watches.
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

/// Default read timeout while a request is received (and write timeout for its
/// response): a client that opens a socket and then sends nothing (a
/// slowloris) times out instead of pinning its worker thread and fd forever.
pub(crate) const CONN_READ_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Handle one request read by [`serve_connection`] and write its response
    /// (with the connection's [`ConnMeta`] headers). Generic over the transport
    /// so the same code path serves plain [`TcpStream`]s and `rustls` TLS
    /// streams. Routing runs under `catch_unwind`: a panicking handler answers
    /// `500` and leaves the server (and its recovered locks) serving. Returns
    /// whether the response honoured keep-alive (a streamed one does not).
    fn handle<S: Read + Write>(
        &self,
        stream: &mut S,
        req: Result<Request, BadRequest>,
        meta: &ConnMeta,
    ) -> std::io::Result<bool> {
        let req = match req {
            Ok(r) => r,
            Err(bad) => {
                let body = bad.message.as_bytes();
                write_response_ext(stream, bad.status, "text/plain", &meta.headers(&[]), body)?;
                return Ok(false);
            }
        };
        // Auth gate: when credentials are configured every request must present
        // a matching `Authorization: Basic` header.
        if let Some((user, pass)) = &self.auth {
            if !req.authorized(user, pass) {
                write_response_ext(
                    stream,
                    401,
                    "text/plain",
                    &meta.headers(&[("WWW-Authenticate", "Basic realm=\"gStore\"")]),
                    b"unauthorized",
                )?;
                return Ok(true);
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
            } => write_response_maybe_gzip(stream, status, &ctype, &meta.headers(&[]), &body, enc)
                .map(|()| true),
            Reply::Buffered {
                status, ctype, body, ..
            } => write_response_ext(stream, status, &ctype, &meta.headers(&[]), &body).map(|()| true),
            Reply::Stream(fmt, rs) => {
                stream_select(stream, fmt, &rs, &meta.cors_headers()).map(|()| false)
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
}

impl Request {
    /// Parse one request from a byte stream with the default body cap
    /// ([`MAX_REQUEST_BODY`]) — the same parser the connection loop uses, minus
    /// keep-alive and `Expect: 100-continue` handling. Kept for callers (and
    /// tests) that parse a request from raw bytes.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn parse<R: Read>(stream: &mut R) -> std::io::Result<Option<Request>> {
        let mut reader = BufReader::new(stream);
        let bad = |b: BadRequest| io::Error::new(io::ErrorKind::InvalidData, b.message);
        let Some(head) = read_head(&mut reader, MAX_REQUEST_BODY).map_err(bad)? else {
            return Ok(None);
        };
        let body = if head.chunked {
            read_chunked_body(&mut reader, MAX_REQUEST_BODY)
        } else {
            read_body(&mut reader, head.content_length)
        }
        .map_err(bad)?;
        Ok(Some(Request {
            method: head.method,
            path: head.path,
            query: head.query,
            headers: head.headers,
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
            // `get` (not byte slicing): a multi-byte character across the
            // prefix boundary must not panic.
            if h.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("bearer ")) {
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

/// Write a buffered response that closes the connection (no extra headers).
#[allow(dead_code)]
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
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
        body.len()
    );
    let mut has_connection = false;
    for &(k, v) in extra {
        has_connection |= k.eq_ignore_ascii_case("connection");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    // Without an explicit `Connection` header (the connection loop supplies one
    // through `ConnMeta::headers`) the connection closes after this response.
    if !has_connection {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    // One write for head + body, so a kept-alive connection never waits on
    // Nagle between them.
    let mut buf = head.into_bytes();
    buf.extend_from_slice(body);
    stream.write_all(&buf)?;
    stream.flush()
}

/// Stream a SELECT result with `Transfer-Encoding: chunked`, framing each
/// serializer `write_all` (one per row) as its own HTTP chunk. The connection
/// always closes afterwards; `extra` adds headers (CORS).
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
    header.push_str("Connection: close\r\n\r\n");
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
    // `get` (not byte slicing): a multi-byte character across the prefix
    // boundary must not panic.
    if h.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("basic ")) {
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

    // --- connection handling: keep-alive, worker pool, CORS, body cap ---------

    /// Start a server with custom connection settings; returns the serving
    /// thread too so a test can shut down and join it.
    fn start_with(
        opts: HttpOptions,
    ) -> (Arc<Server>, SocketAddr, thread::JoinHandle<ShutdownReport>) {
        let db = Database::build_from_str("conn", DATA).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap().with_http_options(opts));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());
        (server, addr, serving)
    }

    /// Read exactly one `Content-Length`-framed response from a connection
    /// that may stay open; returns (head, body).
    fn read_one<R: BufRead>(r: &mut R) -> (String, String) {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        let len = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .map(|v| v.trim().parse::<usize>().unwrap())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).unwrap();
        (head, String::from_utf8(body).unwrap())
    }

    fn get_status(conn_header: &str) -> String {
        format!("GET /status HTTP/1.1\r\nHost: x\r\n{conn_header}\r\n")
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        let (_srv, addr) = start();
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);

        // HTTP/1.1 without `Connection: close` keeps the connection open.
        w.write_all(get_status("").as_bytes()).unwrap();
        let (head, body) = read_one(&mut r);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Connection: keep-alive"), "{head}");
        assert!(head.contains("Keep-Alive: timeout=60, max=99"), "{head}");
        assert!(body.contains("\"triples\":2"), "{body}");

        // A second request — with a body — on the same connection.
        let raw = format!(
            "POST /sparql HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{SELECT_O}",
            SELECT_O.len()
        );
        w.write_all(raw.as_bytes()).unwrap();
        let (head, body) = read_one(&mut r);
        assert!(head.contains("Connection: close"), "{head}");
        assert!(body.contains("http://ex/b"), "{body}");
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the server closed after `Connection: close`");
    }

    #[test]
    fn pipelined_requests_are_answered_in_order() {
        let (_srv, addr) = start();
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        let both = format!(
            "{}POST /update HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            get_status(""),
            "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }".len(),
            "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }"
        );
        w.write_all(both.as_bytes()).unwrap();
        let (_, first) = read_one(&mut r);
        assert!(first.contains("\"triples\":2"), "{first}");
        let (head, second) = read_one(&mut r);
        assert!(head.contains("Connection: close"), "{head}");
        assert!(second.contains("\"changed\":1"), "{second}");
    }

    #[test]
    fn keep_alive_limits_http10_and_opt_out() {
        let opts = HttpOptions {
            keep_alive_max: 2,
            ..HttpOptions::default()
        };
        let (_srv, addr, _serving) = start_with(opts);
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(get_status("").as_bytes()).unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("Keep-Alive: timeout=60, max=1"), "{head}");
        // The request limit closes the connection after the second response.
        w.write_all(get_status("").as_bytes()).unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("Connection: close"), "{head}");

        // HTTP/1.0 closes by default and keeps alive only when asked to.
        let resp = http(&addr, "GET /status HTTP/1.0\r\n\r\n");
        assert!(resp.contains("Connection: close"), "{resp}");
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(b"GET /status HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("Connection: keep-alive"), "{head}");

        // A client `Keep-Alive: max=1` hint ends it after one response.
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(get_status("Keep-Alive: timeout=5, max=1\r\n").as_bytes()).unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("Connection: close"), "{head}");
    }

    #[test]
    fn keep_alive_can_be_turned_off() {
        let opts = HttpOptions {
            keep_alive_timeout: Duration::ZERO,
            ..HttpOptions::default()
        };
        let (_srv, addr, _serving) = start_with(opts);
        let resp = http(&addr, &get_status(""));
        assert!(resp.contains("Connection: close"), "{resp}");
        assert!(!resp.contains("Keep-Alive"), "{resp}");
    }

    #[test]
    fn an_idle_connection_times_out_and_never_delays_shutdown() {
        let opts = HttpOptions {
            keep_alive_timeout: Duration::from_millis(300),
            ..HttpOptions::default()
        };
        let (server, addr, serving) = start_with(opts);
        // Idle timeout: the server closes a quiet kept-alive connection.
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(get_status("").as_bytes()).unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("Connection: keep-alive"), "{head}");
        let t0 = Instant::now();
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert!(t0.elapsed() < Duration::from_secs(5), "closed after the idle timeout");

        // An idle connection does not hold up a graceful shutdown.
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(get_status("").as_bytes()).unwrap();
        let _ = read_one(&mut r);
        let t0 = Instant::now();
        server.shutdown();
        let report = serving.join().unwrap();
        assert!(report.drained, "{report:?}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the idle connection was closed");
    }

    #[test]
    fn the_worker_pool_bounds_concurrent_connections() {
        let opts = HttpOptions {
            threads: 2,
            queue_len: 8,
            ..HttpOptions::default()
        };
        let (_srv, addr, _serving) = start_with(opts);
        // Two slow clients occupy both workers: they send half a request.
        let mut slow: Vec<TcpStream> = (0..2)
            .map(|_| {
                let mut s = TcpStream::connect(addr).unwrap();
                s.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n").unwrap();
                s
            })
            .collect();
        thread::sleep(Duration::from_millis(200));
        // The third (N+1-th) connection is accepted but must wait for a worker.
        let mut third = TcpStream::connect(addr).unwrap();
        third.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        third.write_all(get_status("Connection: close\r\n").as_bytes()).unwrap();
        let mut buf = [0u8; 64];
        let early = third.read(&mut buf);
        assert!(
            matches!(&early, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "with both workers busy the third connection is not served yet: {early:?}"
        );
        // Finishing one slow request frees its worker for the third connection.
        slow[0].write_all(b"Connection: close\r\n\r\n").unwrap();
        let mut resp = String::new();
        slow[0].set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        slow[0].read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        third.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut resp = String::new();
        third.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"triples\":2"), "{resp}");
        drop(slow);
    }

    #[test]
    fn an_idle_keep_alive_connection_yields_its_worker() {
        let opts = HttpOptions {
            threads: 1,
            ..HttpOptions::default()
        };
        let (_srv, addr, _serving) = start_with(opts);
        // A kept-alive connection holds the only worker while idle …
        let a = TcpStream::connect(addr).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut aw = a.try_clone().unwrap();
        let mut ar = BufReader::new(a);
        aw.write_all(get_status("").as_bytes()).unwrap();
        let (head, _) = read_one(&mut ar);
        assert!(head.contains("Connection: keep-alive"), "{head}");
        // … until another connection needs it: then it closes, far sooner
        // than its 60 s idle timeout.
        let t0 = Instant::now();
        let mut b = TcpStream::connect(addr).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        b.write_all(get_status("Connection: close\r\n").as_bytes()).unwrap();
        let mut resp = String::new();
        b.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        let mut rest = Vec::new();
        ar.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the idle connection was closed");
    }

    #[test]
    fn a_dripping_client_cannot_hold_a_worker() {
        let opts = HttpOptions {
            threads: 1,
            read_timeout: Duration::from_millis(500),
            ..HttpOptions::default()
        };
        let (_srv, addr, _serving) = start_with(opts);
        // One byte of request head every 100 ms: each read is well within the
        // read timeout, but the head as a whole is not.
        let mut drip = TcpStream::connect(addr).unwrap();
        let dripper = thread::spawn(move || {
            for b in b"GET /status HTTP/1.1\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
                if drip.write_all(&[*b]).is_err() {
                    return true; // the server hung up
                }
                thread::sleep(Duration::from_millis(100));
            }
            let mut buf = [0u8; 256];
            drip.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            // A response, EOF or a reset all mean the server let go; only a
            // timeout would mean it still holds the connection.
            !matches!(
                drip.read(&mut buf),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
            )
        });
        thread::sleep(Duration::from_millis(150));
        let t0 = Instant::now();
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(get_status("Connection: close\r\n").as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert!(dripper.join().unwrap(), "the dripping connection was dropped");
    }

    #[test]
    fn options_preflight_and_cors_headers() {
        let (_srv, addr) = start();
        let resp = http(
            &addr,
            "OPTIONS /file/upload HTTP/1.1\r\nOrigin: http://w\r\nConnection: close\r\n\r\n",
        );
        let (head, body) = split_resp(&resp);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Access-Control-Allow-Origin: *"), "{head}");
        assert!(head.contains("Access-Control-Allow-Methods: POST"), "{head}");
        assert_eq!(body, "ok");
        let resp = http(
            &addr,
            "OPTIONS /file/download/x.nt HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        assert!(resp.contains("Access-Control-Allow-Methods: GET"), "{resp}");
        let resp = http(
            &addr,
            "OPTIONS /sparql HTTP/1.1\r\nAccess-Control-Request-Headers: X-Foo, Content-Type\r\n\
             Connection: close\r\n\r\n",
        );
        assert!(resp.contains("Access-Control-Allow-Headers: X-Foo, Content-Type"), "{resp}");
        // Ordinary responses carry the allow-origin header too.
        let resp = http(&addr, &get_status("Connection: close\r\n"));
        assert!(resp.contains("Access-Control-Allow-Origin: *"), "{resp}");

        // With CORS off: no header, and OPTIONS reaches the router (405 here).
        let opts = HttpOptions {
            cors: false,
            ..HttpOptions::default()
        };
        let (_srv2, addr2, _serving) = start_with(opts);
        let resp = http(&addr2, &get_status("Connection: close\r\n"));
        assert!(!resp.contains("Access-Control"), "{resp}");
        let resp = http(&addr2, "OPTIONS /sparql HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 405"), "{resp}");
    }

    #[test]
    fn the_body_cap_answers_413_before_reading() {
        let opts = HttpOptions {
            max_body: 16,
            ..HttpOptions::default()
        };
        let (_srv, addr, _serving) = start_with(opts);
        let resp = http(
            &addr,
            "POST /update HTTP/1.1\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 413 Payload Too Large"), "{resp}");
        // An absurd Content-Length is refused the same way.
        let resp = http(
            &addr,
            "POST /update HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
        // A malformed one is a plain 400.
        let resp = http(&addr, "POST /update HTTP/1.1\r\nContent-Length: abc\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        // Within the cap, requests work.
        let resp = http(&addr, &get_status("Connection: close\r\n"));
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    }

    #[test]
    fn chunked_request_bodies_and_expect_continue() {
        let (_srv, addr) = start();
        let update = "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }";
        let (a, b) = update.split_at(10);
        let raw = format!(
            "POST /update HTTP/1.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
             {:x}\r\n{a}\r\n{:x};ext=1\r\n{b}\r\n0\r\nX-Trailer: t\r\n\r\n",
            a.len(),
            b.len()
        );
        let resp = http(&addr, &raw);
        assert!(resp.contains("\"changed\":1"), "{resp}");

        // `Expect: 100-continue`: the interim response comes before the body.
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(
            format!(
                "POST /sparql HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                SELECT_O.len()
            )
            .as_bytes(),
        )
        .unwrap();
        let mut interim = [0u8; 25];
        s.read_exact(&mut interim).unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        s.write_all(SELECT_O.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200") && resp.contains("http://ex/b"), "{resp}");
    }

    #[test]
    fn request_parse_reads_raw_bytes() {
        let raw = b"POST /x?a=1 HTTP/1.1\r\nHost: h\r\nContent-Length: 3\r\n\r\nabc";
        let req = Request::parse(&mut &raw[..]).unwrap().unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/x"));
        assert_eq!(req.query.as_deref(), Some("a=1"));
        assert_eq!(req.header("HOST"), Some("h"));
        assert_eq!(req.body, b"abc");
        assert!(Request::parse(&mut &b""[..]).unwrap().is_none());
        let truncated = b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nab";
        assert!(Request::parse(&mut &truncated[..]).is_err());
    }

    #[test]
    fn a_non_ascii_authorization_header_is_rejected_without_a_panic() {
        // A multi-byte character straddling the `Basic ` / `Bearer ` prefix
        // boundary must not make the byte-indexed prefix check panic.
        for value in ["abcde\u{e9}xyz", "abcdef\u{e9}xyz", "\u{e9}", "Basic\u{e9}", "Bearer\u{e9}x"] {
            let raw = format!("GET / HTTP/1.1\r\nAuthorization: {value}\r\n\r\n");
            let req = Request::parse(&mut raw.as_bytes()).unwrap().unwrap();
            assert_eq!(parse_basic_auth(&req), None, "{value:?}");
            assert_eq!(req.bearer_or_session_token(), None, "{value:?}");
            assert!(!req.authorized("u", "p"), "{value:?}");
        }
        // Well-formed headers still work, with the scheme in any case.
        let raw = "GET / HTTP/1.1\r\nAuthorization: bAsIc dTpw\r\n\r\n";
        let req = Request::parse(&mut raw.as_bytes()).unwrap().unwrap();
        assert_eq!(parse_basic_auth(&req), Some(("u".into(), "p".into())));
        assert!(req.authorized("u", "p"));
        let raw = "GET / HTTP/1.1\r\nAuthorization: BEARER tok\u{e9}n\r\n\r\n";
        let req = Request::parse(&mut raw.as_bytes()).unwrap().unwrap();
        assert_eq!(req.bearer_or_session_token().as_deref(), Some("tok\u{e9}n"));
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

    /// Keep-alive works over TLS too: two requests on one TLS connection.
    #[cfg(feature = "tls")]
    #[test]
    fn tls_keep_alive_serves_two_requests() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("gstore_tls_ka_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, CERT_PEM).unwrap();
        std::fs::write(&key, KEY_PEM).unwrap();
        let config = load_tls_config(&cert, &key).unwrap();
        let db = Database::build_from_str("tls_ka", DATA).unwrap();
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
        tls.get_mut().write_all(get_status("").as_bytes()).unwrap();
        let (head, body) = read_one(&mut tls);
        assert!(head.contains("Connection: keep-alive"), "{head}");
        assert!(body.contains("\"triples\":2"), "{body}");
        tls.get_mut().write_all(get_status("").as_bytes()).unwrap();
        let (head, body) = read_one(&mut tls);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(body.contains("\"triples\":2"), "{body}");
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
