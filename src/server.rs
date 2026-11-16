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
//! Parameters come from the URL and form bodies only (see the crate-private
//! `Request`'s *Parameters*): a POSTed `application/sparql-query` /
//! `application/sparql-update` body is the statement, verbatim. The
//! protocol's dataset parameters (`default-graph-uri`, `named-graph-uri`,
//! `using-graph-uri`, `using-named-graph-uri`) are answered `400`: there is
//! no RDF dataset support (FROM / FROM NAMED are not supported either).
//!
//! SELECT/ASK results are content-negotiated (see [`crate::sparql_results`]):
//! JSON (default), XML, CSV, or TSV via the `Accept` header or a `format=`
//! query parameter; CONSTRUCT/DESCRIBE return N-Triples. Connections run
//! concurrently (on the worker pool), and so do reads: the [`Database`] is
//! shared behind a [`RwLock`] (see *Locking* below), so any number of
//! SELECT/ASK/CONSTRUCT/DESCRIBE requests evaluate at once while an UPDATE
//! runs alone. Everything around the statement itself — parsing it, auth,
//! serialization, response compression — runs off the lock.
//!
//! **Locking** (contract C8 of the Phase 6 plan):
//!
//! * A request is parsed into a [`Statement`] **before** any lock is taken
//!   (a syntax error is answered `400` without touching the database). A read
//!   then takes a **shared** guard and runs through [`Database::read_as`]; an
//!   UPDATE takes the **exclusive** guard and runs through
//!   [`Database::execute_as`], and its check_point save happens before that
//!   guard is released. Everything a read reaches inside the database is
//!   `&self` and synchronized there (the query cache, the CSR cache, the
//!   string-index cache, the PFN registry; the settings change only under
//!   the exclusive guard).
//! * `/status` never takes the database lock: it reads counters the server
//!   publishes after every write (`DbCounters`), so it answers while a long
//!   query or an update runs.
//! * Lock order: this server's `RwLock` → the database's internals (its
//!   query / CSR / string-index caches; `persist_lock` → disk sink →
//!   `persist_error` → the pager) → the `DiskTriples` gate → the pager. Never
//!   the reverse: nothing inside the database ever waits for the server's
//!   lock.
//! * No thread ever holds two database guards, takes a guard it already
//!   holds, or upgrades a shared guard to an exclusive one without dropping
//!   the shared guard first. The guards are poison-tolerant
//!   (`read_locked` / `write_locked`): a request that panicked while
//!   holding one does not wedge the server.
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
//! * **CORS**: with [`HttpOptions::cors`] (the default) `OPTIONS` preflights
//!   are answered directly (as upstream does for `/file/upload` and
//!   `/file/download`) and responses carry `Access-Control-Allow-Origin` —
//!   for the origins of [`HttpOptions::cors_allow_origin`] only (none by
//!   default; `*` is upstream's header on every response);
//! * **the browser policy** (secure defaults, each with an upstream-compat
//!   switch in [`HttpOptions`]): a request arriving over loopback must name a
//!   local `Host` (DNS rebinding); a state-changing request with a foreign
//!   `Origin` is refused (CSRF); `GET` never runs a SPARQL UPDATE and
//!   `/update` takes typed bodies only (`allow_get_update`); a loopback peer
//!   forwarding proxy headers is not a local client
//!   (`peer_is_trusted_local`, `trust_proxy_headers`);
//! * **request body caps** ([`HttpOptions::preauth_max_body`] before
//!   authentication, [`HttpOptions::max_body`] for a payload route whose
//!   request is authenticated from its head) and a **body budget** shared by
//!   all connections ([`HttpOptions::body_budget`]; see [`crate::preauth`]):
//!   a larger `Content-Length` is refused with `413` before anything is
//!   allocated, a request finding the budget exhausted gets `503`, and
//!   `Expect: 100-continue` is honoured only after the caps are decided.
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
//!   panicking handler answers `500` with a JSON error, and a poisoned
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
//! **HTTPS:** with the optional `tls` feature, `Server::serve_forever_tls`
//! (and its [`ApiServer`](crate::http_api::ApiServer) counterpart) terminate
//! TLS in-process via `rustls` — build a `rustls::ServerConfig` from a
//! cert+key PEM with `load_tls_config`. The default (zero-dependency) build
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::db::{Database, LoadPolicy, Statement};
use crate::query::{QueryResult, ResultSet};
use crate::sparql_results::{self, json_str, ResultFormat};

const SPARQL_JSON: &str = "application/sparql-results+json";

/// Default maximum accepted request-body size (`Content-Length`) — the default
/// of [`HttpOptions::max_body`]. Bounds the pre-auth allocation in request
/// parsing so a single header can't make the server reserve arbitrary memory or
/// panic on capacity overflow. Shared by the simple server and the multi-DB
/// `http_api` server. (`gserver` sets it to `conf.ini`'s
/// `upload_max_body_size`, 256 MiB by default; a request not authenticated
/// from its head is held to [`HttpOptions::preauth_max_body`].)
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
    /// Minimum average rate, in bytes per second, at which a request body
    /// must arrive (default [`DEFAULT_MIN_BODY_RATE`], 64 KiB/s), after a
    /// grace of [`read_timeout`](Self::read_timeout): `t` after the end of
    /// its head, at least `(t - read_timeout) * min_body_rate` bytes of it
    /// must have been received, or the connection is closed. The allowance
    /// grows with the bytes actually received (what the client declares in
    /// `Content-Length` earns it nothing), so a client trickling a body in
    /// cannot hold a worker of the bounded pool for longer than
    /// `read_timeout` plus what it really sent at that rate — never longer
    /// than `read_timeout + n / min_body_rate` for a body of `n` bytes. `0`
    /// turns this off: then only no single read may wait longer than
    /// `read_timeout`. `conf.ini`: `min_body_rate`.
    ///
    /// The same floor applies to a **streamed response** (a file download,
    /// `write_response_streamed`) read by its client: after a grace of
    /// [`write_timeout`](Self::write_timeout), the bytes the connection has
    /// taken so far must keep up this rate, or the connection is closed. A
    /// client draining a download a few bytes at a time cannot hold its
    /// worker (and its download slot) for longer than `write_timeout + n /
    /// min_body_rate` for `n` bytes.
    pub min_body_rate: u64,
    /// Most connections one client IP address may have open at once, those
    /// waiting for a worker included (default [`DEFAULT_MAX_CONNS_PER_IP`],
    /// 64; `0`: no limit). A further connection from that address is closed
    /// as soon as it is accepted, so one client cannot take every worker and
    /// queue place. Clients behind one NAT or proxy share an address: raise
    /// it for them. `conf.ini`: `max_conns_per_ip`.
    pub max_conns_per_ip: usize,
    /// Maximum request body in bytes (default [`MAX_REQUEST_BODY`]); larger
    /// requests are answered `413` without reading the body. Only a request
    /// authenticated from its head, on a route that carries payloads, may
    /// carry this much: every other one is held to
    /// [`preauth_max_body`](Self::preauth_max_body) (see [`crate::preauth`]).
    /// `gserver`: `upload_max_body_size`.
    pub max_body: usize,
    /// The body cap of a request before it is authenticated (default
    /// [`PREAUTH_MAX_BODY`](crate::preauth::PREAUTH_MAX_BODY), 64 MiB;
    /// never more than [`max_body`](Self::max_body)): every route, every
    /// client whose request head carries no valid credentials. `gserver`:
    /// `preauth_max_body_size`.
    pub preauth_max_body: usize,
    /// The request-body bytes all connections may hold in memory at once
    /// (default [`DEFAULT_BODY_BUDGET`](crate::preauth::DEFAULT_BODY_BUDGET),
    /// 512 MiB; never less than [`max_body`](Self::max_body)), charged as
    /// they arrive: a request that would exceed it is answered `503`.
    /// `gserver`: `request_body_budget`.
    pub body_budget: usize,
    /// CORS processing (default on): answer `OPTIONS` preflights directly
    /// and add `Access-Control-Allow-Origin` to the responses — for the
    /// origins [`cors_allow_origin`](Self::cors_allow_origin) lists only.
    pub cors: bool,
    /// The origins (`scheme://host[:port]`) whose web pages may call the
    /// server cross-origin: they get `Access-Control-Allow-Origin` (and
    /// `Vary: Origin`), their preflights are answered, and their
    /// state-changing requests are accepted. Empty by default: no foreign
    /// origin. `*` restores upstream's `Access-Control-Allow-Origin: *` on
    /// every response (any web page could then read the answers and send
    /// writes through a visitor's browser). `conf.ini`: `cors_allow_origin`.
    pub cors_allow_origin: Vec<String>,
    /// Host names a request arriving over loopback may carry in its `Host`
    /// header besides the ones always accepted — IP literals, `localhost`
    /// (and `*.localhost`), and names without a dot. Any other name is
    /// refused with `403`: a DNS-rebinding page (`http://attacker.example`
    /// re-resolved to 127.0.0.1) cannot reach a loopback server. `*` turns
    /// the check off. Empty by default. `conf.ini`: `allowed_hosts`.
    pub allowed_hosts: Vec<String>,
    /// Upstream behaviour (default off): let `GET /sparql` run a SPARQL
    /// UPDATE and `/update` accept a `text/plain` or `multipart/form-data`
    /// body. Off, an UPDATE must be POSTed (SPARQL 1.1 Protocol), and
    /// `/update` refuses those two types (`415`) — the ones besides forms a
    /// web page can POST cross-site without a CORS preflight; it takes
    /// `application/sparql-update`, a form, an unlabelled body, or another
    /// type (which a browser sends only after a preflight). `conf.ini`:
    /// `allow_get_update`.
    pub allow_get_update: bool,
    /// Treat a loopback peer as a local client even when it forwards
    /// `Forwarded` / `X-Forwarded-For` / `X-Real-IP` (upstream behaviour;
    /// default off). Off, such a request comes from a reverse proxy on this
    /// machine, not from a local client: it loses the loopback privileges
    /// (the unauthenticated single-database mode, root's default password,
    /// the `/api` layer's `trust_inner_localhost`). `conf.ini`:
    /// `trust_proxy_headers`.
    pub trust_proxy_headers: bool,
}

/// The default worker-pool size (gStore's `thread_num` default).
pub const DEFAULT_THREADS: usize = 30;

/// The default of [`HttpOptions::min_body_rate`]: 64 KiB/s.
pub const DEFAULT_MIN_BODY_RATE: u64 = 64 * 1024;

/// The default of [`HttpOptions::max_conns_per_ip`].
pub const DEFAULT_MAX_CONNS_PER_IP: usize = 64;

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            threads: DEFAULT_THREADS,
            queue_len: 128,
            keep_alive_timeout: Duration::from_secs(60),
            keep_alive_max: 100,
            read_timeout: CONN_READ_TIMEOUT,
            write_timeout: CONN_READ_TIMEOUT,
            min_body_rate: DEFAULT_MIN_BODY_RATE,
            max_conns_per_ip: DEFAULT_MAX_CONNS_PER_IP,
            max_body: MAX_REQUEST_BODY,
            preauth_max_body: crate::preauth::PREAUTH_MAX_BODY,
            body_budget: crate::preauth::DEFAULT_BODY_BUDGET,
            cors: true,
            cors_allow_origin: Vec::new(),
            allowed_hosts: Vec::new(),
            allow_get_update: false,
            trust_proxy_headers: false,
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
/// `Arc`): the [`Database`] (shared by readers, exclusive to an UPDATE — see
/// the module's *Locking*) and the optional Basic credentials. Kept separate
/// from the non-`Send` bookkeeping so a worker only needs to clone this `Arc`.
struct ServerInner {
    db: RwLock<Database>,
    /// The served database's name (fixed: the server never renames it).
    name: String,
    /// The database's figures as of its last write, for `/status` (read
    /// without the database lock).
    counters: DbCounters,
    /// Set (under the `db` lock) when the in-memory database has changes that
    /// are not yet saved to [`dir`](Self::dir); cleared by a successful save.
    dirty: AtomicBool,
    /// Optional HTTP Basic credentials (with their failed-login lockout).
    /// `None` ⇒ the server is open.
    auth: Option<SingleAuth>,
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
    /// Raised when a graceful shutdown's grace period ends with requests
    /// still running: every statement runs under it
    /// ([`Database::query_cancellable`]), so one still running then — a
    /// graph function may run for as long as its `maxIter` says — stops at
    /// its next check and releases the database for the final save.
    cancel: Arc<AtomicBool>,
    /// How long one statement may run (`Duration::ZERO`: no limit; see
    /// [`Server::with_query_timeout`]).
    query_timeout: Duration,
    /// The most bytes one serialized answer may take (`0`: no limit; see
    /// [`Server::with_max_result_bytes`]).
    max_result_bytes: u64,
}

/// The single-database server's HTTP Basic credentials and their
/// failed-login lockout ([`LoginLockout`](crate::gapi::ipfilter::LoginLockout),
/// review http#6 / sec#7): failures are counted per presented user name and
/// client address, and while one is locked its requests are answered `429`
/// (with `Retry-After`) without being compared. Clones share the lockout
/// (the pre-body hook of [`crate::preauth`] holds one).
#[derive(Clone)]
pub(crate) struct SingleAuth {
    user: String,
    pass: String,
    lockout: Arc<crate::gapi::ipfilter::LoginLockout>,
}

/// How [`SingleAuth::check`] judged a request's credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SingleAuthCheck {
    /// The credentials match.
    Valid,
    /// Missing or wrong (a wrong one was counted).
    Invalid,
    /// The presented user is locked out at this address (nothing
    /// compared): for how long, and by which lock.
    Locked(crate::gapi::ipfilter::LockRefusal),
}

