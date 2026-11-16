//! What a request may make the server buffer **before it is authenticated**
//! (review C-2), decided for both servers ([`Server`](crate::server::Server)
//! and [`ApiServer`](crate::http_api::ApiServer)) between reading a request's
//! head and reading its body.
//!
//! The connection loop reads a request body into memory before any handler,
//! IP list or credential check runs, so the body caps are the only guard
//! against an unauthenticated client:
//!
//! * **two caps by route class.** Every request may carry up to
//!   [`HttpOptions::preauth_max_body`] (default [`PREAUTH_MAX_BODY`], 64 MiB;
//!   `conf.ini`'s `preauth_max_body_size`). Only a request that is
//!   **authenticated from its head** — before a body byte is read — on a
//!   route that carries payloads (an upload, a SPARQL UPDATE, a build from
//!   inline RDF) may carry up to [`HttpOptions::max_body`] (`gserver`:
//!   `upload_max_body_size`). The server's *pre-body hook* decides: in API
//!   mode it checks the credentials a head can carry — HTTP Basic, a session
//!   token (`Authorization: Bearer`, `X-Session-Token`, `session=` /
//!   `token=` in the URL), or upstream's `username` / `password`
//!   (`encryption`) in the URL query string — against the user store, the IP
//!   lists and the failed-login lockout; the single-database server compares
//!   its Basic credentials (and, without any, serves every client as
//!   authenticated, as it always did — except a forwarded request, which it
//!   refuses after the body anyway). In both modes a request the browser
//!   policy (`allowed_hosts`, `cors_allow_origin`) refuses before routing
//!   never earns the upload cap. The hook runs only for a body that may
//!   exceed the pre-authentication cap (a larger or a chunked one), and
//!   `100 Continue` is sent only after it. A larger `Content-Length` is
//!   answered `413` without reading the body; a chunked body is cut off at
//!   its cap; credentials that fail the check are answered `401` (counted
//!   towards the lockout once).
//! * **a body budget.** All request bodies held in memory at once — being
//!   received, or held by a request being served — share
//!   [`HttpOptions::body_budget`] (default [`DEFAULT_BODY_BUDGET`], 512 MiB;
//!   `request_body_budget`), charged as the bytes arrive (not as a
//!   `Content-Length` announces them, so a client cannot reserve budget it
//!   does not send). A request that would exceed it is answered `503` and its
//!   connection closed. The budget is per server (per `serve_forever`), and
//!   never smaller than the largest body one request may have.
//!
//! The `/api` layer bounds what a body may make its parameter parser
//! allocate on top of this (`gapi::params`'s bounds).
//!
//! The functions marked hidden at the end let tests observe a running
//! server's budget.

use std::io::{self, BufRead, Read};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use crate::gapi::auth::{self, PreAuth};
use crate::gapi::params::parse_form;
use crate::http_api::{peer_ip, ApiInner};
use crate::server::{
    base64_decode, browser_refusal, current_peer, ip_is_loopback, locked, strip_basic_prefix,
    BadRequest, HttpOptions, SingleAuth, FORWARDING_HEADERS,
};

/// The default [`HttpOptions::preauth_max_body`]: 64 MiB.
pub const PREAUTH_MAX_BODY: usize = 64 * 1024 * 1024;

/// The default [`HttpOptions::body_budget`]: 512 MiB.
pub const DEFAULT_BODY_BUDGET: usize = 512 * 1024 * 1024;

/// `413` for a body over the pre-authentication cap.
const PREAUTH_TOO_LARGE: BadRequest = BadRequest {
    status: 413,
    message:
        "request body exceeds the limit for unauthenticated requests (preauth_max_body_size); \
              to send a larger upload, authenticate in the request head: HTTP Basic, a session \
              token, or username and password in the URL query string",
};

/// `413` for a body over the cap of an authenticated upload.
const UPLOAD_TOO_LARGE: BadRequest = BadRequest {
    status: 413,
    message: "request body exceeds the maximum size (upload_max_body_size)",
};

