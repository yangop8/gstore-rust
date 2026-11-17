//! A dependency-free HTTP/1.1 front-end for gNLQA (std `TcpListener`, mirroring
//! gStore's `server.rs`). Endpoints:
//!
//! * `POST /ask`    `{ "question": "…" }` → `{ answer, sparql, values, rounds }`
//! * `POST /gSolve` `{ "question": "…" }` → gAnswer-compatible subset
//!   `{ status, question, sparql, answers }`
//! * `GET  /health` → `{ "status": "ok" }`
//!
//! The handler holds an [`Arc<SolveEngine>`] and answers each connection on its
//! own thread.
//!
//! ## Authentication and limits (review cluster-nlqa#13, sec#18)
//!
//! `/ask` and `/gSolve` run LLM-chosen read-only SPARQL with the server's own
//! KB credentials and spend the operator's LLM credit, so they are **not**
//! open by default: a server built with [`HttpServer::bind`] refuses them
//! (401) until a bearer token is configured ([`HttpServer::with_token`]) or
//! the explicit loopback-only opt-out is chosen
//! ([`HttpServer::allow_unauthenticated_loopback`], which serves only clients
//! connecting from a loopback address). `/health` stays open: it does no work.
//!
//! Every `/ask` / `/gSolve` request is also rate-limited per client IP
//! (counted *before* authentication, so token guessing is bounded too), its
//! question length is capped, and the engine work it causes runs under a
//! per-request [`CostLimits`] budget (LLM calls, KB rows, wall-clock time;
//! see [`crate::budget`]). See [`HttpLimits`] for the defaults.

use std::collections::HashMap;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::budget::{self, CostLimits};
use crate::error::{Error, Result};
use crate::secret::Secret;
use crate::solve::SolveEngine;

/// Max request body (bytes) — bounds attacker-controlled Content-Length.
const MAX_BODY: usize = 1 << 20; // 1 MiB
/// Max bytes for a single request/header line.
const MAX_LINE: usize = 16 * 1024;
/// Per-read socket timeout: bounds a single stalled `recv`.
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Absolute deadline for reading a whole request. Combined with [`IO_TIMEOUT`]
/// this defeats slowloris: a per-read timeout can be reset by a trickle of bytes,
/// but this hard wall-clock cap can't — so a slow-drip client can't pin a
/// connection (and its `max_conn` slot) open indefinitely.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// Max number of header lines accepted (bounds an unbounded header flood).
const MAX_HEADERS: usize = 100;
/// Max number of client IPs the rate limiter tracks at once (bounds its
/// memory against a flood from many source addresses).
const MAX_TRACKED_CLIENTS: usize = 10_000;

/// How clients of `/ask` and `/gSolve` must authenticate.
#[derive(Debug, Clone)]
pub enum HttpAuth {
    /// Nothing configured: every `/ask` and `/gSolve` request is refused with
    /// 401. The default of [`HttpServer::bind`] — the server never answers
    /// questions anonymously by accident.
    Unconfigured,
    /// Clients must send `Authorization: Bearer <token>`.
    Token(Secret),
    /// Explicit opt-out: no credential is required, but only clients that
    /// connect from a loopback address are served (others get 403). Behind a
    /// reverse proxy on the same host every client looks local — use a token
    /// there.
    LoopbackOnly,
}

/// Per-client and per-request limits of the HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    /// `/ask` + `/gSolve` requests per client IP per minute (a token bucket
    /// with that burst); 0 = unlimited. Excess requests get 429 with
    /// `Retry-After`.
    pub requests_per_minute: u32,
    /// Maximum question length in bytes; longer questions get 413.
    pub max_question_bytes: usize,
    /// Budget for the engine work one request may cause.
    pub cost: CostLimits,
}

