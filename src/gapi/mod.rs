//! The upstream-compatible gStore operation API ("gapi").
//!
//! gStore 1.4's HTTP server (`src/Main/grpc.cpp` — despite the name it is
//! HTTP + JSON, not protobuf) exposes every operation through one endpoint:
//! `GET|POST /api` (and `POST /`) with an `operation=` parameter, per-request
//! `username` / `password` credentials, and a JSON envelope answered with
//! HTTP 200 whatever the outcome. The upstream SDKs (`GstoreConnector.py` and
//! its C++/Java/Node.js/PHP siblings), the Workbench and the upstream test
//! scripts (`scripts/test/*.sh`) all speak this protocol. This module
//! implements it on the **same state** as the Rust-only REST endpoints of
//! [`crate::http_api::ApiServer`] (loaded databases, users, logs, tasks,
//! transactions), which keep working unchanged.
//!
//! # Routing
//!
//! [`wants`] claims, for this layer:
//!
//! * `GET|POST /api` — the upstream endpoint (also `/grpc/api` and
//!   `/ghttp/api`, the paths older upstream docs use);
//! * `POST /`;
//! * `GET /?operation=…` — only with an `operation` parameter, so a plain
//!   `GET /` still serves the web console;
//! * the path routes operation packages register ([`Registry::add_path`]):
//!   upstream's `/shutdown` and `/restart` (also under `/grpc/`; see
//!   `ops_lifecycle`), `/file/upload` and `/file/download[/*]` (also under
//!   `/grpc/`; see `ops_file`).
//!
//! # Request flow
//!
//! [`serve`] mirrors upstream `checkRequest` (`grpc.cpp` ~188-330):
//!
//! 1. the IP access check ([`auth::check_ip`]) — blocked → `1101`;
//! 2. parse the parameters ([`params::Params`]: URL query + JSON / form /
//!    multipart body) — malformed → `1003`;
//! 3. look up `operation` in the [`Registry`] — missing or unknown → `1100`;
//! 4. unless the operation is public (`check`): authenticate the per-request
//!    `username` / `password` (`encryption=1` = the password's MD5 hex) —
//!    missing → `1003 "username or password is empty"`, wrong → `1001`;
//! 5. validate `db_name` when the operation needs one — `1003`;
//! 6. the privilege check ([`auth::check_privilege`]) — denied →
//!    `1004 "You have no <operation> privilege"` (upstream's code for it);
//! 7. run the operation's handler — or, for `async=true` on an operation
//!    registered [`async_capable`](OpSpec::async_capable), hand it to the
//!    background task manager ([`tasks`]) and answer with its `opt_id`;
//! 8. record the answered operation in the access log ([`apilog`]).
//!
//! The IP checks are those of [`ipfilter`]: the allow / deny lists at step 1,
//! the failed-login lockout (every operation but `login` / `testConnect`)
//! before step 4.
//!
//! Handlers run with **no lock held**; they take the server state lock only
//! briefly (see the locking rule on `ApiState` in [`crate::http_api`]).
//!
//! # Adding operations
//!
//! Each area lives in its own `ops_<area>.rs` with a `register(&mut Registry)`
//! function, called from one line in [`registry`]. A handler is a plain
//! `fn(&mut Ctx, &Params) -> GResp`; its [`OpSpec`] says whether it needs
//! credentials, a `db_name`, and whether it can run asynchronously.

pub mod apilog;
pub mod auth;
pub mod envelope;
pub mod ipfilter;
mod ops_db;
mod ops_file;
mod ops_lifecycle;
mod ops_query;
mod ops_sys;
mod ops_task;
mod ops_user;
pub mod params;
pub mod tasks;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{MutexGuard, OnceLock};
use std::time::Instant;

pub use envelope::{GBody, GResp, Status};
pub use params::{Params, UploadedFile};

use crate::http_api::{ApiInner, ApiState};
use crate::server::{locked, Request};

/// Upstream's default `max_output_size` (`src/Util/Util.cpp`: `100000`).
pub const DEFAULT_MAX_OUTPUT_SIZE: usize = 100_000;