/// `503` when the body budget is exhausted.
const BUDGET_EXHAUSTED: BadRequest = BadRequest {
    status: 503,
    message: "the server holds too many request bodies in memory (request_body_budget); retry \
              later",
};

/// `401` for credentials in a request head that fail the check.
const HEAD_CREDENTIALS_INVALID: BadRequest = BadRequest {
    status: 401,
    message: "authentication failed: the credentials in the request head are not valid",
};

// ---------------------------------------------------------------------------
// The pre-body decision
// ---------------------------------------------------------------------------

/// The request line and headers of a request whose body has not been read.
pub(crate) struct HeadView<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) query: Option<&'a str>,
    /// Header names lowercased.
    pub(crate) headers: &'a [(String, String)],
}

impl HeadView<'_> {
    /// A header by (case-insensitive) name.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// A URL query parameter (form-decoded; the first occurrence).
    fn query_param(&self, name: &str) -> Option<String> {
        let q = self.query?;
        // The request line is at most 64 KiB: a bounded scan.
        parse_form(q)
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// The request path without one trailing `/` (as both servers route it).
    fn route_path(&self) -> &str {
        match self.path.strip_suffix('/') {
            Some(p) if !p.is_empty() => p,
            _ => self.path,
        }
    }
}

/// What a request's head entitles its body to (a pre-body hook's verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Not authenticated by its head (or not a payload route): at most
    /// [`HttpOptions::preauth_max_body`].
    PreAuth,
    /// Authenticated by its head on a payload route: at most
    /// [`HttpOptions::max_body`].
    Upload,
    /// Answered with this error without reading the body.
    Refuse(BadRequest),
}

/// A server's pre-body hook: classifies a request from its head (the client
/// address is [`current_peer`]). Runs only for a body that may exceed the
/// pre-authentication cap.
pub(crate) type PreBodyHook = Box<dyn Fn(&HeadView<'_>) -> Admission + Send + Sync>;

/// The cap a request body is read with, and the error a larger one gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyCap {
    pub(crate) max: usize,
    pub(crate) too_large: BadRequest,
}

/// The body caps, the budget and the pre-body hook of one server (shared by
/// its connections through `ConnEnv`).
pub(crate) struct Ingest {
    /// [`HttpOptions::preauth_max_body`], at most `upload_max`.
    preauth_max: usize,
    /// [`HttpOptions::max_body`].
    upload_max: usize,
    budget: Arc<BodyBudget>,
    hook: Option<PreBodyHook>,
}

impl Ingest {
    /// The caps and a fresh budget of `opts`, with `hook` (none: every
    /// request is held to the pre-authentication cap).
    pub(crate) fn new(opts: &HttpOptions, hook: Option<PreBodyHook>) -> Ingest {
        let preauth_max = opts.preauth_max_body.min(opts.max_body);
        Ingest {
            preauth_max,
            upload_max: opts.max_body,
            budget: Arc::new(BodyBudget::new(opts.body_budget.max(opts.max_body))),
            hook,
        }
    }

    /// The largest body any request may have (what the head parser checks a
    /// `Content-Length` against before the hook runs).
    pub(crate) fn largest_body(&self) -> usize {
        self.upload_max
    }

    /// The cap of the body of the request `head`, which announces
    /// `content_length` bytes (`None`: a chunked body) — decided before any
    /// byte of it is read. `Err`: answer that without reading the body.
    pub(crate) fn body_cap(
        &self,
        head: &HeadView<'_>,
        content_length: Option<usize>,
    ) -> Result<BodyCap, BadRequest> {
        let preauth = BodyCap {
            max: self.preauth_max,
            too_large: PREAUTH_TOO_LARGE,
        };
        // Within the pre-authentication cap nothing needs deciding.
        if content_length.is_some_and(|n| n <= self.preauth_max) {
            return Ok(preauth);
        }
        let Some(hook) = &self.hook else {
            return Ok(preauth);
        };
        match hook(head) {
            Admission::PreAuth => Ok(preauth),
            Admission::Upload => Ok(BodyCap {
                max: self.upload_max,
                too_large: UPLOAD_TOO_LARGE,
            }),
            Admission::Refuse(bad) => Err(bad),
        }
    }