/// Default `/ask` + `/gSolve` requests per client IP per minute.
pub const DEFAULT_REQUESTS_PER_MINUTE: u32 = 30;
/// Default maximum question length (bytes).
pub const DEFAULT_MAX_QUESTION_BYTES: usize = 4096;
/// Default maximum LLM calls per request.
pub const DEFAULT_MAX_LLM_CALLS: usize = 16;
/// Default maximum KB rows fetched per request.
pub const DEFAULT_MAX_KB_ROWS: usize = 200_000;
/// Default wall-clock budget per request (seconds).
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;

impl Default for HttpLimits {
    fn default() -> Self {
        HttpLimits {
            requests_per_minute: DEFAULT_REQUESTS_PER_MINUTE,
            max_question_bytes: DEFAULT_MAX_QUESTION_BYTES,
            cost: CostLimits {
                max_llm_calls: Some(DEFAULT_MAX_LLM_CALLS),
                max_kb_rows: Some(DEFAULT_MAX_KB_ROWS),
                timeout: Some(Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS)),
            },
        }
    }
}

/// HTTP server wrapping a [`SolveEngine`].
pub struct HttpServer {
    engine: Arc<SolveEngine>,
    listener: TcpListener,
    max_conn: usize,
    auth: HttpAuth,
    limits: HttpLimits,
    read_deadline: Duration,
    /// Test failpoint: fail this many thread spawns (review conc#10).
    #[cfg(test)]
    fail_spawns: AtomicUsize,
}

impl HttpServer {
    /// Bind to `addr` (e.g. `127.0.0.1:0` for an ephemeral port). The server
    /// starts with [`HttpAuth::Unconfigured`] (question endpoints refused
    /// until [`with_token`](Self::with_token) or
    /// [`allow_unauthenticated_loopback`](Self::allow_unauthenticated_loopback))
    /// and the default [`HttpLimits`].
    pub fn bind(engine: Arc<SolveEngine>, addr: impl ToSocketAddrs) -> Result<HttpServer> {
        let listener = TcpListener::bind(addr).map_err(|e| Error::Http(e.to_string()))?;
        Ok(HttpServer {
            engine,
            listener,
            max_conn: 64,
            auth: HttpAuth::Unconfigured,
            limits: HttpLimits::default(),
            read_deadline: REQUEST_DEADLINE,
            #[cfg(test)]
            fail_spawns: AtomicUsize::new(0),
        })
    }

    /// Cap the wall-clock time for reading one request (headers and body;
    /// default 30 s). A client still sending after it gets a 400 and the
    /// connection is closed, so a slow drip cannot pin a slot.
    pub fn with_read_deadline(mut self, d: Duration) -> HttpServer {
        self.read_deadline = d;
        self
    }

    /// Cap the number of concurrently-handled connections (excess → 503).
    pub fn with_max_conn(mut self, n: usize) -> HttpServer {
        self.max_conn = n.max(1);
        self
    }

    /// Require `Authorization: Bearer <token>` on `/ask` and `/gSolve`. An
    /// empty token is refused (the server stays [`HttpAuth::Unconfigured`]).
    pub fn with_token(mut self, token: impl Into<String>) -> HttpServer {
        let token = token.into();
        self.auth = if token.trim().is_empty() {
            HttpAuth::Unconfigured
        } else {
            HttpAuth::Token(Secret::new(token))
        };
        self
    }

    /// Serve `/ask` and `/gSolve` without a credential, to loopback clients
    /// only (the explicit opt-out; see [`HttpAuth::LoopbackOnly`]).
    pub fn allow_unauthenticated_loopback(mut self) -> HttpServer {
        self.auth = HttpAuth::LoopbackOnly;
        self
    }

    /// Set the authentication mode directly.
    pub fn with_auth(mut self, auth: HttpAuth) -> HttpServer {
        self.auth = auth;
        self
    }

    /// Replace the per-client / per-request limits.
    pub fn with_limits(mut self, limits: HttpLimits) -> HttpServer {
        self.limits = limits;
        self
    }

    /// The configured authentication mode.
    pub fn auth(&self) -> &HttpAuth {
        &self.auth
    }