/// The gStore version this layer reports (`CoreVersion`) by default: the
/// upstream release whose protocol it implements.
pub const UPSTREAM_CORE_VERSION: &str = "1.4";

/// Settings of the `/api` layer. Install with
/// [`ApiServer::with_gapi_config`](crate::http_api::ApiServer::with_gapi_config)
/// (or change one field, e.g.
/// [`with_max_output_size`](crate::http_api::ApiServer::with_max_output_size)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapiConfig {
    /// The most result rows a `query` returns in its response (upstream's
    /// `max_output_size`, default [`DEFAULT_MAX_OUTPUT_SIZE`]). A larger
    /// result is truncated and its `OutputLimit` says so; `AnsNum` still
    /// counts every answer. `format=file` outputs are not capped (upstream).
    pub max_output_size: usize,
    /// `CoreVersion` in `login` / `testConnect` / `getCoreVersion`.
    pub core_version: String,
    /// `licensetype` in the same responses.
    pub license_type: String,
    /// `type` in the same responses (upstream reports `"http"`).
    pub http_type: String,
    /// Honour upstream's *inner* requests: `inner=true` from `127.0.0.1`
    /// skips the password check (upstream's own tools use it). **Off by
    /// default** — it lets any local process act as any user.
    pub trust_inner_localhost: bool,
    /// Where `format=file` / `n-triple-file` query results are written
    /// (upstream's `export/`); `None` = `<root>/export`.
    pub export_dir: Option<PathBuf>,
}

impl Default for GapiConfig {
    fn default() -> GapiConfig {
        GapiConfig {
            max_output_size: DEFAULT_MAX_OUTPUT_SIZE,
            core_version: UPSTREAM_CORE_VERSION.to_string(),
            license_type: "opensource".to_string(),
            http_type: "http".to_string(),
            trust_inner_localhost: false,
            export_dir: None,
        }
    }
}

/// Everything an operation handler may use: the shared server state, the
/// request, the caller.
pub struct Ctx<'a> {
    pub(crate) inner: &'a ApiInner,
    pub(crate) req: &'a Request,
    /// The client's IP address (`"unknown"` when the transport has none).
    pub remote_ip: String,
    /// The operation being served (for a path route: its registered name).
    pub op: String,
    /// The authenticated user (`None` before authentication, or for a public
    /// operation).
    pub user: Option<String>,
    /// The validated `db_name` of an operation that needs one.
    pub db_name: Option<String>,
    /// Whether the caller was let in as an upstream *inner* request (see
    /// [`GapiConfig::trust_inner_localhost`]).
    pub inner_request: bool,
    /// A snapshot of the layer's settings, taken when the request arrived.
    pub config: GapiConfig,
    /// When the request arrived.
    pub started: Instant,
}

impl Ctx<'_> {
    /// The authenticated user's name (empty for a public operation).
    pub fn user(&self) -> &str {
        self.user.as_deref().unwrap_or_default()
    }

    /// Whether the caller is the built-in `root` user.
    pub fn is_root(&self) -> bool {
        self.user.as_deref() == Some(crate::http_users::ROOT_USER)
    }

    /// The server's root directory (databases, logs, users).
    pub fn root_dir(&self) -> &Path {
        &self.inner.root
    }

    /// The directory `format=file` query results go to (created on demand).
    pub fn export_dir(&self) -> PathBuf {
        self.config
            .export_dir
            .clone()
            .unwrap_or_else(|| self.inner.root.join("export"))
    }

    /// The server state, locked. Hold it briefly and never wait for a
    /// database lock while holding it (the locking rule on `ApiState`).
    pub(crate) fn state(&self) -> MutexGuard<'_, ApiState> {
        locked(&self.inner.state)
    }

    /// A request header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.req.header(name)
    }

    /// The request method (`GET`, `POST`, …).
    pub fn method(&self) -> &str {
        &self.req.method
    }

    /// The request path (without the query string).
    pub fn path(&self) -> &str {
        &self.req.path
    }
}