impl SingleAuth {
    pub(crate) fn new(user: &str, pass: &str) -> SingleAuth {
        SingleAuth {
            user: user.to_string(),
            pass: pass.to_string(),
            lockout: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn lockout_for_tests(&self) -> &crate::gapi::ipfilter::LoginLockout {
        &self.lockout
    }

    /// The user name a decoded Basic token (`user:pass` bytes) presents.
    fn presented_user(decoded: &[u8]) -> String {
        let user = decoded
            .iter()
            .position(|&b| b == b':')
            .map_or(decoded, |i| &decoded[..i]);
        String::from_utf8_lossy(user).into_owned()
    }

    /// Whether the decoded Basic token `decoded` matches — unless its user is
    /// locked out at `ip` (then `false`, without comparing). Records nothing:
    /// the pre-body hook's check, which the handler repeats after the body.
    pub(crate) fn accepts(&self, decoded: &[u8], ip: &str) -> bool {
        self.lockout
            .refusal(&Self::presented_user(decoded), ip)
            .is_none()
            && ct_eq(decoded, format!("{}:{}", self.user, self.pass).as_bytes())
    }

    /// Check `req`'s `Authorization: Basic` header from client `ip`, and
    /// record the outcome: a wrong one counts against its user name at
    /// `ip`, a valid one clears that count. A request without credentials is
    /// [`Invalid`](SingleAuthCheck::Invalid) without counting (a browser's
    /// first request before it prompts).
    pub(crate) fn check(&self, req: &Request, ip: &str) -> SingleAuthCheck {
        let Some(decoded) = req.basic_token() else {
            return SingleAuthCheck::Invalid;
        };
        let user = Self::presented_user(&decoded);
        if let Some(refusal) = self.lockout.refusal_detail(&user, ip) {
            return SingleAuthCheck::Locked(refusal);
        }
        if ct_eq(&decoded, format!("{}:{}", self.user, self.pass).as_bytes()) {
            self.lockout.note_success(&user, ip);
            SingleAuthCheck::Valid
        } else {
            self.lockout.note_failure(&user, ip);
            SingleAuthCheck::Invalid
        }
    }
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
        let counters = DbCounters::default();
        counters.publish(&db);
        Ok(Server {
            inner: Arc::new(ServerInner {
                name: db.name().to_string(),
                db: RwLock::new(db),
                counters,
                dirty: AtomicBool::new(false),
                auth: None,
                dir: None,
                check_point: true,
                closing: AtomicBool::new(false),
                cancel: Arc::new(AtomicBool::new(false)),
                query_timeout: crate::config::DEFAULT_QUERY_TIMEOUT,
                max_result_bytes: crate::config::DEFAULT_MAX_RESULT_BYTES,
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
        write_locked(&self.inner.db).set_load_policy(policy);
        self
    }

    /// Apply query-evaluation settings to the served database: the
    /// integer-division mode, the query-cache budget, the Top-K /
    /// early-termination options and the graph-function output bound
    /// ([`Database::apply_query_settings`]). `gserver` passes `conf.ini`'s
    /// `[query]` section ([`crate::config::GStoreConfig::query_settings`]);
    /// without this call the database keeps the settings it was given.
    pub fn with_query_settings(self, settings: crate::db::QuerySettings) -> Server {
        write_locked(&self.inner.db).apply_query_settings(&settings);
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

    /// Stop a statement that runs longer than `timeout` (`conf.ini`'s
    /// `query_timeout`; default [`DEFAULT_QUERY_TIMEOUT`](crate::config::DEFAULT_QUERY_TIMEOUT),
    /// `Duration::ZERO` = no limit): it is cancelled at its next check,
    /// releasing the database — so an UPDATE queued behind a runaway read
    /// runs — and answered `503` "query timed out" (review sec#10).
    pub fn with_query_timeout(mut self, timeout: Duration) -> Server {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.query_timeout = timeout;
        }
        self
    }

    /// Refuse an answer whose serialization passes `bytes` (`conf.ini`'s
    /// `max_result_bytes`; default
    /// [`DEFAULT_MAX_RESULT_BYTES`](crate::config::DEFAULT_MAX_RESULT_BYTES),
    /// `0` = no limit): a buffered answer is replaced by `503` "result too
    /// large" as soon as it passes the budget; a streamed one
    /// (`?stream=true`, its status already sent) is cut off there, without
    /// the closing chunk (review sec#10).
    pub fn with_max_result_bytes(mut self, bytes: u64) -> Server {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.max_result_bytes = bytes;
        }
        self
    }

    /// Require HTTP Basic auth with these credentials on every request.
    /// Failed attempts are throttled per presented user name and client
    /// address: after four, that name is locked for the address — 1 s at
    /// first, doubling with each further failure up to 15 min — and its
    /// requests are answered `429` without being checked (review http#6).
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
            inner.auth = Some(SingleAuth::new(user, pass));
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
        self.life.request_plain_stop();
    }

    /// Accept connections until [`shutdown`](Self::shutdown), serving them on
    /// the bounded worker pool (see [`HttpOptions`]) so a slow query on one
    /// connection does not block the others (reads share the [`Database`],
    /// an UPDATE has it to itself — see the module's *Locking*). On shutdown the listener is closed, idle
    /// kept-alive connections are closed, in-flight requests are given up to the
    /// grace period to finish (statements still running then are cancelled),
    /// unsaved changes are saved, and a [`ShutdownReport`] is returned.
    pub fn serve_forever(&self) -> ShutdownReport {
        // The body caps, budget and pre-body hook of this run (see
        // `crate::preauth`).
        let ingest = crate::preauth::single_db_ingest(&self.http, &self.inner.auth, self.addr);
        let (drained, workers) =
            run_accept_loop(&self.listener, &self.life, self.grace, &self.http, |acc| {
                let inner = Arc::clone(&self.inner);
                let mut acc = acc;
                acc.env.ingest = Arc::clone(&ingest);
                Box::new(move || {
                    let ctl = acc.stream.try_clone().ok();
                    serve_connection(
                        acc.stream,
                        ctl.as_ref(),
                        Some(acc.peer),
                        acc.guard,
                        &acc.env,
                        |s, req, meta| inner.handle(s, req, meta),
                    );
                })
            });
        let report = self.finish_shutdown(drained);
        join_workers(workers);
        report
    }

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS
    /// in-process, wrapping every accepted connection in a `rustls` stream built
    /// from `config` (see [`load_tls_config`]). Requires the `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        // The body caps, budget and pre-body hook of this run (see
        // `crate::preauth`).
        let ingest = crate::preauth::single_db_ingest(&self.http, &self.inner.auth, self.addr);
        let (drained, workers) =
            run_accept_loop(&self.listener, &self.life, self.grace, &self.http, |acc| {
                let inner = Arc::clone(&self.inner);
                let config = Arc::clone(&config);
                let mut acc = acc;
                acc.env.ingest = Arc::clone(&ingest);
                Box::new(move || {
                    let Ok(conn) = rustls::ServerConnection::new(config) else {
                        return;
                    };
                    let ctl = acc.stream.try_clone().ok();
                    let tls = rustls::StreamOwned::new(conn, acc.stream);
                    serve_connection(
                        tls,
                        ctl.as_ref(),
                        Some(acc.peer),
                        acc.guard,
                        &acc.env,
                        |s, req, meta| inner.handle(s, req, meta),
                    );
                })
            });
        let report = self.finish_shutdown(drained);
        join_workers(workers);
        report
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
        if !drained {
            // Past the grace period: stop the statements still running, so
            // the save below does not wait for them (see `cancel`).
            self.inner.cancel.store(true, Ordering::SeqCst);
        }
        if self.inner.dirty.load(Ordering::SeqCst) {
            // Exclusive: no update runs while the final save does.
            let db = write_locked(&self.inner.db);
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

/// Join a stopped server's connection workers (at most
/// [`THREAD_JOIN_WAIT`]), so an embedder that stops a server and starts
/// another in the same process is not left with the old one's threads
/// (and what they hold) — logging any still serving a connection then.
pub(crate) fn join_workers(workers: WorkerThreads) {
    let lingering = workers.join(THREAD_JOIN_WAIT);
    if lingering > 0 {
        crate::gs_warn!(
            "shutdown: {lingering} connection worker thread(s) still serving a connection were \
             left to finish on their own"
        );
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

    /// Request a graceful shutdown (idempotent, non-blocking). It also calls
    /// off a [`restart`](Self::restart) requested earlier.
    pub fn shutdown(&self) {
        self.life.request_plain_stop();
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

    /// An explicit stop ([`ShutdownHandle::shutdown`], a signal, `/shutdown`):
    /// [`request_stop`](Self::request_stop), and a restart requested earlier
    /// is called off — the latest request wins, so an operator stopping the
    /// server while a `/restart` is still shutting it down gets a stop.
    pub(crate) fn request_plain_stop(&self) {
        self.restart.store(false, Ordering::SeqCst);
        self.request_stop();
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
    /// The socket (it dereferences to its [`TcpStream`]); serve the
    /// connection on it, directly or under TLS, so that the request deadlines
    /// apply to every socket read (see [`ConnSocket`]).
    pub(crate) stream: ConnSocket,
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
    /// The body caps, the body budget and the pre-body hook (see
    /// [`crate::preauth`]); a server's `serve_forever` installs its own.
    pub(crate) ingest: Arc<crate::preauth::Ingest>,
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

/// A fixed set of worker threads fed through a bounded queue. Closing the
/// pool ([`close`](Self::close)) closes the queue: the workers finish what
/// is queued, then exit — and the threads are handed back to be joined.
struct WorkerPool {
    /// `None` when not a single worker thread could be started — jobs then run
    /// inline on the accepting thread (degraded, but still serving).
    tx: Option<SyncSender<Job>>,
    waiting: Arc<AtomicUsize>,
    threads: WorkerThreads,
}

/// The threads of a closed [`WorkerPool`] (or of any set of server threads
/// that count their exits in `exited`), to be joined, bounded, once a
/// server stopped (review conc#14).
pub(crate) struct WorkerThreads {
    threads: Vec<thread::JoinHandle<()>>,
    /// How many of them ended (each counts itself when its loop returns, or
    /// unwinds), and its notification.
    exited: Arc<(Mutex<usize>, Condvar)>,
}

impl WorkerThreads {
    fn new() -> WorkerThreads {
        WorkerThreads {
            threads: Vec::new(),
            exited: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    /// Wait — at most `wait` — for every thread to end, and join them.
    /// Returns how many were still running (left detached: each ends with
    /// the connection it serves).
    pub(crate) fn join(self, wait: Duration) -> usize {
        let deadline = Instant::now() + wait;
        let total = self.threads.len();
        let (count, cv) = &*self.exited;
        let mut ended = locked(count);
        while *ended < total {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            ended = cv
                .wait_timeout(ended, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        // Every thread counted its exit: each has left its loop (joining
        // waits only for its last steps). Otherwise join the ones done.
        let all = *ended >= total;
        drop(ended);
        let mut lingering = 0;
        for t in self.threads {
            if all || t.is_finished() {
                let _ = t.join();
            } else {
                lingering += 1;
            }
        }
        lingering
    }
}

/// How long a server that stopped waits for each kind of its threads (its
/// connection workers, its background statistics refreshes) to end before
/// it returns from `serve_forever` without them.
pub(crate) const THREAD_JOIN_WAIT: Duration = Duration::from_secs(2);

impl WorkerPool {
    fn start(threads: usize, queue_len: usize, waiting: Arc<AtomicUsize>) -> WorkerPool {
        let (tx, rx) = mpsc::sync_channel::<Job>(queue_len);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = WorkerThreads::new();
        for i in 0..threads.max(1) {
            let rx = Arc::clone(&rx);
            let waiting = Arc::clone(&waiting);
            let exited = Arc::clone(&handles.exited);
            let spawned = thread::Builder::new()
                .name(format!("gstore-http-{i}"))
                .stack_size(WORKER_STACK_SIZE)
                .spawn(move || worker_loop(&rx, &waiting, &exited));
            if let Ok(t) = spawned {
                handles.threads.push(t);
            }
        }
        WorkerPool {
            tx: (!handles.threads.is_empty()).then_some(tx),
            waiting,
            threads: handles,
        }
    }

    /// Close the queue — the workers serve what is queued, then exit — and
    /// hand back their threads.
    fn close(self) -> WorkerThreads {
        drop(self.tx);
        self.threads
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

fn worker_loop(rx: &Mutex<Receiver<Job>>, waiting: &AtomicUsize, exited: &(Mutex<usize>, Condvar)) {
    /// Counts the worker's exit, also when it unwinds.
    struct Exit<'a>(&'a (Mutex<usize>, Condvar));
    impl Drop for Exit<'_> {
        fn drop(&mut self) {
            *locked(&self.0 .0) += 1;
            self.0 .1.notify_all();
        }
    }
    let _exit = Exit(exited);
    loop {
        let job = locked(rx).recv();
        let Ok(job) = job else {
            return; // the pool was closed and the queue is drained
        };
        waiting.fetch_sub(1, Ordering::SeqCst);
        // A panic is contained to this connection; the worker keeps serving.
        if catch_unwind(AssertUnwindSafe(job)).is_err() {
            crate::gs_error!("a connection handler panicked; the connection was dropped");
        }
    }
}

/// The shared accept loop: turn every accepted connection (with its timeouts
/// set and an [`InFlightGuard`]) into a job with `make_job` and run it on a
/// bounded [`WorkerPool`] (sized by `opts`), until a shutdown is requested;
/// then close the listener and wait — at most `grace` — for the in-flight
/// requests to finish. Returns whether they all did, and the pool's worker
/// threads (for the caller to join once its shutdown is done, see
/// [`WorkerThreads::join`]).
pub(crate) fn run_accept_loop(
    listener: &Mutex<Option<TcpListener>>,
    life: &Arc<Lifecycle>,
    grace: Duration,
    opts: &HttpOptions,
    mut make_job: impl FnMut(Accepted) -> Job,
) -> (bool, WorkerThreads) {
    let mut workers = WorkerThreads::new();
    // Accept on a clone of the socket so the `Mutex` is not held while blocked.
    let sock = locked(listener).as_ref().and_then(|l| l.try_clone().ok());
    if let Some(sock) = sock {
        let waiting = Arc::new(AtomicUsize::new(0));
        let env = ConnEnv {
            life: Arc::clone(life),
            opts: Arc::new(opts.clone()),
            ingest: Arc::new(crate::preauth::Ingest::new(opts, None)),
            waiting: Arc::clone(&waiting),
        };
        let pool = WorkerPool::start(opts.threads, opts.queue_len, waiting);
        // When `accept` failures were last logged (at most once a second).
        let mut accept_error_logged: Option<std::time::Instant> = None;
        // The per-address cap, and when a refusal was last logged (at most
        // once a second).
        let per_ip = IpLimiter::new(opts.max_conns_per_ip);
        let mut refusal_logged: Option<Instant> = None;
        let limits = ReadLimits {
            read_timeout: opts.read_timeout,
            write_timeout: opts.write_timeout,
            min_body_rate: opts.min_body_rate,
        };
        while !life.stopping() {
            match sock.accept() {
                Ok((stream, peer)) => {
                    if life.stopping() {
                        break; // the wake-up self-connection (or a late client)
                    }
                    // An address already holding its share of connections
                    // gets no more: closed at once, before any worker or
                    // queue place is spent on it.
                    let Some(slot) = per_ip.admit(peer.ip()) else {
                        if refusal_logged.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                            crate::gs_warn!(
                                "refused a connection from {}: it already has {} open \
                                 ([server] max_conns_per_ip)",
                                peer.ip(),
                                opts.max_conns_per_ip
                            );
                            refusal_logged = Some(Instant::now());
                        }
                        drop(stream);
                        continue;
                    };
                    let _ = stream.set_read_timeout(Some(opts.read_timeout));
                    let _ = stream.set_write_timeout(Some(opts.write_timeout));
                    // Responses go out as one write; don't let Nagle hold back
                    // a kept-alive connection's next response.
                    let _ = stream.set_nodelay(true);
                    let job = make_job(Accepted {
                        stream: ConnSocket::new(stream, limits, slot),
                        peer,
                        guard: life.enter(),
                        env: env.clone(),
                    });
                    pool.submit(job, life);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // E.g. EMFILE under a connection flood: back off instead of
                // spinning on the failing accept.
                Err(e) => {
                    if accept_error_logged.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                        crate::gs_warn!("accepting a connection failed: {e}; backing off");
                        accept_error_logged = Some(std::time::Instant::now());
                    }
                    thread::sleep(Duration::from_millis(10))
                }
            }
        }
        // Queued connections are still served (they hold in-flight guards);
        // the workers exit once the queue is drained.
        workers = pool.close();
    }
    // Close both descriptors so new connections are refused from now on.
    locked(listener).take();
    (life.wait_idle(grace), workers)
}

// --- per-connection loop: keep-alive, CORS, body cap -------------------------

/// How often an idle kept-alive connection re-checks for a shutdown or for
/// connections waiting for its worker.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// How long an idle kept-alive connection looks for a request on its way
/// before it yields its worker to a waiting connection (see
/// [`wait_for_request`]).
const QUEUED_LOOK: Duration = Duration::from_millis(1);

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
    /// The request (head or body) did not arrive in time — a read deadline
    /// passed (see [`ConnSocket`]).
    const TIMED_OUT: BadRequest = BadRequest {
        status: 400,
        message: "bad request: the request did not arrive in time",
    };
    /// A header line that is not `name: value` with a token name right before
    /// the colon (RFC 9112 §5.1), e.g. whitespace before the colon or an
    /// obs-fold continuation line (§5.2).
    const BAD_FIELD: BadRequest = BadRequest {
        status: 400,
        message: "bad request: malformed header field",
    };
    /// A control character (other than HTAB) in the request line, a header
    /// line, a chunk-size line or a trailer — a bare CR or a NUL included
    /// (RFC 9112 §2.2, RFC 9110 §5.5).
    const BAD_CONTROL: BadRequest = BadRequest {
        status: 400,
        message: "bad request: control character in a request line, header or chunk line",
    };
    /// A body length that cannot be determined unambiguously (RFC 9112 §6):
    /// `Content-Length` and `Transfer-Encoding` together, differing or
    /// non-decimal `Content-Length` values, a transfer coding other than
    /// exactly `chunked`, or a malformed chunk size.
    const BAD_FRAMING: BadRequest = BadRequest {
        status: 400,
        message: "bad request: ambiguous or unsupported message framing \
                  (Content-Length / Transfer-Encoding)",
    };

    /// A failed read: a timeout ([`TIMED_OUT`](Self::TIMED_OUT)) or else a
    /// malformed or truncated request.
    fn from_read(e: &io::Error) -> BadRequest {
        if matches!(
            e.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            BadRequest::TIMED_OUT
        } else {
            BadRequest::MALFORMED
        }
    }
}

/// Per-request connection facts a request handler must honour: the headers
/// to add to its response (`Connection`/`Keep-Alive`, CORS) and the verdict
/// of the connection's browser policy ([`HttpOptions`]: `Host` / `Origin`
/// admission, whether the client counts as local, `allow_get_update`). The
/// client's address is available through [`current_peer`].
pub(crate) struct ConnMeta {
    headers: Vec<(&'static str, String)>,
    /// The `Access-Control-Allow-Origin` value of this response (also in
    /// `headers`): `*`, or the request's own `Origin` when it is listed.
    allow_origin: Option<String>,
    /// Why the request must be refused (`403`) before it is routed: a
    /// foreign `Host` on a loopback connection, or a foreign `Origin` on a
    /// state-changing request.
    refusal: Option<&'static str>,
    /// The client is local ([`peer_is_trusted_local`]).
    local_trusted: bool,
    /// The client is a reverse proxy on this machine whose forwarding
    /// headers are not trusted (a loopback peer, not local).
    forwarded: bool,
    /// [`HttpOptions::allow_get_update`].
    allow_get_update: bool,
}

impl ConnMeta {
    /// The facts for one response: keep-alive (`remaining` requests left on
    /// the connection, `idle` timeout), and — for a parsed `req` — the CORS
    /// header and the policy verdict of `opts`. Without a request (a
    /// malformed one is answered) the policy allows nothing.
    pub(crate) fn new(
        keep_alive: bool,
        idle: Duration,
        remaining: usize,
        opts: &HttpOptions,
        req: Option<&Request>,
    ) -> ConnMeta {
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
        let origin = req.and_then(|r| r.header("origin"));
        let allow_origin = if !opts.cors {
            None
        } else if origin_listed("*", &opts.cors_allow_origin) {
            Some("*".to_string())
        } else {
            origin
                .filter(|o| origin_listed(o, &opts.cors_allow_origin))
                .map(str::to_string)
        };
        if let Some(o) = &allow_origin {
            headers.push(("Access-Control-Allow-Origin", o.clone()));
        }
        if opts.cors && !opts.cors_allow_origin.is_empty() && allow_origin.as_deref() != Some("*") {
            // The answer depends on the request's Origin: caches must know.
            headers.push(("Vary", "Origin".to_string()));
        }
        let peer = current_peer();
        let loopback = peer.is_some_and(|p| ip_is_loopback(p.ip()));
        ConnMeta {
            headers,
            allow_origin,
            refusal: req.and_then(|r| admission_refusal(r, loopback, opts)),
            local_trusted: req
                .is_some_and(|r| peer_is_trusted_local(peer, r, opts.trust_proxy_headers)),
            forwarded: req
                .is_some_and(|r| loopback && r.is_forwarded() && !opts.trust_proxy_headers),
            allow_get_update: opts.allow_get_update,
        }
    }

    /// `extra` followed by this connection's headers, ready for
    /// [`write_response_ext`] / [`write_response_maybe_gzip`].
    pub(crate) fn headers<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut out = extra.to_vec();
        out.extend(self.headers.iter().map(|(k, v)| (*k, v.as_str())));
        out
    }

    /// The CORS headers alone (for a streamed response, which always closes).
    fn cors_headers(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        if let Some(o) = &self.allow_origin {
            out.push(("Access-Control-Allow-Origin", o.as_str()));
        }
        out.extend(
            self.headers
                .iter()
                .filter(|(k, _)| *k == "Vary")
                .map(|(k, v)| (*k, v.as_str())),
        );
        out
    }

    /// Why the request is refused before routing (answer `403`), if it is:
    /// see [`HttpOptions::allowed_hosts`] and
    /// [`HttpOptions::cors_allow_origin`].
    pub(crate) fn refusal(&self) -> Option<&'static str> {
        self.refusal
    }

    /// Whether the client counts as local ([`peer_is_trusted_local`]): the
    /// loopback privileges (root's default password, the `/api` layer's
    /// `trust_inner_localhost`) are granted to it only.
    pub(crate) fn local_trusted(&self) -> bool {
        self.local_trusted
    }

    /// Whether the client is a reverse proxy on this machine (a loopback
    /// peer forwarding `Forwarded` / `X-Forwarded-For` / `X-Real-IP`) whose
    /// headers are not trusted ([`HttpOptions::trust_proxy_headers`]).
    pub(crate) fn forwarded(&self) -> bool {
        self.forwarded
    }

    /// [`HttpOptions::allow_get_update`].
    pub(crate) fn allow_get_update(&self) -> bool {
        self.allow_get_update
    }
}

/// Message of a refused `Host` (see [`HttpOptions::allowed_hosts`]).
const HOST_REFUSED: &str = "request refused: a request arriving over loopback must name a local \
     host (an IP address, localhost, or a name without a dot) in its Host header; add the \
     name to [server] allowed_hosts to serve it";

/// Message of a refused cross-origin write (see
/// [`HttpOptions::cors_allow_origin`]).
const ORIGIN_REFUSED: &str = "request refused: a state-changing request from a foreign web \
     origin (its Origin header names neither this server nor an origin listed in [server] \
     cors_allow_origin)";

/// Message of a refused proxied request to a server without credentials
/// (see [`HttpOptions::trust_proxy_headers`]).
const FORWARDED_REFUSED: &str = "request refused: this server has no authentication and serves \
     local clients only, but the request was forwarded by a proxy (Forwarded / X-Forwarded-For \
     / X-Real-IP); serve it through the authenticated multi-database API, or set [server] \
     trust_proxy_headers=on if the proxy forwards local clients only";

/// Message of a refused `GET` UPDATE (see [`HttpOptions::allow_get_update`]).
pub(crate) const GET_UPDATE_REFUSED: &str = "a SPARQL UPDATE is not run over GET (SPARQL 1.1 \
     Protocol): POST it to /update (set [server] allow_get_update=on to restore the upstream \
     behaviour)";

/// Message of a refused `/update` body type (`415`; see
/// [`HttpOptions::allow_get_update`] and [`BodyKind::Simple`]).
pub(crate) const UPDATE_BODY_REFUSED: &str = "/update takes an application/sparql-update or \
     application/x-www-form-urlencoded body, not text/plain or multipart/form-data (set \
     [server] allow_get_update=on to accept them, as upstream does)";

/// Whether the SPARQL text `q` is an UPDATE, by a parse alone (nothing
/// runs). A text holding none of the keywords an update starts with
/// (`INSERT`, `DELETE`, `LOAD`, `CLEAR`, `DROP`, `CREATE`, `WITH`, `ADD`,
/// `MOVE`, `COPY`, in any case) is a query without being parsed, so a
/// `GET` query is not parsed twice.
pub(crate) fn is_update_text(q: &str) -> bool {
    const STARTS: [&[u8]; 10] = [
        b"INSERT", b"DELETE", b"LOAD", b"CLEAR", b"DROP", b"CREATE", b"WITH", b"ADD", b"MOVE",
        b"COPY",
    ];
    let b = q.as_bytes();
    // A backslash (an escape sequence) is parsed whatever it spells.
    let maybe = b.contains(&b'\\')
        || STARTS
            .iter()
            .any(|kw| b.windows(kw.len()).any(|w| w.eq_ignore_ascii_case(kw)));
    maybe && Statement::parse(q).is_ok_and(|stmt| stmt.is_update())
}

/// Whether `origin` is one of `list` (case-insensitive, a trailing `/`
/// ignored). `*` in the list matches only the query `*`.
fn origin_listed(origin: &str, list: &[String]) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('/').to_ascii_lowercase();
    let origin = norm(origin);
    list.iter().any(|o| norm(o) == origin)
}

/// The browser policy's verdict on a request (`None`: admitted): on a
/// loopback connection the `Host` must be local; a state-changing request
/// (any method but `GET`, `HEAD` and `OPTIONS`) with an `Origin` header must
/// come from this server's own origin or a listed one.
fn admission_refusal(req: &Request, loopback: bool, opts: &HttpOptions) -> Option<&'static str> {
    browser_refusal(
        &req.method,
        req.header("host"),
        req.header("origin"),
        loopback,
        opts,
    )
}

/// [`admission_refusal`] of a request's `method` and its `Host` and
/// `Origin` header values — also applied to a request head before its body
/// is read ([`crate::preauth`]), so a request refused after its body never
/// earns the upload cap.
pub(crate) fn browser_refusal(
    method: &str,
    host: Option<&str>,
    origin: Option<&str>,
    loopback: bool,
    opts: &HttpOptions,
) -> Option<&'static str> {
    if loopback && host.is_some_and(|h| !host_allowed(h, &opts.allowed_hosts)) {
        return Some(HOST_REFUSED);
    }
    if !matches!(method, "GET" | "HEAD" | "OPTIONS") {
        if let Some(origin) = origin {
            // `get`, not slicing: a header may hold any UTF-8, and this runs
            // outside the handlers' panic isolation.
            let same = host.is_some_and(|h| {
                let o = origin.trim().trim_end_matches('/');
                ["http://", "https://"].iter().any(|scheme| {
                    o.get(..scheme.len())
                        .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
                        && o.get(scheme.len()..)
                            .is_some_and(|rest| rest.eq_ignore_ascii_case(h.trim()))
                })
            });
            if !same
                && !origin_listed(origin, &opts.cors_allow_origin)
                && !origin_listed("*", &opts.cors_allow_origin)
            {
                return Some(ORIGIN_REFUSED);
            }
        }
    }
    None
}

/// Whether a `Host` header value names a local host: an IP literal (a
/// browser sends one only for a URL naming that address, so it cannot be a
/// rebound name), `localhost` or `*.localhost`, a name without a dot (not
/// registrable in public DNS), or a name of `allowed` (`*`: any).
fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return true; // as no Host header: no name to check
    }
    if host.starts_with('[') {
        // `[v6]` or `[v6]:port`
        return host.find(']').is_some_and(|end| {
            host[1..end].parse::<Ipv6Addr>().is_ok()
                && (host.len() == end + 1 || host[end + 1..].starts_with(':'))
        });
    }
    let name = match host.rsplit_once(':') {
        Some((n, port)) if port.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => host,
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name.parse::<Ipv4Addr>().is_ok() || name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    if !name.is_empty() && !name.contains('.') && !name.contains(':') {
        return true;
    }
    allowed.iter().any(|a| {
        let a = a.trim().trim_end_matches('.');
        a == "*" || a.eq_ignore_ascii_case(&name)
    })
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

/// The headers a reverse proxy forwards its client's address in (lowercase):
/// a loopback peer sending one of them speaks for another client (see
/// [`peer_is_trusted_local`]; the pre-body hook of [`crate::preauth`] applies
/// the same rule to a request head).
pub(crate) const FORWARDING_HEADERS: [&str; 3] = ["forwarded", "x-forwarded-for", "x-real-ip"];

/// Whether the client of `req` counts as **local** — the one predicate
/// behind the loopback privileges (the unauthenticated single-database
/// mode, root's default password, the `/api` layer's
/// `trust_inner_localhost`): its TCP `peer` is a loopback address, and it
/// is not a reverse proxy on this machine — it forwards none of
/// `Forwarded`, `X-Forwarded-For`, `X-Real-IP` — unless
/// `trust_proxy_headers` (upstream behaviour: every loopback peer is local).
pub(crate) fn peer_is_trusted_local(
    peer: Option<SocketAddr>,
    req: &Request,
    trust_proxy_headers: bool,
) -> bool {
    peer.is_some_and(|p| ip_is_loopback(p.ip())) && (trust_proxy_headers || !req.is_forwarded())
}

/// A loopback address, IPv4-mapped IPv6 (`::ffff:127.0.0.1`, a dual-stack
/// listener's view of an IPv4 client) included.
pub(crate) fn ip_is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
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
    mut handle: impl FnMut(
        &mut ResponseSink<S>,
        Result<Request, BadRequest>,
        &ConnMeta,
    ) -> io::Result<bool>,
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
        inner: ResponseSink::new(stream),
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
        // `_body` keeps the body's share of the budget until the response is
        // written (the end of the iteration).
        let (req, info, _body) = match read_request(&mut reader, &env.ingest, opts.read_timeout) {
            Ok(None) => return, // closed before sending anything
            Ok(Some(r)) => r,
            Err(bad) => {
                let meta = ConnMeta::new(false, idle, 0, opts, None);
                let stream = &mut reader.get_mut().inner;
                stream.start_response(false);
                let _ = handle(stream, Err(bad), &meta);
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
        // While connections wait for a worker this response closes the
        // connection — and says so — giving the worker up, instead of
        // promising keep-alive and closing the idle connection a moment
        // later, when the client may be sending its next request (review
        // http#15). A next request already received is served first.
        let keep = ctl.is_some()
            && !idle.is_zero()
            && served < max_requests
            && wants_keep_alive(&req, info.http10)
            && !life.stopping()
            && (env.waiting.load(Ordering::SeqCst) == 0 || !reader.buffer().is_empty());
        let meta = ConnMeta::new(
            keep,
            idle,
            max_requests.saturating_sub(served),
            opts,
            Some(&req),
        );
        let stream = &mut reader.get_mut().inner;
        // A response to HEAD is written like any other, minus its body.
        stream.start_response(req.method.eq_ignore_ascii_case("HEAD"));
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

/// A connection's transport as the response writers see it: everything
/// passes through, except that while the response to a `HEAD` request is
/// written ([`start_response`](Self::start_response)) its body — whatever
/// follows the empty line that ends the header block — is dropped. Every
/// route can then answer `HEAD` exactly like `GET` (same status and headers,
/// `Content-Length` included, RFC 9110 §9.3.2), and a client reusing the
/// connection never misreads a body it does not expect as the start of the
/// next response.
pub(crate) struct ResponseSink<S> {
    inner: S,
    /// Drop the body of the response being written.
    head_only: bool,
    /// How much of the header terminator `\r\n\r\n` the bytes written so
    /// far end with (`4`: the header block is complete).
    matched: u8,
}

impl<S> ResponseSink<S> {
    fn new(inner: S) -> ResponseSink<S> {
        ResponseSink {
            inner,
            head_only: false,
            matched: 0,
        }
    }

    /// A new response begins; `head_only` for the response to a `HEAD`.
    fn start_response(&mut self, head_only: bool) {
        self.head_only = head_only;
        self.matched = 0;
    }
}

impl<S: Read> Read for ResponseSink<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Write> Write for ResponseSink<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.head_only {
            return self.inner.write(buf);
        }
        // Pass the header block through, up to and including its end.
        let mut n = 0;
        while n < buf.len() && self.matched < 4 {
            self.matched = match (self.matched, buf[n]) {
                (0 | 2, b'\r') => self.matched + 1,
                (1 | 3, b'\n') => self.matched + 1,
                (_, b'\r') => 1,
                _ => 0,
            };
            n += 1;
        }
        self.inner.write_all(&buf[..n])?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The connection's transport plus an optional deadline: while one is set,
/// it is published to the socket reads underneath ([`ConnSocket`]), which wait
/// at most for the time left and fail once it has passed — a TLS handshake's
/// and a TLS record's many socket reads within one read here included — so a
/// request head must arrive *in full* within [`HttpOptions::read_timeout`]:
/// a client dripping one byte at a time cannot hold a worker of the bounded
/// pool indefinitely.
struct Timed<'a, S> {
    inner: S,
    ctl: Option<&'a TcpStream>,
    deadline: Option<Instant>,
}

impl<S: Read> Read for Timed<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(at) = self.deadline else {
            return self.inner.read(buf);
        };
        // Enforced by the `ConnSocket` under `inner` at every socket read,
        // which also makes a passed deadline final for the connection.
        let _deadline = DeadlineScope::enter(Deadline { at, hard: true });
        self.inner.read(buf)
    }
}

/// Wait (at most `idle`) for the next request on a kept-alive connection.
/// `false` — close the connection — on EOF, timeout, a shutdown, or when other
/// connections are waiting for a worker (an idle connection yields its
/// worker) — but only after a last short look ([`QUEUED_LOOK`]) finds
/// nothing on its way: a request already in the socket, in the TLS layer's
/// decrypted buffer or partly received is served, not reset by the close
/// (review http#15; the response before promised keep-alive only while no
/// connection waited, see `serve_connection`). `true` once the next request
/// has begun: its first byte is readable, or — under TLS — bytes of the
/// record carrying it have reached the socket (its head deadline then covers
/// the rest of that record).
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
    let arrived = socket_bytes();
    loop {
        if life.stopping() {
            return false;
        }
        let queued = waiting.load(Ordering::SeqCst) > 0;
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let poll = (deadline - now).min(if queued { QUEUED_LOOK } else { IDLE_POLL });
        let _ = ctl.set_read_timeout(Some(poll));
        // Bounds the whole poll, every socket read under TLS included, so a
        // record trickling in cannot keep this wait from its checks above.
        let _poll = DeadlineScope::enter(Deadline {
            at: now + poll,
            hard: false,
        });
        match reader.fill_buf() {
            Ok(buf) => return !buf.is_empty(),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if socket_bytes() != arrived {
                    return true; // part of a TLS record: the request has begun
                }
                if queued {
                    return false; // nothing on its way: yield the worker
                }
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod keep_alive_tests {
    use super::*;
    use std::net::TcpListener;

    /// A connected pair: (client, server side).
    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    /// review http#15: with a connection queued for a worker, an idle
    /// kept-alive connection is yielded only if nothing is on its way — a
    /// request already in the socket is served.
    #[test]
    fn a_queued_connection_does_not_drop_a_request_already_sent() {
        let life = Lifecycle::new("127.0.0.1:9".parse().unwrap());
        let waiting = AtomicUsize::new(1);
        let (mut client, server) = pair();
        client.write_all(b"GET /status HTTP/1.1\r\n\r\n").unwrap();
        // The bytes are in the server's socket buffer (a blocking peek
        // returns once they are).
        let mut one = [0u8; 1];
        assert_eq!(server.peek(&mut one).unwrap(), 1);
        let ctl = server.try_clone().unwrap();
        let mut reader = BufReader::new(server);
        assert!(wait_for_request(
            &mut reader,
            &ctl,
            &life,
            Duration::from_secs(60),
            &waiting
        ));
        assert!(reader.buffer().starts_with(b"GET /status"));

        // Nothing on its way: yielded after the short look, not after the
        // idle timeout (60 s here; the test's hang guard).
        let (_quiet_client, quiet) = pair();
        let ctl = quiet.try_clone().unwrap();
        let mut reader = BufReader::new(quiet);
        assert!(!wait_for_request(
            &mut reader,
            &ctl,
            &life,
            Duration::from_secs(60),
            &waiting
        ));
    }

    /// The response headers `serve_connection` writes for one request with
    /// `waiting` connections queued for a worker.
    fn response_with_waiting(waiting: usize) -> String {
        let opts = HttpOptions::default();
        let life = Lifecycle::new("127.0.0.1:9".parse().unwrap());
        let env = ConnEnv {
            life: Arc::clone(&life),
            opts: Arc::new(opts.clone()),
            ingest: Arc::new(crate::preauth::Ingest::new(&opts, None)),
            waiting: Arc::new(AtomicUsize::new(waiting)),
        };
        let (mut client, server) = pair();
        client
            .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let ctl = server.try_clone().unwrap();
        let serving = thread::spawn(move || {
            serve_connection(
                &server,
                Some(&ctl),
                None,
                life.enter(),
                &env,
                |s, _req, meta| {
                    write_response_ext(s, 200, "text/plain", &meta.headers(&[]), b"ok")?;
                    Ok(true)
                },
            );
        });
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(
                client.read(&mut byte).unwrap(),
                1,
                "the response ended early"
            );
            head.push(byte[0]);
        }
        // Let the connection end (keep-alive: at its idle wait, by EOF).
        drop(client);
        serving.join().unwrap();
        String::from_utf8(head).unwrap()
    }

    /// review http#15: while a connection waits for a worker, a response
    /// does not promise keep-alive (the connection is closed after it, as
    /// announced); otherwise it does.
    #[test]
    fn a_response_promises_keep_alive_only_while_no_connection_waits() {
        let idle = response_with_waiting(0);
        assert!(idle.contains("Connection: keep-alive"), "{idle}");
        let busy = response_with_waiting(1);
        assert!(busy.contains("Connection: close"), "{busy}");
        assert!(!busy.contains("Keep-Alive"), "{busy}");
    }
}

// --- read deadlines below the transport; the per-address cap ------------------

/// A read deadline, published by the request reader for the socket reads it
/// causes on this worker thread ([`DeadlineScope`], read by [`ConnSocket`]).
#[derive(Debug, Clone, Copy)]
struct Deadline {
    at: Instant,
    /// `true`: the request has failed once `at` passes — the connection
    /// reads nothing more (its head's deadline, see [`Timed`]). `false`: an
    /// idle poll's, after which the waiting just re-checks
    /// ([`wait_for_request`]).
    hard: bool,
}

/// The request body being received on this worker thread ([`BodyScope`]).
#[derive(Debug, Clone, Copy)]
struct BodyClock {
    /// When its first byte was awaited (the end of the head).
    start: Instant,
    /// Its `Content-Length` (`None` for a chunked body): the most its
    /// allowance can grow to.
    declared: Option<u64>,
    /// Bytes read from the socket since `start`.
    received: u64,
}

/// What the request reader on this worker thread expects of the socket
/// reads underneath it. A worker serves one connection at a time, so this
/// is the state of the connection it is serving; it travels beside the
/// readers' signatures (which are generic over any [`BufRead`]) the way the
/// peer address does ([`current_peer`]).
#[derive(Debug, Clone, Copy)]
struct ReadClock {
    deadline: Option<Deadline>,
    body: Option<BodyClock>,
    /// Bytes read from connection sockets on this thread so far (wrapping).
    socket_bytes: u64,
}

thread_local! {
    static READ_CLOCK: Cell<ReadClock> = const {
        Cell::new(ReadClock {
            deadline: None,
            body: None,
            socket_bytes: 0,
        })
    };
}

fn with_read_clock<T>(f: impl FnOnce(&mut ReadClock) -> T) -> T {
    READ_CLOCK.with(|c| {
        let mut clock = c.get();
        let out = f(&mut clock);
        c.set(clock);
        out
    })
}

fn socket_bytes() -> u64 {
    READ_CLOCK.with(|c| c.get().socket_bytes)
}

/// Publishes a [`Deadline`] for the socket reads made while it lives; the
/// previous one is back when it is dropped.
struct DeadlineScope(Option<Deadline>);

impl DeadlineScope {
    fn enter(deadline: Deadline) -> DeadlineScope {
        DeadlineScope(with_read_clock(|c| c.deadline.replace(deadline)))
    }
}

impl Drop for DeadlineScope {
    fn drop(&mut self) {
        let previous = self.0;
        with_read_clock(|c| c.deadline = previous);
    }
}

/// Starts the clock of a request body ([`HttpOptions::min_body_rate`]) for
/// the socket reads made while it lives; the previous state is back when it
/// is dropped.
struct BodyScope(Option<BodyClock>);

impl BodyScope {
    fn enter(declared: Option<u64>) -> BodyScope {
        let clock = BodyClock {
            start: Instant::now(),
            declared,
            received: 0,
        };
        BodyScope(with_read_clock(|c| c.body.replace(clock)))
    }
}

impl Drop for BodyScope {
    fn drop(&mut self) {
        let previous = self.0;
        with_read_clock(|c| c.body = previous);
    }
}

/// The clock of a streamed response being sent on this worker thread
/// ([`write_response_streamed`]): when it started, how many bytes it has,
/// and how many the connection has taken so far.
#[derive(Debug, Clone, Copy)]
struct SendClock {
    start: Instant,
    declared: u64,
    sent: u64,
}

thread_local! {
    static SEND_CLOCK: Cell<Option<SendClock>> = const { Cell::new(None) };
}

/// Starts the clock of a streamed response ([`HttpOptions::min_body_rate`])
/// for the connection socket of this thread; restores the previous one when
/// dropped.
struct SendScope(Option<SendClock>);

impl SendScope {
    fn enter(declared: u64) -> SendScope {
        let clock = SendClock {
            start: Instant::now(),
            declared,
            sent: 0,
        };
        SendScope(SEND_CLOCK.with(|c| c.replace(Some(clock))))
    }
}

impl Drop for SendScope {
    fn drop(&mut self) {
        let previous = self.0;
        SEND_CLOCK.with(|c| c.set(previous));
    }
}

/// The read (and streamed-send) limits of a connection, fixed when it is
/// accepted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadLimits {
    read_timeout: Duration,
    write_timeout: Duration,
    min_body_rate: u64,
}

impl ReadLimits {
    /// When the next bytes of streamed response `send` must have been taken
    /// by the connection: `write_timeout` after its start plus the bytes
    /// taken so far (at most its length) at `min_body_rate` — the rule of
    /// [`body_deadline`](Self::body_deadline) for sending; `None` when the
    /// rate floor is off.
    fn send_deadline(&self, send: &SendClock) -> Option<Instant> {
        if self.min_body_rate == 0 {
            return None;
        }
        let allowance = send.sent.min(send.declared);
        let nanos = u128::from(allowance) * 1_000_000_000 / u128::from(self.min_body_rate);
        let extra = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        send.start
            .checked_add(self.write_timeout)?
            .checked_add(extra)
    }

    /// When the next bytes of `body` must be in: `read_timeout` after its
    /// start plus its allowance — the bytes received so far (at most its
    /// declared length) — at `min_body_rate`; `None` when the rate floor is
    /// off. The allowance is earned, not declared: a client announcing a
    /// large `Content-Length` and dripping it is cut off after about
    /// `read_timeout` (review robust#3's scenario), while one that keeps
    /// sending at the rate keeps moving the deadline ahead of it.
    fn body_deadline(&self, body: &BodyClock) -> Option<Instant> {
        if self.min_body_rate == 0 {
            return None;
        }
        let allowance = body
            .declared
            .map_or(body.received, |len| body.received.min(len));
        let nanos = u128::from(allowance) * 1_000_000_000 / u128::from(self.min_body_rate);
        let extra = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        body.start
            .checked_add(self.read_timeout)?
            .checked_add(extra)
    }
}

/// An accepted connection's socket, the transport every connection is served
/// on — directly, or under TLS. Each of its reads honours the deadline the
/// request reader published for this worker thread: the head's ([`Timed`]),
/// an idle poll's ([`wait_for_request`]) or the body's
/// ([`HttpOptions::min_body_rate`], see [`read_body`]). It narrows the
/// socket's read timeout to the time left before every single socket read,
/// so the deadline also holds where one read above makes many below — a
/// `rustls` stream drives a whole handshake, or waits for a whole record,
/// inside one read — and after a head or body deadline has passed it fails
/// every further read: the connection is done (even writing the error
/// response cannot make a TLS stream wait for an unfinished handshake). It
/// also holds the connection's place in its address's count
/// ([`HttpOptions::max_conns_per_ip`]) until it is dropped — the connection
/// ended. Dereferences to the [`TcpStream`].
pub(crate) struct ConnSocket {
    sock: TcpStream,
    limits: ReadLimits,
    /// A head or body deadline has passed: every read fails (an error
    /// response may still be written).
    expired: bool,
    /// A streamed response's deadline has passed: every read and write
    /// fails.
    send_expired: bool,
    /// The socket's write timeout was narrowed for a streamed response's
    /// deadline (restored to `write_timeout` by the next write outside one).
    write_narrowed: bool,
    _slot: IpSlot,
}

impl ConnSocket {
    fn new(sock: TcpStream, limits: ReadLimits, slot: IpSlot) -> ConnSocket {
        ConnSocket {
            sock,
            limits,
            expired: false,
            send_expired: false,
            write_narrowed: false,
            _slot: slot,
        }
    }

    /// Before a write: while a streamed response is being sent
    /// ([`SendScope`]), narrow the socket's write timeout to the time left
    /// before its deadline ([`ReadLimits::send_deadline`]), or fail once the
    /// deadline has passed (the connection is then done).
    fn before_write(&mut self) -> io::Result<()> {
        if self.send_expired {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let deadline = SEND_CLOCK
            .with(Cell::get)
            .and_then(|send| self.limits.send_deadline(&send));
        match deadline {
            Some(at) => {
                let now = Instant::now();
                if now >= at {
                    self.expired = true;
                    self.send_expired = true;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the client reads the response too slowly ([server] min_body_rate)",
                    ));
                }
                let wait = (at - now).min(self.limits.write_timeout);
                let _ = self.sock.set_write_timeout(Some(wait));
                self.write_narrowed = true;
            }
            None if self.write_narrowed => {
                let _ = self.sock.set_write_timeout(Some(self.limits.write_timeout));
                self.write_narrowed = false;
            }
            None => {}
        }
        Ok(())
    }

    /// After a write of `n` bytes: count them on the streamed response's
    /// clock, if one runs.
    fn wrote(n: usize) {
        SEND_CLOCK.with(|c| {
            if let Some(mut send) = c.get() {
                send.sent = send.sent.saturating_add(n as u64);
                c.set(Some(send));
            }
        });
    }
}

impl std::ops::Deref for ConnSocket {
    type Target = TcpStream;

    fn deref(&self) -> &TcpStream {
        &self.sock
    }
}

impl Read for ConnSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.expired {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let clock = READ_CLOCK.with(Cell::get);
        let body = clock.body.and_then(|b| self.limits.body_deadline(&b));
        let deadline = match (clock.deadline, body) {
            (Some(d), Some(at)) if at < d.at => Some(Deadline { at, hard: true }),
            (Some(d), _) => Some(d),
            (None, Some(at)) => Some(Deadline { at, hard: true }),
            (None, None) => None,
        };
        if let Some(d) = deadline {
            let now = Instant::now();
            if now >= d.at {
                self.expired |= d.hard;
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut wait = d.at - now;
            if clock.body.is_some() {
                // Within a body, no single read waits longer than this either.
                wait = wait.min(self.limits.read_timeout);
            }
            let _ = self.sock.set_read_timeout(Some(wait));
        }
        let n = self.sock.read(buf)?;
        with_read_clock(|c| {
            c.socket_bytes = c.socket_bytes.wrapping_add(n as u64);
            if let Some(body) = &mut c.body {
                body.received = body.received.saturating_add(n as u64);
            }
        });
        Ok(n)
    }
}

impl Write for ConnSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.before_write()?;
        let n = self.sock.write(buf)?;
        ConnSocket::wrote(n);
        Ok(n)
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.before_write()?;
        let n = self.sock.write_vectored(bufs)?;
        ConnSocket::wrote(n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

type OpenPerIp = Arc<Mutex<std::collections::HashMap<IpAddr, usize>>>;

/// The per-address connection cap of one accept loop
/// ([`HttpOptions::max_conns_per_ip`]).
struct IpLimiter {
    max: usize,
    open: OpenPerIp,
}

/// An open connection's place in its address's count, given back when it is
/// dropped (with the connection's [`ConnSocket`]).
pub(crate) struct IpSlot(Option<(OpenPerIp, IpAddr)>);

impl IpLimiter {
    fn new(max: usize) -> IpLimiter {
        IpLimiter {
            max,
            open: Arc::default(),
        }
    }

    /// A place for a connection from `ip`, or `None` when that address
    /// already has `max` connections open. (An IPv4-mapped IPv6 address
    /// counts as its IPv4 address.)
    fn admit(&self, ip: IpAddr) -> Option<IpSlot> {
        if self.max == 0 {
            return Some(IpSlot(None));
        }
        let ip = ip.to_canonical();
        let mut open = locked(&self.open);
        let n = open.entry(ip).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(IpSlot(Some((Arc::clone(&self.open), ip))))
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        if let Some((open, ip)) = &self.0 {
            let mut open = locked(open);
            if let Some(n) = open.get_mut(ip) {
                *n -= 1;
                if *n == 0 {
                    open.remove(ip);
                }
            }
        }
    }
}

/// Whether the client allows the connection to stay open: HTTP/1.1 unless it
/// sent `Connection: close`; HTTP/1.0 only with `Connection: keep-alive`.
fn wants_keep_alive(req: &Request, http10: bool) -> bool {
    let tokens = |name: &str| {
        req.header("connection")
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(name)))
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
/// `/file/download` routes — `200 ok` with `Access-Control-Allow-*` headers —
/// when the request's `Origin` is allowed ([`HttpOptions::cors_allow_origin`]);
/// a preflight from any other origin is refused (`403`, no CORS headers), as
/// is one the browser policy refuses outright (a foreign `Host`). A
/// preflight without `Origin` (no browser sent it) is answered without
/// `Access-Control-Allow-Origin`.
fn write_preflight<W: Write>(stream: &mut W, req: &Request, meta: &ConnMeta) -> io::Result<()> {
    if let Some(why) = meta.refusal() {
        return write_response_ext(
            stream,
            403,
            "text/plain",
            &meta.headers(&[]),
            why.as_bytes(),
        );
    }
    if req.header("origin").is_some() && meta.allow_origin.is_none() {
        let body =
            b"CORS preflight refused: the origin is not listed in [server] cors_allow_origin";
        return write_response_ext(stream, 403, "text/plain", &meta.headers(&[]), body);
    }
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

/// Read one request from a connection. The request line and headers must
/// arrive within `read_timeout` in total; the body must not stall for
/// longer than that between reads and must keep up
/// [`HttpOptions::min_body_rate`] overall (see [`read_body`]; a large upload
/// may take longer than `read_timeout`).
/// `Ok(None)` when the client closed the connection before sending anything.
///
/// Between the head and the body, `ingest` decides the body's cap (see
/// [`crate::preauth`]: the pre-authentication cap, or the upload cap for a
/// request its server's pre-body hook authenticates from the head) — a
/// larger `Content-Length` is answered `413` before a body byte is read, and
/// `Expect: 100-continue` is answered only after that. The body is charged
/// to the server's body budget as it arrives (`503` when it has no room);
/// the returned [`BodyHold`](crate::preauth::BodyHold) keeps that charge
/// until it is dropped.
fn read_request<S: Read + Write>(
    reader: &mut BufReader<Timed<'_, S>>,
    ingest: &crate::preauth::Ingest,
    read_timeout: Duration,
) -> Result<Option<(Request, RequestInfo, crate::preauth::BodyHold)>, BadRequest> {
    reader.get_mut().deadline = Some(Instant::now() + read_timeout);
    let head = read_head(reader, ingest.largest_body());
    reader.get_mut().deadline = None;
    if let Some(ctl) = reader.get_ref().ctl {
        let _ = ctl.set_read_timeout(Some(read_timeout));
    }
    let Some(head) = head? else {
        return Ok(None);
    };
    let mut hold = ingest.hold();
    let has_body = head.chunked || head.content_length > 0;
    let body = if has_body {
        let view = crate::preauth::HeadView {
            method: &head.method,
            path: &head.path,
            query: head.query.as_deref(),
            headers: &head.headers,
        };
        let declared = (!head.chunked).then_some(head.content_length);
        let cap = ingest.body_cap(&view, declared)?;
        if let Some(n) = declared {
            if n > cap.max {
                return Err(cap.too_large);
            }
            ingest.admit(n)?;
        }
        if head.expect_continue {
            let w = &mut reader.get_mut().inner;
            w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .and_then(|_| w.flush())
                .map_err(|_| BadRequest::MALFORMED)?;
        }
        let read = {
            let mut budgeted = crate::preauth::Budgeted::new(reader, &mut hold);
            if head.chunked {
                read_chunked_body(&mut budgeted, cap.max)
            } else {
                read_body(&mut budgeted, head.content_length)
            }
        };
        read.map_err(|e| {
            if hold.exhausted() {
                crate::preauth::Ingest::budget_exhausted()
            } else if e == BadRequest::TOO_LARGE {
                cap.too_large
            } else {
                e
            }
        })?
    } else {
        Vec::new()
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
        hold,
    )))
}

/// Read one bounded CRLF/LF-terminated line (without the terminator).
/// `Ok(None)` at EOF before any byte. A line holding a control character
/// other than HTAB — a bare CR (RFC 9112 §2.2), a NUL, … — is refused: as a
/// header value it could otherwise be reflected into a response header
/// (review robust#14).
fn read_line_bounded<R: BufRead>(r: &mut R) -> Result<Option<String>, BadRequest> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_HEADER_LINE)
        .read_until(b'\n', &mut buf)
        .map_err(|e| BadRequest::from_read(&e))?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(BadRequest::MALFORMED); // over-long line, or EOF mid-line
    }
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(BadRequest::BAD_CONTROL);
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| BadRequest::MALFORMED)
}

/// An RFC 9110 `tchar`: what a header field name is made of.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Split a header line into name and value (RFC 9112 §5): the name is a
/// token directly followed by the colon — whitespace before it (§5.1), an
/// obs-fold continuation line (§5.2) or a line without a colon is refused —
/// and the value loses the optional whitespace around it.
fn split_field(line: &str) -> Result<(&str, &str), BadRequest> {
    let (name, value) = line.split_once(':').ok_or(BadRequest::BAD_FIELD)?;
    if name.is_empty() || !name.bytes().all(is_tchar) {
        return Err(BadRequest::BAD_FIELD);
    }
    Ok((name, value.trim_matches([' ', '\t'])))
}

/// A `Content-Length` value: decimal digits only (RFC 9110 §8.6 — no sign,
/// no list). Digits too many for a `usize` are an absurd length: too large.
fn content_length_value(v: &str) -> Result<usize, BadRequest> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BadRequest::BAD_FRAMING);
    }
    v.parse().map_err(|_| BadRequest::TOO_LARGE)
}