    /// Whether a body announcing `content_length` bytes fits in what the
    /// budget has left now — checked before the body is read (and before
    /// `100 Continue`), so a request the budget cannot take is answered `503`
    /// at once instead of part-way through its body. Nothing is reserved:
    /// the bytes are charged as they arrive (see [`hold`](Self::hold)).
    pub(crate) fn admit(&self, content_length: usize) -> Result<(), BadRequest> {
        let held = locked(&self.budget.state).held;
        if held.saturating_add(content_length) > self.budget.total {
            Err(BUDGET_EXHAUSTED)
        } else {
            Ok(())
        }
    }

    /// A new, empty charge against this server's budget, for one body.
    pub(crate) fn hold(&self) -> BodyHold {
        BodyHold {
            budget: Arc::clone(&self.budget),
            held: 0,
            exhausted: false,
        }
    }

    /// The error a body read that ran out of budget is answered with.
    pub(crate) fn budget_exhausted() -> BadRequest {
        BUDGET_EXHAUSTED
    }

    /// Make this server's budget observable by tests (see
    /// [`body_budget_in_use`]) under its listening address.
    pub(crate) fn register(&self, addr: SocketAddr) {
        let reg = registry();
        let mut list = locked(&reg.list);
        list.retain(|(_, b)| b.strong_count() > 0);
        list.push((addr, Arc::downgrade(&self.budget)));
        drop(list);
        reg.added.notify_all();
    }
}

// ---------------------------------------------------------------------------
// The body budget
// ---------------------------------------------------------------------------

/// The bytes of request bodies a server holds in memory, bounded.
pub(crate) struct BodyBudget {
    total: usize,
    state: Mutex<BudgetState>,
    /// Notified on every change while someone waits on it (only
    /// [`wait_body_budget`] does: the serving path never waits for budget).
    changed: Condvar,
    /// Threads in [`wait_body_budget`] (incremented before they lock
    /// `state`, so a change made after their check sees them).
    waiters: AtomicUsize,
}

#[derive(Default)]
struct BudgetState {
    held: usize,
    peak: usize,
}

impl BodyBudget {
    fn new(total: usize) -> BodyBudget {
        BodyBudget {
            total,
            state: Mutex::new(BudgetState::default()),
            changed: Condvar::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Wake the threads in [`wait_body_budget`], if any (a condition
    /// variable's wake-up costs a system call even when nobody waits).
    fn notify(&self) {
        if self.waiters.load(Ordering::SeqCst) > 0 {
            self.changed.notify_all();
        }
    }

    /// Charge `n` more bytes, unless that would exceed the budget.
    fn try_take(&self, n: usize) -> bool {
        let mut st = locked(&self.state);
        match st.held.checked_add(n) {
            Some(held) if held <= self.total => {
                st.held = held;
                st.peak = st.peak.max(held);
                drop(st);
                self.notify();
                true
            }
            _ => false,
        }
    }

    fn give_back(&self, n: usize) {
        if n == 0 {
            return;
        }
        let mut st = locked(&self.state);
        st.held = st.held.saturating_sub(n);
        drop(st);
        self.notify();
    }

    fn in_use(&self) -> (usize, usize) {
        let st = locked(&self.state);
        (st.held, st.peak)
    }
}

/// The budget one request body has taken; given back when dropped — the
/// connection loop keeps it until the request's response is written.
pub(crate) struct BodyHold {
    budget: Arc<BodyBudget>,
    held: usize,
    exhausted: bool,
}

impl BodyHold {
    /// Charge `n` more bytes; `false` (and from then on
    /// [`exhausted`](Self::exhausted)) when the budget has no room.
    fn charge(&mut self, n: usize) -> bool {
        if self.exhausted {
            return false;
        }
        if n == 0 {
            return true;
        }
        if self.budget.try_take(n) {
            self.held += n;
            true
        } else {
            self.exhausted = true;
            false
        }
    }

    /// Whether a charge failed: the body read failed for lack of budget.
    pub(crate) fn exhausted(&self) -> bool {
        self.exhausted
    }
}

impl Drop for BodyHold {
    fn drop(&mut self) {
        self.budget.give_back(self.held);
    }
}

/// A reader that charges every byte read through it to a [`BodyHold`] and
/// fails once the budget has no room — wrapped around the connection while
/// a body is read, so the body readers need not know about the budget.
pub(crate) struct Budgeted<'a, R> {
    inner: &'a mut R,
    hold: &'a mut BodyHold,
}

impl<'a, R> Budgeted<'a, R> {
    pub(crate) fn new(inner: &'a mut R, hold: &'a mut BodyHold) -> Budgeted<'a, R> {
        Budgeted { inner, hold }
    }
}

fn over_budget() -> io::Error {
    io::Error::other("request body budget exhausted")
}

impl<R: Read> Read for Budgeted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.hold.exhausted() {
            return Err(over_budget());
        }
        let n = self.inner.read(buf)?;
        if !self.hold.charge(n) {
            return Err(over_budget());
        }
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Budgeted<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.hold.exhausted() {
            return Err(over_budget());
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
        // A failed charge marks the hold exhausted: the next read fails.
        self.hold.charge(amt);
    }
}