/// An operation handler.
pub type OpHandler = fn(&mut Ctx<'_>, &Params) -> GResp;

/// One registered operation.
#[derive(Clone, Copy)]
pub struct OpSpec {
    /// The `operation=` name (case-sensitive, as upstream).
    pub name: &'static str,
    pub handler: OpHandler,
    /// Whether the request must carry valid `username` / `password` (false
    /// only for public operations such as `check`).
    pub auth: bool,
    /// The privilege name handed to [`auth::check_privilege`] — the
    /// operation's own name by default, which [`auth::requirement`] maps the
    /// way upstream does; set it to check an operation as another one.
    pub privilege: &'static str,
    /// Whether a valid `db_name` is required (validated before the handler
    /// runs, available as [`Ctx::db_name`]).
    pub needs_db: bool,
    /// Whether the operation honours `async=true` (upstream's `jsonBoolParam`:
    /// `true` / `"true"` / `"1"`). The dispatcher then runs the handler on
    /// the task manager's runner threads (`tasks::run_async`) and answers
    /// with an `opt_id` at once; the handler itself is written synchronously
    /// (it sees `async=false`) and may reach its task through
    /// [`tasks::current`] (cancellation flag, progress).
    pub async_capable: bool,
}

impl OpSpec {
    /// An authenticated operation without a required database.
    pub fn new(name: &'static str, handler: OpHandler) -> OpSpec {
        OpSpec {
            name,
            handler,
            auth: true,
            privilege: name,
            needs_db: false,
            async_capable: false,
        }
    }

    /// No credentials required (and no privilege check).
    pub fn public(mut self) -> OpSpec {
        self.auth = false;
        self
    }

    /// Check the caller's privilege as for operation `name` instead of this
    /// operation's own name.
    pub fn privilege(mut self, name: &'static str) -> OpSpec {
        self.privilege = name;
        self
    }

    /// A valid `db_name` is required.
    pub fn needs_db(mut self) -> OpSpec {
        self.needs_db = true;
        self
    }

    /// The operation supports `async=1`.
    pub fn async_capable(mut self) -> OpSpec {
        self.async_capable = true;
        self
    }
}

/// A route on its own HTTP path (upstream's `/shutdown`, `/restart`,
/// `/file/upload`, `/file/download/*`). Its handler does its own
/// authentication, as upstream's differ — through [`auth::authenticate`], or
/// [`auth::verify`] for credentials gathered another way (the lifecycle
/// routes also read headers and accept the `system` user).
#[derive(Clone, Copy)]
pub struct PathRoute {
    /// A name for logs and [`Ctx::op`].
    pub name: &'static str,
    /// The path (exact, or a prefix when `prefix`).
    pub path: &'static str,
    pub prefix: bool,
    /// Accepted methods (`&["GET", "POST"]`, …).
    pub methods: &'static [&'static str],
    pub handler: OpHandler,
}

impl PathRoute {
    fn matches(&self, method: &str, path: &str) -> bool {
        let path_ok = if self.prefix {
            path.starts_with(self.path)
        } else {
            path == self.path
        };
        path_ok && self.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// The operation registry.
#[derive(Default)]
pub struct Registry {
    ops: HashMap<&'static str, OpSpec>,
    paths: Vec<PathRoute>,
}

impl Registry {
    /// Register an operation. Panics on a duplicate name (a merge mistake).
    pub fn add(&mut self, spec: OpSpec) {
        let prev = self.ops.insert(spec.name, spec);
        assert!(
            prev.is_none(),
            "gapi operation '{}' registered twice",
            spec.name
        );
    }

    /// Register a path route (checked in registration order).
    pub fn add_path(&mut self, route: PathRoute) {
        self.paths.push(route);
    }

    /// The operation named `name`.
    pub fn get(&self, name: &str) -> Option<&OpSpec> {
        self.ops.get(name)
    }

    /// The registered operation names, sorted.
    pub fn op_names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.ops.keys().copied().collect();
        v.sort_unstable();
        v
    }

    fn path_route(&self, method: &str, path: &str) -> Option<&PathRoute> {
        self.paths.iter().find(|r| r.matches(method, path))
    }
}

/// The process-wide registry, built on first use.
pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut reg = Registry::default();
        ops_sys::register(&mut reg);
        ops_query::register(&mut reg);
        ops_lifecycle::register(&mut reg);
        ops_user::register(&mut reg);
        ops_task::register(&mut reg);
        ops_db::register(&mut reg);
        ops_file::register(&mut reg);
        reg
    })
}