/// The size of a chunk from its chunk-size line (RFC 9112 §7.1): hex digits
/// — no sign, prefix or leading whitespace — optionally followed by
/// whitespace and `;` extensions, which are ignored.
fn chunk_size(line: &str) -> Result<usize, BadRequest> {
    let size = line
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '\t']);
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BadRequest::BAD_FRAMING);
    }
    usize::from_str_radix(size, 16).map_err(|_| BadRequest::TOO_LARGE)
}

/// Read a request line and its header fields. The body's framing must be
/// unambiguous (RFC 9112 §6; review sec#16, http#14) — a front end that
/// framed the request differently could otherwise make this server read part
/// of one request as the next one — so these are refused (`400`, and the
/// connection is closed): `Content-Length` together with
/// `Transfer-Encoding`; `Content-Length` values that differ or are not plain
/// decimal digits; a `Transfer-Encoding` other than exactly `chunked` (the
/// only coding implemented), or in an HTTP/1.0 request; malformed header
/// lines (see [`split_field`]).
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

    let mut content_length: Option<usize> = None;
    // The transfer codings of every `Transfer-Encoding` line, in order.
    let mut codings: Option<Vec<String>> = None;
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
        let (k, v) = split_field(&h)?;
        if k.eq_ignore_ascii_case("content-length") {
            let n = content_length_value(v)?;
            if content_length.is_some_and(|m| m != n) {
                return Err(BadRequest::BAD_FRAMING);
            }
            content_length = Some(n);
        } else if k.eq_ignore_ascii_case("transfer-encoding") {
            // A list; empty elements do not count (RFC 9110 §5.6.1).
            codings.get_or_insert_with(Vec::new).extend(
                v.split(',')
                    .map(|c| c.trim_matches([' ', '\t']))
                    .filter(|c| !c.is_empty())
                    .map(str::to_ascii_lowercase),
            );
        } else if k.eq_ignore_ascii_case("expect") {
            expect_continue = v.eq_ignore_ascii_case("100-continue");
        }
        headers.push((k.to_ascii_lowercase(), v.to_string()));
    }
    let chunked = match codings {
        None => false,
        Some(codings) => {
            if content_length.is_some() || http10 || codings != ["chunked"] {
                return Err(BadRequest::BAD_FRAMING);
            }
            true
        }
    };
    let content_length = content_length.unwrap_or(0);

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

