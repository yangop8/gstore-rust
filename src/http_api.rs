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
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::db::Database;
use crate::http_users::{Privilege, UserStore, ROOT_USER};
use crate::query::QueryResult;
use crate::server::{
    err_json, ntriples_body, parse_basic_auth, write_response_ext, Request,
};
use crate::sparql_results::{self, json_str, ResultFormat};

const JSON: &str = "application/json";

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

        // `/login` is the only fully public endpoint.
        if path == "/login" {
            return self.handle_login(st, req);
        }
        if path == "/logout" {
            if let Some(tok) = req.bearer_or_session_token() {
                st.users.logout(&tok);
            }
            return Resp::ok("logged out");
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
        let Some(db) = st.dbs.get_mut(name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        match db.query(sparql) {
            Ok(result) => render_result(fmt, &result),
            Err(e) => Resp::new(
                400,
                "application/sparql-results+json",
                err_json(&e.to_string()),
            ),
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

    fn handle_export(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let Some(db) = st.dbs.get_mut(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        match db.query("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }") {
            Ok(QueryResult::Construct(ts)) => {
                Resp::new(200, "application/n-triples", ntriples_body(&ts))
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
}