    /// The bound local address (useful after binding to port 0).
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| Error::Http(e.to_string()))
    }

    /// Accept and serve connections forever. Each connection gets socket
    /// timeouts (anti-slowloris) and runs on its own thread, with a cap on the
    /// number of concurrent handlers (excess connections get a 503).
    pub fn serve_forever(&self) {
        let ctx = Arc::new(Ctx {
            engine: Arc::clone(&self.engine),
            auth: self.auth.clone(),
            limits: self.limits,
            limiter: RateLimiter::new(self.limits.requests_per_minute),
            read_deadline: self.read_deadline,
            failures: AtomicU64::new(0),
        });
        let active = Arc::new(AtomicUsize::new(0));
        for stream in self.listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

            if active.load(Ordering::Acquire) >= self.max_conn {
                let _ = write_json(
                    &mut stream,
                    503,
                    &json!({"status":"error","message":"server busy"}),
                );
                continue;
            }
            // The slot is released by a drop guard (review conc#10 /
            // cluster-nlqa#20): a handler that panics no longer leaks it, so
            // max_conn panics can't turn into a permanent "server busy".
            let slot = Slot::take(&active);
            // A second handle on the socket, so a failed thread spawn can
            // still answer 503 instead of taking the accept loop down.
            let fallback = stream.try_clone().ok();
            let ctx = Arc::clone(&ctx);
            let spawned = self.spawn_handler(move || {
                let mut stream = stream;
                {
                    let _slot = slot; // released at the end of this block, also on unwind
                    serve_connection(&mut stream, &ctx);
                }
                // The connection closes only after the slot is free, so a
                // client that has read the response to EOF can rely on its
                // slot being available again.
                drop(stream);
            });
            if let Err(e) = spawned {
                // The closure (and with it the slot) was dropped by the failed
                // spawn; answer on the second handle and keep accepting.
                eprintln!("gnlqa: cannot start a connection handler thread: {e}");
                if let Some(mut s) = fallback {
                    let _ = write_json(
                        &mut s,
                        503,
                        &json!({"status":"error","message":"server busy"}),
                    );
                }
            }
        }
    }

    /// Start a handler thread (`Builder::spawn`, so an OS refusal is an
    /// error, not a panic of the accept loop).
    fn spawn_handler(&self, f: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
        #[cfg(test)]
        {
            let n = self.fail_spawns.load(Ordering::Acquire);
            if n > 0 {
                self.fail_spawns.store(n - 1, Ordering::Release);
                return Err(std::io::Error::other("injected thread-spawn failure"));
            }
        }
        thread::Builder::new()
            .name("gnlqa-http".into())
            .spawn(f)
            .map(|_| ())
    }
}

/// One taken connection slot; dropping it gives the slot back.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>) -> Slot {
        active.fetch_add(1, Ordering::AcqRel);
        Slot(Arc::clone(active))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serve one connection; a panic in the handler is caught, logged and
/// answered with a 500 (the response had not been written yet: replies are
/// written only after routing returns).
fn serve_connection(stream: &mut TcpStream, ctx: &Ctx) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_connection(stream, ctx)
    }));
    if r.is_err() {
        eprintln!("gnlqa: a request handler panicked; answered 500");
        let _ = write_json(
            stream,
            500,
            &json!({"status":"error","message":"internal error"}),
        );
    }
}

/// What every connection handler shares.
struct Ctx {
    engine: Arc<SolveEngine>,
    auth: HttpAuth,
    limits: HttpLimits,
    limiter: RateLimiter,
    /// Wall-clock cap on reading one request (anti-slowloris).
    read_deadline: Duration,
    /// Numbers failed requests, so a client-visible `request_id` can be
    /// matched with the detailed server-log line.
    failures: AtomicU64,
}

impl Ctx {
    /// Log an engine error in full on the server side and return the id the
    /// client sees with the redacted message (review cluster-nlqa#18: the
    /// client gets [`Error::client_message`], never the internal detail —
    /// endpoint URLs, provider error bodies, gStore error text).
    fn log_failure(&self, endpoint: &str, e: &Error) -> String {
        let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("err-{n}");
        eprintln!("gnlqa: {endpoint} request {id} failed: {e}");
        id
    }
}