/// Read a `Content-Length` body of `len` bytes. On a connection it must
/// keep up [`HttpOptions::min_body_rate`] on average after a grace of
/// `read_timeout` from here — so it is in within `read_timeout + len /
/// min_body_rate` at the latest, however it trickles in (enforced by
/// [`ConnSocket`]).
fn read_body<R: BufRead>(r: &mut R, len: usize) -> Result<Vec<u8>, BadRequest> {
    // Grow as the bytes actually arrive instead of trusting the header.
    let mut body = Vec::with_capacity(len.min(1 << 20));
    if len > 0 {
        let _clock = BodyScope::enter(Some(len as u64));
        r.by_ref()
            .take(len as u64)
            .read_to_end(&mut body)
            .map_err(|e| BadRequest::from_read(&e))?;
        if body.len() != len {
            return Err(BadRequest::MALFORMED); // truncated
        }
    }
    Ok(body)
}

/// The framing a chunked body may carry besides its data
/// ([`read_chunked_body`]): chunk-size lines with their extensions, the CRLFs
/// and the trailer fields may take [`CHUNK_FRAMING_MIN`] plus a sixteenth of
/// the body cap — ordinary chunking (a few bytes per chunk of kilobytes)
/// stays far below, and a body near its cap in chunks of 128 bytes or more
/// still fits.
fn chunk_framing_allowance(max_body: usize) -> usize {
    CHUNK_FRAMING_MIN.saturating_add(max_body / 16)
}