// ---------------------------------------------------------------------------
// The servers' hooks
// ---------------------------------------------------------------------------

/// The [`Ingest`] of one `Server::serve_forever` run: `opts`' caps and a
/// fresh budget (observable under `addr`), with [`single_db_hook`].
pub(crate) fn single_db_ingest(
    opts: &HttpOptions,
    auth: &Option<SingleAuth>,
    addr: SocketAddr,
) -> Arc<Ingest> {
    let ingest = Arc::new(Ingest::new(
        opts,
        Some(single_db_hook(auth.clone(), opts.clone())),
    ));
    ingest.register(addr);
    ingest
}

/// The [`Ingest`] of one `ApiServer::serve_forever` run: `opts`' caps and a
/// fresh budget (observable under `addr`), with [`api_hook`].
pub(crate) fn api_ingest(
    opts: &HttpOptions,
    inner: &Arc<ApiInner>,
    addr: SocketAddr,
) -> Arc<Ingest> {
    let hook = api_hook(Arc::clone(inner), opts.clone());
    let ingest = Arc::new(Ingest::new(opts, Some(hook)));
    ingest.register(addr);
    ingest
}

/// The single-database server's hook: `POST /update` and `POST /sparql`
/// carry payloads; with Basic credentials configured (`auth`) a request is
/// authenticated by a matching `Authorization: Basic` header (a user name
/// locked out at the client's address is not compared: the handler answers
/// it `429` after the body, which is held to the pre-auth cap), without any
/// every client is (the open server serves everyone) — except a request the
/// server refuses after its body anyway (`ServerInner::handle`): one the
/// browser policy of `opts` refuses (a foreign `Host` or `Origin`), and, on
/// the open server, one a reverse proxy on this machine forwards (unless
/// [`HttpOptions::trust_proxy_headers`]). Those are held to the
/// pre-authentication cap.
pub(crate) fn single_db_hook(auth: Option<SingleAuth>, opts: HttpOptions) -> PreBodyHook {
    Box::new(move |head| {
        let payload_route =
            head.method == "POST" && matches!(head.route_path(), "/update" | "/sparql");
        if !payload_route || browser_refused(head, &opts) {
            return Admission::PreAuth;
        }
        let authenticated = match &auth {
            None => !forwarded_by_untrusted_proxy(head, opts.trust_proxy_headers),
            Some(auth) => basic_credentials(head)
                .is_some_and(|decoded| auth.accepts(&decoded, &peer_ip(current_peer()))),
        };
        if authenticated {
            Admission::Upload
        } else {
            Admission::PreAuth
        }
    })
}