/// A per-client-IP token bucket: `per_minute` requests of burst, refilled at
/// `per_minute / 60` per second.
struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl RateLimiter {
    fn new(per_minute: u32) -> RateLimiter {
        RateLimiter {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token for `ip`; `Err(retry_after_secs)` when none is left.
    fn check(&self, ip: IpAddr) -> std::result::Result<(), u64> {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> std::result::Result<(), u64> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let cap = f64::from(self.per_minute);
        let per_sec = cap / 60.0;
        let refill = |tokens: f64, at: Instant| {
            (tokens + now.saturating_duration_since(at).as_secs_f64() * per_sec).min(cap)
        };
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_TRACKED_CLIENTS && !map.contains_key(&ip) {
            // Forget clients whose bucket has refilled completely (they are
            // indistinguishable from new ones); if a flood from many addresses
            // still fills the table, start over rather than refuse new clients.
            map.retain(|_, (t, at)| refill(*t, *at) < cap);
            if map.len() >= MAX_TRACKED_CLIENTS {
                map.clear();
            }
        }
        let (tokens, at) = map.entry(ip).or_insert((cap, now));
        let available = refill(*tokens, *at);
        *at = now;
        if available >= 1.0 {
            *tokens = available - 1.0;
            Ok(())
        } else {
            *tokens = available;
            Err(((1.0 - available) / per_sec).ceil().max(1.0) as u64)
        }
    }
}

/// One parsed request (only what the routes need).
struct Request {
    method: String,
    path: String,
    /// The value of the `Authorization` header, if any.
    authorization: Option<String>,
    body: String,
}

/// A response: status, JSON body and extra headers.
struct Reply {
    status: u16,
    payload: Value,
    headers: Vec<(&'static str, String)>,
}

impl Reply {
    fn json(status: u16, payload: Value) -> Reply {
        Reply {
            status,
            payload,
            headers: Vec::new(),
        }
    }
    fn error(status: u16, message: &str) -> Reply {
        Reply::json(status, json!({"status":"error","message": message}))
    }
}

/// Parse one request, route it, and write the response.
fn handle_connection(stream: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    let deadline = Instant::now() + ctx.read_deadline;
    let peer = stream.peer_addr().ok().map(|a| a.ip());
    let Some(req) = read_request(stream, deadline)? else {
        return write_json(
            stream,
            400,
            &json!({"status":"error","message":"bad request"}),
        );
    };

    let reply = route(ctx, peer, &req);
    write_reply(stream, &reply)
}

/// Constant-time byte comparison (the length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Admission control for the question endpoints: rate limit (before
/// authentication, so it also bounds token guessing), then authentication.
fn admit(ctx: &Ctx, peer: Option<IpAddr>, req: &Request) -> Option<Reply> {
    let ip = peer.unwrap_or(IpAddr::from([0, 0, 0, 0]));
    if let Err(retry) = ctx.limiter.check(ip) {
        let mut r = Reply::error(429, "too many requests");
        r.headers.push(("Retry-After", retry.to_string()));
        return Some(r);
    }
    match &ctx.auth {
        HttpAuth::Unconfigured => {
            let mut r = Reply::error(
                401,
                "authentication required: this server has no client token configured",
            );
            r.headers.push(("WWW-Authenticate", "Bearer".to_string()));
            Some(r)
        }
        HttpAuth::Token(token) => {
            let presented = req
                .authorization
                .as_deref()
                .and_then(|v| {
                    let (scheme, rest) = v.trim().split_once(' ')?;
                    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
                })
                .unwrap_or("");
            if ct_eq(presented.as_bytes(), token.expose().as_bytes()) {
                None
            } else {
                let mut r = Reply::error(401, "authentication required");
                r.headers.push(("WWW-Authenticate", "Bearer".to_string()));
                Some(r)
            }
        }
        HttpAuth::LoopbackOnly => {
            if peer.is_some_and(|ip| ip.is_loopback()) {
                None
            } else {
                Some(Reply::error(
                    403,
                    "unauthenticated access is limited to loopback clients",
                ))
            }
        }
    }
}

/// Answer `q` under the request's cost budget.
fn ask(ctx: &Ctx, q: &str) -> Result<crate::pipeline::Answer> {
    budget::with_limits(ctx.limits.cost, || ctx.engine.ask(q))
}

/// The status for an engine error: an exhausted budget is 503, anything else 500.
fn error_status(e: &Error) -> u16 {
    if matches!(e, Error::Limit(_)) {
        503
    } else {
        500
    }
}

/// Dispatch by method+path.
fn route(ctx: &Ctx, peer: Option<IpAddr>, req: &Request) -> Reply {
    let question_endpoint = matches!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", "/ask") | ("POST", "/gSolve")
    );
    if question_endpoint {
        if let Some(refused) = admit(ctx, peer, req) {
            return refused;
        }
    }
    let question = || -> std::result::Result<String, Reply> {
        let q = question_of(&req.body).ok_or_else(|| Reply::error(400, "missing 'question'"))?;
        if q.len() > ctx.limits.max_question_bytes {
            return Err(Reply::error(
                413,
                &format!("question exceeds {} bytes", ctx.limits.max_question_bytes),
            ));
        }
        Ok(q)
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => Reply::json(200, json!({"status":"ok"})),
        ("POST", "/ask") => match question() {
            Ok(q) => match ask(ctx, &q) {
                Ok(a) => Reply::json(
                    200,
                    json!({
                        "answer": a.text,
                        "explanation": a.explanation,
                        "sparql": a.sparql,
                        "values": a.values,
                        "citations": citations_json(&a.citations),
                        "confidence": round3(a.confidence),
                        "abstained": a.abstained,
                        "rounds": a.rounds,
                        "provenance": a.provenance.tag(),
                    }),
                ),
                // Native endpoint: a real failure is a 5xx (proxies/monitoring
                // can detect it), unlike the gAnswer-compat /gSolve below.
                Err(e) => {
                    let id = ctx.log_failure("/ask", &e);
                    Reply::json(
                        error_status(&e),
                        json!({
                            "status": "error",
                            "code": e.code(),
                            "message": e.client_message(),
                            "request_id": id,
                        }),
                    )
                }
            },
            Err(r) => r,
        },
        ("POST", "/gSolve") => match question() {
            Ok(q) => match ask(ctx, &q) {
                // gAnswer-compatible subset. Honor abstention: withhold answers
                // (they're already empty) and flag it so a client can't mistake a
                // suppressed guess for a confident result.
                Ok(a) => Reply::json(
                    200,
                    json!({
                        "status": if a.abstained { "abstained" } else { "ok" },
                        "question": q,
                        "sparql": a.sparql,
                        "answers": a.values,
                        "confidence": round3(a.confidence),
                    }),
                ),
                Err(e) => {
                    let id = ctx.log_failure("/gSolve", &e);
                    Reply::json(
                        200,
                        json!({
                            "status": "error",
                            "question": q,
                            "code": e.code(),
                            "message": e.client_message(),
                            "request_id": id,
                        }),
                    )
                }
            },
            Err(r) => r,
        },
        _ => Reply::error(404, "not found"),
    }
}

/// Round an f32 to 3 decimals as f64, so JSON doesn't print f32 widening noise.
fn round3(x: f32) -> f64 {
    ((x * 1000.0).round() / 1000.0) as f64
}

/// Serialize the supporting triples so a client can audit the answer (the engine
/// already gathered them; the previous response silently dropped the field).
fn citations_json(citations: &[crate::ground::Citation]) -> Value {
    Value::Array(
        citations
            .iter()
            .map(|c| json!({ "subject": c.subject, "predicate": c.predicate, "object": c.object }))
            .collect(),
    )
}

/// Extract `question` from a JSON body (or a raw non-JSON body as the question).
fn question_of(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return v["question"]
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
    }
    let t = body.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Read method, path, and (Content-Length) body from an HTTP/1.1 request, with
/// bounded line/body sizes so a malicious client can't exhaust memory.
fn read_request(stream: &mut TcpStream, deadline: Instant) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let Some(request_line) = read_line_capped(&mut reader, MAX_LINE, deadline)? else {
        return Ok(None); // empty, over-long, or past the request deadline
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (
        method.to_string(),
        target.split('?').next().unwrap_or("/").to_string(),
    );

    // Headers → find Content-Length and Authorization (bounded line length
    // AND header count).
    let mut content_length = 0usize;
    let mut authorization = None;
    let mut headers = 0usize;
    loop {
        let Some(line) = read_line_capped(&mut reader, MAX_LINE, deadline)? else {
            return Ok(None);
        };
        if line.is_empty() {
            break;
        }
        headers += 1;
        if headers > MAX_HEADERS {
            return Ok(None); // header flood (slowloris variant) — refuse
        }
        if let Some(v) = line.split_once(':') {
            let name = v.0.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = v.1.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(v.1.trim().to_string());
            }
        }
    }

    if content_length > MAX_BODY {
        return Ok(None); // refuse oversized bodies (don't pre-allocate)
    }
    let Some(body) = read_body(&mut reader, content_length, deadline)? else {
        return Ok(None); // slow-drip body past the deadline
    };
    Ok(Some(Request {
        method,
        path,
        authorization,
        body: String::from_utf8_lossy(&body).into_owned(),
    }))
}

/// Before a read that will hit the socket, bound the socket's read timeout
/// by the time left until `deadline` (and [`IO_TIMEOUT`]), so a client that
/// goes silent is cut off at the request deadline rather than only after a
/// full per-read timeout. Returns `false` once the deadline has passed.
fn arm(reader: &BufReader<TcpStream>, deadline: Instant) -> std::io::Result<bool> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Ok(false);
    }
    if reader.buffer().is_empty() {
        reader
            .get_ref()
            .set_read_timeout(Some(left.min(IO_TIMEOUT)))?;
    }
    Ok(true)
}