/// The fixed part of [`chunk_framing_allowance`]: 64 KiB.
const CHUNK_FRAMING_MIN: usize = 64 * 1024;

/// Decode a `Transfer-Encoding: chunked` request body (RFC 9112 §7.1), at most
/// `max_body` bytes in total; chunk extensions and trailers are ignored, a
/// malformed chunk size is refused. Everything it reads — data and framing
/// (chunk-size lines, extensions, CRLFs, trailers) — counts against
/// `max_body` plus [`chunk_framing_allowance`] (and at most one line of
/// [`MAX_HEADER_LINE`] past it before the check), so megabytes of
/// extensions or trailers cannot ride along a small body: such a body is
/// answered as too large. On a connection it must keep up
/// [`HttpOptions::min_body_rate`] on average: from here, `read_timeout` plus
/// the bytes received so far — at most that wire bound — at that rate
/// ([`ConnSocket`]), so it is in within `read_timeout + (max_body +
/// allowance) / min_body_rate` at the latest.
fn read_chunked_body<R: BufRead>(r: &mut R, max_body: usize) -> Result<Vec<u8>, BadRequest> {
    let wire_max = max_body.saturating_add(chunk_framing_allowance(max_body));
    let _clock = BodyScope::enter(Some(
        wire_max.saturating_add(MAX_HEADER_LINE as usize) as u64
    ));
    let mut body = Vec::new();
    // The framing bytes read so far (a line counts with its CRLF).
    let mut framing = 0usize;
    let mut count_line = |line: &str, body_len: usize| {
        framing = framing.saturating_add(line.len() + 2);
        if body_len.saturating_add(framing) > wire_max {
            Err(BadRequest::TOO_LARGE)
        } else {
            Ok(())
        }
    };
    loop {
        let line = read_line_bounded(r)?.ok_or(BadRequest::MALFORMED)?;
        count_line(&line, body.len())?;
        let size = chunk_size(&line)?;
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
            .map_err(|e| BadRequest::from_read(&e))?;
        if body.len() - start != size {
            return Err(BadRequest::MALFORMED);
        }
        // The CRLF after the chunk data.
        match read_line_bounded(r)? {
            Some(l) if l.is_empty() => count_line(&l, body.len())?,
            _ => return Err(BadRequest::MALFORMED),
        }
    }
    // Trailer fields, up to the empty line that ends the message.
    for _ in 0..MAX_HEADERS {
        match read_line_bounded(r)? {
            Some(l) if !l.is_empty() => count_line(&l, body.len())?,
            _ => return Ok(body),
        }
    }
    Err(BadRequest::MALFORMED)
}

/// Arrange for `SIGINT`/`SIGTERM` to trigger `handle.shutdown()` (unix only;
/// returns `false` — and does nothing — elsewhere or if the handlers could not
/// be installed). A tiny watcher thread polls the flag the async-signal-safe
/// handler sets. After the first signal `SIGINT` gets its default disposition
/// back, so a second Ctrl-C terminates the process immediately; a repeated
/// `SIGTERM` is ignored (see [`on_termination_signal`]).
pub fn shutdown_on_signals(handle: ShutdownHandle) -> bool {
    on_termination_signal(move || handle.shutdown())
}