/// The multi-database server's hook (see the module docs): an upload route
/// whose head carries valid credentials gets the upload cap — unless the
/// browser policy of `opts` refuses the request (it is answered `403`
/// before routing, after its body). `opts`'
/// [`HttpOptions::trust_proxy_headers`] decides whether the client counts
/// as local (see [`client_is_local`]).
pub(crate) fn api_hook(inner: Arc<ApiInner>, opts: HttpOptions) -> PreBodyHook {
    Box::new(move |head| {
        if browser_refused(head, &opts) {
            return Admission::PreAuth;
        }
        api_admission(&inner, head, opts.trust_proxy_headers)
    })
}

/// Whether the browser policy of `opts` refuses the request of `head` (a
/// foreign `Host` on a loopback connection, a foreign `Origin` on a
/// state-changing request): the rule of `ConnMeta::refusal`, which both
/// servers apply before routing.
fn browser_refused(head: &HeadView<'_>, opts: &HttpOptions) -> bool {
    let loopback = current_peer().is_some_and(|p| ip_is_loopback(p.ip()));
    browser_refusal(
        head.method,
        head.header("host"),
        head.header("origin"),
        loopback,
        opts,
    )
    .is_some()
}

/// Whether `head` comes from a reverse proxy on this machine whose
/// forwarding headers are not trusted: a loopback peer forwarding one of
/// [`FORWARDING_HEADERS`] — the rule of `ConnMeta::forwarded`, by which the
/// open single-database server refuses a request after its body.
fn forwarded_by_untrusted_proxy(head: &HeadView<'_>, trust_proxy_headers: bool) -> bool {
    !trust_proxy_headers
        && current_peer().is_some_and(|p| ip_is_loopback(p.ip()))
        && FORWARDING_HEADERS.iter().any(|h| head.header(h).is_some())
}

/// Whether the client of `head` counts as local — the rule of
/// [`peer_is_trusted_local`](crate::server::peer_is_trusted_local), applied
/// to a head whose body has not been read: a loopback peer that forwards no
/// proxy header (unless `trust_proxy_headers`). The full credential check
/// after the body (`gapi::auth::verify`) grants the loopback privileges by
/// the same rule, so the pre-body check never accepts what it would refuse.
fn client_is_local(head: &HeadView<'_>, trust_proxy_headers: bool) -> bool {
    current_peer().is_some_and(|p| ip_is_loopback(p.ip()))
        && (trust_proxy_headers || !FORWARDING_HEADERS.iter().any(|h| head.header(h).is_some()))
}

/// Which credentials a payload route takes in its head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadRoute {
    /// The upstream `/api` protocol: `username` / `password` (and
    /// `encryption`) in the URL query string.
    Upstream,
    /// The REST endpoints: HTTP Basic or a session token.
    Rest,
}

/// The payload routes of the multi-database server: upstream's
/// `/file/upload` (`gapi::ops_file`), its `/api` endpoint when the
/// operation is one that carries a SPARQL text (`query`, `tquery`) or is not
/// in the URL (it is then in the body), and the REST endpoints that read
/// RDF or SPARQL from their body.
fn payload_route(head: &HeadView<'_>) -> Option<PayloadRoute> {
    if head.method != "POST" {
        return None;
    }
    match head.route_path() {
        "/file/upload" | "/grpc/file/upload" => Some(PayloadRoute::Upstream),
        "/api" | "/grpc/api" | "/ghttp/api" | "/" => {
            let op = head.query_param("operation");
            matches!(op.as_deref(), None | Some("query" | "tquery"))
                .then_some(PayloadRoute::Upstream)
        }
        "/upload" | "/build" | "/update" | "/tquery" | "/batchInsert" | "/batchRemove" => {
            Some(PayloadRoute::Rest)
        }
        _ => None,
    }
}

