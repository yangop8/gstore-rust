//! A richer HTTP API endpoint bringing the [`crate::server`] SPARQL endpoint up
//! to parity with gStore's C++ `ghttp` / `ApiProvider`.
//!
//! Where [`crate::server::Server`] is a single-database SPARQL endpoint, this
//! [`ApiServer`] manages a *directory of named databases* and layers on the
//! operational surface gStore exposes over HTTP:
//!
//! * **User management & RBAC** ([`crate::http_users`]) — a persistent user
//!   store, `/login` returning a session token, and privilege-gated endpoints.
//! * **Transactions over HTTP** — `/begin`, `/tquery`, `/commit`, `/rollback`,
//!   `/checkpoint`, mapping to [`Database::begin`]/[`commit`](Database::commit)/
//!   [`rollback`](Database::rollback).
//! * **Database lifecycle** — `/build`, `/load`, `/unload`, `/drop`, `/show`.
//! * **Backup / restore / export** — `/backup`, `/restore`, `/export`.
//! * **Batch operations** — `/batchInsert`, `/batchRemove`.
//! * **Logging & monitoring** — query / access / transaction logs and a live
//!   `/monitor` (and `/status`) JSON snapshot.
//!
//! It reuses the dependency-free HTTP primitives (request parsing, response
//! writing, content negotiation, Basic-auth parsing) from [`crate::server`], so
//! it remains std-only and the existing `/sparql`, `/update`, `/status`
//! behaviour of [`crate::server::Server`] is untouched.
//!
//! **Auth.** Every endpoint except `/login` requires a caller identity, supplied
//! either as HTTP Basic credentials (validated against the user store) or as a
//! session token from `/login` (carried via `Authorization: Bearer …`, an
//! `X-Session-Token` header, or a `session=`/`token=` parameter). Lacking a
//! valid identity is `401`; lacking the required privilege is `403`.
//!
//! As with [`crate::server`], real TLS is out of scope — terminate HTTPS in a
//! reverse proxy and do not send credentials over plain `http://` in production.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::db::Database;
use crate::http_users::{Privilege, UserStore, ROOT_USER};
use crate::model::Triple;
use crate::query::QueryResult;
use crate::server::{
    err_json, ntriples_body, parse_basic_auth, write_response_ext, Request,
};
use crate::sparql_results::{self, json_str, ResultFormat};

const JSON: &str = "application/json";