/// Run `f` once, on a watcher thread, when the process first receives
/// `SIGINT` or `SIGTERM` (unix only; returns `false` — and does nothing —
/// elsewhere or if the handlers could not be installed). After that first
/// signal `SIGINT` gets its default disposition back, so a second Ctrl-C
/// terminates the process immediately, while further `SIGTERM`s are ignored:
/// one often arrives twice — a service manager signals the whole process
/// group, `gserver`'s supervisor forwards the one it got, `gserver -t` is
/// run again — and must not kill a graceful shutdown half-way through its
/// final save (`gserver -k` sends `SIGKILL`). [`shutdown_on_signals`] is this
/// with a graceful shutdown; `gserver`'s supervisor uses it to forward the
/// signal to the server it watches.
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
                    signals::restore_default_sigint();
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

    /// `SIGINT` back to its default disposition (`SIGTERM` keeps the
    /// handler, which only sets the flag again).
    pub(super) fn restore_default_sigint() {
        // SAFETY: restoring the default disposition is always valid.
        unsafe {
            signal(SIGINT, SIG_DFL);
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
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A shared guard of `l`, tolerating poisoning as [`locked`] does (a writer
/// that panicked while holding it does not make every later reader panic).
pub(crate) fn read_locked<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

/// The exclusive guard of `l`, tolerating poisoning as [`locked`] does.
pub(crate) fn write_locked<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

/// One database's figures as of its last write — triples, entities,
/// literals, predicates, its [`mutation_count`](Database::mutation_count)
/// (the *generation*), when it was last saved and a few flags — published by
/// whoever holds the database exclusively (after a load and at the end of
/// every write section) and read by monitoring (`/status`, `/monitor`,
/// `show`) **without the database lock**, so monitoring answers while a long
/// query or an update holds it.
///
/// Publishing is a sequence lock: the (single) publisher makes the sequence
/// odd, stores the figures, and makes it even again; a reader retries until
/// it read the same even sequence on both sides, so it never sees half of
/// one write and half of the next. Only the holder of the database's
/// exclusive guard publishes, which is what keeps the publisher single.
#[derive(Debug, Default)]
pub(crate) struct DbCounters {
    seq: AtomicU64,
    triples: AtomicU64,
    entities: AtomicU64,
    literals: AtomicU64,
    predicates: AtomicU64,
    generation: AtomicU64,
    last_save: AtomicU64,
    index_valid: AtomicBool,
    in_transaction: AtomicBool,
    dirty: AtomicBool,
    live: AtomicBool,
}

/// The figures [`DbCounters::read`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CounterSnapshot {
    /// Triples (the committed ones while a transaction is open, when the
    /// publisher says so).
    pub(crate) triples: u64,
    pub(crate) entities: u64,
    pub(crate) literals: u64,
    pub(crate) predicates: u64,
    /// The database's [`mutation_count`](Database::mutation_count).
    pub(crate) generation: u64,
    /// Unix milliseconds of the last successful save; `0` = none yet.
    pub(crate) last_save: u64,
    pub(crate) index_valid: bool,
    pub(crate) in_transaction: bool,
    pub(crate) dirty: bool,
    /// Published from a served database (`false` before the first publish
    /// and once it is retired: the other figures are then zero).
    pub(crate) live: bool,
}

impl DbCounters {
    /// Publish `db`'s current figures (the caller holds `db` exclusively);
    /// the last save time and the dirty flag are kept.
    pub(crate) fn publish(&self, db: &Database) {
        let s = db.stats();
        let prev = self.read();
        self.store(CounterSnapshot {
            triples: s.triple_num,
            entities: s.entity_num as u64,
            literals: s.literal_num as u64,
            predicates: s.predicate_num as u64,
            generation: db.mutation_count(),
            index_valid: s.index_valid,
            in_transaction: s.in_transaction,
            live: true,
            ..prev
        });
    }

    /// Publish `snap` (the caller is the single publisher, see the type docs).
    pub(crate) fn store(&self, snap: CounterSnapshot) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.triples.store(snap.triples, Ordering::SeqCst);
        self.entities.store(snap.entities, Ordering::SeqCst);
        self.literals.store(snap.literals, Ordering::SeqCst);
        self.predicates.store(snap.predicates, Ordering::SeqCst);
        self.generation.store(snap.generation, Ordering::SeqCst);
        self.last_save.store(snap.last_save, Ordering::SeqCst);
        self.index_valid.store(snap.index_valid, Ordering::SeqCst);
        self.in_transaction
            .store(snap.in_transaction, Ordering::SeqCst);
        self.dirty.store(snap.dirty, Ordering::SeqCst);
        self.live.store(snap.live, Ordering::SeqCst);
        self.seq.fetch_add(1, Ordering::SeqCst);
    }

    /// Record a successful save, now (the caller is the single publisher).
    pub(crate) fn note_saved(&self) {
        let snap = self.read();
        self.store(CounterSnapshot {
            last_save: unix_millis(),
            ..snap
        });
    }

    /// The figures last published. Never waits for the database (only, very
    /// briefly, for a publish in progress).
    pub(crate) fn read(&self) -> CounterSnapshot {
        let mut spins = 0u32;
        loop {
            let before = self.seq.load(Ordering::SeqCst);
            if before.is_multiple_of(2) {
                let snap = CounterSnapshot {
                    triples: self.triples.load(Ordering::SeqCst),
                    entities: self.entities.load(Ordering::SeqCst),
                    literals: self.literals.load(Ordering::SeqCst),
                    predicates: self.predicates.load(Ordering::SeqCst),
                    generation: self.generation.load(Ordering::SeqCst),
                    last_save: self.last_save.load(Ordering::SeqCst),
                    index_valid: self.index_valid.load(Ordering::SeqCst),
                    in_transaction: self.in_transaction.load(Ordering::SeqCst),
                    dirty: self.dirty.load(Ordering::SeqCst),
                    live: self.live.load(Ordering::SeqCst),
                };
                if self.seq.load(Ordering::SeqCst) == before {
                    return snap;
                }
            }
            // A publish is a handful of stores: spin briefly, then yield.
            spins = spins.saturating_add(1);
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                thread::yield_now();
            }
        }
    }
}

/// Milliseconds since the Unix epoch (`0` if the clock is before it).
pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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
        // The browser policy (HttpOptions): a foreign Host on a loopback
        // connection, a foreign Origin on a state-changing request.
        if let Some(why) = meta.refusal() {
            write_response_ext(
                stream,
                403,
                "text/plain",
                &meta.headers(&[]),
                why.as_bytes(),
            )?;
            return Ok(true);
        }
        // Auth gate: when credentials are configured every request must present
        // a matching `Authorization: Basic` header; a user name locked out at
        // the client's address (failed-login lockout) is refused `429`.
        if let Some(auth) = &self.auth {
            let ip = crate::http_api::peer_ip(current_peer());
            match auth.check(&req, &ip) {
                SingleAuthCheck::Valid => {}
                SingleAuthCheck::Invalid => {
                    write_response_ext(
                        stream,
                        401,
                        "text/plain",
                        &meta.headers(&[("WWW-Authenticate", "Basic realm=\"gStore\"")]),
                        b"unauthorized",
                    )?;
                    return Ok(true);
                }
                SingleAuthCheck::Locked(refusal) => {
                    let retry = crate::gapi::ipfilter::retry_after_secs(refusal.wait).to_string();
                    write_response_ext(
                        stream,
                        429,
                        "text/plain",
                        &meta.headers(&[("Retry-After", retry.as_str())]),
                        refusal.message().as_bytes(),
                    )?;
                    return Ok(true);
                }
            }
        } else if meta.forwarded() {
            // Without credentials the server relies on serving local clients
            // only (gserver refuses to listen beyond loopback); a reverse
            // proxy on this machine would hand that to anyone reaching it.
            write_response_ext(
                stream,
                403,
                "text/plain",
                &meta.headers(&[]),
                FORWARDED_REFUSED.as_bytes(),
            )?;
            return Ok(true);
        }
        let reply =
            catch_unwind(AssertUnwindSafe(|| self.route(&req, meta))).unwrap_or_else(|_| {
                crate::gs_error!("request handler panicked: {} {}", req.method, req.path);
                Reply::plain(500, "application/json", &err_json(PANIC_MSG))
            });
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
                status,
                ctype,
                body,
                ..
            } => {
                write_response_ext(stream, status, &ctype, &meta.headers(&[]), &body).map(|()| true)
            }
            Reply::Stream(fmt, rs) => stream_select(
                stream,
                fmt,
                &rs,
                &meta.cors_headers(),
                self.max_result_bytes,
            )
            .map(|()| false),
        }
    }

    /// Route a parsed, authorized request to its handler.
    fn route(&self, req: &Request, meta: &ConnMeta) -> Reply {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET" | "POST", "/sparql") => self.handle_sparql(req, meta.allow_get_update()),
            ("POST", "/update") => {
                if let Some(p) = req.first_param_of(UPDATE_DATASET_PARAMS) {
                    return Reply::buffered(
                        400,
                        "application/json",
                        err_json(&dataset_param_refusal(p)),
                    );
                }
                match req.update_text(meta.allow_get_update()) {
                    Ok(update) => self.run_update(&update),
                    Err((status, msg)) => {
                        Reply::buffered(status, "application/json", err_json(msg))
                    }
                }
            }
            ("GET", "/status") => self.status_json(),
            ("GET", "/" | "/console") => Reply::buffered(
                200,
                "text/html; charset=utf-8",
                CONSOLE_HTML.as_bytes().to_vec(),
            ),
            // Test-only trigger for the panic-isolation tests: panics while
            // holding the database's exclusive guard (poisoning it).
            #[cfg(test)]
            ("GET", "/__test/panic") => {
                let _db = write_locked(&self.db);
                panic!("test-triggered handler panic");
            }
            ("GET" | "POST", _) => Reply::plain(404, "text/plain", b"not found"),
            _ => Reply::plain(405, "text/plain", b"method not allowed"),
        }
    }

    /// Run a `/sparql` request: negotiate a result format, then answer with a
    /// buffered (`Content-Length`) body, or a chunked stream when `?stream=true`.
    /// The [`Database`] guard is released before any serialization or network
    /// write, so it is held only for the statement itself. A `GET` never runs
    /// an UPDATE (SPARQL 1.1 Protocol; a web page triggers a GET with a mere
    /// `<img>`) unless `allow_get_update`: such a statement is classified by
    /// a parse alone, before anything runs. Dataset parameters are refused.
    fn handle_sparql(&self, req: &Request, allow_get_update: bool) -> Reply {
        let q = match req.query_string() {
            Some(q) => q,
            None => return Reply::plain(400, SPARQL_JSON, &err_json("missing query")),
        };
        if let Some(p) = req.first_param_of(QUERY_DATASET_PARAMS) {
            return Reply::plain(400, SPARQL_JSON, &err_json(&dataset_param_refusal(p)));
        }
        if req.method == "GET" && !allow_get_update && is_update_text(&q) {
            return Reply::plain(400, SPARQL_JSON, &err_json(GET_UPDATE_REFUSED));
        }
        let fmt = ResultFormat::negotiate(req.format_param().as_deref(), req.header("accept"));
        let (result, warning) = self.execute(&q);
        // The result byte budget (review sec#10): serialization stops as
        // soon as it passes `max_result_bytes`.
        let limit = self.max_result_bytes;
        type Budgeted = crate::http_api::CountingWriter<Vec<u8>>;
        let budgeted = |ctype: &str, write: &dyn Fn(&mut Budgeted) -> io::Result<()>| {
            let mut out = crate::http_api::CountingWriter::new(Vec::new(), limit);
            if write(&mut out).is_err() && out.exceeded() {
                return Reply::plain(
                    503,
                    SPARQL_JSON,
                    &err_json(&crate::http_api::result_too_large_msg(limit)),
                );
            }
            Reply::buffered(200, ctype, out.into_inner())
        };
        match result {
            Ok(QueryResult::Select(rs)) => {
                if req.wants_stream() {
                    Reply::Stream(fmt, rs)
                } else {
                    budgeted(fmt.content_type(), &|w| {
                        sparql_results::write_select(fmt, &rs, w)
                    })
                }
            }
            Ok(QueryResult::Ask(b)) => budgeted(fmt.content_type(), &|w| {
                sparql_results::write_ask(fmt, b, w)
            }),
            Ok(QueryResult::Construct(ts)) => budgeted("application/n-triples", &|w| {
                crate::http_api::write_ntriples(&ts, w)
            }),
            Ok(QueryResult::Update { changed }) => update_reply(changed, warning),
            // A statement stopped by `query_timeout`: a refused request
            // (SPARQL Protocol), not a query error.
            Err(e @ crate::GStoreError::Timeout(_)) => {
                Reply::plain(503, SPARQL_JSON, &err_json(&e.to_string()))
            }
            Err(e) => Reply::plain(
                failure_status(&warning),
                SPARQL_JSON,
                &err_json(&e.to_string()),
            ),
        }
    }

    fn run_update(&self, u: &str) -> Reply {
        let (result, warning) = self.execute(u);
        match result {
            Ok(QueryResult::Update { changed }) => update_reply(changed, warning),
            Ok(_) => Reply::buffered(
                400,
                "application/json",
                err_json("/update expects a SPARQL UPDATE request"),
            ),
            // Stopped by `query_timeout` before it applied anything: a
            // refused request, as on `/sparql` (an UPDATE stopped after
            // applying part of its operations is the query error that says
            // so).
            Err(e @ crate::GStoreError::Timeout(_)) => {
                Reply::buffered(503, "application/json", err_json(&e.to_string()))
            }
            Err(e) => Reply::buffered(
                failure_status(&warning),
                "application/json",
                err_json(&e.to_string()),
            ),
        }
    }

    /// Run `sparql`: parsed first, with no lock held (a syntax error is
    /// answered without touching the database). A read then runs on a shared
    /// guard ([`Database::read_as`]), concurrently with other reads. An
    /// UPDATE takes the exclusive guard ([`Database::execute_as`]); one that
    /// changed data marks the database dirty and — under the check_point
    /// policy, or once a shutdown has begun — saves it before the guard is
    /// released, so the on-disk copy never lags an acknowledged write. That
    /// includes an UPDATE that failed part-way: its operations before the
    /// failing one stay applied (and visible), so they are saved too. A
    /// failed save does not undo the (applied) update: it is returned as the
    /// warning `"applied in memory but not durable: <cause>"` and the
    /// database stays dirty: an in-memory snapshot database retries the save
    /// on shutdown, while an on-disk database whose commit point failed stays
    /// broken until it is reopened, which discards the write
    /// ([`Database::storage_failure`]). The caller answers a
    /// statement with a warning as a failure, HTTP 500 ([`update_reply`],
    /// [`failure_status`]) — never as an acknowledged write (review
    /// durability#2).
    ///
    /// With check_point off an on-disk / RocksDB database's statement is made
    /// durable by its own commit point ([`Database::execute_as`]). A failed
    /// statement that changed data gets one more commit point
    /// ([`Database::sync`], as REST's `DbSlot::wrote` runs): it fails while
    /// the commit failure persists (a broken on-disk store refuses every
    /// commit until reopened), and that is the same not-durable warning — so
    /// a statement whose commit point failed is answered HTTP 500, not as a
    /// query error.
    ///
    /// The statement runs under `query_timeout`
    /// ([`Server::with_query_timeout`], review sec#10): its own cancel flag,
    /// raised by its deadline or by the server's shutdown flag; one stopped
    /// by its deadline fails as [`GStoreError::Timeout`](crate::GStoreError::Timeout).
    /// Only the evaluation counts: the deadline is armed once the statement
    /// holds the database's lock (a statement queued behind a long write is
    /// not charged for the wait) and disarmed when the evaluation returned
    /// (not during the save after an UPDATE).
    fn execute(&self, sparql: &str) -> (crate::Result<QueryResult>, Option<String>) {
        let stmt = match Statement::parse(sparql) {
            Ok(stmt) => stmt,
            Err(e) => return (Err(e), None),
        };
        let timed_out = |r: crate::Result<QueryResult>,
                         deadline: Option<crate::deadline::Deadline>| match (
            &r, &deadline,
        ) {
            (Err(crate::GStoreError::Cancelled), Some(d)) if d.fired() => {
                Err(crate::GStoreError::Timeout(d.timeout()))
            }
            _ => r,
        };
        use crate::http_api::{txn_park, TxnParkPoint};
        txn_park(&self.name, TxnParkPoint::StatementQueued);
        if !stmt.is_update() {
            let db = read_locked(&self.db);
            let (flag, deadline) = crate::deadline::arm_linked(&self.cancel, self.query_timeout);
            txn_park(&self.name, TxnParkPoint::StatementHolding);
            let result = db.read_as(&stmt, Some(flag), None);
            return (timed_out(result, deadline), None);
        }
        let mut db = write_locked(&self.db);
        let before = db.mutation_count();
        let (flag, deadline) = crate::deadline::arm_linked(&self.cancel, self.query_timeout);
        txn_park(&self.name, TxnParkPoint::StatementHolding);
        let result = db.execute_as(stmt, Some(flag), None);
        let mut result = timed_out(result, deadline);
        let mut warning = None;
        if db.mutation_count() != before {
            self.dirty.store(true, Ordering::SeqCst);
            if self.check_point || self.closing.load(Ordering::SeqCst) {
                if let Err((dir, e)) = self.save_locked(&db) {
                    let msg = format!(
                        "{}: saving to '{}' failed: {e}",
                        crate::http_api::NOT_DURABLE,
                        dir.display()
                    );
                    crate::gs_error!("{msg}");
                    warning = Some(msg);
                }
            } else if result.is_err() {
                // The statement's own commit point, or the one that committed
                // the part applied before a failing operation, may have failed.
                if let Err(e) = db.sync() {
                    let msg = format!(
                        "{}: the commit point of '{}' failed: {e}",
                        crate::http_api::NOT_DURABLE,
                        self.dir
                            .as_deref()
                            .map_or_else(|| self.name.clone(), |d| d.display().to_string())
                    );
                    crate::gs_error!("{msg}");
                    warning = Some(msg);
                }
            }
            if let Err(e) = &result {
                result = Err(match &warning {
                    // A persistence failure (answered 500), not a query error.
                    // The failure may be the statement's commit point (then
                    // every operation was applied) or one of its operations.
                    Some(w) => crate::GStoreError::Database(format!(
                        "{w} (the update failed: {e}; the operations before the failing one \
                         were applied — all of them when the commit point failed)"
                    )),
                    None => crate::GStoreError::Query(format!(
                        "{e} (operations before the failing one were applied)"
                    )),
                });
            }
            self.counters.publish(&db);
        }
        (result, warning)
    }

    /// Save `db` (whose exclusive guard the caller holds) to the attached
    /// directory and clear the dirty flag. `Ok(None)` when no directory is
    /// attached.
    fn save_locked(&self, db: &Database) -> Result<Option<PathBuf>, (PathBuf, String)> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        match db.save(dir) {
            Ok(()) => {
                self.dirty.store(false, Ordering::SeqCst);
                self.counters.note_saved();
                Ok(Some(dir.clone()))
            }
            Err(e) => Err((dir.clone(), e.to_string())),
        }
    }

    /// `/status`: the figures published after the last write
    /// ([`DbCounters`]). It never waits for the database lock, so it answers
    /// while a long query or an update runs.
    fn status_json(&self) -> Reply {
        let s = self.counters.read();
        let dir = match &self.dir {
            Some(d) => json_str(&d.to_string_lossy()),
            None => "null".to_string(),
        };
        let body = format!(
            "{{\"name\":{},\"triples\":{},\"entities\":{},\"literals\":{},\"predicates\":{},\"index_valid\":{},\"in_transaction\":{},\"db_dir\":{dir},\"check_point\":{},\"dirty\":{}}}",
            json_str(&self.name),
            s.triples,
            s.entities,
            s.literals,
            s.predicates,
            s.index_valid,
            s.in_transaction,
            self.check_point,
            self.dirty.load(Ordering::SeqCst),
        );
        Reply::buffered(200, "application/json", body.into_bytes())
    }
}