fn api_admission(inner: &ApiInner, head: &HeadView<'_>, trust_proxy_headers: bool) -> Admission {
    let Some(route) = payload_route(head) else {
        return Admission::PreAuth;
    };
    let ip = peer_ip(current_peer());
    let local = client_is_local(head, trust_proxy_headers);
    // A blocked or locked-out address is not checked at all (its handler
    // answers it `403`): the body is held to the pre-authentication cap,
    // whatever the head carries — a session token included.
    if !inner.ipfilter.permits(&ip) || inner.ipfilter.locked_out(&ip) {
        return Admission::PreAuth;
    }
    let verdict = match route {
        PayloadRoute::Upstream => match auth::credentials_in_query(head.query) {
            Some(creds) => auth::check_before_body(inner, &ip, local, &creds),
            None => PreAuth::Unchecked,
        },
        PayloadRoute::Rest => {
            // A session token first (no password to guess), then Basic.
            let token_user =
                session_token(head).and_then(|tok| locked(&inner.state).users.session_user(&tok));
            if token_user.is_some() {
                PreAuth::Valid
            } else {
                match basic_credentials(head)
                    .and_then(|d| String::from_utf8(d).ok())
                    .and_then(|s| {
                        s.split_once(':')
                            .map(|(u, p)| (u.to_string(), p.to_string()))
                    }) {
                    Some((username, password)) => {
                        let creds = auth::Credentials {
                            username,
                            password,
                            ..auth::Credentials::default()
                        };
                        auth::check_before_body(inner, &ip, local, &creds)
                    }
                    None => PreAuth::Unchecked,
                }
            }
        }
    };
    match verdict {
        PreAuth::Valid => Admission::Upload,
        PreAuth::Unchecked => Admission::PreAuth,
        PreAuth::Invalid => Admission::Refuse(HEAD_CREDENTIALS_INVALID),
    }
}

/// The decoded `Authorization: Basic` token of `head` (`user:pass` bytes).
fn basic_credentials(head: &HeadView<'_>) -> Option<Vec<u8>> {
    let token = strip_basic_prefix(head.header("authorization")?)?.trim();
    base64_decode(token)
}

/// A session token in the head: `Authorization: Bearer`, `X-Session-Token`,
/// or `session=` / `token=` in the URL query string (as the REST endpoints
/// read it, minus a form body, which is not read yet).
fn session_token(head: &HeadView<'_>) -> Option<String> {
    if let Some(h) = head.header("authorization") {
        let h = h.trim_start();
        if h.get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
        {
            return Some(h[7..].trim().to_string());
        }
    }
    if let Some(h) = head.header("x-session-token") {
        return Some(h.trim().to_string());
    }
    head.query_param("session")
        .or_else(|| head.query_param("token"))
}

// ---------------------------------------------------------------------------
// Test observation (hidden)
// ---------------------------------------------------------------------------

/// The budgets of the servers serving in this process, by listening
/// address (registered when `serve_forever` starts).
struct Registry {
    list: Mutex<Vec<(SocketAddr, Weak<BodyBudget>)>>,
    /// Notified when a budget is registered.
    added: Condvar,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry {
        list: Mutex::new(Vec::new()),
        added: Condvar::new(),
    })
}

/// The budget of the server serving on `addr` (the most recent one).
fn budget_of(addr: SocketAddr) -> Option<Arc<BodyBudget>> {
    find_budget(&locked(&registry().list), addr)
}

fn find_budget(
    list: &[(SocketAddr, Weak<BodyBudget>)],
    addr: SocketAddr,
) -> Option<Arc<BodyBudget>> {
    list.iter()
        .rev()
        .filter(|(a, _)| *a == addr)
        .find_map(|(_, b)| b.upgrade())
}

/// The request-body bytes the server serving on `addr` holds in memory now,
/// and the most it held at once: `(held, peak)`. `None` when no server
/// serves there. For tests and diagnostics.
#[doc(hidden)]
pub fn body_budget_in_use(addr: SocketAddr) -> Option<(usize, usize)> {
    budget_of(addr).map(|b| b.in_use())
}