/// Read into `buf`, mapping a read timeout to `None` (the request deadline or
/// the per-read timeout expired: answer 400 and close).
fn read_some(reader: &mut BufReader<TcpStream>, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    match reader.read(buf) {
        Ok(n) => Ok(Some(n)),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read exactly `len` body bytes, giving up once the request `deadline` passes so
/// a slow-drip body can't hold the connection open forever.
fn read_body(
    reader: &mut BufReader<TcpStream>,
    len: usize,
    deadline: Instant,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut body = Vec::with_capacity(len.min(8192));
    let mut chunk = [0u8; 4096];
    let mut got = 0usize;
    while got < len {
        if !arm(reader, deadline)? {
            return Ok(None);
        }
        let want = (len - got).min(chunk.len());
        let Some(n) = read_some(reader, &mut chunk[..want])? else {
            return Ok(None);
        };
        if n == 0 {
            break; // EOF before Content-Length; return what we have
        }
        body.extend_from_slice(&chunk[..n]);
        got += n;
    }
    Ok(Some(body))
}

/// Read one `\n`-terminated line (trailing `\r` stripped), capped at `max`
/// bytes. Returns `None` on EOF-with-nothing, if the line exceeds `max`, or once
/// the request `deadline` has passed (checked between bytes, and the socket's
/// read timeout is bounded by it, so neither a trickle nor silence can extend
/// the read past the deadline).
fn read_line_capped(
    reader: &mut BufReader<TcpStream>,
    max: usize,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if !arm(reader, deadline)? {
            return Ok(None);
        }
        let Some(n) = read_some(reader, &mut byte)? else {
            return Ok(None);
        };
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > max {
            return Ok(None);
        }
    }
    let s = String::from_utf8_lossy(&buf)
        .trim_end_matches('\r')
        .to_string();
    Ok(Some(s))
}

/// Write a JSON response.
fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) -> std::io::Result<()> {
    write_reply(stream, &Reply::json(status, payload.clone()))
}