/// Whether `req` belongs to this layer (see the module docs' *Routing*).
pub(crate) fn wants(req: &Request) -> bool {
    let method = req.method.as_str();
    let get_or_post = matches!(method, "GET" | "POST");
    match req.path.as_str() {
        "/api" | "/grpc/api" | "/ghttp/api" => get_or_post,
        "/" => {
            method == "POST"
                || (method == "GET"
                    && req.query.as_deref().is_some_and(|q| {
                        params::parse_form(q).iter().any(|(k, _)| k == "operation")
                    }))
        }
        path => registry().path_route(method, path).is_some(),
    }
}

/// Serve one `/api` request (no lock held). Returns the response and the
/// caller's display name for the access log.
pub(crate) fn serve(inner: &ApiInner, req: &Request, peer: Option<SocketAddr>) -> (GResp, String) {
    let config = {
        let mut st = locked(&inner.state);
        st.requests += 1;
        st.gapi.clone()
    };
    let mut ctx = Ctx {
        inner,
        req,
        remote_ip: peer.map_or_else(|| "unknown".to_string(), |a| a.ip().to_string()),
        op: String::new(),
        user: None,
        db_name: None,
        inner_request: false,
        config,
        started: Instant::now(),
    };
    let resp = dispatch(&mut ctx);
    let user = ctx.user.clone().unwrap_or_else(|| "-".to_string());
    (resp, user)
}

fn dispatch(ctx: &mut Ctx<'_>) -> GResp {
    if let Err(r) = auth::check_ip(ctx, auth::IpStage::Connect) {
        return r;
    }
    let params = match Params::from_request(ctx.req) {
        Ok(p) => p,
        Err(e) => return GResp::error(Status::ParamIsIllegal, e),
    };
    let resp = route(ctx, &params);
    apilog::log_access(ctx, &params, &resp);
    resp
}

/// Steps 3-7 of the request flow (see the module docs).
fn route(ctx: &mut Ctx<'_>, params: &Params) -> GResp {
    let reg = registry();
    if !matches!(
        ctx.req.path.as_str(),
        "/api" | "/grpc/api" | "/ghttp/api" | "/"
    ) {
        if let Some(route) = reg.path_route(&ctx.req.method, &ctx.req.path) {
            ctx.op = route.name.to_string();
            return (route.handler)(ctx, params);
        }
    }
    let Some(op) = params.non_empty("operation") else {
        return GResp::error(Status::OperationUndefined, "");
    };
    ctx.op = op.clone();
    let Some(spec) = reg.get(&op) else {
        return GResp::error(Status::OperationUndefined, "");
    };
    if !spec.auth {
        return (spec.handler)(ctx, params);
    }
    // Upstream exempts login / testConnect from the second (operation-level)
    // IP check.
    if !matches!(op.as_str(), "login" | "testConnect") {
        if let Err(r) = auth::check_ip(ctx, auth::IpStage::Operation) {
            return r;
        }
    }
    if let Err(r) = auth::authenticate(ctx, params) {
        return r;
    }
    let db_name = params.str("db_name");
    if spec.needs_db {
        let name = db_name.clone().unwrap_or_default();
        if let Err(msg) = check_db_name(&name) {
            return GResp::error(Status::ParamIsIllegal, msg);
        }
        ctx.db_name = Some(name);
    }
    if !(ctx.inner_request && ctx.is_root())
        && !auth::check_privilege(ctx, spec.privilege, db_name.as_deref())
    {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            format!("You have no {op} privilege"),
        );
    }
    if spec.async_capable && params.bool("async", false) {
        return tasks::run_async(ctx, spec, params);
    }
    (spec.handler)(ctx, params)
}

/// The name of upstream's system database, which no operation may target.
pub const SYSTEM_DB: &str = "system";