/// Wait (at most `timeout`, a hang guard) until the request-body bytes the
/// server serving on `addr` holds satisfy `cond`; whether they do. For
/// tests: a deterministic point at which a request's body has been read (or
/// released).
#[doc(hidden)]
pub fn wait_body_budget(addr: SocketAddr, timeout: Duration, cond: impl Fn(usize) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    // The server may not have started serving yet.
    let reg = registry();
    let mut list = locked(&reg.list);
    let budget = loop {
        if let Some(b) = find_budget(&list, addr) {
            break b;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        list = reg
            .added
            .wait_timeout(list, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    };
    drop(list);
    budget.waiters.fetch_add(1, Ordering::SeqCst);
    let met = {
        let mut st = locked(&budget.state);
        loop {
            if cond(st.held) {
                break true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break false;
            }
            st = budget
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    };
    budget.waiters.fetch_sub(1, Ordering::SeqCst);
    met
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn opts(preauth: usize, max: usize, budget: usize) -> HttpOptions {
        HttpOptions {
            preauth_max_body: preauth,
            max_body: max,
            body_budget: budget,
            ..HttpOptions::default()
        }
    }

    fn head<'a>(
        method: &'a str,
        path: &'a str,
        query: Option<&'a str>,
        headers: &'a [(String, String)],
    ) -> HeadView<'a> {
        HeadView {
            method,
            path,
            query,
            headers,
        }
    }

    #[test]
    fn small_bodies_never_reach_the_hook() {
        let hook: PreBodyHook = Box::new(|_| panic!("the hook must not run"));
        let ingest = Ingest::new(&opts(100, 1000, 1000), Some(hook));
        let h = head("POST", "/update", None, &[]);
        let cap = ingest.body_cap(&h, Some(100)).unwrap();
        assert_eq!((cap.max, cap.too_large), (100, PREAUTH_TOO_LARGE));
    }

    #[test]
    fn the_hook_picks_the_cap_of_larger_and_chunked_bodies() {
        for (verdict, want) in [
            (Admission::PreAuth, Ok(100)),
            (Admission::Upload, Ok(1000)),
            (
                Admission::Refuse(HEAD_CREDENTIALS_INVALID),
                Err(HEAD_CREDENTIALS_INVALID),
            ),
        ] {
            let hook: PreBodyHook = Box::new(move |_| verdict);
            let ingest = Ingest::new(&opts(100, 1000, 1000), Some(hook));
            let h = head("POST", "/update", None, &[]);
            for len in [Some(101), None] {
                assert_eq!(
                    ingest.body_cap(&h, len).map(|c| c.max),
                    want,
                    "{verdict:?} {len:?}"
                );
            }
        }
        // Without a hook everything is held to the pre-authentication cap,
        // and that cap never exceeds the largest body.
        let ingest = Ingest::new(&opts(100, 1000, 1000), None);
        let h = head("POST", "/update", None, &[]);
        assert_eq!(ingest.body_cap(&h, None).unwrap().max, 100);
        let ingest = Ingest::new(&opts(5000, 1000, 1000), None);
        assert_eq!(ingest.body_cap(&h, None).unwrap().max, 1000);
    }

    #[test]
    fn the_budget_is_charged_as_bytes_arrive_and_given_back_on_drop() {
        let ingest = Ingest::new(&opts(100, 100, 150), None);
        let mut first = ingest.hold();
        let mut src = BufReader::new(Cursor::new(vec![b'x'; 120]));
        let mut body = Vec::new();
        Budgeted::new(&mut src, &mut first)
            .take(100)
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(ingest.budget.in_use(), (100, 100));
        // A second body finds 50 bytes of room.
        let mut second = ingest.hold();
        let mut src2 = BufReader::new(Cursor::new(vec![b'y'; 80]));
        let mut body2 = Vec::new();
        let err = Budgeted::new(&mut src2, &mut second)
            .take(80)
            .read_to_end(&mut body2)
            .unwrap_err();
        assert_eq!(err.to_string(), "request body budget exhausted");
        assert!(second.exhausted() && !first.exhausted());
        drop(second);
        assert_eq!(ingest.budget.in_use().0, 100);
        drop(first);
        // The second body got as far as the budget let it (never past it).
        let (held, peak) = ingest.budget.in_use();
        assert_eq!(held, 0);
        assert!((100..=150).contains(&peak), "{peak}");
        // Line-wise reads (chunked framing) are charged on consume.
        let mut third = ingest.hold();
        let mut src3 = BufReader::new(Cursor::new(b"abc\ndef\n".to_vec()));
        let mut line = Vec::new();
        Budgeted::new(&mut src3, &mut third)
            .read_until(b'\n', &mut line)
            .unwrap();
        assert_eq!(line, b"abc\n");
        assert_eq!(ingest.budget.in_use().0, 4);
    }

    #[test]
    fn the_budget_is_never_smaller_than_the_largest_body() {
        let ingest = Ingest::new(&opts(10, 1000, 1), None);
        assert_eq!(ingest.budget.total, 1000);
        assert!(ingest.hold().charge(1000));
    }

    #[test]
    fn payload_routes_of_the_api_server() {
        let none: Vec<(String, String)> = Vec::new();
        let route = |m: &str, p: &str, q: Option<&str>| payload_route(&head(m, p, q, &none));
        assert_eq!(
            route("POST", "/file/upload", None),
            Some(PayloadRoute::Upstream)
        );
        assert_eq!(
            route("POST", "/grpc/file/upload/", None),
            Some(PayloadRoute::Upstream)
        );
        assert_eq!(route("POST", "/api", None), Some(PayloadRoute::Upstream));
        assert_eq!(
            route("POST", "/api", Some("operation=query&db_name=x")),
            Some(PayloadRoute::Upstream)
        );
        assert_eq!(route("POST", "/api", Some("operation=check")), None);
        assert_eq!(route("POST", "/update", None), Some(PayloadRoute::Rest));
        assert_eq!(route("POST", "/upload", None), Some(PayloadRoute::Rest));
        assert_eq!(route("GET", "/upload", None), None);
        assert_eq!(route("POST", "/login", None), None);
        assert_eq!(route("POST", "/sparql", None), None);
    }

    #[test]
    fn head_credentials_are_read_from_headers_and_the_query() {
        let headers = vec![
            ("authorization".to_string(), "Basic dTpwOnE=".to_string()), // u:p:q
        ];
        let h = head("POST", "/update", Some("token=t1"), &headers);
        assert_eq!(basic_credentials(&h).as_deref(), Some(&b"u:p:q"[..]));
        assert_eq!(session_token(&h).as_deref(), Some("t1"));
        let bearer = vec![("authorization".to_string(), "Bearer  abc ".to_string())];
        let h = head("POST", "/update", None, &bearer);
        assert_eq!(session_token(&h).as_deref(), Some("abc"));
        assert_eq!(basic_credentials(&h), None);
    }

    #[test]
    fn the_single_database_hook() {
        let basic = vec![("authorization".to_string(), "Basic dTpw".to_string())]; // u:p
        let open = single_db_hook(None, HttpOptions::default());
        assert_eq!(open(&head("POST", "/update", None, &[])), Admission::Upload);
        assert_eq!(
            open(&head("POST", "/status", None, &[])),
            Admission::PreAuth
        );
        let guarded = single_db_hook(Some(SingleAuth::new("u", "p")), HttpOptions::default());
        assert_eq!(
            guarded(&head("POST", "/sparql", None, &basic)),
            Admission::Upload
        );
        assert_eq!(
            guarded(&head("POST", "/sparql", None, &[])),
            Admission::PreAuth
        );
        let wrong = single_db_hook(Some(SingleAuth::new("u", "x")), HttpOptions::default());
        assert_eq!(
            wrong(&head("POST", "/update", None, &basic)),
            Admission::PreAuth
        );
        // review http#6: a user locked out at the client's address is not
        // compared — the body stays at the pre-auth cap and the handler
        // answers 429.
        let auth = SingleAuth::new("u", "p");
        for _ in 0..=crate::gapi::ipfilter::MAX_AUTH_FAILURES {
            auth.lockout_for_tests().note_failure("u", "unknown");
        }
        let locked = single_db_hook(Some(auth), HttpOptions::default());
        assert_eq!(
            locked(&head("POST", "/update", None, &basic)),
            Admission::PreAuth
        );
    }
}