/// `{"changed":N}`, or — when persisting the change failed (`warning`, see
/// [`ServerInner::execute`]) — `{"error":"applied in memory but not durable:
/// <cause>","changed":N,"warning":…}`.
fn changed_json(changed: usize, warning: Option<String>) -> Vec<u8> {
    match warning {
        Some(w) => format!(
            "{{\"error\":{},\"changed\":{changed},\"warning\":{}}}",
            json_str(&w),
            json_str(&w)
        )
        .into_bytes(),
        None => format!("{{\"changed\":{changed}}}").into_bytes(),
    }
}

/// The reply to an applied UPDATE: HTTP 200 `{"changed":N}` when it is
/// durable; HTTP 500 when its check_point save failed (`warning`) — a write
/// applied in memory but not saved is not acknowledged (review durability#2).
fn update_reply(changed: usize, warning: Option<String>) -> Reply {
    let status = if warning.is_some() { 500 } else { 200 };
    Reply::buffered(status, "application/json", changed_json(changed, warning))
}

/// The HTTP status of a failed statement: 500 when the save of what it
/// applied failed (`warning`: the failure is the server's, and the change is
/// not durable), else 400.
fn failure_status(warning: &Option<String>) -> u16 {
    if warning.is_some() {
        500
    } else {
        400
    }
}

/// A parsed HTTP request (method, path, query string, headers, body).
///
/// `pub(crate)` so the richer [`crate::http_api`] endpoint can reuse the same
/// parser and accessors rather than re-implementing HTTP request parsing.
///
/// **Parameters** ([`param`](Self::param) and the accessors built on it) come
/// from the URL query string and from a **form body** only: one labelled
/// `application/x-www-form-urlencoded`, or an unlabelled one (no
/// `Content-Type`) shaped like a form — see [`body_kind`](Self::body_kind).
/// Any other body — `application/sparql-query`, `application/sparql-update`,
/// RDF, JSON, `text/plain`, an unlabelled SPARQL text — is content, taken
/// verbatim: text inside it that looks like `&update=…` or `&db_name=…` is
/// never a parameter (review H-9). A lookup scans the form body once
/// without copying it; nothing scans the body for a session token (http#4).
/// (The upstream `/api` protocol reads its parameters with
/// `gapi::params::Params`, which keeps upstream's own rules.)
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
        // The stored names are lowercase: no lowercased copy of `name`.
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// True iff the request carries valid `Authorization: Basic` credentials
    /// equal to `user:pass`.
    #[cfg_attr(not(test), allow(dead_code))]
    fn authorized(&self, user: &str, pass: &str) -> bool {
        self.basic_token()
            .is_some_and(|decoded| ct_eq(&decoded, format!("{user}:{pass}").as_bytes()))
    }

    /// The decoded `Authorization: Basic` token (`user:pass` bytes), if the
    /// request carries a well-formed one.
    fn basic_token(&self) -> Option<Vec<u8>> {
        let token = strip_basic_prefix(self.header("authorization")?)?.trim();
        base64_decode(token)
    }

    /// The negotiated result format token from `?format=` (URL or form body).
    pub(crate) fn format_param(&self) -> Option<String> {
        self.param("format")
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

    /// What the body is, for parameter lookups (see the type's docs): by its
    /// `Content-Type`, or — without one — by its shape (a legacy client's
    /// unlabelled form body, versus raw SPARQL / RDF text).
    pub(crate) fn body_kind(&self) -> BodyKind {
        if self.body.is_empty() {
            return BodyKind::Empty;
        }
        let Some(ct) = self.header("content-type") else {
            return if looks_like_form(&self.body) {
                BodyKind::Form
            } else {
                BodyKind::Untyped
            };
        };
        let media = ct.split(';').next().unwrap_or_default().trim();
        if media.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
            BodyKind::Form
        } else if media.eq_ignore_ascii_case("application/sparql-query") {
            BodyKind::SparqlQuery
        } else if media.eq_ignore_ascii_case("application/sparql-update") {
            BodyKind::SparqlUpdate
        } else if media.eq_ignore_ascii_case("text/plain")
            || media.eq_ignore_ascii_case("multipart/form-data")
        {
            BodyKind::Simple
        } else {
            BodyKind::Other
        }
    }

    /// The body, when it is a form ([`BodyKind::Form`]), without a trailing
    /// line break (a literal one is never part of a form value).
    fn form_body(&self) -> Option<&[u8]> {
        if self.body_kind() != BodyKind::Form {
            return None;
        }
        let end = self
            .body
            .iter()
            .rposition(|&b| b != b'\r' && b != b'\n')
            .map_or(0, |i| i + 1);
        Some(&self.body[..end])
    }

    /// Look up a parameter by name in the URL query string first, then in a
    /// form body ([`BodyKind::Form`]; never in any other body). Used by
    /// [`crate::http_api`] for its `db=`/`name=`/`txn=`/`username=` …
    /// parameters.
    pub(crate) fn param(&self, key: &str) -> Option<String> {
        if let Some(q) = &self.query {
            if let Some(v) = form_get(q, key) {
                return Some(v);
            }
        }
        self.form_body().and_then(|b| form_lookup(b, key))
    }

    /// The raw request body as a UTF-8 (lossy) string.
    pub(crate) fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as a statement or document: its (lossy UTF-8, trimmed) text
    /// when it is not a form and not empty.
    pub(crate) fn raw_statement(&self) -> Option<String> {
        if matches!(self.body_kind(), BodyKind::Form | BodyKind::Empty) {
            return None;
        }
        let text = String::from_utf8_lossy(&self.body);
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Whether the request was forwarded by a proxy: it carries a
    /// `Forwarded`, `X-Forwarded-For` or `X-Real-IP` header
    /// ([`FORWARDING_HEADERS`]).
    pub(crate) fn is_forwarded(&self) -> bool {
        FORWARDING_HEADERS.iter().any(|h| self.header(h).is_some())
    }

    /// Extract a session/bearer token from `Authorization: Bearer …`, the
    /// `X-Session-Token` header, or a `session=`/`token=` parameter of the
    /// URL. The body is never searched: the caller is not authenticated yet,
    /// and the body is theirs to make as large as the cap allows (http#4).
    pub(crate) fn bearer_or_session_token(&self) -> Option<String> {
        if let Some(h) = self.header("authorization") {
            let h = h.trim_start();
            // `get` (not byte slicing): a multi-byte character across the
            // prefix boundary must not panic.
            if h.get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
            {
                return Some(h[7..].trim().to_string());
            }
        }
        if let Some(h) = self.header("x-session-token") {
            return Some(h.trim().to_string());
        }
        let q = self.query.as_deref()?;
        form_get(q, "session").or_else(|| form_get(q, "token"))
    }

    /// The query text: from the URL `?query=`, a form body's `query=`, or
    /// the raw body of any other kind (`application/sparql-query`, the SPARQL
    /// Protocol's "POST directly"; an unlabelled one too). A form body
    /// without `query=` has no query.
    pub(crate) fn query_string(&self) -> Option<String> {
        if let Some(q) = &self.query {
            if let Some(v) = form_get(q, "query") {
                return Some(v);
            }
        }
        if let Some(form) = self.form_body() {
            return form_lookup(form, "query");
        }
        self.raw_statement()
    }

    /// The update text of a `POST /update`: an `application/sparql-update`
    /// (or unlabelled, non-form) body verbatim, or the `update=` parameter
    /// of a form body (or of the URL). A `text/plain` or
    /// `multipart/form-data` body — the types a web page can send cross-site
    /// without a CORS preflight ([`BodyKind::Simple`]) — is refused with
    /// `415` unless `any_body` ([`HttpOptions::allow_get_update`]), which
    /// takes it verbatim. Any other type needs a preflight (which follows
    /// [`HttpOptions::cors_allow_origin`]) and is taken verbatim.
    pub(crate) fn update_text(&self, any_body: bool) -> Result<String, (u16, &'static str)> {
        const MISSING: (u16, &str) = (400, "missing update statement");
        match self.body_kind() {
            BodyKind::Form | BodyKind::Empty => self.param("update").ok_or(MISSING),
            BodyKind::Simple if !any_body => Err((415, UPDATE_BODY_REFUSED)),
            BodyKind::SparqlUpdate
            | BodyKind::Untyped
            | BodyKind::SparqlQuery
            | BodyKind::Simple
            | BodyKind::Other => self.raw_statement().ok_or(MISSING),
        }
    }

    /// The first of `keys` the request carries as a parameter with a
    /// non-blank value (see [`param`](Self::param)): an empty one, such as a
    /// blank "default graph" field of a query form (`default-graph-uri=`),
    /// names nothing and does not count.
    pub(crate) fn first_param_of(&self, keys: &[&'static str]) -> Option<&'static str> {
        keys.iter()
            .copied()
            .find(|k| self.param(k).is_some_and(|v| !v.trim().is_empty()))
    }
}

/// What a request body is, for parameter lookups ([`Request::body_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyKind {
    /// No body.
    Empty,
    /// `application/x-www-form-urlencoded`, or an unlabelled body shaped
    /// like one: it supplies parameters.
    Form,
    /// `application/sparql-query`.
    SparqlQuery,
    /// `application/sparql-update`.
    SparqlUpdate,
    /// No `Content-Type`, and not shaped like a form (SPARQL or RDF text).
    Untyped,
    /// `text/plain` or `multipart/form-data`: besides forms, the types a web
    /// page can POST cross-site without a CORS preflight.
    Simple,
    /// Any other `Content-Type` (JSON, RDF, …).
    Other,
}

/// The SPARQL Protocol's dataset parameters of a query
/// (`default-graph-uri`, `named-graph-uri`), which the endpoints do not
/// support: a request naming one is refused rather than answered over the
/// whole default graph (review gap-doc-claims#2).
pub(crate) const QUERY_DATASET_PARAMS: &[&str] = &["default-graph-uri", "named-graph-uri"];

/// The dataset parameters of an update (`using-graph-uri`,
/// `using-named-graph-uri`), refused likewise.
pub(crate) const UPDATE_DATASET_PARAMS: &[&str] = &["using-graph-uri", "using-named-graph-uri"];

/// The message refusing dataset parameter `param`.
pub(crate) fn dataset_param_refusal(param: &str) -> String {
    format!(
        "the SPARQL Protocol dataset parameter '{param}' is not supported (this endpoint has no \
         RDF dataset support; the query would run over the default graph instead)"
    )
}

/// Whether an unlabelled body is shaped like an
/// `application/x-www-form-urlencoded` form: `name=value` pairs joined by
/// `&` in the characters such a form is written in (no whitespace, braces,
/// angle brackets or quotes, which every SPARQL statement and RDF document
/// contains), apart from a trailing line break.
fn looks_like_form(body: &[u8]) -> bool {
    let end = body
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    let body = &body[..end];
    // The character check first: it stops at the first space or brace of a
    // SPARQL / RDF body, where a search for `=` would read all of it.
    !body.is_empty()
        && body.iter().all(|&b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'.'
                        | b'_'
                        | b'~'
                        | b'%'
                        | b'+'
                        | b'&'
                        | b'='
                        | b'*'
                        | b'!'
                        | b'\''
                        | b'('
                        | b')'
                        | b','
                        | b';'
                        | b':'
                        | b'/'
                        | b'?'
                        | b'@'
                        | b'$'
                        | b'['
                        | b']'
                )
        })
        && body.contains(&b'=')
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
    let head = response_head(status, ctype, extra, body.len() as u64);
    // One write for head + body, so a kept-alive connection never waits on
    // Nagle between them.
    let mut buf = head.into_bytes();
    buf.extend_from_slice(body);
    stream.write_all(&buf)?;
    stream.flush()
}

/// The status line and header block of a response with a `Content-Length`
/// of `len` (see [`write_response_ext`]).
fn response_head(status: u16, ctype: &str, extra: &[(&str, &str)], len: u64) -> String {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut head =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\n");
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
    head
}

/// The chunk size of [`write_response_streamed`].
pub(crate) const STREAM_CHUNK: usize = 64 * 1024;

/// Write a response whose body — exactly `len` bytes — is copied from
/// `body` in chunks of [`STREAM_CHUNK`] bytes, never held in memory as a
/// whole (a file download; review gapi#4, robust#7, sec#11). The head goes
/// out with the first chunk. The body is sent as is, with its
/// `Content-Length`: a streamed body is never compressed (its compressed
/// length is not known before it has been read, and a `Range` answer must
/// count the bytes of the file). A `body` that ends before `len` bytes
/// fails with `UnexpectedEof` — the response cannot be completed, and the
/// caller closes the connection.
///
/// On a connection socket the client must take the response at
/// [`HttpOptions::min_body_rate`] on average, after a grace of
/// `write_timeout` ([`ConnSocket`]): a client draining it a few bytes at a
/// time fails the write with `TimedOut` once it falls behind, so it holds
/// the worker — and a download's slot — for at most `write_timeout + len /
/// min_body_rate` (review of round 1: only each single write was bounded,
/// by `write_timeout`).
pub(crate) fn write_response_streamed<W: Write, R: Read>(
    stream: &mut W,
    status: u16,
    ctype: &str,
    extra: &[(&str, &str)],
    mut body: R,
    len: u64,
) -> std::io::Result<()> {
    let head = response_head(status, ctype, extra, len);
    let _clock = SendScope::enter((head.len() as u64).saturating_add(len));
    let mut buf = Vec::with_capacity(head.len() + STREAM_CHUNK);
    buf.extend_from_slice(head.as_bytes());
    let mut left = len;
    loop {
        let start = buf.len();
        let want = usize::try_from(left).map_or(STREAM_CHUNK, |l| l.min(STREAM_CHUNK));
        buf.resize(start + want, 0);
        let mut filled = start;
        while filled < buf.len() {
            match body.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf.truncate(filled);
        let got = (filled - start) as u64;
        left -= got;
        stream.write_all(&buf)?;
        if left == 0 {
            break;
        }
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the streamed body ended before its Content-Length",
            ));
        }
        buf.clear();
    }
    stream.flush()
}

#[cfg(test)]
mod streamed_response_tests {
    use super::*;