/// Write a [`Reply`] (JSON body plus its extra headers).
fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let body = reply.payload.to_string();
    let status = reply.status;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut extra = String::new();
    for (k, v) in &reply.headers {
        extra.push_str(&format!("{k}: {v}\r\n"));
    }
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;
    use std::io::Read; // the in-test HTTP client reads the response body

    fn answer_row(v: &str) -> Vec<Option<RdfTerm>> {
        vec![Some(RdfTerm {
            kind: TermKind::Uri,
            value: v.into(),
            datatype: None,
            lang: None,
        })]
    }

    /// Start an in-process server backed by mocks; return its address.
    fn start() -> std::net::SocketAddr {
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![answer_row("http://ex/a")],
        }]);
        let engine = Arc::new(SolveEngine::new(Box::new(llm), Box::new(kb)));
        // review cluster-nlqa#13: the question endpoints now require a client
        // token by default, so the test server is given one (and `request`
        // below presents it).
        let server = HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TEST_TOKEN);
        let addr = server.local_addr().unwrap();
        thread::spawn(move || server.serve_forever());
        addr
    }

    const TEST_TOKEN: &str = "unit-test-token";

    /// Minimal HTTP client: send `method path` with `body`, return the response body.
    fn request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        // review cluster-nlqa#13: present the test server's client token.
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TEST_TOKEN}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    }

    #[test]
    fn health_ask_and_gsolve() {
        let addr = start();

        let h = request(addr, "GET", "/health", "");
        assert!(h.contains("\"status\":\"ok\""));

        let a: Value =
            serde_json::from_str(&request(addr, "POST", "/ask", r#"{"question":"which x?"}"#))
                .unwrap();
        assert_eq!(a["values"][0], "http://ex/a");
        assert!(a["sparql"].as_str().unwrap().contains("<http://ex/p>"));
        // citations are now surfaced (was silently dropped) — always an array.
        assert!(a["citations"].is_array());
    }

    #[test]
    fn header_flood_is_rejected() {
        let addr = start();
        let mut s = TcpStream::connect(addr).unwrap();
        let mut req = String::from("POST /ask HTTP/1.1\r\nHost: x\r\n");
        for i in 0..(MAX_HEADERS + 5) {
            req.push_str(&format!("X-Pad-{i}: y\r\n"));
        }
        req.push_str("\r\n"); // end of headers
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(
            resp.contains("bad request"),
            "expected 400 for a header flood, got: {resp}"
        );
    }

    #[test]
    fn gsolve_compat_shape() {
        let addr = start();
        let g: Value =
            serde_json::from_str(&request(addr, "POST", "/gSolve", r#"{"question":"q"}"#)).unwrap();
        assert_eq!(g["status"], "ok");
        assert_eq!(g["answers"][0], "http://ex/a");
        assert_eq!(g["question"], "q");
    }

    #[test]
    fn missing_question_and_not_found() {
        let addr = start();
        let bad = request(addr, "POST", "/ask", r#"{}"#);
        assert!(bad.contains("missing 'question'"));
        let nf = request(addr, "GET", "/nope", "");
        assert!(nf.contains("not found"));
    }

    #[test]
    fn rate_limiter_refills_per_client() {
        let rl = RateLimiter::new(2);
        let t0 = Instant::now();
        let a = IpAddr::from([10, 0, 0, 1]);
        let b = IpAddr::from([10, 0, 0, 2]);
        assert!(rl.check_at(a, t0).is_ok());
        assert!(rl.check_at(a, t0).is_ok());
        let retry = rl.check_at(a, t0).unwrap_err();
        assert!(
            retry >= 1,
            "Retry-After must be a positive number of seconds"
        );
        // another client has its own bucket
        assert!(rl.check_at(b, t0).is_ok());
        // 2/min refills one token every 30 s
        assert!(rl.check_at(a, t0 + Duration::from_secs(29)).is_err());
        assert!(rl.check_at(a, t0 + Duration::from_secs(60)).is_ok());
        // 0 = unlimited
        let off = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(off.check_at(a, t0).is_ok());
        }
    }

    #[test]
    fn rate_limiter_table_is_bounded() {
        let rl = RateLimiter::new(1);
        let t0 = Instant::now();
        for i in 0..(MAX_TRACKED_CLIENTS as u32 + 50) {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(i));
            let _ = rl.check_at(ip, t0);
        }
        assert!(rl.buckets.lock().unwrap().len() <= MAX_TRACKED_CLIENTS);
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    fn ctx_with(auth: HttpAuth) -> Ctx {
        Ctx {
            engine: Arc::new(SolveEngine::new(
                Box::new(MockLlm::fixed("x")),
                Box::new(MockKb::new(vec![])),
            )),
            auth,
            limits: HttpLimits::default(),
            limiter: RateLimiter::new(0),
            read_deadline: REQUEST_DEADLINE,
            failures: AtomicU64::new(0),
        }
    }

    fn ask_req(authorization: Option<&str>) -> Request {
        Request {
            method: "POST".into(),
            path: "/ask".into(),
            authorization: authorization.map(String::from),
            body: r#"{"question":"q"}"#.into(),
        }
    }

    #[test]
    fn loopback_opt_out_refuses_remote_peers() {
        // review cluster-nlqa#13: the unauthenticated mode is loopback-only.
        let ctx = ctx_with(HttpAuth::LoopbackOnly);
        let remote = Some(IpAddr::from([192, 0, 2, 7]));
        let r = admit(&ctx, remote, &ask_req(None)).expect("remote peer refused");
        assert_eq!(r.status, 403);
        assert!(admit(&ctx, Some(IpAddr::from([127, 0, 0, 1])), &ask_req(None)).is_none());
        assert!(admit(&ctx, Some("::1".parse().unwrap()), &ask_req(None)).is_none());
        assert_eq!(admit(&ctx, None, &ask_req(None)).unwrap().status, 403);
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_token_exact() {
        let ctx = ctx_with(HttpAuth::Token(Secret::new("tok")));
        let peer = Some(IpAddr::from([192, 0, 2, 7]));
        assert!(admit(&ctx, peer, &ask_req(Some("Bearer tok"))).is_none());
        assert!(admit(&ctx, peer, &ask_req(Some("bearer  tok "))).is_none());
        assert_eq!(
            admit(&ctx, peer, &ask_req(Some("Bearer tok2")))
                .unwrap()
                .status,
            401
        );
        assert_eq!(
            admit(&ctx, peer, &ask_req(Some("tok"))).unwrap().status,
            401
        );
        assert_eq!(admit(&ctx, peer, &ask_req(None)).unwrap().status, 401);
        let none = ctx_with(HttpAuth::Unconfigured);
        assert_eq!(
            admit(&none, peer, &ask_req(Some("Bearer tok")))
                .unwrap()
                .status,
            401
        );
    }

    #[test]
    fn failed_thread_spawn_answers_503_and_keeps_serving() {
        // review conc#10: thread::spawn used to panic the accept loop when the
        // OS refused a thread; now the client gets a 503, the slot is
        // returned and the next connection is served.
        let engine = Arc::new(SolveEngine::new(
            Box::new(MockLlm::fixed("x")),
            Box::new(MockKb::new(vec![])),
        ));
        let server = HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_max_conn(1);
        server.fail_spawns.store(1, Ordering::SeqCst);
        let addr = server.local_addr().unwrap();
        thread::spawn(move || server.serve_forever());
        // The refusal is written without reading the request, so this client
        // sends nothing (unread request bytes would make the close a reset).
        let first = raw_status(addr, "");
        assert!(first.starts_with("HTTP/1.1 503"), "{first}");
        let second = raw_status(addr, "GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(second.starts_with("HTTP/1.1 200"), "{second}");
    }

    fn raw_status(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        if !req.is_empty() {
            s.write_all(req.as_bytes()).unwrap();
        }
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    }
}