/// A minimal, self-contained (inline CSS/JS, no external assets) single-page
/// web console served at `GET /` and `GET /console`. It gives an operator a
/// textarea to run SPARQL against a loaded database via `/sparql` and shows
/// the raw JSON response, plus a short reference of the available endpoints.
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
  .row { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; margin-bottom: 0.5rem; }
  input[type=text] { padding: 0.3rem; font-size: 0.9rem; }
  code { background: #eee; padding: 0 0.25rem; }
  .endpoints { font-size: 0.85rem; line-height: 1.6; }
</style>
</head>
<body>
<h1>gStore web console</h1>
<div class="row">
  <label>db_name: <input type="text" id="db" placeholder="(auto if only one db loaded)"></label>
</div>
<p>SPARQL query:</p>
<textarea id="q">SELECT * WHERE { ?s ?p ?o } LIMIT 20</textarea>
<div class="row">
  <button id="run">Run</button>
  <span id="runStatus"></span>
</div>
<pre id="out">(results appear here)</pre>
<h2>Endpoints</h2>
<p class="endpoints">
  Query/update: <code>GET|POST /sparql</code>, <code>POST /update</code>,
  <code>POST /begin</code>, <code>POST /tquery</code>, <code>POST /commit</code>,
  <code>POST /rollback</code>, <code>POST /checkpoint</code><br>
  Database lifecycle: <code>POST /build</code>, <code>POST /load</code>,
  <code>POST /upload</code>, <code>POST /unload</code>, <code>POST /drop</code>,
  <code>GET /show</code><br>
  Backup/export: <code>POST /backup</code>, <code>POST /restore</code>,
  <code>GET /export[?download=1]</code><br>
  Batch: <code>POST /batchInsert</code>, <code>POST /batchRemove</code><br>
  Monitoring: <code>GET /status</code>, <code>GET /monitor</code>,
  <code>GET /querylog</code>, <code>GET /accesslog</code>, <code>GET /txnlog</code><br>
  Tasks: <code>GET /tasklist</code>, <code>POST /cancel</code><br>
  Users: <code>POST /login</code>, <code>POST /logout</code>,
  <code>POST /user/create</code>, <code>POST /user/drop</code>,
  <code>POST /user/password</code>, <code>POST /user/grant</code>,
  <code>POST /user/revoke</code>, <code>GET /user/show</code><br>
  Every endpoint but <code>/login</code>, <code>GET /</code>, and
  <code>GET /console</code> requires HTTP Basic credentials or an
  <code>Authorization: Bearer &lt;session&gt;</code> token from
  <code>/login</code>.
</p>
<script>
document.getElementById('run').addEventListener('click', function () {
  var q = document.getElementById('q').value;
  var db = document.getElementById('db').value;
  var out = document.getElementById('out');
  var runStatus = document.getElementById('runStatus');
  runStatus.textContent = 'running...';
  var body = 'sparql=' + encodeURIComponent(q);
  if (db) { body += '&db_name=' + encodeURIComponent(db); }
  fetch('/sparql', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body
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

/// A built HTTP response (status, content type, body, extra headers).
struct Resp {
    status: u16,
    ctype: String,
    body: Vec<u8>,
    extra: Vec<(String, String)>,
}

impl Resp {
    fn new(status: u16, ctype: &str, body: Vec<u8>) -> Resp {
        Resp {
            status,
            ctype: ctype.to_string(),
            body,
            extra: Vec::new(),
        }
    }

    /// A `{"StatusCode":0,"StatusMsg":…}` success envelope (gStore-style).
    fn ok(msg: &str) -> Resp {
        Resp::new(200, JSON, status_json(0, msg))
    }

    /// A `{"StatusCode":code,"StatusMsg":…}` error envelope at HTTP `status`.
    fn fail(status: u16, msg: &str) -> Resp {
        Resp::new(status, JSON, status_json(status as i64, msg))
    }
}

/// Build a `{"StatusCode":code,"StatusMsg":"msg"}` JSON body.
fn status_json(code: i64, msg: &str) -> Vec<u8> {
    format!("{{\"StatusCode\":{code},\"StatusMsg\":{}}}", json_str(msg)).into_bytes()
}

/// One open HTTP transaction: which database it operates on.
struct Txn {
    db: String,
}

/// One in-flight query/update task, tracked so `/tasklist` can list it and
/// `/cancel` can flag it for cancellation.
///
/// INTEGRATION: `cancel` is a cooperative-cancellation flag. Nothing in the
/// query engine consults it yet — see the `INTEGRATION` comment at the
/// `db.query(...)` call site in [`ApiServer::run_query`] for where a future
/// `Database::query_cancellable` hook should poll it.
struct TaskEntry {
    db: String,
    /// A short, log-safe snippet of the query text (newlines flattened,
    /// truncated) — not necessarily the full statement.
    query: String,
    user: String,
    started: Instant,
    cancel: Arc<AtomicBool>,
}

/// Mutable server state guarded by a single mutex (correctness over throughput,
/// matching [`crate::server::Server`]'s single `Mutex<Database>`).
struct ApiState {
    root: PathBuf,
    users: UserStore,
    /// Loaded, in-memory databases by name.
    dbs: HashMap<String, Database>,
    /// Open transactions by id.
    txns: HashMap<u64, Txn>,
    /// The single open transaction per database (single-writer), if any.
    db_txn: HashMap<String, u64>,
    /// In-flight query/update tasks by id (see [`TaskEntry`]).
    tasks: HashMap<u64, TaskEntry>,
    started: Instant,
    requests: u64,
}

/// The parity HTTP API server. Construct with [`ApiServer::bind`], then drive it
/// with [`serve_forever`](ApiServer::serve_forever) (e.g. in a background
/// thread).
pub struct ApiServer {
    state: Mutex<ApiState>,
    listener: TcpListener,
}

impl ApiServer {
    /// Bind to `addr`, rooting all databases and logs under `root_dir`. The user
    /// store is loaded from `root_dir/users.db` (created if absent) with a
    /// built-in `root` user seeded at password `root_password` the first time.
    pub fn bind<A: ToSocketAddrs>(
        root_dir: impl AsRef<Path>,
        root_password: &str,
        addr: A,
    ) -> std::io::Result<ApiServer> {
        let root = root_dir.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let users = UserStore::open(root.join("users.db"), root_password)?;
        let listener = TcpListener::bind(addr)?;
        Ok(ApiServer {
            state: Mutex::new(ApiState {
                root,
                users,
                dbs: HashMap::new(),
                txns: HashMap::new(),
                db_txn: HashMap::new(),
                tasks: HashMap::new(),
                started: Instant::now(),
                requests: 0,
            }),
            listener,
        })
    }

    /// The actual bound address (useful after binding to port 0).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// INTEGRATION: the live cancel flag for an in-flight task, if it still
    /// exists. A future cooperative-cancellation hook in the query engine
    /// (e.g. `Database::query_cancellable(&mut self, sparql: &str, cancel:
    /// &AtomicBool) -> Result<QueryResult>`, polling `cancel.load(Ordering::
    /// Relaxed)` between operator steps and aborting early with a dedicated
    /// error) can fetch the flag for a task id — reported by `/tasklist` and
    /// set by `POST /cancel` — via this accessor and pass it through to the
    /// engine. `run_query` does not yet call any such hook: today `/cancel`
    /// only sets the flag, nothing reads it.
    pub fn task_cancel_flag(&self, task_id: u64) -> Option<Arc<AtomicBool>> {
        self.state
            .lock()
            .unwrap()
            .tasks
            .get(&task_id)
            .map(|t| Arc::clone(&t.cancel))
    }

    /// Accept and handle connections forever (one request each).
    pub fn serve_forever(&self) {
        for stream in self.listener.incoming().flatten() {
            let _ = self.handle(stream);
        }
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let req = match Request::parse(&mut stream) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(_) => {
                return write_response_ext(&mut stream, 400, JSON, &[], &err_json("bad request"))
            }
        };

        let resp = {
            let mut st = self.state.lock().unwrap();
            st.requests += 1;
            let user = self.resolve_user(&mut st, &req);
            let user_disp = user.clone().unwrap_or_else(|| "-".to_string());
            let resp = self.dispatch(&mut st, &req, user);
            // Access log (inside the lock so the log order matches request order).
            append_log(
                &st.root,
                "access.log",
                &format!(
                    "{ts}\t{method}\t{path}\t{user}\t{status}",
                    ts = now_millis(),
                    method = req.method,
                    path = req.path,
                    user = user_disp,
                    status = resp.status,
                ),
            );
            resp
        };

        let extra: Vec<(&str, &str)> = resp
            .extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        write_response_ext(&mut stream, resp.status, &resp.ctype, &extra, &resp.body)
    }

    /// Resolve the caller's identity from Basic auth or a session token.
    fn resolve_user(&self, st: &mut ApiState, req: &Request) -> Option<String> {
        if let Some((u, p)) = parse_basic_auth(req) {
            if st.users.verify(&u, &p) {
                return Some(u);
            }
        }
        if let Some(tok) = req.bearer_or_session_token() {
            if let Some(u) = st.users.session_user(&tok) {
                return Some(u);
            }
        }
        None
    }

    fn unauthorized(&self) -> Resp {
        let mut r = Resp::fail(401, "authentication required");
        r.extra.push((
            "WWW-Authenticate".to_string(),
            "Basic realm=\"gStore\"".to_string(),
        ));
        r
    }

    fn forbidden(&self) -> Resp {
        Resp::fail(403, "permission denied: insufficient privilege")
    }

    /// Run `f` only if `user` holds privilege `p`, else `403`.
    fn guarded(
        &self,
        st: &mut ApiState,
        user: &str,
        p: Privilege,
        f: impl FnOnce(&Self, &mut ApiState) -> Resp,
    ) -> Resp {
        if st.users.has_priv(user, p) {
            f(self, st)
        } else {
            self.forbidden()
        }
    }

    fn dispatch(&self, st: &mut ApiState, req: &Request, user: Option<String>) -> Resp {
        let method = req.method.as_str();
        let path = req.path.as_str();

        // `/login`, and the static web console, are the only fully public
        // endpoints — the console itself only ever *reads* by calling back
        // into the (still auth-gated) `/sparql` from the browser.
        if path == "/login" {
            return self.handle_login(st, req);
        }
        if path == "/logout" {
            if let Some(tok) = req.bearer_or_session_token() {
                st.users.logout(&tok);
            }
            return Resp::ok("logged out");
        }
        if method == "GET" && (path == "/" || path == "/console") {
            return Resp::new(200, "text/html; charset=utf-8", CONSOLE_HTML.as_bytes().to_vec());
        }

        let user = match user {
            Some(u) => u,
            None => return self.unauthorized(),
        };

        match (method, path) {
            ("GET" | "POST", "/sparql") => self.handle_query(st, req, &user),
            ("POST", "/update") => {
                self.guarded(st, &user, Privilege::Update, |s, st| {
                    s.handle_update(st, req, &user)
                })
            }
            ("POST", "/build") => self.guarded(st, &user, Privilege::Load, |s, st| {
                s.handle_build(st, req)
            }),
            ("POST", "/load") => self.guarded(st, &user, Privilege::Load, |s, st| {
                s.handle_load(st, req)
            }),
            ("POST", "/upload") => self.guarded(st, &user, Privilege::Load, |s, st| {
                s.handle_upload(st, req)
            }),
            ("POST", "/unload") => self.guarded(st, &user, Privilege::Unload, |s, st| {
                s.handle_unload(st, req)
            }),
            ("POST", "/drop") => self.guarded(st, &user, Privilege::Unload, |s, st| {
                s.handle_drop(st, req)
            }),
            ("GET" | "POST", "/show") => self.handle_show(st),
            ("POST", "/begin") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_begin(st, req, &user)
            }),
            ("POST", "/tquery") => self.handle_tquery(st, req, &user),
            ("POST", "/commit") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_txn_end(st, req, &user, true)
            }),
            ("POST", "/rollback") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_txn_end(st, req, &user, false)
            }),
            ("POST", "/checkpoint") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_checkpoint(st, req)
            }),
            ("POST", "/backup") => self.guarded(st, &user, Privilege::Backup, |s, st| {
                s.handle_backup(st, req)
            }),
            ("POST", "/restore") => self.guarded(st, &user, Privilege::Restore, |s, st| {
                s.handle_restore(st, req)
            }),
            ("GET" | "POST", "/export") => self.guarded(st, &user, Privilege::Export, |s, st| {
                s.handle_export(st, req)
            }),
            ("POST", "/batchInsert") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_batch(st, req, &user, true)
            }),
            ("POST", "/batchRemove") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_batch(st, req, &user, false)
            }),
            ("GET", "/status") => self.handle_status(st),
            ("GET", "/monitor") => self.handle_monitor(st),
            ("POST", "/user/create") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.handle_user_create(st, req)
            }),
            ("POST", "/user/drop") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.handle_user_drop(st, req)
            }),
            ("POST", "/user/password") => self.handle_user_password(st, req, &user),
            ("POST", "/user/grant") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.handle_user_grant(st, req, true)
            }),
            ("POST", "/user/revoke") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.handle_user_grant(st, req, false)
            }),
            ("GET", "/user/show") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.handle_user_show(st)
            }),
            ("GET", "/querylog") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.read_log(st, "query.log")
            }),
            ("GET", "/accesslog") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.read_log(st, "access.log")
            }),
            ("GET", "/txnlog") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.read_log(st, "txn.log")
            }),
            ("GET", "/tasklist") => self.guarded(st, &user, Privilege::Admin, |s, st| {
                s.handle_tasklist(st)
            }),
            ("POST", "/cancel") => {
                // Either privilege can cancel: Update (you can cancel your own
                // long-running work) or Admin (an operator cancelling anyone's).
                if st.users.has_priv(&user, Privilege::Admin)
                    || st.users.has_priv(&user, Privilege::Update)
                {
                    self.handle_cancel(st, req)
                } else {
                    self.forbidden()
                }
            }
            _ => Resp::fail(404, "not found"),
        }
    }

    // ---- auth -------------------------------------------------------------

    fn handle_login(&self, st: &mut ApiState, req: &Request) -> Resp {
        let (Some(username), Some(password)) = (req.param("username"), req.param("password")) else {
            return Resp::fail(400, "missing username or password");
        };
        match st.users.login(&username, &password) {
            Some(token) => {
                let priv_str = st
                    .users
                    .get(&username)
                    .map(|u| u.privilege_str())
                    .unwrap_or_default();
                let body = format!(
                    "{{\"StatusCode\":0,\"StatusMsg\":\"login success\",\"session\":{},\"username\":{},\"privilege\":{}}}",
                    json_str(&token),
                    json_str(&username),
                    json_str(&priv_str),
                );
                Resp::new(200, JSON, body.into_bytes())
            }
            None => Resp::fail(401, "invalid username or password"),
        }
    }

    // ---- query / update ---------------------------------------------------

    fn handle_query(&self, st: &mut ApiState, req: &Request, user: &str) -> Resp {
        let Some(sparql) = req.param("sparql").or_else(|| req.query_string()) else {
            return Resp::fail(400, "missing sparql/query parameter");
        };
        // A read endpoint: an UPDATE-shaped statement additionally needs Update.
        let need = if is_update_sparql(&sparql) {
            Privilege::Update
        } else {
            Privilege::Query
        };
        if !st.users.has_priv(user, need) {
            return self.forbidden();
        }
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        self.run_query(st, &name, &sparql, req, user)
    }

    fn handle_update(&self, st: &mut ApiState, req: &Request, user: &str) -> Resp {
        let Some(sparql) = req
            .param("sparql")
            .or_else(|| req.param("update"))
            .or_else(|| Some(req.body_str()).filter(|s| !s.trim().is_empty()))
        else {
            return Resp::fail(400, "missing update statement");
        };
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        self.run_query(st, &name, &sparql, req, user)
    }

    /// Run a SPARQL request against loaded database `name`, content-negotiating
    /// the result, and append it to the query log.
    fn run_query(
        &self,
        st: &mut ApiState,
        name: &str,
        sparql: &str,
        req: &Request,
        user: &str,
    ) -> Resp {
        append_log(
            &st.root,
            "query.log",
            &format!("{}\t{}\t{}\t{}", now_millis(), name, user, sparql.replace('\n', " ")),
        );
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let task_id = self.register_task(st, name, sparql, user);
        let Some(db) = st.dbs.get_mut(name) else {
            self.unregister_task(st, task_id);
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        // INTEGRATION: this runs the query to completion without consulting
        // the cancel flag registered above (retrievable via
        // `ApiServer::task_cancel_flag(task_id)`, and settable by `POST
        // /cancel`). When the query engine grows a cooperative-cancellation
        // hook — e.g. `Database::query_cancellable(&mut self, sparql: &str,
        // cancel: &AtomicBool) -> Result<QueryResult>` that polls
        // `cancel.load(Ordering::Relaxed)` between VS-tree/join steps and
        // aborts early — call it here instead of `db.query`, passing the
        // flag through. Until then `/cancel` only marks the flag; nothing
        // reads it.
        let result = db.query(sparql);
        self.unregister_task(st, task_id);
        match result {
            Ok(result) => render_result(fmt, &result),
            Err(e) => Resp::new(
                400,
                "application/sparql-results+json",
                err_json(&e.to_string()),
            ),
        }
    }

    // ---- task registry / cancellation --------------------------------------

    /// Register a new in-flight task for `db`/`sparql`/`user`, returning its
    /// id. Always pair with [`unregister_task`](Self::unregister_task) once
    /// the query completes (success or error).
    fn register_task(&self, st: &mut ApiState, db: &str, sparql: &str, user: &str) -> u64 {
        let mut id = new_id();
        while st.tasks.contains_key(&id) {
            id = new_id();
        }
        let snippet: String = sparql.replace(['\n', '\r', '\t'], " ").chars().take(200).collect();
        st.tasks.insert(
            id,
            TaskEntry {
                db: db.to_string(),
                query: snippet,
                user: user.to_string(),
                started: Instant::now(),
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        id
    }

    /// Remove a completed task from the registry (no-op if already gone, e.g.
    /// if it somehow raced a future timeout/cleanup).
    fn unregister_task(&self, st: &mut ApiState, id: u64) {
        st.tasks.remove(&id);
    }

    fn handle_tasklist(&self, st: &mut ApiState) -> Resp {
        let mut ids: Vec<u64> = st.tasks.keys().copied().collect();
        ids.sort_unstable();
        let mut items = Vec::new();
        for id in ids {
            let t = &st.tasks[&id];
            items.push(format!(
                "{{\"id\":{},\"db\":{},\"user\":{},\"query\":{},\"elapsed_ms\":{},\"cancelled\":{}}}",
                id,
                json_str(&t.db),
                json_str(&t.user),
                json_str(&t.query),
                t.started.elapsed().as_millis(),
                t.cancel.load(Ordering::Relaxed),
            ));
        }
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"tasks\":[{}]}}",
            items.join(",")
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    fn handle_cancel(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(id) = req
            .param("id")
            .or_else(|| req.param("task_id"))
            .or_else(|| req.param("tid"))
            .and_then(|s| s.trim().parse::<u64>().ok())
        else {
            return Resp::fail(400, "missing or malformed id/task_id parameter");
        };
        match st.tasks.get(&id) {
            Some(t) => {
                t.cancel.store(true, Ordering::Relaxed);
                Resp::ok(&format!("cancel requested for task {id}"))
            }
            None => Resp::fail(404, &format!("no active task {id}")),
        }
    }

    // ---- database lifecycle ----------------------------------------------

    fn handle_build(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "database is busy in a transaction");
        }
        // Build from a server-local file path, or from the request body (RDF).
        let built = if let Some(path) = req.param("db_path").or_else(|| req.param("path")) {
            Database::build_from_files(name.clone(), &[path])
        } else {
            Database::build_from_str(name.clone(), &req.body_str())
        };
        let db = match built {
            Ok(db) => db,
            Err(e) => return Resp::fail(400, &format!("build failed: {e}")),
        };
        let dir = st.root.join(crate::db_dir_for(&name));
        if let Err(e) = db.save(&dir) {
            return Resp::fail(500, &format!("save failed: {e}"));
        }
        let triples = db.triple_num();
        st.dbs.insert(name.clone(), db);
        Resp::ok(&format!("database '{name}' built ({triples} triples)"))
    }

    fn handle_load(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        let dir = st.root.join(crate::db_dir_for(&name));
        if !dir.is_dir() {
            return Resp::fail(404, &format!("database '{name}' not found on disk"));
        }
        match Database::load(&dir) {
            Ok(db) => {
                st.dbs.insert(name.clone(), db);
                Resp::ok(&format!("database '{name}' loaded"))
            }
            Err(e) => Resp::fail(500, &format!("load failed: {e}")),
        }
    }

    /// `POST /upload?db_name=<name>[&format=turtle|ntriples|nquads|rdfxml|trig]`
    /// with the raw RDF document as the request body (the format may also be
    /// inferred from `Content-Type`, defaulting to Turtle — a superset of
    /// N-Triples). Parses with the crate's existing bundled parsers (the same
    /// ones [`Database::build_from_files`] uses) and loads the triples into
    /// `db_name`: an already-loaded database is updated incrementally via
    /// [`Database::insert_triple`]; otherwise an on-disk copy is loaded first,
    /// or — if neither exists yet — a fresh empty database is created (mirrors
    /// gStore's `LoadFile`, which may target a not-yet-built db).
    fn handle_upload(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "database is busy in a transaction");
        }
        let format = upload_format(req);
        let content = req.body_str();
        let triples = match parse_rdf(&format, &content) {
            Ok(ts) => ts,
            Err(e) => return Resp::fail(400, &format!("upload parse failed ({format}): {e}")),
        };
        let parsed = triples.len();

        if !st.dbs.contains_key(&name) {
            let dir = st.root.join(crate::db_dir_for(&name));
            let opened = if dir.is_dir() {
                Database::load(&dir)
            } else {
                Database::build_from_str(name.clone(), "")
            };
            match opened {
                Ok(db) => {
                    st.dbs.insert(name.clone(), db);
                }
                Err(e) => return Resp::fail(500, &format!("upload failed to open database: {e}")),
            }
        }
        let inserted = {
            let db = st
                .dbs
                .get_mut(&name)
                .expect("just inserted or already loaded above");
            triples.iter().filter(|t| db.insert_triple(t)).count()
        };
        // (Lifecycle endpoints like /build and /load don't append to
        // query.log either; the access log — written unconditionally in
        // `handle` — already records that this request happened.)
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"upload complete\",\"db_name\":{},\"format\":{},\"triples_parsed\":{parsed},\"triples_inserted\":{inserted}}}",
            json_str(&name),
            json_str(&format),
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    fn handle_unload(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "database is busy in a transaction");
        }
        if st.dbs.remove(&name).is_none() {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        }
        Resp::ok(&format!("database '{name}' unloaded"))
    }

    fn handle_drop(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "database is busy in a transaction");
        }
        let was_loaded = st.dbs.remove(&name).is_some();
        let dir = st.root.join(crate::db_dir_for(&name));
        let existed_on_disk = dir.is_dir();
        if existed_on_disk {
            if let Err(e) = fs::remove_dir_all(&dir) {
                return Resp::fail(500, &format!("drop failed: {e}"));
            }
        }
        if !was_loaded && !existed_on_disk {
            return Resp::fail(404, &format!("database '{name}' does not exist"));
        }
        Resp::ok(&format!("database '{name}' dropped"))
    }

    fn handle_show(&self, st: &mut ApiState) -> Resp {
        let mut names: Vec<String> = Vec::new();
        // On-disk databases (directories ending in the .db suffix).
        if let Ok(rd) = fs::read_dir(&st.root) {
            for entry in rd.flatten() {
                if entry.path().is_dir() {
                    let fname = entry.file_name().to_string_lossy().into_owned();
                    if let Some(stripped) = fname.strip_suffix(crate::DB_SUFFIX) {
                        names.push(stripped.to_string());
                    }
                }
            }
        }
        // Plus any loaded-but-not-yet-persisted databases.
        for k in st.dbs.keys() {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }
        names.sort();
        names.dedup();
        let mut items = Vec::new();
        for n in &names {
            let loaded = st.dbs.contains_key(n);
            let triples = st.dbs.get(n).map(|d| d.triple_num());
            let in_txn = st.db_txn.contains_key(n);
            let triples_field = match triples {
                Some(t) => t.to_string(),
                None => "null".to_string(),
            };
            items.push(format!(
                "{{\"name\":{},\"loaded\":{loaded},\"in_transaction\":{in_txn},\"triples\":{triples_field}}}",
                json_str(n)
            ));
        }
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"databases\":[{}]}}",
            items.join(",")
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    // ---- transactions -----------------------------------------------------

    fn handle_begin(&self, st: &mut ApiState, req: &Request, user: &str) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        if !st.dbs.contains_key(&name) {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        }
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "a transaction is already open on this database");
        }
        if let Some(db) = st.dbs.get_mut(&name) {
            if let Err(e) = db.begin() {
                return Resp::fail(500, &format!("begin failed: {e}"));
            }
        }
        let mut tid = new_id();
        while st.txns.contains_key(&tid) {
            tid = new_id();
        }
        st.txns.insert(tid, Txn { db: name.clone() });
        st.db_txn.insert(name.clone(), tid);
        append_log(
            &st.root,
            "txn.log",
            &format!("{}\tbegin\t{}\t{}\t{}", now_millis(), name, tid, user),
        );
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"transaction started\",\"TID\":{}}}",
            json_str(&tid.to_string())
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    fn handle_tquery(&self, st: &mut ApiState, req: &Request, user: &str) -> Resp {
        let Some(tid) = self.txn_id(req) else {
            return Resp::fail(400, "missing or malformed tid/txn parameter");
        };
        let Some(name) = st.txns.get(&tid).map(|t| t.db.clone()) else {
            return Resp::fail(404, &format!("no open transaction {tid}"));
        };
        let Some(sparql) = req
            .param("sparql")
            .or_else(|| req.query_string())
            .or_else(|| Some(req.body_str()).filter(|s| !s.trim().is_empty()))
        else {
            return Resp::fail(400, "missing sparql parameter");
        };
        let need = if is_update_sparql(&sparql) {
            Privilege::Update
        } else {
            Privilege::Query
        };
        if !st.users.has_priv(user, need) {
            return self.forbidden();
        }
        self.run_query(st, &name, &sparql, req, user)
    }

    fn handle_txn_end(&self, st: &mut ApiState, req: &Request, user: &str, commit: bool) -> Resp {
        let Some(tid) = self.txn_id(req) else {
            return Resp::fail(400, "missing or malformed tid/txn parameter");
        };
        let Some(txn) = st.txns.remove(&tid) else {
            return Resp::fail(404, &format!("no open transaction {tid}"));
        };
        st.db_txn.remove(&txn.db);
        let result = st.dbs.get_mut(&txn.db).map(|db| {
            if commit {
                db.commit()
            } else {
                db.rollback()
            }
        });
        let verb = if commit { "commit" } else { "rollback" };
        append_log(
            &st.root,
            "txn.log",
            &format!("{}\t{}\t{}\t{}\t{}", now_millis(), verb, txn.db, tid, user),
        );
        match result {
            Some(Ok(())) => Resp::ok(&format!("transaction {tid} {verb} done")),
            Some(Err(e)) => Resp::fail(500, &format!("{verb} failed: {e}")),
            None => Resp::fail(404, &format!("database '{}' is not loaded", txn.db)),
        }
    }

    fn handle_checkpoint(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let dir = st.root.join(crate::db_dir_for(&name));
        let Some(db) = st.dbs.get(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        match db.save(&dir) {
            Ok(()) => Resp::ok(&format!("database '{name}' checkpointed")),
            Err(e) => Resp::fail(500, &format!("checkpoint failed: {e}")),
        }
    }

    // ---- backup / restore / export ---------------------------------------

    fn handle_backup(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let path = req
            .param("path")
            .or_else(|| req.param("backup_path"))
            .map(PathBuf::from)
            .unwrap_or_else(|| st.root.join(format!("{name}.backup")));
        let Some(db) = st.dbs.get(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        match db.backup(&path) {
            Ok(()) => {
                let body = format!(
                    "{{\"StatusCode\":0,\"StatusMsg\":\"backup success\",\"path\":{}}}",
                    json_str(&path.to_string_lossy())
                );
                Resp::new(200, JSON, body.into_bytes())
            }
            Err(e) => Resp::fail(500, &format!("backup failed: {e}")),
        }
    }

    fn handle_restore(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        let Some(path) = req.param("path").or_else(|| req.param("backup_path")) else {
            return Resp::fail(400, "missing backup path");
        };
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "database is busy in a transaction");
        }
        match Database::restore(&path) {
            Ok(db) => {
                let dir = st.root.join(crate::db_dir_for(&name));
                if let Err(e) = db.save(&dir) {
                    return Resp::fail(500, &format!("restore persist failed: {e}"));
                }
                st.dbs.insert(name.clone(), db);
                Resp::ok(&format!("database '{name}' restored"))
            }
            Err(e) => Resp::fail(500, &format!("restore failed: {e}")),
        }
    }

    /// `GET /export?db_name=<name>[&download=0]` — the whole database as
    /// N-Triples. Sent as a file download (`Content-Disposition: attachment`)
    /// by default; pass `download=0`/`false`/`no`/`off` for an inline
    /// (browser-rendered) response instead.
    fn handle_export(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let Some(db) = st.dbs.get_mut(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        match db.query("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }") {
            Ok(QueryResult::Construct(ts)) => {
                let mut resp = Resp::new(200, "application/n-triples", ntriples_body(&ts));
                let disposition = if wants_inline(req) { "inline" } else { "attachment" };
                resp.extra.push((
                    "Content-Disposition".to_string(),
                    format!("{disposition}; filename=\"{name}.nt\""),
                ));
                resp
            }
            Ok(_) => Resp::fail(500, "export produced an unexpected result"),
            Err(e) => Resp::fail(500, &format!("export failed: {e}")),
        }
    }

    // ---- batch ------------------------------------------------------------

    fn handle_batch(&self, st: &mut ApiState, req: &Request, user: &str, insert: bool) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let triples = req
            .param("triples")
            .unwrap_or_else(|| req.body_str())
            .trim()
            .to_string();
        if triples.is_empty() {
            return Resp::fail(400, "no triples supplied");
        }
        // Reuse the SPARQL engine: wrap the triples in INSERT/DELETE DATA. The
        // triples must use full IRIs (N-Triples), as SPARQL DATA blocks require.
        let stmt = if insert {
            format!("INSERT DATA {{ {triples} }}")
        } else {
            format!("DELETE DATA {{ {triples} }}")
        };
        append_log(
            &st.root,
            "query.log",
            &format!(
                "{}\t{}\t{}\t{}",
                now_millis(),
                name,
                user,
                stmt.replace('\n', " ")
            ),
        );
        let Some(db) = st.dbs.get_mut(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        match db.query(&stmt) {
            Ok(QueryResult::Update { changed }) => {
                let op = if insert { "inserted" } else { "removed" };
                let body = format!(
                    "{{\"StatusCode\":0,\"StatusMsg\":\"batch {op}\",\"changed\":{changed}}}"
                );
                Resp::new(200, JSON, body.into_bytes())
            }
            Ok(_) => Resp::fail(400, "batch statement did not produce an update"),
            Err(e) => Resp::fail(400, &format!("batch failed: {e}")),
        }
    }

    // ---- monitoring -------------------------------------------------------

    fn handle_status(&self, st: &mut ApiState) -> Resp {
        let total_triples: u64 = st.dbs.values().map(|d| d.triple_num()).sum();
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"running\",\"databases_loaded\":{},\"total_triples\":{},\"open_transactions\":{},\"users\":{},\"requests\":{},\"uptime_secs\":{}}}",
            st.dbs.len(),
            total_triples,
            st.txns.len(),
            st.users.count(),
            st.requests,
            st.started.elapsed().as_secs(),
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    fn handle_monitor(&self, st: &mut ApiState) -> Resp {
        let mut dbs = Vec::new();
        let mut names: Vec<&String> = st.dbs.keys().collect();
        names.sort();
        for n in names {
            let s = st.dbs[n].stats();
            dbs.push(format!(
                "{{\"name\":{},\"triples\":{},\"entities\":{},\"literals\":{},\"predicates\":{},\"index_valid\":{},\"in_transaction\":{}}}",
                json_str(&s.name),
                s.triple_num,
                s.entity_num,
                s.literal_num,
                s.predicate_num,
                s.index_valid,
                s.in_transaction,
            ));
        }
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"uptime_secs\":{},\"requests\":{},\"users\":{},\"open_transactions\":{},\"databases\":[{}]}}",
            st.started.elapsed().as_secs(),
            st.requests,
            st.users.count(),
            st.txns.len(),
            dbs.join(","),
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    fn read_log(&self, st: &mut ApiState, file: &str) -> Resp {
        let path = st.root.join(file);
        let text = fs::read_to_string(&path).unwrap_or_default();
        Resp::new(200, "text/plain; charset=utf-8", text.into_bytes())
    }

    // ---- user management --------------------------------------------------

    fn handle_user_create(&self, st: &mut ApiState, req: &Request) -> Resp {
        let (Some(name), Some(password)) = (
            req.param("op_username").or_else(|| req.param("username")),
            req.param("op_password").or_else(|| req.param("password")),
        ) else {
            return Resp::fail(400, "missing op_username or op_password");
        };
        let privileges = req.param("privileges").unwrap_or_default();
        let Some(privs) = Privilege::parse_list(&privileges) else {
            return Resp::fail(400, "invalid privileges (use names or codes 1..8)");
        };
        match st.users.add(&name, &password, privs) {
            Ok(()) => Resp::ok(&format!("user '{name}' created")),
            Err(e) => Resp::fail(400, &e),
        }
    }

    fn handle_user_drop(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = req.param("op_username").or_else(|| req.param("username")) else {
            return Resp::fail(400, "missing op_username");
        };
        match st.users.drop_user(&name) {
            Ok(()) => Resp::ok(&format!("user '{name}' dropped")),
            Err(e) => Resp::fail(400, &e),
        }
    }

    fn handle_user_password(&self, st: &mut ApiState, req: &Request, user: &str) -> Resp {
        let Some(target) = req
            .param("username")
            .or_else(|| req.param("op_username"))
            .or_else(|| Some(user.to_string()))
        else {
            return Resp::fail(400, "missing username");
        };
        let Some(password) = req.param("op_password").or_else(|| req.param("password")) else {
            return Resp::fail(400, "missing op_password");
        };
        // A user may change their own password; only admins may change others'.
        let is_admin = st.users.has_priv(user, Privilege::Admin);
        if target != user && !is_admin {
            return self.forbidden();
        }
        match st.users.set_password(&target, &password) {
            Ok(()) => Resp::ok(&format!("password for '{target}' changed")),
            Err(e) => Resp::fail(400, &e),
        }
    }

    fn handle_user_grant(&self, st: &mut ApiState, req: &Request, grant: bool) -> Resp {
        let Some(name) = req.param("op_username").or_else(|| req.param("username")) else {
            return Resp::fail(400, "missing op_username");
        };
        // gStore privilege type "3" = clear all privileges.
        if req.param("type").as_deref() == Some("3") {
            return match st.users.clear_privileges(&name) {
                Ok(()) => Resp::ok(&format!("privileges for '{name}' cleared")),
                Err(e) => Resp::fail(400, &e),
            };
        }
        let Some(privileges) = req.param("privileges") else {
            return Resp::fail(400, "missing privileges");
        };
        let Some(privs) = Privilege::parse_list(&privileges) else {
            return Resp::fail(400, "invalid privileges (use names or codes 1..8)");
        };
        let res = if grant {
            st.users.grant(&name, &privs)
        } else {
            st.users.revoke(&name, &privs)
        };
        let verb = if grant { "granted" } else { "revoked" };
        match res {
            Ok(()) => Resp::ok(&format!("privileges {verb} for '{name}'")),
            Err(e) => Resp::fail(400, &e),
        }
    }

    fn handle_user_show(&self, st: &mut ApiState) -> Resp {
        let mut items = Vec::new();
        for u in st.users.list() {
            items.push(format!(
                "{{\"username\":{},\"privileges\":{},\"is_root\":{}}}",
                json_str(&u.name),
                json_str(&u.privilege_str()),
                u.name == ROOT_USER,
            ));
        }
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"users\":[{}]}}",
            items.join(",")
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    // ---- helpers ----------------------------------------------------------

    /// Resolve the target database name for query-like endpoints: an explicit
    /// `db_name`/`db` parameter, or — when exactly one database is loaded — that
    /// one.
    fn pick_db(&self, st: &ApiState, req: &Request) -> Option<String> {
        if let Some(n) = req.param("db_name").or_else(|| req.param("db")) {
            return valid_db_name(&n).then_some(n);
        }
        if st.dbs.len() == 1 {
            return st.dbs.keys().next().cloned();
        }
        None
    }

    /// A required, validated database name from `db_name`/`name`/`db`.
    fn req_db_name(&self, req: &Request) -> Option<String> {
        let n = req
            .param("db_name")
            .or_else(|| req.param("name"))
            .or_else(|| req.param("db"))?;
        valid_db_name(&n).then_some(n)
    }

    fn txn_id(&self, req: &Request) -> Option<u64> {
        req.param("tid")
            .or_else(|| req.param("txn"))
            .and_then(|s| s.trim().parse::<u64>().ok())
    }
}

// --- free helpers ------------------------------------------------------------

/// Render a [`QueryResult`] into an HTTP response using the negotiated format.
fn render_result(fmt: ResultFormat, result: &QueryResult) -> Resp {
    match result {
        QueryResult::Select(rs) => {
            let mut buf = Vec::new();
            let _ = sparql_results::write_select(fmt, rs, &mut buf);
            Resp::new(200, fmt.content_type(), buf)
        }
        QueryResult::Ask(b) => {
            let mut buf = Vec::new();
            let _ = sparql_results::write_ask(fmt, *b, &mut buf);
            Resp::new(200, fmt.content_type(), buf)
        }
        QueryResult::Construct(ts) => Resp::new(200, "application/n-triples", ntriples_body(ts)),
        QueryResult::Update { changed } => Resp::new(
            200,
            JSON,
            format!("{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"changed\":{changed}}}")
                .into_bytes(),
        ),
    }
}

/// Whether a SPARQL request is an UPDATE (mutating) statement, skipping any
/// leading `PREFIX`/`BASE` declarations.
fn is_update_sparql(s: &str) -> bool {
    let mut toks = s.split_whitespace().peekable();
    while let Some(t) = toks.peek() {
        match t.to_ascii_uppercase().as_str() {
            "PREFIX" => {
                toks.next();
                toks.next();
                toks.next();
            }
            "BASE" => {
                toks.next();
                toks.next();
            }
            _ => break,
        }
    }
    match toks.peek() {
        Some(t) => matches!(
            t.to_ascii_uppercase().as_str(),
            "INSERT" | "DELETE" | "LOAD" | "CLEAR" | "DROP" | "CREATE" | "ADD" | "MOVE" | "COPY"
                | "WITH"
        ),
        None => false,
    }
}

/// Parse an RDF document of the named `format` into triples, dispatching to
/// the crate's existing bundled parsers ([`crate::parser`]) — no new parsing
/// code. `nquads`/`trig` inputs have their graph labels dropped (loaded into
/// the target database's default graph), matching what `/upload` can insert
/// via [`Database::insert_triple`].
fn parse_rdf(format: &str, content: &str) -> crate::error::Result<Vec<Triple>> {
    match format {
        "turtle" | "ttl" => crate::parser::turtle::parse_str(content),
        "ntriples" | "nt" | "n-triples" => crate::parser::ntriples::parse_str(content),
        "nquads" | "nq" | "n-quads" => crate::parser::nquads::parse_str_triples(content),
        "rdfxml" | "rdf" | "xml" => crate::parser::rdfxml::parse_str(content),
        "trig" => crate::parser::trig::parse_str_triples(content),
        other => Err(crate::error::GStoreError::RdfParse {
            line: 0,
            msg: format!(
                "unknown RDF format '{other}' (expected turtle, ntriples, nquads, rdfxml, or trig)"
            ),
        }),
    }
}

/// Resolve `/upload`'s RDF format: an explicit `format=` parameter, else
/// inferred from `Content-Type`, else Turtle (a superset of N-Triples, same
/// default as [`Database::build_from_str`]).
fn upload_format(req: &Request) -> String {
    if let Some(f) = req.param("format") {
        return f.trim().to_ascii_lowercase();
    }
    if let Some(ct) = req.header("content-type") {
        let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        let mapped = match ct.as_str() {
            "text/turtle" | "application/x-turtle" => Some("turtle"),
            "application/n-triples" => Some("ntriples"),
            "application/n-quads" => Some("nquads"),
            "application/rdf+xml" => Some("rdfxml"),
            "application/trig" => Some("trig"),
            _ => None,
        };
        if let Some(m) = mapped {
            return m.to_string();
        }
    }
    "turtle".to_string()
}

/// Whether `/export` should render its `Content-Disposition` as `inline`
/// rather than the default `attachment` (file download). Only an explicit
/// falsy `download=` parameter switches to inline.
fn wants_inline(req: &Request) -> bool {
    matches!(
        req.param("download").as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// Validate a database name: non-empty, ≤ 64 chars, `[A-Za-z0-9_-]` only (so it
/// is a safe single path component).
fn valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Append one line (with a trailing newline) to `root/file`, best effort.
fn append_log(root: &Path, file: &str, line: &str) {
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(file))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Milliseconds since the Unix epoch (for log timestamps).
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A fresh, non-zero transaction id.
fn new_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(now_millis());
    let id = h.finish();
    if id == 0 {
        1
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_update_statements() {
        assert!(is_update_sparql("INSERT DATA { <a> <b> <c> }"));
        assert!(is_update_sparql("  delete data { <a> <b> <c> }"));
        assert!(is_update_sparql(
            "PREFIX : <http://x/> INSERT DATA { :a :b :c }"
        ));
        assert!(!is_update_sparql("SELECT * WHERE { ?s ?p ?o }"));
        assert!(!is_update_sparql("ASK { ?s ?p ?o }"));
        assert!(!is_update_sparql(
            "PREFIX : <http://x/> SELECT * WHERE { ?s ?p ?o }"
        ));
    }

    #[test]
    fn validates_db_names() {
        assert!(valid_db_name("my_db-1"));
        assert!(!valid_db_name(""));
        assert!(!valid_db_name("../etc"));
        assert!(!valid_db_name("a b"));
    }

    // --- in-process HTTP client tests: upload, export download, console,
    // task registry (mirrors the pattern in tests/dt_http_parity.rs) --------

    use std::io::Read as _;
    use std::thread;

    /// A unique temp directory for one server's databases + logs + user store.
    fn tmp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static CTR: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        p.push(format!("gstore_api_unit_{tag}_{}_{n}", std::process::id()));
        p
    }

    /// Start an `ApiServer` (root password "root") in a background thread.
    fn start(tag: &str) -> (Arc<ApiServer>, SocketAddr) {
        let root = tmp_root(tag);
        let server = Arc::new(ApiServer::bind(&root, "root", "127.0.0.1:0").expect("bind"));
        let addr = server.local_addr().expect("addr");
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        (server, addr)
    }

    /// Send one HTTP/1.1 request; return the full raw response (status line +
    /// headers + body) so callers can inspect headers as well as the body.
    fn raw_request(addr: &SocketAddr, method: &str, target: &str, auth: Option<&str>, body: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
        if let Some(a) = auth {
            raw.push_str(&format!("Authorization: {a}\r\n"));
        }
        raw.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    }

    /// Split a raw response into `(status_code, body)`.
    fn split_status_body(resp: &str) -> (u16, String) {
        let status = resp
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .expect("status code");
        let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (status, body.to_string())
    }

    /// RFC 4648 base64 (client side of the server's decoder).
    fn base64(input: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(A[((n >> 18) & 63) as usize] as char);
            out.push(A[((n >> 12) & 63) as usize] as char);
            out.push(if chunk.len() > 1 { A[((n >> 6) & 63) as usize] as char } else { '=' });
            out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
        }
        out
    }

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))
    }

    #[test]
    fn upload_round_trip_then_query() {
        let (_srv, addr) = start("upload");
        let root = basic("root", "root");

        // Build an empty database, then /upload N-Triples into it.
        let (status, _) = split_status_body(&raw_request(&addr, "POST", "/build?db_name=up", Some(&root), ""));
        assert_eq!(status, 200);

        let ntriples = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                         <http://ex/a> <http://ex/name> \"Alice\" .\n";
        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/upload?db_name=up&format=ntriples",
            Some(&root),
            ntriples,
        ));
        assert_eq!(status, 200, "upload: {body}");
        assert!(body.contains("\"triples_parsed\":2"), "{body}");
        assert!(body.contains("\"triples_inserted\":2"), "{body}");

        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/sparql?db_name=up",
            Some(&root),
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
        ));
        assert_eq!(status, 200);
        assert!(body.contains("http://ex/b"), "uploaded triple must be queryable: {body}");

        // Re-uploading the same content inserts nothing new.
        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/upload?db_name=up&format=ntriples",
            Some(&root),
            ntriples,
        ));
        assert_eq!(status, 200);
        assert!(body.contains("\"triples_inserted\":0"), "dup upload: {body}");
    }

    #[test]
    fn upload_creates_database_when_absent() {
        let (_srv, addr) = start("upload_new");
        let root = basic("root", "root");

        // No prior /build: the default format (turtle) and an unseen db_name
        // must still work, creating the database on the fly.
        let ttl = "@prefix : <http://ex/> .\n:a :p :b .\n";
        let (status, body) = split_status_body(&raw_request(&addr, "POST", "/upload?db_name=fresh", Some(&root), ttl));
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"triples_parsed\":1"), "{body}");

        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/sparql?db_name=fresh",
            Some(&root),
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
        ));
        assert_eq!(status, 200);
        assert!(body.contains("http://ex/b"), "{body}");
    }

    #[test]
    fn export_download_headers() {
        let (_srv, addr) = start("export_dl");
        let root = basic("root", "root");
        let data = "@prefix : <http://ex/> .\n:a :p :b .\n";
        let (status, _) = split_status_body(&raw_request(&addr, "POST", "/build?db_name=ex2", Some(&root), data));
        assert_eq!(status, 200);

        // Default: an attachment (file download) with a filename.
        let resp = raw_request(&addr, "GET", "/export?db_name=ex2", Some(&root), "");
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("Content-Type: application/n-triples"), "{resp}");
        assert!(
            resp.contains("Content-Disposition: attachment; filename=\"ex2.nt\""),
            "{resp}"
        );

        // `download=0` switches to an inline disposition.
        let inline = raw_request(&addr, "GET", "/export?db_name=ex2&download=0", Some(&root), "");
        assert!(
            inline.contains("Content-Disposition: inline; filename=\"ex2.nt\""),
            "{inline}"
        );
    }

    #[test]
    fn console_served_without_auth() {
        let (_srv, addr) = start("console");

        let resp = raw_request(&addr, "GET", "/", None, "");
        assert!(resp.contains("200 OK"), "{resp}");
        assert!(resp.contains("text/html"), "{resp}");
        assert!(resp.contains("gStore web console"), "{resp}");
        assert!(resp.contains("/sparql"), "endpoint note present: {resp}");

        let resp2 = raw_request(&addr, "GET", "/console", None, "");
        assert!(resp2.contains("200 OK"));
        assert!(resp2.contains("gStore web console"));
    }

    #[test]
    fn tasklist_and_cancel_basic() {
        let (_srv, addr) = start("tasks");
        let root = basic("root", "root");
        let (status, _) = split_status_body(&raw_request(&addr, "POST", "/build?db_name=tk", Some(&root), ""));
        assert_eq!(status, 200);

        // A completed query leaves no residue in the registry (single-
        // threaded server: the task is registered and unregistered within
        // the same request).
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/sparql?db_name=tk",
            Some(&root),
            "SELECT * WHERE { ?s ?p ?o }",
        ));
        assert_eq!(status, 200);
        let (status, body) = split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
        assert_eq!(status, 200);
        assert!(body.contains("\"tasks\":[]"), "{body}");

        // Cancelling an unknown task id is 404.
        let (status, _) = split_status_body(&raw_request(&addr, "POST", "/cancel?id=999999", Some(&root), ""));
        assert_eq!(status, 404);

        // /tasklist is Admin-gated.
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/user/create?op_username=reader&op_password=pw&privileges=query",
            Some(&root),
            "",
        ));
        assert_eq!(status, 200);
        let reader = basic("reader", "pw");
        let (status, _) = split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&reader), ""));
        assert_eq!(status, 403, "query-only user must not see /tasklist");

        // /cancel needs Admin or Update; a Query-only user is forbidden.
        let (status, _) = split_status_body(&raw_request(&addr, "POST", "/cancel?id=1", Some(&reader), ""));
        assert_eq!(status, 403, "query-only user must not reach /cancel");

        // An Update-privileged (non-admin) user CAN reach the /cancel
        // handler (404 for an unknown id, not 403).
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/user/create?op_username=writer&op_password=pw&privileges=update",
            Some(&root),
            "",
        ));
        assert_eq!(status, 200);
        let writer = basic("writer", "pw");
        let (status, _) = split_status_body(&raw_request(&addr, "POST", "/cancel?id=999999", Some(&writer), ""));
        assert_eq!(status, 404, "update-privileged user must reach the handler, not be forbidden");
    }
}