/// Validate a `db_name` like upstream's `check_param_value("db_name", …)`
/// (non-empty, not the system database, at least 3 characters, no `.db`
/// suffix), plus this server's rule that it be a safe single path component
/// (`[A-Za-z0-9_-]`, at most 64 characters). The error is the message for a
/// `1003` answer.
pub fn check_db_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the value of db_name can not be empty!".to_string());
    }
    if name == SYSTEM_DB {
        return Err("you can not operate the system database".to_string());
    }
    if name.len() <= 2 {
        return Err("the database name length at least need 3".to_string());
    }
    if name.ends_with(crate::DB_SUFFIX) {
        return Err(format!(
            "Your db name to be built should not end with \"{}\".",
            crate::DB_SUFFIX
        ));
    }
    if !crate::http_api::valid_db_name(name) {
        return Err(
            "the database name may only contain the characters A-Z a-z 0-9 _ - (at most 64)"
                .to_string(),
        );
    }
    Ok(())
}

/// A required, non-empty string parameter, or the upstream `1003` answer
/// `"the value of <name> can not be empty!"`.
pub fn required(params: &Params, name: &str) -> Result<String, GResp> {
    params.non_empty(name).ok_or_else(|| {
        GResp::error(
            Status::ParamIsIllegal,
            format!("the value of {name} can not be empty!"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_names_follow_upstream_rules() {
        assert!(check_db_name("lubm").is_ok());
        assert!(check_db_name("my_db-1").is_ok());
        for bad in [
            "",
            "system",
            "ab",
            "x.db",
            "../etc",
            "a b c",
            &"x".repeat(65),
        ] {
            assert!(check_db_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            check_db_name("").unwrap_err(),
            "the value of db_name can not be empty!"
        );
    }

    #[test]
    fn the_registry_holds_the_core_operations() {
        let names = registry().op_names();
        for op in [
            "check",
            "login",
            "testConnect",
            "getCoreVersion",
            "stat",
            "query",
            "begin",
            "tquery",
            "commit",
            "rollback",
            "checkpoint",
            "show",
            "load",
            "loadcsr",
            "unload",
            "monitor",
            "build",
            "drop",
            "rename",
            "backup",
            "backuppath",
            "restore",
            "export",
            "batchInsert",
            "batchRemove",
            "schema",
        ] {
            assert!(names.contains(&op), "{op} missing from {names:?}");
        }
        assert!(!registry().get("check").unwrap().auth);
        assert!(registry().get("query").unwrap().needs_db);
    }

    #[test]
    fn routing_claims_only_upstream_paths() {
        let req = |method: &str, path: &str, query: Option<&str>| Request {
            method: method.into(),
            path: path.into(),
            query: query.map(Into::into),
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert!(wants(&req("GET", "/api", None)));
        assert!(wants(&req("POST", "/api", None)));
        assert!(wants(&req("POST", "/grpc/api", None)));
        assert!(wants(&req("POST", "/", None)));
        assert!(wants(&req("GET", "/", Some("operation=check"))));
        assert!(!wants(&req("GET", "/", None)), "plain GET / is the console");
        assert!(!wants(&req("GET", "/", Some("db=x"))));
        assert!(!wants(&req("PUT", "/api", None)));
        assert!(!wants(&req("POST", "/sparql", None)));
        assert!(!wants(&req("GET", "/console", None)));
        // The lifecycle path routes (see `ops_lifecycle`).
        for path in ["/shutdown", "/grpc/shutdown", "/restart", "/grpc/restart"] {
            assert!(wants(&req("GET", path, None)), "{path}");
            assert!(wants(&req("POST", path, None)), "{path}");
            assert!(!wants(&req("DELETE", path, None)), "{path}");
        }
        // The file routes (see `ops_file`).
        for prefix in ["", "/grpc"] {
            let at =
                |method: &str, path: &str| wants(&req(method, &format!("{prefix}{path}"), None));
            assert!(at("POST", "/file/upload"));
            assert!(!at("GET", "/file/upload"));
            assert!(at("POST", "/file/download"));
            assert!(at("GET", "/file/download"));
            assert!(at("GET", "/file/download/export/x.nt"));
            assert!(!at("POST", "/file/download/x.nt"));
        }
        assert!(!wants(&req("GET", "/file/other", None)));
    }
}