    /// Records the size of every write.
    #[derive(Default)]
    struct Sink {
        bytes: Vec<u8>,
        largest_write: usize,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.largest_write = self.largest_write.max(buf.len());
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn streamed_bodies_go_out_in_bounded_chunks() {
        let body: Vec<u8> = (0..STREAM_CHUNK * 3 + 17).map(|i| i as u8).collect();
        let mut sink = Sink::default();
        write_response_streamed(
            &mut sink,
            206,
            "application/octet-stream",
            &[("Content-Range", "bytes 0-1/2")],
            &body[..],
            body.len() as u64,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&sink.bytes);
        assert!(
            text.starts_with("HTTP/1.1 206 Partial Content\r\n"),
            "{text:.80}"
        );
        assert!(text.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(text.contains("Content-Range: bytes 0-1/2\r\n"));
        let split = sink
            .bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(&sink.bytes[split..], &body[..]);
        assert!(
            sink.largest_write <= split + STREAM_CHUNK,
            "{}",
            sink.largest_write
        );
        // Only `len` bytes are sent, even from a longer reader.
        let mut sink = Sink::default();
        write_response_streamed(&mut sink, 200, "x", &[], &body[..], 10).unwrap();
        let split = sink
            .bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(&sink.bytes[split..], &body[..10]);
        // An empty body.
        let mut sink = Sink::default();
        write_response_streamed(&mut sink, 200, "x", &[], io::empty(), 0).unwrap();
        assert!(String::from_utf8_lossy(&sink.bytes)
            .ends_with("Content-Length: 0\r\nConnection: close\r\n\r\n"));
        // A reader shorter than the announced length cannot complete the
        // response.
        let mut sink = Sink::default();
        let err = write_response_streamed(&mut sink, 200, "x", &[], &body[..5], 6).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}

/// Stream a SELECT result with `Transfer-Encoding: chunked`, framing each
/// serializer `write_all` (one per row) as its own HTTP chunk. The connection
/// always closes afterwards; `extra` adds headers (CORS).
fn stream_select<W: Write>(
    stream: &mut W,
    fmt: ResultFormat,
    rs: &ResultSet,
    extra: &[(&str, &str)],
    max_result_bytes: u64,
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
    // The result byte budget counts the result, not its chunk framing; past
    // it the write fails and the stream ends without its closing chunk, so
    // the client sees a truncated answer (review sec#10).
    let mut counted =
        crate::http_api::CountingWriter::new(ChunkedWriter { inner: stream }, max_result_bytes);
    sparql_results::write_select(fmt, rs, &mut counted)?;
    counted.into_inner().finish()
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
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no private key found in the key PEM",
        )
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

/// CONSTRUCT / DESCRIBE triples as an `application/n-triples` body, one
/// statement per line through the shared escaping serializer
/// ([`Triple::to_ntriples`](crate::model::Triple::to_ntriples)), so no term
/// — an IRI built by `IRI()` from text holding `>` and a newline, say — can
/// end its line and forge a statement (review sparqlconf#11).
pub(crate) fn ntriples_body(triples: &[crate::model::Triple]) -> Vec<u8> {
    let mut s = String::new();
    for t in triples {
        s.push_str(&t.to_ntriples());
        s.push('\n');
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
pub(crate) fn strip_basic_prefix(h: &str) -> Option<&str> {
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
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
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
    form_lookup(qs.as_bytes(), key)
}

/// Look up `key` in an `application/x-www-form-urlencoded` byte string: the
/// value of the first pair whose decoded name is `key`. One pass, and no
/// allocation but the value found: names are compared as they are decoded.
fn form_lookup(form: &[u8], key: &str) -> Option<String> {
    form.split(|&b| b == b'&').find_map(|pair| {
        let eq = pair.iter().position(|&b| b == b'=')?;
        decoded_eq(&pair[..eq], key.as_bytes()).then(|| url_decode_bytes(&pair[eq + 1..]))
    })
}

/// Whether the form-decoding of `enc` is exactly `want`.
fn decoded_eq(enc: &[u8], want: &[u8]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < enc.len() {
        let (b, step) = decode_at(enc, i);
        if want.get(j) != Some(&b) {
            return false;
        }
        i += step;
        j += 1;
    }
    j == want.len()
}

/// The byte a form encoding holds at `i` (`+` a space, `%XX` an escape,
/// a malformed `%` itself) and how many input bytes it took.
fn decode_at(b: &[u8], i: usize) -> (u8, usize) {
    match b[i] {
        b'+' => (b' ', 1),
        b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
            (Some(h), Some(l)) => (h * 16 + l, 3),
            _ => (b'%', 1),
        },
        c => (c, 1),
    }
}

fn url_decode_bytes(b: &[u8]) -> String {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (c, step) = decode_at(b, i);
        out.push(c);
        i += step;
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

    /// Two `/sparql` reads each call a function that waits for a second
    /// caller (15 s at most): both answer `1` only if they ran at the same
    /// time (one exclusive lock would make the first time out with `0`).
    #[test]
    fn two_sparql_reads_run_at_once() {
        #[derive(Default)]
        struct Latch {
            n: Mutex<usize>,
            cv: std::sync::Condvar,
        }
        let latch = Arc::new(Latch::default());
        let l2 = Arc::clone(&latch);
        let mut db = Database::build_from_str("latch", DATA).unwrap();
        db.register_function("http://ex/latch", move |_| {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut n = l2.n.lock().unwrap();
            *n += 1;
            l2.cv.notify_all();
            while *n < 2 {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Some(crate::query::Value::Int(0));
                }
                n = l2.cv.wait_timeout(n, left).unwrap().0;
            }
            Some(crate::query::Value::Int(1))
        });
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        let reads: Vec<_> = (0..2)
            .map(|_| {
                thread::spawn(move || {
                    post(
                        &addr,
                        "/sparql",
                        "SELECT (<http://ex/latch>(1) AS ?x) WHERE {}",
                    )
                })
            })
            .collect();
        for r in reads {
            let resp = r.join().unwrap();
            assert!(resp.contains("200 OK"), "{resp}");
            assert!(resp.contains("\"value\":\"1\""), "{resp}");
        }
        server.shutdown();
    }

    /// `/status` answers at once while a statement holds the database —
    /// shared or exclusive — with the figures of the last write; a
    /// statement that does not parse is answered `400` without the lock.
    #[test]
    fn status_and_syntax_errors_do_not_wait_for_the_database() {
        let (srv, addr) = start();
        let upd = post(
            &addr,
            "/update",
            "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }",
        );
        assert!(upd.contains("\"changed\":1"), "{upd}");
        let within = |raw: String| {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(http(&addr, &raw));
            });
            rx.recv_timeout(Duration::from_secs(1))
                .expect("answered within a second")
        };
        let status = || within("GET /status HTTP/1.1\r\nConnection: close\r\n\r\n".into());
        let bad = || {
            let body = "SELEKT nothing";
            within(format!(
                "POST /sparql HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
        };
        {
            let _shared = read_locked(&srv.inner.db);
            let s = status();
            assert!(
                s.contains("\"triples\":3") && s.contains("\"name\":\"srv\""),
                "{s}"
            );
            assert!(bad().contains("400 Bad Request"));
        }
        {
            let _excl = write_locked(&srv.inner.db);
            let s = status();
            assert!(s.contains("\"triples\":3"), "{s}");
            assert!(bad().contains("400 Bad Request"));
        }
        srv.shutdown();
    }

    /// The counters' sequence lock never shows half of one publish and half
    /// of the next.
    #[test]
    fn counters_are_read_whole() {
        let c = Arc::new(DbCounters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let (c, stop) = (Arc::clone(&c), Arc::clone(&stop));
                thread::spawn(move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        let s = c.read();
                        // Every publish stores the same value everywhere.
                        assert_eq!(s.triples, s.entities);
                        assert_eq!(s.triples, s.literals);
                        assert_eq!(s.triples, s.predicates);
                        assert_eq!(s.triples, s.generation);
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        // Publish for a while, with every reader running meanwhile.
        let t0 = Instant::now();
        let mut v = 0u64;
        while t0.elapsed() < Duration::from_millis(150) {
            v += 1;
            c.store(CounterSnapshot {
                triples: v,
                entities: v,
                literals: v,
                predicates: v,
                generation: v,
                live: true,
                ..CounterSnapshot::default()
            });
        }
        stop.store(true, Ordering::SeqCst);
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
        assert_eq!(c.read().generation, v);
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("gstore_server_unit_{tag}_{}", std::process::id()));
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
        let resp = http(
            &addr,
            "GET /__test/panic HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        assert!(resp.contains("500 Internal Server Error"), "{resp}");
        assert!(resp.contains("application/json"), "{resp}");
        assert!(
            resp.contains("\"error\"") && resp.contains("panicked"),
            "{resp}"
        );
        // The poisoned database lock is recovered: reads and writes still work.
        let ok = post(&addr, "/sparql", SELECT_O);
        assert!(ok.contains("200 OK") && ok.contains("http://ex/b"), "{ok}");
        let upd = post(
            &addr,
            "/update",
            "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }",
        );
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
        assert_eq!(
            server.serve_forever(),
            ShutdownReport {
                drained: true,
                ..Default::default()
            }
        );
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
        let resp = post(
            &addr,
            "/sparql",
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
        );
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
        let resp2 = post(
            &addr,
            "/sparql",
            "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/d> }",
        );
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
        let server = Arc::new(
            Server::bind(db, "127.0.0.1:0")
                .unwrap()
                .with_basic_auth("user", "pass"),
        );
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
        let csv = post_accept(
            &addr,
            "/sparql?format=csv",
            SELECT_O,
            "application/sparql-results+xml",
        );
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
        assert!(
            !resp.contains("Content-Encoding"),
            "must not compress: {resp}"
        );
        assert!(resp.contains("http://ex/b"), "{resp}");
    }

    // --- connection handling: keep-alive, worker pool, CORS, body cap ---------

    /// Start a server with custom connection settings; returns the serving
    /// thread too so a test can shut down and join it.
    fn start_with(
        opts: HttpOptions,
    ) -> (Arc<Server>, SocketAddr, thread::JoinHandle<ShutdownReport>) {
        let db = Database::build_from_str("conn", DATA).unwrap();
        let server = Arc::new(
            Server::bind(db, "127.0.0.1:0")
                .unwrap()
                .with_http_options(opts),
        );
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
        assert!(
            rest.is_empty(),
            "the server closed after `Connection: close`"
        );
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
        w.write_all(b"GET /status HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
            .unwrap();
        let (head, _) = read_one(&mut r);
        assert!(head.contains("Connection: keep-alive"), "{head}");

        // A client `Keep-Alive: max=1` hint ends it after one response.
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(get_status("Keep-Alive: timeout=5, max=1\r\n").as_bytes())
            .unwrap();
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
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "closed after the idle timeout"
        );

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
        third
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        third
            .write_all(get_status("Connection: close\r\n").as_bytes())
            .unwrap();
        let mut buf = [0u8; 64];
        let early = third.read(&mut buf);
        assert!(
            matches!(&early, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "with both workers busy the third connection is not served yet: {early:?}"
        );
        // Finishing one slow request frees its worker for the third connection.
        slow[0].write_all(b"Connection: close\r\n\r\n").unwrap();
        let mut resp = String::new();
        slow[0]
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        slow[0].read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        third
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
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
        b.write_all(get_status("Connection: close\r\n").as_bytes())
            .unwrap();
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
        s.write_all(get_status("Connection: close\r\n").as_bytes())
            .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert!(
            dripper.join().unwrap(),
            "the dripping connection was dropped"
        );
    }

    #[test]
    fn options_preflight_and_cors_headers() {
        // review http#2 / sec#8 / infra#3: by default no origin is listed, so
        // a browser's preflight from a foreign origin is refused and no
        // response carries Access-Control-Allow-Origin (it used to be `*`
        // on every response, letting any web page read the database).
        let (_srv, addr) = start();
        let resp = http(
            &addr,
            "OPTIONS /file/upload HTTP/1.1\r\nOrigin: http://w\r\nConnection: close\r\n\r\n",
        );
        let (head, _) = split_resp(&resp);
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(!head.contains("Access-Control-Allow"), "{head}");
        let resp = http(
            &addr,
            &get_status("Origin: http://w\r\nConnection: close\r\n"),
        );
        assert!(!resp.contains("Access-Control-Allow-Origin"), "{resp}");
        // A preflight without Origin (no browser's) is still answered.
        let resp = http(
            &addr,
            "OPTIONS /file/download/x.nt HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        assert!(resp.contains("Access-Control-Allow-Methods: GET"), "{resp}");

        // `cors_allow_origin = *` restores upstream's headers (the compat
        // switch): every response, and every preflight, as before.
        let star = HttpOptions {
            cors_allow_origin: vec!["*".to_string()],
            ..HttpOptions::default()
        };
        let (_srv3, addr, _serving3) = start_with(star);
        let resp = http(
            &addr,
            "OPTIONS /file/upload HTTP/1.1\r\nOrigin: http://w\r\nConnection: close\r\n\r\n",
        );
        let (head, body) = split_resp(&resp);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Access-Control-Allow-Origin: *"), "{head}");
        assert!(
            head.contains("Access-Control-Allow-Methods: POST"),
            "{head}"
        );
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
        assert!(
            resp.contains("Access-Control-Allow-Headers: X-Foo, Content-Type"),
            "{resp}"
        );
        // Ordinary responses carry the allow-origin header too.
        let resp = http(&addr, &get_status("Connection: close\r\n"));
        assert!(resp.contains("Access-Control-Allow-Origin: *"), "{resp}");

        // With CORS off: no header, and OPTIONS reaches the router (405 here).
        let opts = HttpOptions {
            cors: false,
            cors_allow_origin: vec!["*".to_string()],
            ..HttpOptions::default()
        };
        let (_srv2, addr2, _serving) = start_with(opts);
        let resp = http(&addr2, &get_status("Connection: close\r\n"));
        assert!(!resp.contains("Access-Control"), "{resp}");
        let resp = http(
            &addr2,
            "OPTIONS /sparql HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
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
        let resp = http(
            &addr,
            "POST /update HTTP/1.1\r\nContent-Length: abc\r\n\r\n",
        );
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
        assert!(
            resp.starts_with("HTTP/1.1 200") && resp.contains("http://ex/b"),
            "{resp}"
        );
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
        for value in [
            "abcde\u{e9}xyz",
            "abcdef\u{e9}xyz",
            "\u{e9}",
            "Basic\u{e9}",
            "Bearer\u{e9}x",
        ] {
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
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
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

    /// A `HEAD` response keeps its headers (`Content-Length` included) but
    /// sends no body, so the next response on a kept-alive connection is
    /// read as a response.
    #[test]
    fn head_responses_carry_no_body_on_a_kept_alive_connection() {
        let (_srv, addr) = start();
        let resp = http(
            &addr,
            "HEAD /status HTTP/1.1\r\nHost: x\r\n\r\n\
             GET /status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        let (first, rest) = resp.split_once("\r\n\r\n").unwrap();
        assert!(first.starts_with("HTTP/1.1 405"), "{resp}");
        assert!(first.contains("Content-Length: 18"), "{first}");
        assert!(first.contains("Connection: keep-alive"), "{first}");
        assert!(rest.starts_with("HTTP/1.1 200"), "the body leaked: {rest}");
        assert!(rest.contains("\"triples\":2"), "{rest}");
    }

    #[test]
    fn response_sink_drops_only_the_body_of_a_head_response() {
        let mut sink = ResponseSink::new(Vec::new());
        sink.start_response(true);
        // The header terminator split across writes.
        for part in [
            &b"HTTP/1.1 200 OK\r\nA: b\r"[..],
            b"\n\r",
            b"\nbody",
            b"more",
        ] {
            assert_eq!(sink.write(part).unwrap(), part.len());
        }
        assert_eq!(sink.inner, b"HTTP/1.1 200 OK\r\nA: b\r\n\r\n");
        sink.start_response(false);
        sink.write_all(b"HTTP/1.1 200 OK\r\n\r\nbody").unwrap();
        assert!(sink.inner.ends_with(b"\r\n\r\nbody"));
    }

    /// The latest request wins: stopping the server while a restart is under
    /// way (a signal, `gserver -t`) stops it.
    #[test]
    fn an_explicit_stop_calls_off_a_pending_restart() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let life = Lifecycle::new(listener.local_addr().unwrap());
        let handle = ShutdownHandle::new(Arc::clone(&life));
        handle.restart();
        assert!(handle.restart_requested() && handle.is_shutdown());
        handle.shutdown();
        assert!(!handle.restart_requested() && handle.is_shutdown());
        // A restart asked for once the shutdown is under way stays a shutdown.
        handle.restart();
        assert!(!handle.restart_requested());
    }
}
