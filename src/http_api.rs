//! A richer HTTP API endpoint bringing the [`crate::server`] SPARQL endpoint up
//! to parity with gStore's C++ `ghttp` / `ApiProvider`.
//!
//! Where [`crate::server::Server`] is a single-database SPARQL endpoint, this
//! [`ApiServer`] manages a *directory of named databases* and layers on the
//! operational surface gStore exposes over HTTP:
//!
//! * **User management & RBAC** ([`crate::http_users`], [`crate::sysdb`]) —
//!   a persistent user store with per-database privileges, `/login` returning
//!   a session token, and privilege-gated endpoints.
//! * **Transactions over HTTP** — `/begin`, `/tquery`, `/commit`, `/rollback`,
//!   mapping to [`Database::begin`]/[`commit`](Database::commit)/
//!   [`rollback`](Database::rollback). See *Transactions* below.
//! * **Database lifecycle** — `/build`, `/load`, `/unload`, `/drop`, `/show`.
//!   A `/build?db_path=` reads a server-local file only as the server's
//!   [`LoadPolicy`] allows (the policy of SPARQL `LOAD`; by default nothing).
//!   A database directory is locked against other processes while it is
//!   served ([`crate::db::dirlock`]), and `/build` (over an existing
//!   database), `/drop` and `/restore` take that lock before they rewrite,
//!   delete or swap a directory — the rule the `/api` operations
//!   (`gapi::ops_db`) follow too. Two REST semantics differ from upstream's
//!   `/api` ones and are kept as Rust extensions: `/build` rebuilds an
//!   existing database (`/api` `build` refuses it) and `/drop` deletes the
//!   directory (`/api` `drop` keeps a `<db>.db.bak` by default).
//! * **Backup / restore / export** — `/backup`, `/restore`, `/export`. Backups
//!   are written only inside the server's backup directory (`<root>/backups`,
//!   or `conf.ini`'s `backup_path`) and never over another database's files;
//!   `/restore` reads from there, or from a directory the [`LoadPolicy`]
//!   allows.
//! * **Batch operations** — `/batchInsert`, `/batchRemove`: the payload is
//!   parsed as RDF (N-Triples, or Turtle) and the ground triples are applied
//!   directly — no SPARQL text is ever built from user input.
//! * **Logging & monitoring** — query / access / transaction logs and a live
//!   `/monitor` (and `/status`) JSON snapshot.
//! * **The upstream `/api` protocol** ([`crate::gapi`]) — `GET|POST /api`,
//!   `POST /` and `GET /?operation=…` with per-request credentials and the
//!   gStore JSON envelope, on the same state as the endpoints above; also
//!   upstream's `/shutdown` and `/restart` path routes (root, or the `system`
//!   user with the per-run password `gserver` writes to its pid file), checked
//!   by the layer's one credential check (`gapi::auth::verify`).
//! * **Configuration** — connection handling (worker pool, keep-alive, CORS,
//!   body cap), the `check_point` policy, the `/api` output cap
//!   (`max_output_size`) and `licensetype` come from `conf.ini` through
//!   [`ApiServer::with_config`] / [`ApiServer::bind_with_config`].
//!
//! **Durability.** Each loaded database tracks whether it has unsaved changes.
//! Under gStore's `check_point` policy (on by default,
//! [`ApiServer::with_check_point`]) every successful write — `/update`, an
//! update via `/sparql`, `/batchInsert`, `/batchRemove`, `/upload`, a `/commit`
//! that changed data — is saved to `<root>/<name>.db` before it is
//! acknowledged. An UPDATE that fails part-way counts as a write too: its
//! operations before the failing one stay applied (the error says so), so they
//! are saved like any other change. With the policy off, changes stay in
//! memory until `/checkpoint` (which always forces a save), `/unload` (which
//! saves a dirty database before dropping it) or a graceful shutdown.
//!
//! **Graceful shutdown.** [`ApiServer::shutdown`] (or a
//! [`ShutdownHandle`] — e.g. wired to `SIGINT`/`SIGTERM` with
//! [`crate::server::shutdown_on_signals`]) stops accepting, lets in-flight
//! requests finish (bounded; queries still running after the grace period are
//! cancelled), rolls back open transactions, saves every dirty database, and
//! makes [`serve_forever`](ApiServer::serve_forever) return a
//! [`ShutdownReport`]. Once it has begun, every write is saved before it is
//! acknowledged, and a request that reaches a database only after its final
//! save is refused — so no acknowledged write is lost.
//!
//! **Locking.** Every database has its own lock, and no request ever waits for
//! one while holding the server-wide state lock (see `ApiState`): a long
//! query blocks only requests for *its* database, never `/cancel`,
//! `/tasklist` or other databases. Nor does slow per-request work run under
//! the state lock: password hashing (also for bad credentials), parsing a
//! SPARQL statement to classify it, and parsing a batch payload all happen
//! with it released. Every query — `/tquery` included — is a task that
//! `/tasklist` lists and `/cancel` (or a shutdown past its grace period) can
//! cancel.
//!
//! **Tasks.** One task manager ([`crate::gapi::tasks::TaskManager`], see
//! [`ApiServer::task_manager`]) tracks the running SPARQL statements and the
//! upstream protocol's asynchronous operations (`async=true`) under
//! upstream-style `opt_id`s: `/tasklist` and `/cancel` here, and
//! `operationTaskList` / `operationTaskCancel` / `checkOperationState` in
//! [`crate::gapi`], see and cancel the same tasks. A graceful shutdown also
//! cancels the asynchronous operations still running and waits for them
//! (bounded, [`TaskConfig::shutdown_wait`]).
//!
//! **Transactions.** At most one transaction is open per database, owned by
//! the user who began it: only that user (or `root`) may `/tquery`, `/commit`
//! or `/rollback` it. While it is open, every non-transactional write to that
//! database (`/update`, `/batch*`, `/upload`, …) is rejected with `409` rather
//! than silently joining the transaction, and every non-transactional read is
//! answered from a snapshot of the committed state taken at `/begin`, so no
//! client ever observes uncommitted data. The snapshot clones the dictionary
//! and triple indexes — an open transaction roughly doubles its database's
//! memory footprint until it ends. `/begin` accepts gStore's `isolevel`
//! (`1` SERIALIZABLE — the default, `2` SNAPSHOT, `3` READ_COMMITTED, see
//! [`TxnIsolation`]); with a single writer every level is honoured. Like
//! upstream, transaction ids are numbered per loaded database, starting at 1
//! after each load; `/begin` answers the database (`db_name`) with the id,
//! and `/tquery`, `/commit` and `/rollback` take it to tell apart an id open
//! on several databases (without it the id must be open on exactly one and
//! not recently ended on another — a retried `/commit` — else `409`).
//! Unknown transaction ids are `404`; committing or rolling back a finished
//! one is `409`.
//!
//! **Upstream protocol.** The same server also speaks upstream gStore 1.4's
//! own protocol — `GET|POST /api` (and `POST /`, `GET /?operation=…`) with an
//! `operation` parameter, per-request `username`/`password`, and a JSON
//! envelope that is always HTTP 200 — so the upstream SDKs, the Workbench and
//! the upstream test scripts work unmodified. It is implemented in
//! [`crate::gapi`] on this module's state (databases, users, transactions,
//! tasks, logs); the endpoints documented here remain as Rust-only
//! extensions. Transaction ids are small sequential numbers (the first is
//! `1`), as upstream's.
//!
//! **Panic isolation.** Each request runs under `catch_unwind`: a panicking
//! handler answers `500` with a JSON error and the locks it poisoned are
//! recovered, so the server keeps serving.
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
//! Privileges are those of the system store ([`crate::sysdb`]) and decided
//! by the same rules as the `/api` layer: each endpoint counts as the
//! upstream operation it performs and is checked on the database it targets
//! (`/user/create`, `/user/grant` and `/user/revoke` grant on one database
//! with `db_name=`, else on every database); `/build` and `/drop` are
//! reserved to `root` (and `admin` users), as upstream.
//!
//! As with [`crate::server`], real TLS is out of scope — terminate HTTPS in a
//! reverse proxy and do not send credentials over plain `http://` in production.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::analytics::csr::{CsrCache, CsrGraph};
use crate::config::GStoreConfig;
use crate::db::dirlock::DirLock;
use crate::db::{Database, LoadPolicy, LoadSource, QuerySettings};
use crate::dict::Dictionary;
use crate::error::GStoreError;
use crate::gapi::apilog::{ApiLogs, TxnState};
use crate::gapi::ipfilter::IpFilter;
use crate::gapi::tasks::{TaskConfig, TaskManager};
use crate::gapi::GapiConfig;
use crate::http_users::{
    CheckedCredentials, CredentialCheck, PasswordForm, Privilege, UserStore, ROOT_USER,
};
use crate::model::{Term, Triple};
use crate::query::{Evaluator, FunctionRegistry, IntegerDivision, QueryResult, TopKOptions};
use crate::server::{
    err_json, locked, ntriples_body, parse_basic_auth, run_accept_loop, serve_connection,
    write_response_ext, write_response_maybe_gzip, BadRequest, ConnMeta, HttpOptions, Lifecycle,
    Request, ShutdownHandle, ShutdownReport, DEFAULT_SHUTDOWN_GRACE, PANIC_MSG,
};
use crate::sparql_results::{self, json_str, ResultFormat};
use crate::store::{TripleSource, TripleStore};
use crate::sysdb::SysDb;

const JSON: &str = "application/json";
const SPARQL_JSON: &str = "application/sparql-results+json";

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
  <code>/login</code>.<br>
  Upstream gStore protocol (SDKs, Workbench): <code>GET|POST /api</code>
  with <code>operation=…</code>, <code>username</code> and
  <code>password</code> parameters.
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

    /// [`ok`](Self::ok) plus an optional `"warning"` (e.g. a failed check_point
    /// save after an applied write).
    fn ok_warn(msg: &str, warning: &Option<String>) -> Resp {
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":{}{}}}",
            json_str(msg),
            warn_field(warning)
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    /// A `{"StatusCode":code,"StatusMsg":…}` error envelope at HTTP `status`.
    fn fail(status: u16, msg: &str) -> Resp {
        Resp::new(status, JSON, status_json(status as i64, msg))
    }
}

impl From<crate::gapi::GResp> for Resp {
    /// An `/api` response: a JSON envelope is always HTTP 200 (see
    /// [`crate::gapi::envelope`]).
    fn from(g: crate::gapi::GResp) -> Resp {
        let (status, ctype, body, extra) = g.into_parts();
        Resp {
            status,
            ctype,
            body,
            extra,
        }
    }
}

/// Build a `{"StatusCode":code,"StatusMsg":"msg"}` JSON body.
fn status_json(code: i64, msg: &str) -> Vec<u8> {
    format!("{{\"StatusCode\":{code},\"StatusMsg\":{}}}", json_str(msg)).into_bytes()
}

/// `,"warning":"…"` when there is a warning, else empty (spliced into a JSON
/// object just before its closing brace).
fn warn_field(warning: &Option<String>) -> String {
    match warning {
        Some(w) => format!(",\"warning\":{}", json_str(w)),
        None => String::new(),
    }
}

/// gStore's transaction isolation levels (`IsolationLevelType` in upstream
/// `src/Util/Transaction.h`), chosen by `/begin`'s `isolevel` parameter:
/// `1` = SERIALIZABLE, `2` = SNAPSHOT, `3` = READ_COMMITTED.
///
/// HTTP transactions here are single-writer per database (while one is open,
/// every other write to that database is rejected), so a transaction never has
/// a concurrent writer to conflict with: its reads see the begin-time state plus
/// its own writes, and everyone else reads the committed state. Every level is
/// therefore met — all three in fact behave serializably. The requested level is
/// validated, recorded, logged, and echoed in the `/begin` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnIsolation {
    /// `1` — serializable (upstream's default for `Txn_manager::Begin`).
    Serializable,
    /// `2` — snapshot isolation.
    Snapshot,
    /// `3` — read committed.
    ReadCommitted,
}

impl TxnIsolation {
    /// Parse an `isolevel` value: the upstream numeric code (`1`/`2`/`3`) or a
    /// level name (`serializable`, `snapshot`, `read_committed`; case- and
    /// separator-insensitive). `None` for anything else.
    pub fn parse(s: &str) -> Option<TxnIsolation> {
        let s = s.trim();
        let norm: String = if s.chars().all(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '-' | ' ')) {
            s.chars()
                .filter(|c| !matches!(c, '_' | '-' | ' '))
                .collect::<String>()
                .to_ascii_lowercase()
        } else {
            s.to_string()
        };
        match norm.as_str() {
            "1" | "serializable" => Some(TxnIsolation::Serializable),
            "2" | "snapshot" | "si" => Some(TxnIsolation::Snapshot),
            "3" | "readcommitted" | "rc" => Some(TxnIsolation::ReadCommitted),
            _ => None,
        }
    }

    /// The upstream numeric code.
    pub fn code(self) -> u8 {
        match self {
            TxnIsolation::Serializable => 1,
            TxnIsolation::Snapshot => 2,
            TxnIsolation::ReadCommitted => 3,
        }
    }

    /// The upstream level name.
    pub fn name(self) -> &'static str {
        match self {
            TxnIsolation::Serializable => "SERIALIZABLE",
            TxnIsolation::Snapshot => "SNAPSHOT",
            TxnIsolation::ReadCommitted => "READ_COMMITTED",
        }
    }
}

/// One open HTTP transaction (registry entry in [`ApiState::txns`]).
pub(crate) struct Txn {
    /// The database it operates on.
    pub(crate) db: String,
    /// The user who began it: only they (or `root`) may `/tquery`, `/commit` or
    /// `/rollback` it.
    pub(crate) owner: String,
    pub(crate) isolation: TxnIsolation,
}

/// How many finished transaction ids are remembered so that a repeated
/// `/commit` or `/rollback` gets a precise "already committed" error.
const ENDED_TXN_MEMORY: usize = 1024;

/// An immutable copy of a database's committed state, taken at `/begin`.
/// While a transaction is open every *non-transactional* read of that database
/// is answered from here, so other clients never observe its uncommitted
/// writes (no dirty reads).
///
/// Memory cost: the dictionary, the default-graph triple indexes and the named
/// graphs are cloned, so an open transaction roughly doubles its database's
/// in-memory footprint until it commits or rolls back. The VS-tree is not
/// part of the snapshot (it is only a candidate-pruning accelerator, so
/// results are unchanged). The user-defined functions registered at `/begin`
/// are shared with it (the registry is reference-counted), and the advanced
/// graph functions run on one CSR of the snapshot — the database's cached one
/// when it had one for this data, otherwise built on the first graph-function
/// read — shared by every read until the transaction ends.
pub(crate) struct ReadSnapshot {
    dict: Dictionary,
    store: TripleStore,
    named: BTreeMap<u32, TripleStore>,
    functions: FunctionRegistry,
    /// The database's integer-division mode at `/begin`.
    integer_division: IntegerDivision,
    /// The database's Top-K / early-termination options at `/begin`.
    topk: TopKOptions,
    csr: CsrCache,
    /// Whether `SERVICE` may contact remote endpoints: the database's load
    /// policy at `/begin` (a server's restricted policy refuses it), so a
    /// read inside a transaction cannot bypass the `SERVICE` ban.
    allow_service: bool,
    /// The database's graph-function output bound at `/begin`.
    gfun_max_output_bytes: usize,
}

impl ReadSnapshot {
    fn capture(db: &Database) -> ReadSnapshot {
        let store = match db.backend().as_memory() {
            Some(mem) => mem.clone(),
            None => {
                // A persistent backend has no in-RAM store to clone: materialize it.
                let mut s = TripleStore::new();
                s.bulk_load(db.backend().iter_all());
                s
            }
        };
        ReadSnapshot {
            dict: db.dict().clone(),
            store,
            named: db.named_graphs().clone(),
            functions: db.functions().clone(),
            integer_division: db.integer_division(),
            topk: db.topk_options(),
            csr: db
                .cached_csr()
                .map_or_else(CsrCache::new, |g| CsrCache::seeded(0, g)),
            allow_service: db.load_policy().allow_remote,
            gfun_max_output_bytes: db.gfun_max_output_bytes(),
        }
    }

    /// Run a read query (SELECT/ASK/CONSTRUCT/DESCRIBE). Updates are refused:
    /// the snapshot is read-only by construction.
    fn query(&self, sparql: &str, cancel: Option<Arc<AtomicBool>>) -> crate::Result<QueryResult> {
        let q = crate::parser::sparql::parse(sparql)?;
        if matches!(q, crate::parser::sparql::ast::Query::Update(_)) {
            return Err(GStoreError::Query(
                "updates cannot run against a transaction's read snapshot".into(),
            ));
        }
        // The snapshot never changes, so its CSR is cached under one
        // generation for the transaction's lifetime.
        let csr = || self.csr.get_or_build(0, || CsrGraph::build(&self.store));
        let mut eval = Evaluator::new(&self.dict, &self.store)
            .with_service_allowed(self.allow_service)
            .with_csr_provider(&csr)
            .with_integer_division(self.integer_division)
            .with_topk_options(self.topk)
            .with_gfun_max_output_bytes(self.gfun_max_output_bytes);
        if !self.named.is_empty() {
            eval = eval.with_named(&self.named);
        }
        if !self.functions.is_empty() {
            eval = eval.with_functions(self.functions.clone());
        }
        if let Some(c) = cancel {
            eval = eval.with_cancel(c);
        }
        eval.evaluate(&q)
    }

    fn triple_num(&self) -> u64 {
        self.store.triple_count()
    }
}

/// The open transaction on a database, kept *inside* the per-db lock so a
/// request that already holds the lock can tell, race-free, whether a
/// transaction is open (and must then reject a write or read the snapshot).
pub(crate) struct OpenTxn {
    pub(crate) tid: u64,
    /// The committed state at `/begin` (see [`ReadSnapshot`]).
    pub(crate) snapshot: Arc<ReadSnapshot>,
    /// Whether a `/tquery` update changed data — if so, the commit marks the
    /// database dirty (and saves it under the check_point policy).
    pub(crate) changed: bool,
}

/// One loaded database plus its HTTP-layer bookkeeping, behind the per-db
/// mutex.
pub(crate) struct DbSlot {
    pub(crate) db: Database,
    /// Where this database is persisted (`<root>/<name>.db`).
    pub(crate) dir: PathBuf,
    /// The in-memory database has changes not yet saved to [`dir`](Self::dir).
    pub(crate) dirty: bool,
    /// The open HTTP transaction on this database, if any.
    pub(crate) txn: Option<OpenTxn>,
    /// The next transaction id to hand out on this database. Like upstream
    /// gStore, which creates a `Txn_manager` (its counter starting at 1) for
    /// every database it loads, ids are small, sequential and numbered per
    /// loaded database: the first transaction after a load is `1` — the
    /// upstream test scripts address it so. Ownership checks, not
    /// unguessable ids, keep transactions apart.
    pub(crate) next_tid: u64,
    /// Set once the database was unloaded, dropped or replaced while a request
    /// still held this handle: the slot then holds an empty placeholder that
    /// must never be queried, written or saved (it would overwrite `dir`).
    pub(crate) retired: bool,
}

/// A shared handle to one loaded database.
pub(crate) type DbHandle = Arc<Mutex<DbSlot>>;

impl DbSlot {
    /// A freshly loaded / built / created / restored database: clean when it
    /// matches what is on disk at `dir`. The server's `LOAD` policy is
    /// installed on it (clients must not make the server read arbitrary local
    /// files), and so are its query-evaluation settings (`conf.ini`'s
    /// `[query]` section: integer division, query cache, Top-K, graph-function
    /// output bound). Every database the server serves passes through here.
    pub(crate) fn handle(
        mut db: Database,
        dir: PathBuf,
        dirty: bool,
        policy: &LoadPolicy,
        query: &QuerySettings,
    ) -> DbHandle {
        db.set_load_policy(policy.clone());
        db.apply_query_settings(query);
        // Hold `dir`'s inter-process lock while serving it (`crate::db::dirlock`):
        // a database opened from `dir` already does; one built in memory and
        // saved there takes it now. Best effort — a database not on disk yet
        // has nothing to lock (it is locked when it is next opened).
        if dir.is_dir() && db.storage() != crate::StorageKind::RocksDb {
            let _ = db.hold_dir_lock(&dir);
        }
        Arc::new(Mutex::new(DbSlot {
            db,
            dir,
            dirty,
            txn: None,
            next_tid: 1,
            retired: false,
        }))
    }

    /// Retire this slot (its database is being unloaded, dropped or replaced):
    /// drop the database now — closing its store files before anything on disk
    /// replaces them — and leave an empty placeholder that refuses to be saved.
    /// A request that cloned the handle earlier then sees [`retired`](Self::retired).
    pub(crate) fn retire(&mut self) {
        self.retired = true;
        self.dirty = false;
        let name = self.db.name().to_string();
        self.db = Database::new(name);
    }

    /// Save to [`dir`](Self::dir) and clear the dirty flag. Refused while a
    /// transaction is open — the live database then holds uncommitted writes.
    /// For an on-disk / RocksDB database opened from `dir` this is a cheap
    /// commit point (its updates are already written through); an in-memory
    /// snapshot is rewritten atomically.
    pub(crate) fn save(&mut self) -> crate::Result<()> {
        if self.retired {
            return Err(GStoreError::Database(format!(
                "refusing to save: database '{}' is no longer loaded",
                self.db.name()
            )));
        }
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "refusing to save: a transaction is open (its writes are uncommitted)".into(),
            ));
        }
        // Never write a directory another process holds (`crate::db::dirlock`):
        // a database created in memory (`/upload` into a new name) takes the
        // lock before its first save, and keeps it while it is served.
        if !self.db.holds_dir_lock() && self.db.storage() != crate::StorageKind::RocksDb {
            fs::create_dir_all(&self.dir)?;
            self.db.hold_dir_lock(&self.dir)?;
        }
        self.db.save(&self.dir)?;
        self.dirty = false;
        Ok(())
    }

    /// Record that a committed write changed data: mark dirty and, under the
    /// check_point policy, save now. Returns a warning when that save failed
    /// (the write itself stays applied in memory and is retried on unload /
    /// shutdown / `/checkpoint`).
    ///
    /// With check_point off an on-disk / RocksDB database still gets a commit
    /// point ([`Database::sync`], a no-op for an in-memory snapshot): its
    /// direct-API writes (`/batch*`, `/upload`) are already written through
    /// and only need committing, so there is no reason to leave them volatile.
    pub(crate) fn wrote(&mut self, check_point: bool) -> Option<String> {
        self.dirty = true;
        if !check_point {
            return self.db.sync().err().map(|e| {
                format!("change applied in memory but could not be made durable: {e}")
            });
        }
        self.save().err().map(|e| {
            format!(
                "change applied in memory but saving to '{}' failed: {e}",
                self.dir.display()
            )
        })
    }

    /// Run a read on behalf of a *non-transactional* caller: against the
    /// begin-time snapshot while a transaction is open (never its uncommitted
    /// writes), else against the live database.
    pub(crate) fn read(&mut self, sparql: &str) -> crate::Result<QueryResult> {
        match &self.txn {
            Some(open) => open.snapshot.query(sparql, None),
            None => self.db.query(sparql),
        }
    }

    /// The committed triple count (the snapshot's while a transaction is open).
    pub(crate) fn committed_triple_num(&self) -> u64 {
        match &self.txn {
            Some(open) => open.snapshot.triple_num(),
            None => self.db.triple_num(),
        }
    }
}

/// The `409` answer to an `/unload` while transaction `tid` is open.
fn unload_busy(name: &str, tid: u64) -> Resp {
    Resp::fail(
        409,
        &format!("database '{name}' is busy in transaction {tid}; commit or roll back first"),
    )
}

/// The refusal (REST `409`) of a `/begin` while transaction `open` is open.
pub(crate) fn txn_already_open_err(name: &str, open: u64) -> OpError {
    OpError::new(
        OpErrorKind::Conflict,
        format!("a transaction ({open}) is already open on database '{name}'"),
    )
}

/// The `409` answer to a non-transactional write while a transaction is open
/// on the target database. Writes never silently join someone's transaction.
fn txn_conflict(name: &str, tid: u64) -> Resp {
    txn_conflict_err(name, tid).rest()
}

/// [`txn_conflict`] as an [`OpError`].
fn txn_conflict_err(name: &str, tid: u64) -> OpError {
    OpError::new(
        OpErrorKind::Conflict,
        format!(
            "database '{name}' has an open transaction (TID {tid}); non-transactional writes \
             are rejected until it is committed or rolled back (send the statement to /tquery \
             with that tid instead)"
        ),
    )
}

/// Why a database / transaction operation shared by the REST endpoints and
/// the upstream `/api` layer ([`crate::gapi`]) was refused or failed. Each
/// front end renders it its own way: REST as an HTTP status ([`OpError::rest`]),
/// `/api` as an upstream `StatusCode` (see `crate::gapi::ops_query`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpError {
    pub(crate) kind: OpErrorKind,
    pub(crate) msg: String,
}

/// The class of an [`OpError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpErrorKind {
    /// The database is not (or no longer) loaded — REST `404`.
    NotLoaded,
    /// No such transaction — REST `404`.
    TxnUnknown,
    /// The transaction is no longer running — REST `409`.
    TxnEnded,
    /// The transaction belongs to another user — REST `403`.
    NotOwner,
    /// Refused because of an open transaction or a lifecycle operation in
    /// progress — REST `409`.
    Conflict,
    /// The server is shutting down — REST `503`.
    ShuttingDown,
    /// The SPARQL statement failed (syntax, evaluation, cancellation) — REST
    /// `400` with a `{"error":…}` body.
    Query,
    /// An internal failure (I/O, save, begin/commit) — REST `500`.
    Failed,
}

impl OpError {
    pub(crate) fn new(kind: OpErrorKind, msg: impl Into<String>) -> OpError {
        OpError {
            kind,
            msg: msg.into(),
        }
    }

    /// The REST endpoints' HTTP status for this error.
    fn http(&self) -> u16 {
        match self.kind {
            OpErrorKind::NotLoaded | OpErrorKind::TxnUnknown => 404,
            OpErrorKind::TxnEnded | OpErrorKind::Conflict => 409,
            OpErrorKind::NotOwner => 403,
            OpErrorKind::ShuttingDown => 503,
            OpErrorKind::Query => 400,
            OpErrorKind::Failed => 500,
        }
    }

    /// The REST rendering: `{"StatusCode":<http>,"StatusMsg":…}`, or for a
    /// failed SPARQL statement the SPARQL endpoints' `{"error":…}` body.
    fn rest(&self) -> Resp {
        match self.kind {
            OpErrorKind::Query => Resp::new(400, SPARQL_JSON, err_json(&self.msg)),
            _ => Resp::fail(self.http(), &self.msg),
        }
    }
}

/// What a [`QueryJob`] produced.
#[derive(Debug)]
pub(crate) enum JobOutput {
    /// A read (SELECT / ASK / CONSTRUCT / DESCRIBE) result.
    Read(QueryResult),
    /// An applied UPDATE: the triples it changed, and a warning when the
    /// check_point save after it failed (the change stays applied).
    Updated {
        changed: usize,
        warning: Option<String>,
    },
}

/// Mutable server state guarded by a single mutex. The registries here are all
/// cheap to touch, so this lock is only ever held briefly; the *databases* live
/// behind their own per-db locks ([`ApiState::dbs`]) so a long query holds only
/// that db's lock and leaves this one free — which is what lets a concurrent
/// `POST /cancel` reach the task registry while a query runs.
///
/// **Locking rule: never wait for a db lock while holding this lock.** A
/// request that needs a database clones its handle here, releases this lock,
/// and only then locks the database ([`Dispatch::Deferred`]), re-checking
/// under the db lock that the slot is still served. Holding a db lock, a thread may take this lock briefly (to
/// update a registry atomically with the slot) — that cannot deadlock, since
/// whoever holds this lock never waits for a db lock. So a slow query on one
/// database can never wedge this lock, `/cancel`, `/tasklist`, other
/// databases, or a shutdown's cancellation step. The upstream-compatible
/// `/api` layer ([`crate::gapi`]) follows the same rule: its operations run
/// with no lock held and take this one only briefly.
pub(crate) struct ApiState {
    pub(crate) root: PathBuf,
    /// The user table of the system store: users, passwords, and their
    /// per-database privileges (see [`crate::sysdb`]).
    pub(crate) users: UserStore,
    /// The database registry of the system store (creator, built time).
    pub(crate) sysdb: SysDb,
    /// Loaded, in-memory databases by name. Each is behind its own `Mutex` (and
    /// an `Arc` so a worker can clone the handle and run a query without holding
    /// the outer `ApiState` lock).
    pub(crate) dbs: HashMap<String, DbHandle>,
    /// Open transactions by (database, id). Transaction ids are numbered per
    /// loaded database (see [`DbSlot::next_tid`]), so the same id can be open
    /// on two databases at once.
    pub(crate) txns: HashMap<(String, u64), Txn>,
    /// The single open transaction per database (single-writer), if any.
    pub(crate) db_txn: HashMap<String, u64>,
    /// Recently finished transactions (database, `tid`, outcome), newest last,
    /// bounded by [`ENDED_TXN_MEMORY`].
    pub(crate) ended_txns: VecDeque<(String, u64, &'static str)>,
    pub(crate) started: Instant,
    pub(crate) requests: u64,
    /// The `LOAD` policy installed on every database this server serves
    /// (default [`LoadPolicy::restricted`]: SPARQL `LOAD` reads nothing).
    pub(crate) load_policy: LoadPolicy,
    /// The query-evaluation settings installed on every database this server
    /// serves ([`DbSlot::handle`]; `conf.ini`'s `[query]` section, default
    /// [`GStoreConfig::query_settings`] of the default configuration).
    pub(crate) query_settings: QuerySettings,
    /// Databases with a lifecycle operation (`/build`, `/load`, `/unload`,
    /// `/drop`, `/restore`, or an `/upload` that opens/creates the database)
    /// in progress. Those run with this lock released (they may wait for the
    /// db lock or do slow I/O), so this reservation keeps a second lifecycle
    /// operation — or a `/begin` — off the same name meanwhile. Released by
    /// [`BusyGuard`].
    pub(crate) busy: HashSet<String>,
    /// Settings of the upstream-compatible `/api` layer (see
    /// [`ApiServer::with_gapi_config`]).
    pub(crate) gapi: GapiConfig,
}

impl ApiState {
    /// Reserve `name` for a lifecycle operation, or the `409` refusal when
    /// another one is in progress.
    fn claim(&mut self, name: &str) -> Result<(), Resp> {
        if self.busy.insert(name.to_string()) {
            Ok(())
        } else {
            Err(Resp::fail(
                409,
                &format!(
                    "database '{name}' is busy: another build/load/unload/drop/restore of it is \
                     in progress"
                ),
            ))
        }
    }

    pub(crate) fn remember_ended(&mut self, db: &str, tid: u64, outcome: &'static str) {
        if self.ended_txns.len() >= ENDED_TXN_MEMORY {
            self.ended_txns.pop_front();
        }
        self.ended_txns.push_back((db.to_string(), tid, outcome));
    }

    /// How transaction `tid` of database `db` (any database when `None`)
    /// ended, if it is remembered.
    pub(crate) fn ended_outcome(&self, db: Option<&str>, tid: u64) -> Option<&'static str> {
        self.ended_txns
            .iter()
            .rev()
            .find(|(d, t, _)| *t == tid && db.is_none_or(|db| d == db))
            .map(|(_, _, o)| *o)
    }

    /// The database of open transaction `tid`: on `db` when named, else the
    /// one database where `tid` is open. Ids are numbered per database, so
    /// without a name an id is ambiguous when it is open on several databases
    /// — or open on one and recently ended on another: a client retrying the
    /// `/commit` of a transaction that already ended must not end another
    /// database's transaction of the same number.
    pub(crate) fn open_txn_db(&self, db: Option<&str>, tid: u64) -> Result<String, OpError> {
        if let Some(db) = db {
            if self.txns.contains_key(&(db.to_string(), tid)) {
                return Ok(db.to_string());
            }
        } else {
            let mut on: Vec<&String> = self
                .txns
                .keys()
                .filter(|(_, t)| *t == tid)
                .map(|(d, _)| d)
                .collect();
            on.sort();
            if let [one] = on.as_slice() {
                let ended_elsewhere = self
                    .ended_txns
                    .iter()
                    .find(|(d, t, _)| *t == tid && d != *one);
                if let Some((other, _, outcome)) = ended_elsewhere {
                    return Err(OpError::new(
                        OpErrorKind::Conflict,
                        format!(
                            "transaction {tid} is open on database '{one}' but transaction {tid} \
                             of database '{other}' was {outcome} recently: name the database \
                             (db_name)"
                        ),
                    ));
                }
            }
            match on.as_slice() {
                [one] => return Ok((*one).clone()),
                [] => {}
                many => {
                    let names: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
                    return Err(OpError::new(
                        OpErrorKind::Conflict,
                        format!(
                            "transaction {tid} is open on several databases ({}): name the \
                             database (db_name)",
                            names.join(", ")
                        ),
                    ));
                }
            }
        }
        Err(match self.ended_outcome(db, tid) {
            Some(outcome) => OpError::new(
                OpErrorKind::TxnEnded,
                format!("transaction {tid} is not running (already {outcome})"),
            ),
            None => OpError::new(OpErrorKind::TxnUnknown, format!("no open transaction {tid}")),
        })
    }

    /// The on-disk directory of database `name`.
    pub(crate) fn db_dir(&self, name: &str) -> PathBuf {
        self.root.join(crate::db_dir_for(name))
    }

    /// Whether `max` databases (`conf.ini`'s `max_database_num`) already
    /// exist — registered or loaded — so no new one may be built. The one
    /// count both `build`s use (the REST endpoint and the `/api` operation).
    pub(crate) fn database_limit_reached(&self, max: usize) -> bool {
        let mut names: HashSet<String> = self.sysdb.names().into_iter().collect();
        names.extend(self.dbs.keys().cloned());
        names.len() >= max
    }
}

/// The parity HTTP API server. Construct with [`ApiServer::bind`], then drive it
/// with [`serve_forever`](ApiServer::serve_forever) (e.g. in a background
/// thread) until [`shutdown`](ApiServer::shutdown). Connections are served by a
/// bounded worker pool with HTTP/1.1 keep-alive (see [`HttpOptions`] and
/// [`crate::server`]); configure it from `conf.ini` with
/// [`with_config`](ApiServer::with_config).
pub struct ApiServer {
    /// Shared with every connection worker (behind an `Arc`).
    inner: Arc<ApiInner>,
    /// The listening socket; taken (closed) once the server has shut down.
    listener: Mutex<Option<TcpListener>>,
    addr: SocketAddr,
    life: Arc<Lifecycle>,
    /// Worker pool, keep-alive, CORS and body-size settings.
    http: HttpOptions,
}

/// The shared, thread-safe part of an [`ApiServer`]: everything a connection
/// worker needs. Held behind an `Arc` so a worker only clones a pointer.
pub(crate) struct ApiInner {
    pub(crate) state: Mutex<ApiState>,
    /// The root directory (also in [`ApiState::root`]); kept here so the access
    /// log can be written even when a panicking handler poisoned the state lock.
    pub(crate) root: PathBuf,
    /// gStore's `check_point` policy: save a database after every successful
    /// write (update, batch, upload, commit). On by default.
    pub(crate) check_point: bool,
    /// Upper bound on how long a graceful shutdown waits for in-flight requests.
    grace: Duration,
    /// Set when a graceful shutdown begins. From then on every write is saved
    /// before it is acknowledged (whatever the check_point policy), and no new
    /// transaction may begin — so a write still in flight when the shutdown's
    /// final save runs cannot be acknowledged and then lost.
    pub(crate) closing: AtomicBool,
    /// The server's lifecycle (shared with [`ApiServer`]), so the `/shutdown`
    /// and `/restart` routes can stop it.
    life: Arc<Lifecycle>,
    /// The `conf.ini` settings this server runs with (built-in defaults unless
    /// [`ApiServer::with_config`] supplied a file's; `refreshconf` re-reads
    /// the file — see [`ApiInner::refresh_config`]). Read it through
    /// [`ApiInner::config`].
    config: RwLock<Arc<GStoreConfig>>,
    /// Password of the `system` user (`system_username`) for `/shutdown` and
    /// `/restart`: random per run unless set with
    /// [`ApiServer::with_system_password`]; `gserver` writes it to the second
    /// line of its pid file, as upstream does.
    system_password: String,
    /// The `/api` layer's IP allow / deny lists and failed-login lockout
    /// (from the configuration's `ip_allow_path` / `ip_deny_path`).
    pub(crate) ipfilter: IpFilter,
    /// The query / access / transaction log writers.
    pub(crate) apilogs: ApiLogs,
    /// Running SPARQL statements and asynchronous operations (`/tasklist`,
    /// `/cancel`, and the `/api` layer's `opt_id` operations). Its own lock,
    /// independent of the state lock: a cancellation never waits for it.
    /// Every statement is registered here — see
    /// [`register_task`](Self::register_task) — and cancelled through the
    /// flag it hands out (cloned into the [`QueryJob`] and threaded into
    /// [`Database::query_cancellable`], which polls it and aborts an
    /// in-flight read with [`GStoreError::Cancelled`](crate::GStoreError::Cancelled)).
    pub(crate) tasks: TaskManager,
    /// This value's own `Arc` (set when serving starts), so an asynchronous
    /// operation can outlive the request that submitted it.
    self_ref: OnceLock<Weak<ApiInner>>,
}

impl ApiInner {
    /// A weak handle on this value's `Arc`, once the server serves (`None`
    /// before [`ApiServer::serve_forever`]).
    pub(crate) fn weak_self(&self) -> Option<Weak<ApiInner>> {
        self.self_ref.get().cloned()
    }

    /// Whether a write must be saved before it is acknowledged: the
    /// check_point policy, or a shutdown in progress (see [`closing`](Self::closing)).
    pub(crate) fn save_writes(&self) -> bool {
        self.check_point || self.closing.load(Ordering::SeqCst)
    }

    /// The `conf.ini` settings in effect (e.g. `max_output_size`, the IP list
    /// paths, log/backup/upload settings) for the request handlers.
    pub(crate) fn config(&self) -> Arc<GStoreConfig> {
        Arc::clone(&self.config.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Re-read the configuration file the server was started from (upstream
    /// `refreshconf`, `APIUtil::refresh_conf`) and apply what a running
    /// server can change: the settings request handlers read (database and
    /// user limits, upload, backup and log settings, `system_username`, …),
    /// the IP allow / deny lists, and the `/api` layer's `max_output_size` and
    /// `licensetype`, and the `[query]` evaluation settings (applied to the
    /// loaded databases too). The address, worker pool, connection settings,
    /// `check_point` and the database home stay as they are until a restart.
    /// Without a file (a server configured in code) only the IP list files
    /// are re-read. Returns whether a file was re-read.
    pub(crate) fn refresh_config(&self) -> std::io::Result<bool> {
        let current = self.config();
        let Some(path) = current.source().map(Path::to_path_buf) else {
            self.ipfilter.reload();
            return Ok(false);
        };
        let mut fresh = GStoreConfig::load(&path)?;
        fresh.db_home = current.db_home.clone();
        self.ipfilter
            .reconfigure(fresh.ip_allow_path.clone(), fresh.ip_deny_path.clone());
        {
            let mut st = locked(&self.state);
            st.gapi.max_output_size = fresh.max_output_size;
            st.gapi.license_type = fresh.licensetype.clone();
        }
        self.apply_query_settings(fresh.query_settings());
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(fresh);
        Ok(true)
    }

    /// Install `settings` as the query-evaluation settings of every database
    /// this server serves: recorded for the databases it opens from now on
    /// ([`DbSlot::handle`]) and applied to those already loaded — each slot
    /// locked in turn with the state lock released (never both at once).
    pub(crate) fn apply_query_settings(&self, settings: QuerySettings) {
        let loaded: Vec<DbHandle> = {
            let mut st = locked(&self.state);
            st.query_settings = settings;
            st.dbs.values().map(Arc::clone).collect()
        };
        for h in loaded {
            let mut slot = locked(&h);
            if !slot.retired {
                slot.db.apply_query_settings(&settings);
            }
        }
    }

    /// The directory `/backup` writes into and `/restore` reads from:
    /// `conf.ini`'s `backup_path` when it is set, else `<root>/backups`. No
    /// database, log or user file lives there, so a client-chosen backup
    /// target inside it cannot clobber one.
    pub(crate) fn backup_dir(&self) -> PathBuf {
        let config = self.config();
        if config.is_set("backup_path") {
            config.backup_path.clone()
        } else {
            self.root.join("backups")
        }
    }

    /// This run's password of the `system` user (`conf.ini`'s
    /// `system_username`), accepted by `/shutdown` and `/restart`; see
    /// [`ApiServer::system_password`].
    pub(crate) fn system_password(&self) -> &str {
        &self.system_password
    }

    /// Whether a graceful shutdown has begun (requested, or already closing).
    pub(crate) fn stopping(&self) -> bool {
        self.life.stopping() || self.closing.load(Ordering::SeqCst)
    }

    /// Begin a graceful shutdown, exactly as on `SIGTERM`; with `restart`,
    /// flagged as a restart ([`ShutdownHandle::restart_requested`], on which
    /// `gserver` re-executes itself). Returns at once: the request asking for
    /// it is still in flight and is answered first.
    pub(crate) fn request_stop(&self, restart: bool) {
        if restart {
            self.life.request_restart();
        } else {
            self.life.request_plain_stop();
        }
    }

    /// Stop serving `name` (if loaded) *without* saving: take its handle out
    /// of the registry, then — with the state lock released — wait for any
    /// request still using it and retire the slot (see [`DbSlot::retire`]), so
    /// its files are closed before the caller replaces or deletes them and a
    /// request still holding the handle can neither use nor save it. Returns
    /// whether it was loaded. Must not be called with the state lock held.
    pub(crate) fn retire_loaded(&self, name: &str) -> bool {
        let handle = locked(&self.state).dbs.remove(name);
        match handle {
            Some(h) => {
                locked(&h).retire();
                true
            }
            None => false,
        }
    }
}

/// Releases a [`ApiState::busy`] reservation when dropped (also on a panic).
pub(crate) struct BusyGuard<'a> {
    pub(crate) inner: &'a ApiInner,
    pub(crate) name: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        locked(&self.inner.state).busy.remove(&self.name);
    }
}

/// A privilege-gated endpoint that touches a database: it validates the request
/// under the state lock and returns the db work as a [`Dispatch::Deferred`].
type Handler = fn(&ApiInner, &mut ApiState, &Request, &str) -> Dispatch;

/// The outcome of routing a request: either a fully-built response, or work
/// deferred to run *after* the [`ApiState`] lock is released — so a concurrent
/// `POST /cancel` can reach the task registry while it executes.
enum Dispatch {
    Done(Resp),
    /// A response whose (possibly db-lock-contended or slow) work runs with
    /// the `ApiState` lock released — the closure captures cloned
    /// [`DbHandle`]s and does the locking itself (see the locking rule on
    /// [`ApiState`]); it gets the server to take the state lock briefly when
    /// it must update a registry. Every endpoint that touches a database or
    /// does slow work (parsing a statement or payload, hashing a password)
    /// works this way, so neither a slow query holding one db's lock nor a big
    /// request can wedge the state lock and make `POST /cancel` undeliverable.
    Deferred(Box<dyn FnOnce(&ApiInner) -> Resp + Send>),
}

/// A query handed out of [`ApiInner::dispatch`] to run without the `ApiState`
/// lock held. It carries its own database handle and cancel flag.
pub(crate) struct QueryJob {
    /// The target database (its own lock, independent of `ApiState`).
    pub(crate) db: DbHandle,
    pub(crate) name: String,
    /// The task's flag in [`ApiInner::tasks`]; a concurrent `/cancel` (or
    /// `operationTaskCancel`) flips it and [`Database::query_cancellable`]
    /// observes it.
    pub(crate) cancel: Arc<AtomicBool>,
    /// The registry id to unregister once the query finishes.
    pub(crate) task_id: u64,
    pub(crate) sparql: String,
    /// Whether `sparql` is an UPDATE (classified by the parser at dispatch).
    pub(crate) is_update: bool,
    pub(crate) fmt: ResultFormat,
}

/// Ends a task when dropped — also when the query panics (then as failed) —
/// so the task registry never lists a dead statement as running. Record the
/// outcome with [`set_ok`](Self::set_ok) first.
pub(crate) struct TaskGuard<'a> {
    pub(crate) inner: &'a ApiInner,
    pub(crate) id: u64,
    pub(crate) ok: Option<bool>,
}

impl<'a> TaskGuard<'a> {
    pub(crate) fn new(inner: &'a ApiInner, id: u64) -> TaskGuard<'a> {
        TaskGuard {
            inner,
            id,
            ok: None,
        }
    }

    /// Whether the statement succeeded (a failure after a cancellation
    /// request counts as cancelled).
    pub(crate) fn set_ok(&mut self, ok: bool) {
        self.ok = Some(ok);
    }
}

impl Drop for TaskGuard<'_> {
    fn drop(&mut self) {
        self.inner.tasks.finish(self.id, self.ok);
    }
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
        // The system store: `users.db` (users, per-database privileges) and
        // `system.json` (the database registry), migrated on first start.
        let (users, sysdb) = crate::sysdb::open_system(&root, root_password)?;
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let life = Lifecycle::new(addr);
        Ok(ApiServer {
            inner: Arc::new(ApiInner {
                state: Mutex::new(ApiState {
                    root: root.clone(),
                    users,
                    sysdb,
                    dbs: HashMap::new(),
                    txns: HashMap::new(),
                    db_txn: HashMap::new(),
                    ended_txns: VecDeque::new(),
                    started: Instant::now(),
                    requests: 0,
                    load_policy: LoadPolicy::restricted(),
                    query_settings: GStoreConfig::default().query_settings(),
                    busy: HashSet::new(),
                    gapi: GapiConfig::default(),
                }),
                root,
                check_point: true,
                grace: DEFAULT_SHUTDOWN_GRACE,
                closing: AtomicBool::new(false),
                life: Arc::clone(&life),
                ipfilter: IpFilter::from_config(&GStoreConfig::default()),
                apilogs: ApiLogs::new(),
                config: RwLock::new(Arc::new(GStoreConfig::default())),
                system_password: new_system_password(),
                tasks: TaskManager::default(),
                self_ref: OnceLock::new(),
            }),
            listener: Mutex::new(Some(listener)),
            addr,
            life,
            http: HttpOptions::default(),
        })
    }

    /// Bind as `gserver` does from a [`GStoreConfig`]: databases under
    /// `config.db_home`, the root user seeded with `config.root_password`,
    /// listening on `config.listen_addr:config.port` — then
    /// [`with_config`](Self::with_config).
    pub fn bind_with_config(config: GStoreConfig) -> std::io::Result<ApiServer> {
        let addr = (config.listen_addr.clone(), config.port);
        let server = ApiServer::bind(&config.db_home, &config.root_password, addr)?;
        Ok(server.with_config(config))
    }

    /// Run with `conf.ini` settings: the worker pool (`thread_num`,
    /// `queue_size`), keep-alive, CORS and request body cap
    /// (`upload_max_body_size`) via [`GStoreConfig::http_options`], the
    /// `check_point` policy, the `system_username` accepted by `/shutdown`
    /// and `/restart`, the `/api` layer's `max_output_size` and
    /// `licensetype` ([`GapiConfig`]), and the `[query]` section's evaluation
    /// settings ([`GStoreConfig::query_settings`], see
    /// [`with_query_settings`](Self::with_query_settings)). The whole configuration stays readable
    /// by the request handlers (e.g. the IP list paths). `root_password` and
    /// `db_home` are [`bind`](Self::bind)'s arguments (or use
    /// [`bind_with_config`](Self::bind_with_config)). Apply other builders
    /// (`with_check_point`, `with_http_options`, `with_max_output_size`,
    /// `with_gapi_config`) after this one to override it.
    pub fn with_config(mut self, config: GStoreConfig) -> ApiServer {
        self.http = config.http_options();
        {
            let mut st = locked(&self.inner.state);
            st.gapi.max_output_size = config.max_output_size;
            st.gapi.license_type = config.licensetype.clone();
        }
        self.inner.apply_query_settings(config.query_settings());
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.check_point = config.check_point;
            inner.ipfilter = IpFilter::from_config(&config);
            *inner.config.get_mut().unwrap_or_else(PoisonError::into_inner) = Arc::new(config);
        }
        self
    }

    /// The `conf.ini` settings this server runs with.
    pub fn config(&self) -> Arc<GStoreConfig> {
        self.inner.config()
    }

    /// Replace the connection settings (worker-pool size, keep-alive, CORS,
    /// timeouts, body cap; default [`HttpOptions::default`]).
    pub fn with_http_options(mut self, opts: HttpOptions) -> ApiServer {
        self.http = opts;
        self
    }

    /// The connection settings in effect.
    pub fn http_options(&self) -> &HttpOptions {
        &self.http
    }

    /// Fix the `system` user's password for `/shutdown` and `/restart`
    /// (default: random per run, see [`system_password`](Self::system_password)).
    pub fn with_system_password(mut self, password: &str) -> ApiServer {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.system_password = password.to_string();
        }
        self
    }

    /// The password the `system` user (`conf.ini`'s `system_username`) must
    /// present to `/shutdown` or `/restart`. Upstream writes it to the second
    /// line of the pid file; `gserver` does the same.
    pub fn system_password(&self) -> &str {
        &self.inner.system_password
    }

    /// Turn gStore's `check_point` policy on (the default) or off. When on,
    /// every successful write (update, batch, upload, commit) is saved to the
    /// database's directory before it is acknowledged. When off, changes stay
    /// in memory until `/checkpoint`, `/unload`, or a graceful shutdown.
    pub fn with_check_point(mut self, on: bool) -> ApiServer {
        // Builders run right after `bind`, before the `Arc` is shared with any
        // worker thread, so `get_mut` succeeds.
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.check_point = on;
        }
        self
    }

    /// The SPARQL `LOAD` policy for every database this server serves. The
    /// default, [`LoadPolicy::restricted`], lets `LOAD` read nothing — clients
    /// must not be able to make the server read arbitrary local files;
    /// re-allow a directory with [`LoadPolicy::with_allowed_dir`]. The same
    /// policy governs `/build?db_path=` and the `/restore` sources outside the
    /// backup directory, and its `allow_remote` whether federated `SERVICE`
    /// patterns may contact remote endpoints (not by default).
    pub fn with_load_policy(self, policy: LoadPolicy) -> ApiServer {
        locked(&self.inner.state).load_policy = policy;
        self
    }

    /// The query-evaluation settings of every database this server serves —
    /// those it loads, builds, creates or restores from now on, and those
    /// already loaded: the integer-division mode, the query-cache budget, the
    /// Top-K / early-termination options and the graph-function output bound
    /// ([`Database::apply_query_settings`]). [`with_config`](Self::with_config)
    /// sets them from `conf.ini`'s `[query]` section; the default is that of
    /// an empty `conf.ini` ([`GStoreConfig::query_settings`]).
    pub fn with_query_settings(self, settings: QuerySettings) -> ApiServer {
        self.inner.apply_query_settings(settings);
        self
    }

    /// The query-evaluation settings in effect (see
    /// [`with_query_settings`](Self::with_query_settings)).
    pub fn query_settings(&self) -> QuerySettings {
        locked(&self.inner.state).query_settings
    }

    /// Bound the time a graceful shutdown waits for in-flight requests before
    /// it cancels running queries, saves, and returns (default
    /// [`DEFAULT_SHUTDOWN_GRACE`]).
    pub fn with_shutdown_grace(mut self, grace: Duration) -> ApiServer {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.grace = grace;
        }
        self
    }

    /// Replace the settings of the upstream-compatible `/api` layer
    /// ([`crate::gapi`]): the query output cap (`max_output_size`), the
    /// version/licence strings it reports, where `format=file` results go, …
    /// May also be changed later, while serving, with
    /// [`set_gapi_config`](Self::set_gapi_config).
    pub fn with_gapi_config(self, config: GapiConfig) -> ApiServer {
        self.set_gapi_config(config);
        self
    }

    /// Cap the rows an `/api` `query` returns (upstream's `max_output_size`,
    /// default [`crate::gapi::DEFAULT_MAX_OUTPUT_SIZE`]); see
    /// [`GapiConfig::max_output_size`].
    pub fn with_max_output_size(self, rows: usize) -> ApiServer {
        locked(&self.inner.state).gapi.max_output_size = rows;
        self
    }

    /// The current `/api` layer settings.
    pub fn gapi_config(&self) -> GapiConfig {
        locked(&self.inner.state).gapi.clone()
    }

    /// Replace the `/api` layer settings while serving (e.g. after a
    /// configuration reload); requests that already started keep the old ones.
    pub fn set_gapi_config(&self, config: GapiConfig) {
        locked(&self.inner.state).gapi = config;
    }

    /// Whether the check_point policy (save after every write) is on.
    pub fn check_point(&self) -> bool {
        self.inner.check_point
    }

    /// The actual bound address (useful after binding to port 0).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.addr)
    }

    /// The root directory holding the databases, logs and user store.
    pub fn root_dir(&self) -> &Path {
        &self.inner.root
    }

    /// A cloneable handle that can stop this server from any thread (or from a
    /// signal watcher, see [`crate::server::shutdown_on_signals`]).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle::new(Arc::clone(&self.life))
    }

    /// Request a graceful shutdown: stop accepting, let in-flight requests
    /// finish (bounded by the grace period, after which running queries are
    /// cancelled), roll back open transactions, save every database with
    /// unsaved changes, and return from [`serve_forever`](Self::serve_forever).
    /// Returns immediately; join the serving thread to wait for completion.
    pub fn shutdown(&self) {
        self.life.request_plain_stop();
    }

    /// The live cancel flag for an in-flight task, if it still exists. The
    /// engine's cooperative-cancellation hook ([`Database::query_cancellable`])
    /// already consults this flag: `prepare_query` clones it into the [`QueryJob`]
    /// that runs the query, `POST /cancel` sets it, and `/tasklist` reports it.
    /// This accessor lets an embedder reach the same flag directly.
    pub fn task_cancel_flag(&self, task_id: u64) -> Option<Arc<AtomicBool>> {
        self.inner.tasks.cancel_flag(task_id)
    }

    /// Replace the background task manager's settings (asynchronous
    /// operation runners, result retention, callback delivery, cancellation
    /// and shutdown waits; default [`TaskConfig::default`]).
    pub fn with_task_config(self, config: TaskConfig) -> ApiServer {
        self.inner.tasks.set_config(config);
        self
    }

    /// The background task manager: running SPARQL statements (`/tasklist`,
    /// `operationTaskList`) and asynchronous operations (`opt_id`,
    /// `checkOperationState`). An embedder may submit its own background
    /// work through it ([`TaskManager::submit`]).
    pub fn task_manager(&self) -> &TaskManager {
        &self.inner.tasks
    }

    /// Accept connections until [`shutdown`](Self::shutdown) (or `/shutdown`,
    /// `/restart`), serving them on the bounded worker pool with keep-alive
    /// (see [`HttpOptions`]). A long-running query on one connection holds only
    /// its database's lock, so concurrent requests — crucially `POST /cancel` —
    /// proceed on other workers. Returns what the shutdown did.
    pub fn serve_forever(&self) -> ShutdownReport {
        let _ = self.inner.self_ref.set(Arc::downgrade(&self.inner));
        let drained = run_accept_loop(&self.listener, &self.life, self.inner.grace, &self.http, |acc| {
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

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS in-process
    /// using `config` (see [`crate::server::load_tls_config`]). Requires the
    /// `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        let _ = self.inner.self_ref.set(Arc::downgrade(&self.inner));
        let drained = run_accept_loop(&self.listener, &self.life, self.inner.grace, &self.http, |acc| {
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

    /// The tail of a graceful shutdown, after the listener closed and in-flight
    /// requests had their grace period: cancel queries that are still running,
    /// roll back open transactions (uncommitted work is never persisted), save
    /// every database with unsaved changes, and retire every slot so a request
    /// that is still queued on one can no longer apply a write nobody would
    /// save. From the start, every write still in flight saves itself before it
    /// is acknowledged (see [`ApiInner::closing`]).
    fn finish_shutdown(&self, mut drained: bool) -> ShutdownReport {
        let inner = &self.inner;
        inner.closing.store(true, Ordering::SeqCst);
        inner.tasks.close();
        if !drained {
            inner.tasks.cancel_all();
            drained = self.life.wait_idle(Duration::from_secs(2));
        }
        // Asynchronous operations are not requests: they had no grace period
        // of their own, so ask them to stop now and wait for them, bounded
        // (`TaskConfig::shutdown_wait`) — one that ignores the request keeps
        // running past this point, against databases about to be retired.
        inner.tasks.cancel_all();
        let _ = inner.tasks.wait_async_idle(inner.tasks.config().shutdown_wait);
        let mut report = ShutdownReport {
            drained,
            ..ShutdownReport::default()
        };
        // Clone the handles, then lock each database with the state lock
        // released (see the locking rule on `ApiState`).
        let mut handles: Vec<(String, DbHandle)> = locked(&inner.state)
            .dbs
            .iter()
            .map(|(n, h)| (n.clone(), Arc::clone(h)))
            .collect();
        handles.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, h) in handles {
            let mut slot = locked(&h);
            if slot.retired {
                continue;
            }
            if let Some(open) = slot.txn.take() {
                let _ = slot.db.rollback();
                let mut st = locked(&inner.state);
                let owner = st
                    .txns
                    .remove(&(name.clone(), open.tid))
                    .map(|t| t.owner)
                    .unwrap_or_default();
                st.db_txn.remove(&name);
                st.remember_ended(&name, open.tid, "rolled back");
                append_log(
                    &st.root,
                    "txn.log",
                    &format!("{}\trollback\t{}\t{}\t(shutdown)", now_millis(), name, open.tid),
                );
                inner
                    .apilogs
                    .txn_end(&st.root, &name, open.tid, &owner, TxnState::RolledBack);
                report.rolled_back += 1;
            }
            if slot.dirty {
                let dir = slot.dir.clone();
                match slot.save() {
                    Ok(()) => report.saved.push(dir),
                    Err(e) => report.failed.push((dir, e.to_string())),
                }
            }
            slot.retire();
        }
        report
    }
}

impl ApiInner {
    /// Handle one request read by [`serve_connection`] and write its response
    /// (adding the connection's [`ConnMeta`] headers: keep-alive, CORS).
    /// Generic over the transport so the same path serves plain TCP and
    /// `rustls` TLS streams. Everything between parsing and writing runs under
    /// `catch_unwind`: a panicking handler answers `500` (JSON) and the locks it
    /// poisoned are recovered by [`locked`], so the server keeps serving.
    /// Returns whether the response honoured keep-alive.
    fn handle<S: Read + Write>(
        &self,
        stream: &mut S,
        req: Result<Request, BadRequest>,
        meta: &ConnMeta,
    ) -> std::io::Result<bool> {
        let mut req = match req {
            Ok(r) => r,
            Err(bad) => {
                let body = err_json(bad.message);
                write_response_ext(stream, bad.status, JSON, &meta.headers(&[]), &body)?;
                return Ok(false);
            }
        };
        // Upstream's reading of an `/api` request target (an encoded `?`, a
        // trailing `/`).
        crate::gapi::normalize_target(&mut req);
        // The TCP peer of this connection, recorded by `serve_connection` for
        // the worker thread (the upstream `/api` layer's remote IP).
        let peer = crate::server::current_peer();

        let (resp, user_disp) = catch_unwind(AssertUnwindSafe(|| self.respond(&req, peer)))
            .unwrap_or_else(|_| {
                // The upstream protocol answers every outcome with HTTP 200.
                let resp = if crate::gapi::wants(&req) {
                    Resp::from(crate::gapi::GResp::error(
                        crate::gapi::Status::OperationFailed,
                        PANIC_MSG,
                    ))
                } else {
                    Resp::fail(500, PANIC_MSG)
                };
                (resp, "-".to_string())
            });

        // Access log. Written off-lock (O_APPEND keeps each line atomic); strict
        // request ordering is neither achievable nor needed once connections run
        // concurrently.
        append_log(
            &self.root,
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

        let enc = req.header("accept-encoding");
        let extra: Vec<(&str, &str)> = resp
            .extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let headers = meta.headers(&extra);
        write_response_maybe_gzip(stream, resp.status, &resp.ctype, &headers, &resp.body, enc)?;
        Ok(true)
    }

    /// Route and execute one request, returning the response and the caller's
    /// display name for the access log.
    fn respond(&self, req: &Request, peer: Option<SocketAddr>) -> (Resp, String) {
        // The upstream-compatible `/api` protocol (`/api`, `POST /`,
        // `GET /?operation=…`, its path routes): per-request credentials and a
        // JSON envelope, served by `crate::gapi` with no lock held.
        if crate::gapi::wants(req) {
            let (resp, user_disp) = crate::gapi::serve(self, req, peer);
            return (Resp::from(resp), user_disp);
        }
        // The IP checks of the `/api` layer hold for every endpoint on this
        // listener: the allow / deny lists, and the failed-login lockout for
        // everything but `/login` (as `/api` exempts its `login`).
        let ip = peer_ip(peer);
        if !self.ipfilter.permits(&ip) {
            return (Resp::fail(403, crate::gapi::ipfilter::BLOCKED_MSG), "-".to_string());
        }
        if req.path != "/login" && self.ipfilter.locked_out(&ip) {
            return (Resp::fail(403, crate::gapi::ipfilter::LOCKED_MSG), "-".to_string());
        }
        // Basic credentials are checked with the state lock held only to look
        // the user up: the deliberately slow password hash runs with no lock
        // held, so a client sending bad credentials (no account needed) cannot
        // stall every other request — `/cancel` included — behind it.
        let basic = parse_basic_auth(req).map(|(u, p)| {
            let attempt = locked(&self.state).users.begin_check(&u, &p, PasswordForm::Plain);
            (u, attempt.verify())
        });
        // Short critical section: count the request, resolve the caller, route.
        // An endpoint with slow work (parsing, a query, I/O) returns
        // `Dispatch::Deferred` so it runs below with the `ApiState` lock
        // released.
        let (dispatch, user_disp) = {
            let mut st = locked(&self.state);
            st.requests += 1;
            let user = self.resolve_user(&mut st, req, basic);
            let user_disp = user.clone().unwrap_or_else(|| "-".to_string());
            let dispatch = self.dispatch(&mut st, req, user);
            (dispatch, user_disp)
        };

        // Run any deferred work with NO `ApiState` lock held: this is what
        // makes a concurrent `POST /cancel` (which needs that lock) deliverable
        // while a query runs.
        let resp = match dispatch {
            Dispatch::Done(r) => r,
            Dispatch::Deferred(f) => f(self),
        };
        (resp, user_disp)
    }

    /// Run a deferred [`QueryJob`] and render the outcome for the REST
    /// endpoints (see [`run_query_job`](Self::run_query_job)).
    fn execute_query_job(&self, job: QueryJob) -> Resp {
        let fmt = job.fmt;
        match self.run_query_job(job) {
            Ok(JobOutput::Read(result)) => render_result(fmt, &result),
            Ok(JobOutput::Updated { changed, warning }) => update_resp(changed, &warning),
            Err(e) => e.rest(),
        }
    }

    /// Run a [`QueryJob`] with only the target database's lock held (the
    /// `ApiState` lock is free), then unregister the task. Shared by the REST
    /// endpoints and the upstream `/api` layer ([`crate::gapi`]).
    ///
    /// The open-transaction check happens here, under the db lock, so it cannot
    /// race a concurrent `/begin`: a write is rejected while a transaction is
    /// open, and a read is answered from the transaction's begin-time snapshot
    /// (released from the db lock first, so it does not stall the owner's
    /// `/tquery`s). A write that changed data marks the database dirty and is
    /// saved before the lock is released under the check_point policy.
    pub(crate) fn run_query_job(&self, job: QueryJob) -> Result<JobOutput, OpError> {
        // Ends the task (also on a panic) once the job — and so its db lock —
        // is done.
        let mut task = TaskGuard::new(self, job.task_id);
        let out = self.run_query_job_locked(job);
        task.set_ok(out.is_ok());
        out
    }

    fn run_query_job_locked(&self, job: QueryJob) -> Result<JobOutput, OpError> {
        let mut slot = locked(&job.db);
        #[cfg(test)]
        if job.sparql.contains("__gstore_test_panic__") {
            panic!("test-triggered panic inside a query job");
        }
        if slot.retired {
            return Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{}' was unloaded while this request waited", job.name),
            ));
        }
        if let Some(open) = &slot.txn {
            if job.is_update {
                return Err(txn_conflict_err(&job.name, open.tid));
            }
            let snapshot = Arc::clone(&open.snapshot);
            drop(slot);
            return snapshot
                .query(&job.sparql, Some(job.cancel))
                .map(JobOutput::Read)
                .map_err(|e| OpError::new(OpErrorKind::Query, e.to_string()));
        }
        let before = slot.db.mutation_count();
        let result = slot.db.query_cancellable(&job.sparql, job.cancel);
        // Whether data changed — also by an UPDATE that failed part-way: the
        // operations before the failing one stay applied, so the database is
        // dirty and (under check_point) saved like after any other write.
        let wrote = slot.db.mutation_count() != before;
        match result {
            Ok(QueryResult::Update { changed }) => {
                let warning = if wrote {
                    slot.wrote(self.save_writes())
                } else {
                    None
                };
                Ok(JobOutput::Updated { changed, warning })
            }
            Ok(result) => {
                drop(slot);
                Ok(JobOutput::Read(result))
            }
            Err(e) => {
                let msg = if wrote {
                    partial_update_msg(&e, slot.wrote(self.save_writes()))
                } else {
                    e.to_string()
                };
                Err(OpError::new(OpErrorKind::Query, msg))
            }
        }
    }

    /// Resolve the caller's identity from Basic auth (`basic`: the user and
    /// the already-[`verify`](crate::http_users::CredentialAttempt::verify)ed
    /// check of the request's Basic credentials) or a session token.
    fn resolve_user(
        &self,
        st: &mut ApiState,
        req: &Request,
        basic: Option<(String, CheckedCredentials)>,
    ) -> Option<String> {
        if let Some((u, checked)) = basic {
            // Also migrates a legacy password hash on first successful use,
            // so Basic-auth-only clients get upgraded like `/login` users.
            if st.users.finish_check(checked) == CredentialCheck::Valid {
                return Some(u);
            }
            // Counts towards the address's lockout, as a failed `/api` login.
            self.ipfilter.note_failure(&peer_ip(crate::server::current_peer()));
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

    /// Run `f` only if `user` may perform upstream operation `op` (a
    /// database-independent one — user management, logs, …: `root` and
    /// `admin` users only), else `403`. See [`crate::sysdb::allowed`].
    fn guarded(
        &self,
        st: &mut ApiState,
        user: &str,
        op: &str,
        f: impl FnOnce(&Self, &mut ApiState) -> Resp,
    ) -> Resp {
        if crate::sysdb::allowed(&st.users, user, op, None) {
            f(self, st)
        } else {
            self.forbidden()
        }
    }

    /// The privilege check of a REST endpoint performing upstream operation
    /// `op`: the rules of the `/api` layer ([`crate::sysdb::allowed`]) on the
    /// database the endpoint's handler targets, resolved here exactly as the
    /// handler resolves it (under the same state-lock hold). When it cannot be
    /// resolved the request is let through, and the handler refuses it the
    /// same way for everyone (`400` missing database, `404` unknown
    /// transaction, …). `build` and `drop` are reserved to `root` (and
    /// `admin` users), as upstream; `/upload` into an existing database needs
    /// the update privilege on it, and creating one by uploading counts as a
    /// `build`.
    fn rest_allowed(&self, st: &ApiState, req: &Request, user: &str, op: &str) -> bool {
        let target = match op {
            "load" | "unload" | "restore" | "build" | "drop" => self.req_db_name(req),
            "upload" => {
                let Some(name) = self.req_db_name(req) else {
                    return true;
                };
                let exists = st.dbs.contains_key(&name) || st.db_dir(&name).is_dir();
                let op = if exists { "update" } else { "build" };
                return crate::sysdb::allowed(&st.users, user, op, Some(&name));
            }
            "commit" | "rollback" => self
                .txn_id(req)
                .and_then(|tid| st.open_txn_db(self.req_db_name(req).as_deref(), tid).ok()),
            _ => self.pick_db(st, req),
        };
        match target {
            Some(db) => crate::sysdb::allowed(&st.users, user, op, Some(&db)),
            // Root-only operations are decided without a database.
            None if crate::sysdb::requirement(op) == crate::sysdb::Requirement::RootOnly => {
                crate::sysdb::allowed(&st.users, user, op, None)
            }
            None => true,
        }
    }

    fn dispatch(&self, st: &mut ApiState, req: &Request, user: Option<String>) -> Dispatch {
        let method = req.method.as_str();
        let path = req.path.as_str();

        // `/login`, and the static web console, are the only fully public
        // endpoints — the console itself only ever *reads* by calling back
        // into the (still auth-gated) `/sparql` from the browser.
        if path == "/login" {
            return self.handle_login(req);
        }
        if path == "/logout" {
            if let Some(tok) = req.bearer_or_session_token() {
                st.users.logout(&tok);
            }
            return Dispatch::Done(Resp::ok("logged out"));
        }
        if method == "GET" && (path == "/" || path == "/console") {
            return Dispatch::Done(Resp::new(
                200,
                "text/html; charset=utf-8",
                CONSOLE_HTML.as_bytes().to_vec(),
            ));
        }

        let user = match user {
            Some(u) => u,
            None => return Dispatch::Done(self.unauthorized()),
        };

        // Query endpoints defer classification and execution
        // (`Dispatch::Deferred`) so they run with the `ApiState` lock released.
        match (method, path) {
            ("GET" | "POST", "/sparql") => return self.handle_query(req, &user),
            ("POST", "/update") => return self.handle_update(st, req, &user),
            ("POST", "/tquery") => return self.handle_tquery(st, req, &user),
            // Read-only monitoring endpoints defer their db reads off the state
            // lock (see Dispatch::Deferred) so they never wedge POST /cancel.
            // Server-wide figures are `stat`'s (root and admin users only);
            // the per-database listings show only the databases the caller
            // may query, as `/api`'s `show` and `monitor`.
            ("GET", "/status") => {
                if !crate::sysdb::allowed(&st.users, &user, "stat", None) {
                    return Dispatch::Done(self.forbidden());
                }
                return self.handle_status(st);
            }
            ("GET", "/monitor") => return self.handle_monitor(st, &user),
            ("GET" | "POST", "/show") => return self.handle_show(st, &user),
            _ => {}
        }

        // Endpoints that touch a database run their db work deferred (with the
        // state lock released, see the locking rule on `ApiState`).
        // Each is checked as the upstream operation it performs (see
        // `rest_allowed`), on its target database.
        let deferred = match (method, path) {
            ("POST", "/build") => Some(("build", Self::handle_build as Handler)),
            ("POST", "/load") => Some(("load", Self::handle_load as Handler)),
            ("POST", "/upload") => Some(("upload", Self::handle_upload as Handler)),
            ("POST", "/unload") => Some(("unload", Self::handle_unload as Handler)),
            ("POST", "/drop") => Some(("drop", Self::handle_drop as Handler)),
            ("POST", "/begin") => Some(("begin", Self::handle_begin as Handler)),
            ("POST", "/commit") => Some(("commit", Self::handle_commit as Handler)),
            ("POST", "/rollback") => Some(("rollback", Self::handle_rollback as Handler)),
            ("POST", "/checkpoint") => Some(("checkpoint", Self::handle_checkpoint as Handler)),
            ("POST", "/backup") => Some(("backup", Self::handle_backup as Handler)),
            ("POST", "/restore") => Some(("restore", Self::handle_restore as Handler)),
            ("GET" | "POST", "/export") => Some(("export", Self::handle_export as Handler)),
            ("POST", "/batchInsert") => {
                Some(("batchInsert", Self::handle_batch_insert as Handler))
            }
            ("POST", "/batchRemove") => {
                Some(("batchRemove", Self::handle_batch_remove as Handler))
            }
            _ => None,
        };
        if let Some((op, handler)) = deferred {
            return if self.rest_allowed(st, req, &user, op) {
                handler(self, st, req, &user)
            } else {
                Dispatch::Done(self.forbidden())
            };
        }

        let resp = match (method, path) {
            ("POST", "/user/create") => self.guarded(st, &user, "usermanage", |s, st| {
                s.handle_user_create(st, req)
            }),
            ("POST", "/user/drop") => self.guarded(st, &user, "usermanage", |s, st| {
                s.handle_user_drop(st, req)
            }),
            ("POST", "/user/password") => self.handle_user_password(st, req, &user),
            ("POST", "/user/grant") => self.guarded(st, &user, "userprivilegemanage", |s, st| {
                s.handle_user_grant(st, req, true)
            }),
            ("POST", "/user/revoke") => self.guarded(st, &user, "userprivilegemanage", |s, st| {
                s.handle_user_grant(st, req, false)
            }),
            ("GET", "/user/show") => self.guarded(st, &user, "showuser", |s, st| {
                s.handle_user_show(st)
            }),
            ("GET", "/querylog") => self.guarded(st, &user, "querylog", |s, st| {
                s.read_log(st, "query.log")
            }),
            ("GET", "/accesslog") => self.guarded(st, &user, "accesslog", |s, st| {
                s.read_log(st, "access.log")
            }),
            ("GET", "/txnlog") => self.guarded(st, &user, "txnlog", |s, st| {
                s.read_log(st, "txn.log")
            }),
            ("GET", "/tasklist") => self.guarded(st, &user, "tasklist", |s, _| {
                s.handle_tasklist()
            }),
            ("POST", "/cancel") => {
                // Either privilege can cancel: Update on some database (only
                // your *own* long-running work) or Admin (an operator
                // cancelling anyone's).
                let is_admin = st.users.is_superuser(&user);
                if is_admin || st.users.has_priv_anywhere(&user, Privilege::Update) {
                    self.handle_cancel(req, &user, is_admin)
                } else {
                    self.forbidden()
                }
            }
            // Test-only trigger for the panic-isolation tests: panics while
            // holding the state lock (and, given `db_name`, that db's lock),
            // poisoning both.
            #[cfg(test)]
            ("GET", "/__test/panic") => {
                let held = req.param("db_name").and_then(|n| st.dbs.get(&n).cloned());
                let _guard = held.as_ref().map(|h| locked(h));
                panic!("test-triggered handler panic");
            }
            _ => Resp::fail(404, "not found"),
        };
        Dispatch::Done(resp)
    }

    // ---- auth -------------------------------------------------------------

    /// `/login`: verify the credentials — the slow password hash with the
    /// state lock released (see [`UserStore::begin_check`]) — and issue a
    /// session token.
    fn handle_login(&self, req: &Request) -> Dispatch {
        let (Some(username), Some(password)) = (req.param("username"), req.param("password")) else {
            return Dispatch::Done(Resp::fail(400, "missing username or password"));
        };
        Dispatch::Deferred(Box::new(move |inner| {
            let attempt =
                locked(&inner.state).users.begin_check(&username, &password, PasswordForm::Plain);
            let checked = attempt.verify();
            let ip = peer_ip(crate::server::current_peer());
            let mut st = locked(&inner.state);
            if st.users.finish_check(checked) != CredentialCheck::Valid {
                inner.ipfilter.note_failure(&ip);
                return Resp::fail(401, "invalid username or password");
            }
            // As `/api`'s `login`: a successful login clears the address's
            // failed-login count.
            inner.ipfilter.reset(&ip);
            let token = st.users.start_session(&username);
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
        }))
    }

    // ---- query / update ---------------------------------------------------

    /// `/sparql`: a read endpoint, where an UPDATE-shaped statement
    /// additionally needs the Update privilege. Classifying the statement means
    /// parsing it — possibly megabytes of text — so that happens with the state
    /// lock released ([`Dispatch::Deferred`]); the lock is then taken briefly
    /// to check the privilege and register the task.
    fn handle_query(&self, req: &Request, user: &str) -> Dispatch {
        let Some(sparql) = req.param("sparql").or_else(|| req.query_string()) else {
            return Dispatch::Done(Resp::fail(400, "missing sparql/query parameter"));
        };
        let db = db_param(req);
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let is_update = is_update_sparql(&sparql);
            let op = if is_update { "update" } else { "query" };
            let name = {
                let st = locked(&inner.state);
                let Some(name) = pick_db_name(&st, db) else {
                    return Resp::fail(400, "missing or ambiguous db parameter");
                };
                if !crate::sysdb::allowed(&st.users, &user, op, Some(&name)) {
                    return inner.forbidden();
                }
                name
            };
            log_query(&inner.root, &name, &user, &sparql);
            let prepared = {
                let mut st = locked(&inner.state);
                inner.prepare_query(&mut st, &name, sparql, is_update, fmt, &user)
            };
            let job = match prepared {
                Ok(job) => job,
                Err(r) => return r,
            };
            inner.execute_query_job(job)
        }))
    }

    /// `/update` (the caller holds the Update privilege): like
    /// [`handle_query`](Self::handle_query), the statement is classified with
    /// the state lock released.
    fn handle_update(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(sparql) = req
            .param("sparql")
            .or_else(|| req.param("update"))
            .or_else(|| Some(req.body_str()).filter(|s| !s.trim().is_empty()))
        else {
            return Dispatch::Done(Resp::fail(400, "missing update statement"));
        };
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        if !crate::sysdb::allowed(&st.users, user, "update", Some(&name)) {
            return Dispatch::Done(self.forbidden());
        }
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let is_update = is_update_sparql(&sparql);
            log_query(&inner.root, &name, &user, &sparql);
            let prepared = {
                let mut st = locked(&inner.state);
                inner.prepare_query(&mut st, &name, sparql, is_update, fmt, &user)
            };
            let job = match prepared {
                Ok(job) => job,
                Err(r) => return r,
            };
            inner.execute_query_job(job)
        }))
    }

    /// Set up a SPARQL request against loaded database `name`: register the
    /// task, and clone the target database handle + cancel flag into a
    /// [`QueryJob`] that
    /// [`execute_query_job`](Self::execute_query_job) runs *without* the
    /// `ApiState` lock held (letting a concurrent `POST /cancel` reach the
    /// task). Called with the state lock held (briefly: nothing here is slow).
    fn prepare_query(
        &self,
        st: &mut ApiState,
        name: &str,
        sparql: String,
        is_update: bool,
        fmt: ResultFormat,
        user: &str,
    ) -> Result<QueryJob, Resp> {
        // Clone the target database handle — it has its own lock, independent of
        // the `ApiState` lock, so the query runs without blocking `/cancel`.
        let Some(db) = st.dbs.get(name).map(Arc::clone) else {
            return Err(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        let (task_id, cancel) = self.register_task(st, name, &sparql, user);
        Ok(QueryJob {
            db,
            name: name.to_string(),
            cancel,
            task_id,
            sparql,
            is_update,
            fmt,
        })
    }

    // ---- task registry / cancellation --------------------------------------

    /// Register a new in-flight SPARQL task for `db`/`sparql`/`user` in the
    /// task manager ([`ApiInner::tasks`]), returning its id (its `opt_id`)
    /// and cancel flag. The [`TaskGuard`] in
    /// [`run_query_job`](Self::run_query_job) ends it once the query
    /// completes (success, error, or panic). Run by an asynchronous operation
    /// (an `async` `query`), the statement belongs to that operation's task
    /// instead (see [`TaskManager::register`]). The state lock the caller may
    /// hold is not needed (the manager has its own); the parameter keeps the
    /// call sites unchanged.
    pub(crate) fn register_task(
        &self,
        _st: &mut ApiState,
        db: &str,
        sparql: &str,
        user: &str,
    ) -> (u64, Arc<AtomicBool>) {
        self.tasks.register("query", db, sparql, user)
    }

    /// `GET /tasklist` (Admin): the running tasks — SPARQL statements and
    /// asynchronous operations — oldest first. `id` is the task's `opt_id`
    /// (also given as a string, `opt_id`, for clients whose JSON numbers are
    /// doubles).
    fn handle_tasklist(&self) -> Resp {
        let mut items = Vec::new();
        for t in self.tasks.running() {
            let snippet: String = t
                .sparql
                .replace(['\n', '\r', '\t'], " ")
                .chars()
                .take(200)
                .collect();
            items.push(format!(
                "{{\"id\":{id},\"opt_id\":\"{id}\",\"operation\":{},\"async\":{},\"db\":{},\
                 \"user\":{},\"query\":{},\"elapsed_ms\":{},\"cancelled\":{}}}",
                json_str(&t.operation),
                t.is_async,
                json_str(&t.db_name),
                json_str(&t.user),
                json_str(&snippet),
                t.elapsed.as_millis(),
                t.cancel_requested,
                id = t.opt_id,
            ));
        }
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"tasks\":[{}]}}",
            items.join(",")
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    /// `POST /cancel?id=…` (Admin: any task; Update: only your own): raise a
    /// running task's cancellation flag.
    fn handle_cancel(&self, req: &Request, user: &str, is_admin: bool) -> Resp {
        let Some(id) = req
            .param("id")
            .or_else(|| req.param("task_id"))
            .or_else(|| req.param("opt_id"))
            .or_else(|| req.param("tid"))
            .and_then(|s| s.trim().parse::<u64>().ok())
        else {
            return Resp::fail(400, "missing or malformed id/task_id parameter");
        };
        match self.tasks.get(id) {
            Some(t) if !t.state.is_finished() => {
                // A non-admin may only cancel tasks they started; admins cancel any.
                if !is_admin && t.user != user {
                    return Resp::fail(403, &format!("task {id} belongs to another user"));
                }
                self.tasks.request_cancel(id);
                Resp::ok(&format!("cancel requested for task {id}"))
            }
            _ => Resp::fail(404, &format!("no active task {id}")),
        }
    }

    // ---- database lifecycle ----------------------------------------------

    /// `POST /build?db_name=…` (RDF in the body, or a server-local
    /// `db_path`). The parse and save run with the state lock released; the
    /// name is reserved meanwhile ([`ApiState::busy`]). A `db_path` is read
    /// only as the server's [`LoadPolicy`] allows — the policy that governs
    /// SPARQL `LOAD`, so `/build` is not a way around it — and only if it is a
    /// regular file.
    ///
    /// REST-specific: an existing database is **rebuilt** in place (the `/api`
    /// `build` refuses it with `1004 "database already built."`). Both share
    /// the rest of the rules: a new database counts against
    /// `max_database_num` ([`ApiState::database_limit_reached`]), and an
    /// existing directory is locked against other processes
    /// ([`DirLock::acquire_existing`]) before it is rewritten — `409` while
    /// another process (a `gquery`, another server) has it open.
    fn handle_build(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        if st.db_txn.contains_key(&name) {
            return Dispatch::Done(Resp::fail(409, "database is busy in a transaction"));
        }
        let max_dbs = self.config().max_database_num;
        let exists =
            st.dbs.contains_key(&name) || st.sysdb.contains(&name) || st.db_dir(&name).exists();
        // Still registered, but neither loaded nor on disk: dropped outside
        // the server (`gdrop`). The new database must not inherit its grants.
        let vanished = st.sysdb.contains(&name)
            && !st.dbs.contains_key(&name)
            && !st.db_dir(&name).exists();
        if !exists && st.database_limit_reached(max_dbs) {
            return Dispatch::Done(Resp::fail(
                409,
                &format!(
                    "cannot build '{name}': the server already holds max_database_num \
                     ({max_dbs}) databases"
                ),
            ));
        }
        if let Err(r) = st.claim(&name) {
            return Dispatch::Done(r);
        }
        // Build from a server-local file path, or from the request body (RDF).
        let path = req.param("db_path").or_else(|| req.param("path"));
        let body = if path.is_none() { req.body_str() } else { String::new() };
        let dir = st.db_dir(&name);
        let policy = st.load_policy.clone();
        let creator = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let _busy = BusyGuard {
                inner,
                name: name.clone(),
            };
            let built = match path {
                Some(path) => match build_source(&policy, &path) {
                    Ok(file) => Database::build_from_files(name.clone(), &[file]),
                    Err(r) => return r,
                },
                None => Database::build_from_str(name.clone(), &body),
            };
            let db = match built {
                Ok(db) => db,
                Err(e) => return Resp::fail(400, &format!("build failed: {e}")),
            };
            // No other process may have the directory open while it is
            // rewritten (shares the lock a loaded copy holds in this process).
            let _dir_lock = match DirLock::acquire_existing(&dir) {
                Ok(lock) => lock,
                Err(e) => return Resp::fail(409, &format!("build failed: {e}")),
            };
            // A rebuilt database replaces the loaded one: retire it (closing
            // its files) before its directory is rewritten.
            inner.retire_loaded(&name);
            if let Err(e) = db.save(&dir) {
                return Resp::fail(500, &format!("save failed: {e}"));
            }
            let triples = db.triple_num();
            let mut st = locked(&inner.state);
            let slot = DbSlot::handle(db, dir, false, &st.load_policy, &st.query_settings);
            st.dbs.insert(name.clone(), slot);
            // Registry bookkeeping (best effort: the registry is reconciled
            // with the database directories at every start anyway).
            if vanished {
                let _ = st.users.forget_database(&name);
            }
            let _ = st.sysdb.register(&name, &creator);
            Resp::ok(&format!("database '{name}' built ({triples} triples)"))
        }))
    }

    fn handle_load(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        // Re-loading an already-loaded database would silently discard its
        // unsaved in-memory changes; it is a no-op instead (as in gStore).
        if st.dbs.contains_key(&name) {
            return Dispatch::Done(Resp::ok(&format!("database '{name}' already loaded")));
        }
        if let Err(r) = st.claim(&name) {
            return Dispatch::Done(r);
        }
        let dir = st.db_dir(&name);
        // Opening can be slow (an on-disk database is materialized), so it
        // runs with the state lock released.
        Dispatch::Deferred(Box::new(move |inner| {
            let _busy = BusyGuard {
                inner,
                name: name.clone(),
            };
            // Finish an interrupted restore that left the directory mid-swap.
            let _ = Database::recover_restore(&dir);
            if !dir.is_dir() {
                return Resp::fail(404, &format!("database '{name}' not found on disk"));
            }
            // Any layout (snapshot, on-disk `kvstore.kv`, RocksDB): an on-disk /
            // RocksDB database keeps writing its updates through to `dir`.
            match Database::open(&dir) {
                Ok(db) => {
                    let mut st = locked(&inner.state);
                    let slot = DbSlot::handle(db, dir, false, &st.load_policy, &st.query_settings);
                    st.dbs.insert(name.clone(), slot);
                    Resp::ok(&format!("database '{name}' loaded"))
                }
                Err(e) => Resp::fail(500, &format!("load failed: {e}")),
            }
        }))
    }

    /// `POST /upload?db_name=<name>[&format=turtle|ntriples|nquads|rdfxml|trig]`
    /// with the raw RDF document as the request body (the format may also be
    /// inferred from `Content-Type`, defaulting to Turtle — a superset of
    /// N-Triples). Parses with the crate's existing bundled parsers (the same
    /// ones [`Database::build_from_files`] uses) and loads the triples into
    /// `db_name`: an already-loaded database is updated incrementally via
    /// [`Database::insert_triple`]; otherwise an on-disk copy is loaded first,
    /// or — if neither exists yet — a fresh empty database is created (mirrors
    /// gStore's `LoadFile`, which may target a not-yet-built db). The result is
    /// saved under the check_point policy.
    fn handle_upload(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        if let Some(&tid) = st.db_txn.get(&name) {
            return Dispatch::Done(txn_conflict(&name, tid));
        }
        let loaded = st.dbs.get(&name).map(Arc::clone);
        if loaded.is_none() {
            // Creating a database by uploading counts as a build (see
            // `rest_allowed`), so the `max_database_num` limit applies.
            let max_dbs = self.config().max_database_num;
            if !st.sysdb.contains(&name)
                && !st.db_dir(&name).exists()
                && st.database_limit_reached(max_dbs)
            {
                return Dispatch::Done(Resp::fail(
                    409,
                    &format!(
                        "cannot create '{name}': the server already holds max_database_num \
                         ({max_dbs}) databases"
                    ),
                ));
            }
            // Opening / creating it is a lifecycle operation on the name.
            if let Err(r) = st.claim(&name) {
                return Dispatch::Done(r);
            }
        }
        let format = upload_format(req);
        let content = req.body_str();
        let dir = st.db_dir(&name);
        let policy = st.load_policy.clone();
        let query = st.query_settings;
        Dispatch::Deferred(Box::new(move |inner| {
            let busy = loaded.is_none().then(|| BusyGuard {
                inner,
                name: name.clone(),
            });
            let triples = match parse_rdf(&format, &content) {
                Ok(ts) => ts,
                Err(e) => return Resp::fail(400, &format!("upload parse failed ({format}): {e}")),
            };
            let parsed = triples.len();
            let dbh = match loaded {
                Some(h) => h,
                None => {
                    let (opened, fresh) = if dir.is_dir() {
                        (Database::open(&dir), false)
                    } else {
                        (Database::build_from_str(name.clone(), ""), true)
                    };
                    match opened {
                        // A fresh database exists only in memory so far: dirty,
                        // so it is persisted even if nothing ends up inserted.
                        Ok(db) => {
                            let h = DbSlot::handle(db, dir, fresh, &policy, &query);
                            locked(&inner.state).dbs.insert(name.clone(), Arc::clone(&h));
                            h
                        }
                        Err(e) => {
                            return Resp::fail(500, &format!("upload failed to open database: {e}"))
                        }
                    }
                }
            };
            drop(busy);
            let mut slot = locked(&dbh);
            if slot.retired {
                return Resp::fail(404, &format!("database '{name}' was unloaded meanwhile"));
            }
            if let Some(open) = &slot.txn {
                return txn_conflict(&name, open.tid);
            }
            let mut inserted = 0usize;
            let mut failure = None;
            for t in &triples {
                match slot.db.try_insert_triple(t) {
                    Ok(true) => inserted += 1,
                    Ok(false) => {}
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            let warning = if inserted > 0 || slot.dirty {
                slot.wrote(inner.save_writes())
            } else {
                None
            };
            drop(slot);
            if let Some(e) = failure {
                return Resp::fail(
                    500,
                    &format!("upload stopped after inserting {inserted} triple(s): {e}"),
                );
            }
            // (Lifecycle endpoints like /build and /load don't append to
            // query.log either; the access log — written unconditionally in
            // `handle` — already records that this request happened.)
            let body = format!(
                "{{\"StatusCode\":0,\"StatusMsg\":\"upload complete\",\"db_name\":{},\"format\":{},\"triples_parsed\":{parsed},\"triples_inserted\":{inserted}{}}}",
                json_str(&name),
                json_str(&format),
                warn_field(&warning),
            );
            Resp::new(200, JSON, body.into_bytes())
        }))
    }

    /// `POST /unload?db_name=…` — save the database if it has unsaved changes,
    /// then drop it from memory. If the save fails it stays loaded (nothing is
    /// lost) and the error is returned.
    fn handle_unload(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        if let Some(&tid) = st.db_txn.get(&name) {
            return Dispatch::Done(unload_busy(&name, tid));
        }
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        if let Err(r) = st.claim(&name) {
            return Dispatch::Done(r);
        }
        Dispatch::Deferred(Box::new(move |inner| {
            let _busy = BusyGuard {
                inner,
                name: name.clone(),
            };
            let mut slot = locked(&dbh);
            if slot.retired {
                return Resp::fail(404, &format!("database '{name}' is not loaded"));
            }
            if let Some(open) = &slot.txn {
                return unload_busy(&name, open.tid);
            }
            let saved = if slot.dirty {
                if let Err(e) = slot.save() {
                    return Resp::fail(
                        500,
                        &format!("unload aborted: saving '{name}' failed ({e}); it stays loaded"),
                    );
                }
                true
            } else {
                // Nothing unsaved in memory; still a commit point for an
                // on-disk / RocksDB database (its write-through is committed).
                if let Err(e) = slot.db.sync() {
                    return Resp::fail(
                        500,
                        &format!("unload aborted: committing '{name}' failed ({e}); it stays loaded"),
                    );
                }
                false
            };
            // Requests that queued on this handle must not write to (or save)
            // a database that is no longer served.
            slot.retire();
            {
                let mut st = locked(&inner.state);
                if st.dbs.get(&name).is_some_and(|h| Arc::ptr_eq(h, &dbh)) {
                    st.dbs.remove(&name);
                }
            }
            drop(slot);
            if saved {
                Resp::ok(&format!("database '{name}' saved and unloaded"))
            } else {
                Resp::ok(&format!("database '{name}' unloaded"))
            }
        }))
    }

    /// `POST /drop?db_name=…` — delete the database (`root` / `admin` only):
    /// it is unloaded **without** saving and its directory is deleted; its
    /// registry entry and every grant on it go.
    ///
    /// REST-specific: the directory is always deleted. The `/api` `drop`
    /// (`gapi::ops_db`) follows upstream instead — by default
    /// (`is_backup=true`) it saves and keeps the directory as `<db>.db.bak`,
    /// deleting it only with `is_backup=false`. Both take the directory's
    /// inter-process lock first ([`DirLock::acquire_existing`]): `409` while
    /// another process has the database open.
    fn handle_drop(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        if st.db_txn.contains_key(&name) {
            return Dispatch::Done(Resp::fail(409, "database is busy in a transaction"));
        }
        if let Err(r) = st.claim(&name) {
            return Dispatch::Done(r);
        }
        let dir = st.db_dir(&name);
        Dispatch::Deferred(Box::new(move |inner| {
            let _busy = BusyGuard {
                inner,
                name: name.clone(),
            };
            // No other process may have it open (shares the lock a loaded copy
            // holds in this process, so it stays locked once that is retired).
            let _dir_lock = match DirLock::acquire_existing(&dir) {
                Ok(lock) => lock,
                Err(e) => return Resp::fail(409, &format!("drop failed: {e}")),
            };
            // Retire (closing its store files) before the directory is deleted.
            let was_loaded = inner.retire_loaded(&name);
            let existed_on_disk = dir.is_dir();
            if existed_on_disk {
                if let Err(e) = fs::remove_dir_all(&dir) {
                    return Resp::fail(500, &format!("drop failed: {e}"));
                }
            }
            if !was_loaded && !existed_on_disk {
                return Resp::fail(404, &format!("database '{name}' does not exist"));
            }
            // Gone: unregister it and drop every grant on it, so a later
            // database of the same name starts without them (best effort).
            {
                let mut st = locked(&inner.state);
                let _ = st.sysdb.unregister(&name);
                let _ = st.users.forget_database(&name);
            }
            Resp::ok(&format!("database '{name}' dropped"))
        }))
    }

    fn handle_show(&self, st: &ApiState, user: &str) -> Dispatch {
        // Snapshot the loaded-db handles under the state lock; the directory
        // scan and per-db counts run with the state lock released (see
        // Dispatch::Deferred).
        let root = st.root.clone();
        let loaded: HashMap<String, DbHandle> =
            st.dbs.iter().map(|(n, d)| (n.clone(), Arc::clone(d))).collect();
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let mut names: Vec<String> = Vec::new();
            // On-disk databases (directories ending in the .db suffix).
            if let Ok(rd) = fs::read_dir(&root) {
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
            for k in loaded.keys() {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
            names.sort();
            names.dedup();
            {
                let st = locked(&inner.state);
                names.retain(|n| crate::sysdb::allowed(&st.users, &user, "query", Some(n)));
            }
            let mut items = Vec::new();
            for n in &names {
                let (is_loaded, triples, in_transaction, dirty) = match loaded.get(n) {
                    Some(d) => {
                        let slot = locked(d);
                        (
                            true,
                            slot.committed_triple_num().to_string(),
                            slot.txn.is_some(),
                            slot.dirty,
                        )
                    }
                    None => (false, "null".to_string(), false, false),
                };
                items.push(format!(
                    "{{\"name\":{},\"loaded\":{is_loaded},\"in_transaction\":{in_transaction},\"dirty\":{dirty},\"triples\":{triples}}}",
                    json_str(n)
                ));
            }
            let body = format!(
                "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"databases\":[{}]}}",
                items.join(",")
            );
            Resp::new(200, JSON, body.into_bytes())
        }))
    }

    // ---- transactions -----------------------------------------------------

    /// `POST /begin?db_name=…[&isolevel=1|2|3]` — open a transaction owned by
    /// the caller. The committed state is snapshotted for everyone else's
    /// reads (see [`ReadSnapshot`]), and until the transaction ends every
    /// non-transactional write to the database is rejected with `409`.
    fn handle_begin(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        let isolation = match req.param("isolevel").filter(|s| !s.trim().is_empty()) {
            None => TxnIsolation::Serializable,
            Some(v) => match TxnIsolation::parse(&v) {
                Some(level) => level,
                None => {
                    return Dispatch::Done(Resp::fail(
                        400,
                        &format!(
                            "invalid isolevel '{v}': the isolation level's value can only be \
                             1 (SERIALIZABLE), 2 (SNAPSHOT) or 3 (READ_COMMITTED)"
                        ),
                    ))
                }
            },
        };
        let dbh = match Self::begin_target(st, &name) {
            Ok(dbh) => dbh,
            Err(e) => return Dispatch::Done(e.rest()),
        };
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            match inner.begin_txn(&dbh, &name, isolation, &user) {
                Ok(tid) => {
                    // `db_name` too: ids are numbered per database, so it
                    // is what tells this transaction apart (pass it back to
                    // `/tquery`, `/commit` and `/rollback`).
                    let body = format!(
                        "{{\"StatusCode\":0,\"StatusMsg\":\"transaction started\",\"TID\":{},\"db_name\":{},\"isolevel\":{},\"IsolationLevel\":{}}}",
                        json_str(&tid.to_string()),
                        json_str(&name),
                        isolation.code(),
                        json_str(isolation.name()),
                    );
                    Resp::new(200, JSON, body.into_bytes())
                }
                Err(e) => e.rest(),
            }
        }))
    }

    /// The pre-checks of a `begin` on `name`, under the state lock: the loaded
    /// database's handle, or why no transaction can begin on it now.
    pub(crate) fn begin_target(st: &ApiState, name: &str) -> Result<DbHandle, OpError> {
        let Some(dbh) = st.dbs.get(name).map(Arc::clone) else {
            return Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' is not loaded"),
            ));
        };
        if let Some(&open) = st.db_txn.get(name) {
            return Err(txn_already_open_err(name, open));
        }
        if st.busy.contains(name) {
            return Err(OpError::new(
                OpErrorKind::Conflict,
                format!("database '{name}' is busy"),
            ));
        }
        Ok(dbh)
    }

    /// Begin a transaction owned by `user` on loaded database `name` (handle
    /// `dbh`, from [`begin_target`](Self::begin_target)). Must be called with
    /// no lock held: it locks the database, snapshots its committed state,
    /// and registers the transaction while still holding the db lock (so the
    /// registry and the slot always agree). Returns the new transaction id.
    pub(crate) fn begin_txn(
        &self,
        dbh: &DbHandle,
        name: &str,
        isolation: TxnIsolation,
        user: &str,
    ) -> Result<u64, OpError> {
        let mut slot = locked(dbh);
        if slot.retired {
            return Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' is not loaded"),
            ));
        }
        if let Some(open) = &slot.txn {
            return Err(txn_already_open_err(name, open.tid));
        }
        // Checked under the db lock, which the shutdown takes after setting
        // the flag: no transaction can begin after its rollback pass.
        if self.closing.load(Ordering::SeqCst) {
            return Err(OpError::new(OpErrorKind::ShuttingDown, "the server is shutting down"));
        }
        let snapshot = Arc::new(ReadSnapshot::capture(&slot.db));
        if let Err(e) = slot.db.begin() {
            return Err(OpError::new(OpErrorKind::Failed, format!("begin failed: {e}")));
        }
        // Register it while the db lock is held, so the registry and the
        // slot always agree (a /commit locks the db before the registry).
        let tid = {
            let mut st = locked(&self.state);
            let conflict = if let Some(&open) = st.db_txn.get(name) {
                Some(txn_already_open_err(name, open))
            } else if st.busy.contains(name) {
                Some(OpError::new(
                    OpErrorKind::Conflict,
                    format!("database '{name}' is busy"),
                ))
            } else {
                None
            };
            if let Some(e) = conflict {
                drop(st);
                let _ = slot.db.rollback();
                return Err(e);
            }
            // Sequential per loaded database, like upstream (see
            // `DbSlot::next_tid`). An id this database used before it was last
            // (re)loaded starts a new life: forget how it ended then.
            let tid = slot.next_tid.max(1);
            slot.next_tid = tid.checked_add(1).unwrap_or(1);
            st.ended_txns.retain(|(d, t, _)| !(*t == tid && d == name));
            st.txns.insert(
                (name.to_string(), tid),
                Txn {
                    db: name.to_string(),
                    owner: user.to_string(),
                    isolation,
                },
            );
            st.db_txn.insert(name.to_string(), tid);
            tid
        };
        slot.txn = Some(OpenTxn {
            tid,
            snapshot,
            changed: false,
        });
        drop(slot);
        append_log(
            &self.root,
            "txn.log",
            &format!(
                "{}\tbegin\t{}\t{}\t{}\t{}",
                now_millis(),
                name,
                tid,
                user,
                isolation.name()
            ),
        );
        self.apilogs.txn_begin(&self.root, name, tid, user);
        Ok(tid)
    }

    /// Look up open transaction `tid` of database `db` (when not named: the one
    /// database where `tid` is open, see [`ApiState::open_txn_db`]) for `user`:
    /// unknown ids are `404`, already finished ones `409`, an id open on
    /// several databases `409`, and someone else's transaction `403` (only the
    /// owner or `root` may touch it). Returns the transaction's database.
    pub(crate) fn txn_for(
        &self,
        st: &ApiState,
        db: Option<&str>,
        tid: u64,
        user: &str,
    ) -> Result<String, OpError> {
        let name = st.open_txn_db(db, tid)?;
        let txn = &st.txns[&(name, tid)];
        if txn.owner != user && user != ROOT_USER {
            return Err(OpError::new(
                OpErrorKind::NotOwner,
                format!("transaction {tid} belongs to another user"),
            ));
        }
        Ok(txn.db.clone())
    }

    fn handle_tquery(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(tid) = self.txn_id(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or malformed tid/txn parameter"));
        };
        let name = match self.txn_for(st, self.req_db_name(req).as_deref(), tid, user) {
            Ok(name) => name,
            Err(e) => return Dispatch::Done(e.rest()),
        };
        // The handle the transaction lives on, taken with the ownership check:
        // ids restart at 1 when a database is loaded again, so the statement
        // must run on this handle (refused once it is retired), never on
        // whatever is loaded under the name by the time it runs.
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        let Some(sparql) = req
            .param("sparql")
            .or_else(|| req.query_string())
            .or_else(|| Some(req.body_str()).filter(|s| !s.trim().is_empty()))
        else {
            return Dispatch::Done(Resp::fail(400, "missing sparql parameter"));
        };
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let user = user.to_string();
        // Classified (parsed) with the state lock released, like `/sparql`.
        Dispatch::Deferred(Box::new(move |inner| {
            let op = if is_update_sparql(&sparql) {
                "update"
            } else {
                "query"
            };
            if !crate::sysdb::allowed(&locked(&inner.state).users, &user, op, Some(&name)) {
                return inner.forbidden();
            }
            log_query(&inner.root, &name, &user, &sparql);
            match inner.run_in_txn_task(&dbh, &name, tid, &sparql, &user) {
                Ok(r) => render_result(fmt, &r),
                Err(e) => e.rest(),
            }
        }))
    }

    /// [`run_in_txn`](Self::run_in_txn) as a registered task of `user`: listed
    /// by `/tasklist`, and cancellable — by `/cancel`, and by a shutdown past
    /// its grace period — like any other query. Must be called with no lock
    /// held.
    pub(crate) fn run_in_txn_task(
        &self,
        dbh: &DbHandle,
        name: &str,
        tid: u64,
        sparql: &str,
        user: &str,
    ) -> Result<QueryResult, OpError> {
        let (task_id, cancel) = {
            let mut st = locked(&self.state);
            self.register_task(&mut st, name, sparql, user)
        };
        let mut task = TaskGuard::new(self, task_id);
        let out = Self::run_in_txn(dbh, name, tid, sparql, Some(cancel));
        task.set_ok(out.is_ok());
        out
    }

    /// Run `sparql` inside open transaction `tid` on database `name` (handle
    /// `dbh`), cancellable through `cancel` when given. Must be called with no
    /// lock held. The transaction is re-checked under the db lock, which
    /// `/commit` and `/rollback` also take before they end it — so the
    /// statement either runs inside transaction `tid` or is refused, never
    /// applied outside it.
    pub(crate) fn run_in_txn(
        dbh: &DbHandle,
        name: &str,
        tid: u64,
        sparql: &str,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<QueryResult, OpError> {
        let mut slot = locked(dbh);
        if slot.retired || slot.txn.as_ref().map(|t| t.tid) != Some(tid) {
            return Err(OpError::new(
                OpErrorKind::TxnEnded,
                format!("transaction {tid} is not open on database '{name}'"),
            ));
        }
        let before = slot.db.mutation_count();
        let result = match cancel {
            Some(cancel) => slot.db.query_cancellable(sparql, cancel),
            None => slot.db.query(sparql),
        };
        // Also a failing UPDATE may have applied part of its operations.
        if slot.db.mutation_count() != before {
            if let Some(open) = slot.txn.as_mut() {
                open.changed = true;
            }
        }
        drop(slot);
        result.map_err(|e| OpError::new(OpErrorKind::Query, e.to_string()))
    }

    fn handle_commit(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        self.handle_txn_end(st, req, user, true)
    }

    fn handle_rollback(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        self.handle_txn_end(st, req, user, false)
    }

    fn handle_txn_end(&self, st: &mut ApiState, req: &Request, user: &str, commit: bool) -> Dispatch {
        let Some(tid) = self.txn_id(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or malformed tid/txn parameter"));
        };
        let db_name = match self.txn_for(st, self.req_db_name(req).as_deref(), tid, user) {
            Ok(name) => name,
            Err(e) => return Dispatch::Done(e.rest()),
        };
        let dbh = st.dbs.get(&db_name).map(Arc::clone);
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let verb = if commit { "commit" } else { "rollback" };
            match inner.end_txn(dbh.as_ref(), &db_name, tid, &user, commit) {
                Ok(warning) => Resp::ok_warn(&format!("transaction {tid} {verb} done"), &warning),
                Err(e) => e.rest(),
            }
        }))
    }

    /// Commit (or roll back) open transaction `tid` of database `db` on behalf
    /// of `user`; `dbh` is the handle of `db` (looked up beforehand, `None`
    /// when it is no longer loaded). Must be called with no lock held. Returns
    /// the warning of a failed check_point save after a commit that changed
    /// data (the commit itself stands).
    pub(crate) fn end_txn(
        &self,
        dbh: Option<&DbHandle>,
        db: &str,
        tid: u64,
        user: &str,
        commit: bool,
    ) -> Result<Option<String>, OpError> {
        let verb = if commit { "commit" } else { "rollback" };
        let mut slot = dbh.map(|h| locked(h));
        // End it in the registry while the db lock is held (race-free with
        // /begin, /tquery and a concurrent /commit of the same tid).
        let txn = {
            let mut st = locked(&self.state);
            self.txn_for(&st, Some(db), tid, user)?;
            // `dbh` was looked up before this lock was taken: if the database
            // has been unloaded and loaded again since (which restarts its
            // ids), (`db`, `tid`) is now another transaction on the new
            // handle, and this request must not end it.
            if let Some(h) = dbh {
                if !st.dbs.get(db).is_some_and(|cur| Arc::ptr_eq(cur, h)) {
                    return Err(OpError::new(
                        OpErrorKind::TxnEnded,
                        format!("transaction {tid} is not open on database '{db}'"),
                    ));
                }
            }
            let txn = st
                .txns
                .remove(&(db.to_string(), tid))
                .expect("checked by txn_for");
            st.db_txn.remove(&txn.db);
            st.remember_ended(db, tid, if commit { "committed" } else { "rolled back" });
            txn
        };
        let result = match slot.as_mut().filter(|s| !s.retired) {
            Some(slot) => {
                let open = slot.txn.take();
                Some(if commit {
                    // `Database::commit` keeps the writes even when flushing
                    // its redo log fails, so the data changed either way.
                    let committed = slot.db.commit();
                    let warning = if open.is_some_and(|o| o.changed) {
                        slot.wrote(self.save_writes())
                    } else {
                        None
                    };
                    committed.map(|()| warning)
                } else {
                    slot.db.rollback().map(|()| None)
                })
            }
            None => None,
        };
        drop(slot);
        append_log(
            &self.root,
            "txn.log",
            &format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                now_millis(),
                verb,
                txn.db,
                tid,
                user,
                txn.isolation.name()
            ),
        );
        let state = if commit {
            TxnState::Committed
        } else {
            TxnState::RolledBack
        };
        self.apilogs
            .txn_end(&self.root, &txn.db, tid, &txn.owner, state);
        match result {
            Some(Ok(warning)) => Ok(warning),
            Some(Err(e)) => Err(OpError::new(OpErrorKind::Failed, format!("{verb} failed: {e}"))),
            None => Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{}' is not loaded", txn.db),
            )),
        }
    }

    /// Lock a loaded database for a non-transactional maintenance request: the
    /// slot's guard, or the response refusing it (`404` once it is no longer
    /// served; `409` naming `what` while a transaction is open).
    fn lock_for<'a>(
        dbh: &'a DbHandle,
        name: &str,
        what: &str,
    ) -> Result<std::sync::MutexGuard<'a, DbSlot>, Resp> {
        Self::lock_for_op(dbh, name, what).map_err(|e| e.rest())
    }

    /// [`lock_for`](Self::lock_for) with an [`OpError`].
    pub(crate) fn lock_for_op<'a>(
        dbh: &'a DbHandle,
        name: &str,
        what: &str,
    ) -> Result<std::sync::MutexGuard<'a, DbSlot>, OpError> {
        let slot = locked(dbh);
        if slot.retired {
            return Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' is not loaded"),
            ));
        }
        if let Some(open) = &slot.txn {
            return Err(OpError::new(
                OpErrorKind::Conflict,
                format!(
                    "database '{name}' has an open transaction (TID {}); commit or roll back \
                     before {what}",
                    open.tid
                ),
            ));
        }
        Ok(slot)
    }

    /// `POST /checkpoint?db_name=…` — force a save now (whatever the
    /// check_point policy). Refused while a transaction is open, since the live
    /// database then holds uncommitted writes.
    fn handle_checkpoint(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        Dispatch::Deferred(Box::new(move |_| match Self::checkpoint_db(&dbh, &name) {
            Ok(()) => Resp::ok(&format!("database '{name}' checkpointed")),
            Err(e) => e.rest(),
        }))
    }

    /// Save loaded database `name` (handle `dbh`) now, whatever the
    /// check_point policy. Must be called with no lock held.
    pub(crate) fn checkpoint_db(dbh: &DbHandle, name: &str) -> Result<(), OpError> {
        let mut slot = Self::lock_for_op(dbh, name, "checkpointing")?;
        slot.save()
            .map_err(|e| OpError::new(OpErrorKind::Failed, format!("checkpoint failed: {e}")))
    }

    // ---- backup / restore / export ---------------------------------------

    /// `POST /backup?db_name=…[&path=<dir>]` — write a full backup of the
    /// database into `<dir>`, which must lie inside the server's
    /// [backup directory](ApiInner::backup_dir) (a relative `path` is taken
    /// relative to it; the default is `<name>.backup` there). A backup replaces
    /// whatever database files its target holds, so a target that already
    /// holds anything but an earlier backup of this same database is refused,
    /// as is one nested inside another backup: a client can neither write
    /// outside that directory nor clobber another database or its backups.
    fn handle_backup(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        let requested = req.param("path").or_else(|| req.param("backup_path"));
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        Dispatch::Deferred(Box::new(move |inner| {
            let path = match backup_target(&inner.backup_dir(), &name, requested.as_deref()) {
                Ok(path) => path,
                Err(r) => return r,
            };
            // A backup of the live database would capture uncommitted writes.
            let slot = match Self::lock_for(&dbh, &name, "backing up") {
                Ok(slot) => slot,
                Err(r) => return r,
            };
            // Claim the directory for this database before writing into it.
            let claimed = fs::create_dir_all(&path).map_err(GStoreError::from).and_then(|()| {
                crate::db::fsutil::atomic_write(&path.join(BACKUP_OWNER_FILE), name.as_bytes())
            });
            if let Err(e) = claimed {
                return Resp::fail(500, &format!("backup failed: {e}"));
            }
            match slot.db.backup(&path) {
                Ok(()) => {
                    let body = format!(
                        "{{\"StatusCode\":0,\"StatusMsg\":\"backup success\",\"path\":{}}}",
                        json_str(&path.to_string_lossy())
                    );
                    Resp::new(200, JSON, body.into_bytes())
                }
                Err(e) => Resp::fail(500, &format!("backup failed: {e}")),
            }
        }))
    }

    /// `POST /restore?db_name=…&path=<backup>` — replace the database with a
    /// backup (any layout). The backup must lie inside the server's
    /// [backup directory](ApiInner::backup_dir) (a relative `path` is taken
    /// relative to it) or in a directory the server's [`LoadPolicy`] lets
    /// clients read (`gserver --load-dir`); anything else is refused, so a
    /// client cannot make the server read other databases on the host. The
    /// backup is copied next to the database's
    /// directory and validated *before* the loaded database is touched, so a
    /// corrupt backup changes nothing; only then is the loaded database retired
    /// (its unsaved changes saved first, so a failed swap loses nothing) and
    /// the copy swapped in — with the directory's inter-process lock held
    /// ([`DirLock::acquire_existing`]; `409` while another process has the
    /// database open), as the `/api` `restore` does.
    ///
    /// REST-specific: `path` is required and the restored database is left
    /// loaded; the `/api` `restore` defaults to the newest backup in the
    /// backup directory and reloads only a database that was loaded.
    fn handle_restore(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        let Some(path) = req.param("path").or_else(|| req.param("backup_path")) else {
            return Dispatch::Done(Resp::fail(400, "missing backup path"));
        };
        if st.db_txn.contains_key(&name) {
            return Dispatch::Done(Resp::fail(409, "database is busy in a transaction"));
        }
        // A restore that creates the database counts against
        // `max_database_num`, as a build does.
        let fresh = !st.dbs.contains_key(&name) && !st.db_dir(&name).exists();
        let max_dbs = self.config().max_database_num;
        if fresh && !st.sysdb.contains(&name) && st.database_limit_reached(max_dbs) {
            return Dispatch::Done(Resp::fail(
                409,
                &format!(
                    "cannot restore '{name}': the server already holds max_database_num \
                     ({max_dbs}) databases"
                ),
            ));
        }
        if let Err(r) = st.claim(&name) {
            return Dispatch::Done(r);
        }
        let dir = st.db_dir(&name);
        let policy = st.load_policy.clone();
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let _busy = BusyGuard {
                inner,
                name: name.clone(),
            };
            let path = match restore_source(&inner.backup_dir(), &policy, &path) {
                Ok(path) => path,
                Err(r) => return r,
            };
            if !may_restore_from(&locked(&inner.state).users, &user, &name, &path) {
                return Resp::fail(
                    403,
                    &format!(
                        "restore path refused: it is not a backup of database '{name}' (only \
                         root and admin users may restore a database from another one's backup)"
                    ),
                );
            }
            if Database::storage_kind(&path).is_none() {
                return Resp::fail(
                    500,
                    &format!(
                        "restore failed: '{}' does not contain a database backup",
                        path.display()
                    ),
                );
            }
            // Copy the backup beside the database's own directory and validate
            // the copy — never open the backup in place, or later writes would
            // land in the backup. The loaded database is untouched so far.
            let staged = match Database::stage_restore(&path, &dir) {
                Ok(staged) => staged,
                Err(e) => return Resp::fail(500, &format!("restore failed: {e}")),
            };
            // No other process may have the database open while it is swapped
            // (as the `/api` restore; shares a loaded copy's lock).
            let probe = match DirLock::acquire_existing(&dir) {
                Ok(lock) => lock,
                Err(e) => {
                    Database::discard_staged_restore(&staged);
                    return Resp::fail(409, &format!("restore failed: {e}"));
                }
            };
            let loaded = locked(&inner.state).dbs.get(&name).map(Arc::clone);
            if let Some(h) = &loaded {
                let mut slot = locked(h);
                if !slot.retired && slot.dirty {
                    if let Err(e) = slot.save() {
                        drop(slot);
                        Database::discard_staged_restore(&staged);
                        return Resp::fail(
                            500,
                            &format!(
                                "restore aborted: saving the unsaved changes of '{name}' failed \
                                 ({e}); it stays loaded"
                            ),
                        );
                    }
                }
                // Close its files before the directory is swapped out.
                slot.retire();
                let mut st = locked(&inner.state);
                if st.dbs.get(&name).is_some_and(|cur| Arc::ptr_eq(cur, h)) {
                    st.dbs.remove(&name);
                }
            }
            let installed = Database::install_restore(&staged, &dir);
            // Release the probe before reopening: the directory at this path
            // is a new one now, which the reopened database locks itself.
            drop(probe);
            // Serve whatever the directory now holds: the restored copy, or —
            // if the swap failed — the previous database (saved above).
            let reopened = Database::open(&dir);
            match (installed, reopened) {
                (Ok(()), Ok(db)) => {
                    let mut st = locked(&inner.state);
                    let slot = DbSlot::handle(db, dir, false, &st.load_policy, &st.query_settings);
                    st.dbs.insert(name.clone(), slot);
                    // A database restored under a new name joins the
                    // registry with the restoring user as its creator (best
                    // effort, as for /build; no privileges are granted, and
                    // none left from an earlier database of the name hold).
                    if fresh && st.sysdb.contains(&name) {
                        let _ = st.users.forget_database(&name);
                    }
                    if fresh || !st.sysdb.contains(&name) {
                        let _ = st.sysdb.register(&name, &user);
                    }
                    Resp::ok(&format!("database '{name}' restored"))
                }
                (Ok(()), Err(e)) => {
                    Resp::fail(500, &format!("restore failed to open the restored copy: {e}"))
                }
                (Err(e), reopened) => {
                    Database::discard_staged_restore(&staged);
                    if let (Ok(db), true) = (reopened, loaded.is_some()) {
                        let mut st = locked(&inner.state);
                        let slot = DbSlot::handle(db, dir, false, &st.load_policy, &st.query_settings);
                        st.dbs.insert(name.clone(), slot);
                    }
                    Resp::fail(500, &format!("restore failed: {e}"))
                }
            }
        }))
    }

    /// `GET /export?db_name=<name>[&download=0]` — the whole database as
    /// N-Triples (its committed state, while a transaction is open). Sent as a
    /// file download (`Content-Disposition: attachment`) by default; pass
    /// `download=0`/`false`/`no`/`off` for an inline (browser-rendered)
    /// response instead.
    fn handle_export(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        let inline = wants_inline(req);
        Dispatch::Deferred(Box::new(move |_| {
            let exported = {
                let mut slot = locked(&dbh);
                if slot.retired {
                    return Resp::fail(404, &format!("database '{name}' is not loaded"));
                }
                slot.read("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }")
            };
            match exported {
                Ok(QueryResult::Construct(ts)) => {
                    let mut resp = Resp::new(200, "application/n-triples", ntriples_body(&ts));
                    let disposition = if inline { "inline" } else { "attachment" };
                    resp.extra.push((
                        "Content-Disposition".to_string(),
                        format!("{disposition}; filename=\"{name}.nt\""),
                    ));
                    resp
                }
                Ok(_) => Resp::fail(500, "export produced an unexpected result"),
                Err(e) => Resp::fail(500, &format!("export failed: {e}")),
            }
        }))
    }

    // ---- batch ------------------------------------------------------------

    fn handle_batch_insert(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        self.handle_batch(st, req, user, true)
    }

    fn handle_batch_remove(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        self.handle_batch(st, req, user, false)
    }

    /// `POST /batchInsert` / `POST /batchRemove` — the triples (a `triples=`
    /// parameter or the raw body) are *parsed as RDF* (N-Triples by default;
    /// Turtle with `format=ttl` or when the payload declares `@prefix`/`PREFIX`;
    /// see [`parse_batch_payload`]) and the ground triples are applied directly
    /// through [`Database::insert_triple`] / [`Database::remove_triple`]. No
    /// SPARQL text is ever built from the payload, so it cannot smuggle in other
    /// update operations: a malformed payload is a `400` parse error and
    /// nothing is applied. Reports `parsed`, `applied` (= `changed`) and
    /// `failed` (parsed but not applied: duplicates on insert, absent triples on
    /// remove, or a literal/blank in a position RDF does not allow).
    fn handle_batch(&self, st: &mut ApiState, req: &Request, user: &str, insert: bool) -> Dispatch {
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        let payload = req.param("triples").unwrap_or_else(|| req.body_str());
        if payload.trim().is_empty() {
            return Dispatch::Done(Resp::fail(400, "no triples supplied"));
        }
        let op = if insert { "batchInsert" } else { "batchRemove" };
        let format_param = req.param("format");
        let dbh = st.dbs.get(&name).map(Arc::clone);
        let user = user.to_string();
        // The payload (up to the request body cap) is parsed with the state
        // lock released.
        Dispatch::Deferred(Box::new(move |inner| {
            let (format, triples) = match parse_batch_payload(format_param.as_deref(), &payload) {
                Ok(parsed) => parsed,
                Err(e) => return Resp::fail(400, &format!("{op} payload is not valid RDF {e}")),
            };
            drop(payload);
            append_log(
                &inner.root,
                "query.log",
                &format!(
                    "{}\t{}\t{}\t{op} {} triple(s) ({format})",
                    now_millis(),
                    name,
                    user,
                    triples.len()
                ),
            );
            let Some(dbh) = dbh else {
                return Resp::fail(404, &format!("database '{name}' is not loaded"));
            };
            let mut slot = locked(&dbh);
            if slot.retired {
                return Resp::fail(404, &format!("database '{name}' is not loaded"));
            }
            if let Some(open) = &slot.txn {
                return txn_conflict(&name, open.tid);
            }
            let parsed = triples.len();
            let mut applied = 0usize;
            let mut failure = None;
            for t in triples.iter().filter(|t| valid_ground_triple(t)) {
                let changed = if insert {
                    slot.db.try_insert_triple(t)
                } else {
                    Ok(slot.db.remove_triple(t))
                };
                match changed {
                    Ok(true) => applied += 1,
                    Ok(false) => {}
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            let warning = if applied > 0 {
                slot.wrote(inner.save_writes())
            } else {
                None
            };
            drop(slot);
            if let Some(e) = failure {
                return Resp::fail(
                    500,
                    &format!("{op} stopped after applying {applied} triple(s): {e}"),
                );
            }
            let failed = parsed - applied;
            let verb = if insert { "inserted" } else { "removed" };
            let body = format!(
                "{{\"StatusCode\":0,\"StatusMsg\":\"batch {verb}\",\"format\":{},\"parsed\":{parsed},\"applied\":{applied},\"failed\":{failed},\"changed\":{applied},\"success_num\":{applied},\"failed_num\":{failed}{}}}",
                json_str(format),
                warn_field(&warning),
            );
            Resp::new(200, JSON, body.into_bytes())
        }))
    }

    // ---- monitoring -------------------------------------------------------

    fn handle_status(&self, st: &ApiState) -> Dispatch {
        // Snapshot registry scalars under the state lock; clone the db handles so
        // the (lock-contended) triple counts are read with the state lock released.
        let dbs: Vec<DbHandle> = st.dbs.values().cloned().collect();
        let ndbs = st.dbs.len();
        let ntxns = st.txns.len();
        let nusers = st.users.count();
        let nreq = st.requests;
        let uptime = st.started.elapsed().as_secs();
        let check_point = self.check_point;
        Dispatch::Deferred(Box::new(move |_| {
            let mut total_triples: u64 = 0;
            let mut dirty_dbs = 0usize;
            for d in &dbs {
                let slot = locked(d);
                total_triples += slot.committed_triple_num();
                dirty_dbs += usize::from(slot.dirty);
            }
            let body = format!(
                "{{\"StatusCode\":0,\"StatusMsg\":\"running\",\"databases_loaded\":{ndbs},\"total_triples\":{total_triples},\"open_transactions\":{ntxns},\"users\":{nusers},\"requests\":{nreq},\"uptime_secs\":{uptime},\"check_point\":{check_point},\"dirty_databases\":{dirty_dbs}}}"
            );
            Resp::new(200, JSON, body.into_bytes())
        }))
    }

    fn handle_monitor(&self, st: &ApiState, user: &str) -> Dispatch {
        let mut entries: Vec<(String, DbHandle)> = st
            .dbs
            .iter()
            .filter(|(n, _)| crate::sysdb::allowed(&st.users, user, "monitor", Some(n)))
            .map(|(n, d)| (n.clone(), Arc::clone(d)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let uptime = st.started.elapsed().as_secs();
        let nreq = st.requests;
        let nusers = st.users.count();
        let ntxns = st.txns.len();
        Dispatch::Deferred(Box::new(move |_| {
            let mut dbs = Vec::new();
            for (_n, d) in &entries {
                let slot = locked(d);
                let s = slot.db.stats();
                dbs.push(format!(
                    "{{\"name\":{},\"triples\":{},\"entities\":{},\"literals\":{},\"predicates\":{},\"index_valid\":{},\"in_transaction\":{},\"dirty\":{}}}",
                    json_str(&s.name),
                    slot.committed_triple_num(),
                    s.entity_num,
                    s.literal_num,
                    s.predicate_num,
                    s.index_valid,
                    s.in_transaction,
                    slot.dirty,
                ));
            }
            let body = format!(
                "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"uptime_secs\":{uptime},\"requests\":{nreq},\"users\":{nusers},\"open_transactions\":{ntxns},\"databases\":[{}]}}",
                dbs.join(","),
            );
            Resp::new(200, JSON, body.into_bytes())
        }))
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
        // With a `db_name` the privileges are granted on that database only;
        // without, on every database.
        let db = match grant_db_param(req) {
            Ok(db) => db,
            Err(r) => return r,
        };
        let (global, on_db) = match db {
            Some(db) => (BTreeSet::new(), Some((db, privs))),
            None => (privs, None),
        };
        // The system user (`/shutdown`, `/restart`) is not a stored user.
        if name == self.config().system_username {
            return Resp::fail(
                400,
                &format!("'{name}' is the system user's name (system_username)"),
            );
        }
        if let Err(e) = st.users.add(&name, &password, global) {
            return Resp::fail(400, &e);
        }
        if let Some((db, privs)) = on_db.filter(|(_, p)| !p.is_empty()) {
            if let Err(e) = st.users.grant_on(&name, &db, &privs) {
                let _ = st.users.drop_user(&name);
                return Resp::fail(400, &e);
            }
        }
        Resp::ok(&format!("user '{name}' created"))
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
        let is_admin = st.users.is_superuser(user);
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
        // With a `db_name`: on that database only; without, on every database.
        let db = match grant_db_param(req) {
            Ok(db) => db,
            Err(r) => return r,
        };
        let res = match (grant, db) {
            (true, Some(db)) => st.users.grant_on(&name, &db, &privs),
            (false, Some(db)) => st.users.revoke_on(&name, &db, &privs),
            (true, None) => st.users.grant(&name, &privs),
            (false, None) => st.users.revoke(&name, &privs),
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
            let global: Vec<&str> = u.privileges.iter().map(|p| p.as_str()).collect();
            let per_db: Vec<String> = u
                .db_privileges
                .iter()
                .map(|(db, ps)| {
                    let names: Vec<&str> = ps.iter().map(|p| p.as_str()).collect();
                    format!("{}:{}", json_str(db), json_str(&names.join(",")))
                })
                .collect();
            items.push(format!(
                "{{\"username\":{},\"privileges\":{},\"db_privileges\":{{{}}},\"is_root\":{}}}",
                json_str(&u.name),
                json_str(&global.join(",")),
                per_db.join(","),
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
        pick_db_name(st, db_param(req))
    }

    /// A required, validated database name from `db_name`/`name`/`db`.
    fn req_db_name(&self, req: &Request) -> Option<String> {
        let n = req
            .param("db_name")
            .or_else(|| req.param("name"))
            .or_else(|| req.param("db"))?;
        valid_db_name(&n).then_some(n)
    }

    /// The transaction id from `tid`/`txn`. Like gStore, the Workbench's
    /// `beginTime_tid` form is accepted too (the part after the last `_`).
    fn txn_id(&self, req: &Request) -> Option<u64> {
        let raw = req.param("tid").or_else(|| req.param("txn"))?;
        let raw = raw.trim();
        let digits = raw.rsplit_once('_').map_or(raw, |(_, t)| t);
        digits.parse::<u64>().ok().filter(|&t| t != 0)
    }
}

// --- free helpers ------------------------------------------------------------

// --- server-local paths named by clients --------------------------------------

/// Marker every backup this server writes (`/backup`, `/api`'s `backup`)
/// carries in its directory: the name of the database backed up there, so a
/// later backup of a *different* database cannot overwrite it, and a restore
/// knows whose data it holds ([`may_restore_from`]).
pub(crate) const BACKUP_OWNER_FILE: &str = "BACKUP_OF";

/// The database the backup at `dir` was recorded as a backup of (its
/// [`BACKUP_OWNER_FILE`]), if any.
pub(crate) fn backup_owner(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(BACKUP_OWNER_FILE))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Record the backup at `dir` as one of database `db`.
pub(crate) fn mark_backup(dir: &Path, db: &str) -> crate::Result<()> {
    crate::db::fsutil::atomic_write(&dir.join(BACKUP_OWNER_FILE), db.as_bytes())
}

/// Whether `user` may restore database `db` from the backup at `source`:
/// `root` and `admin` users any backup; anyone else — who holds the restore
/// privilege on `db` only — only a backup this server recorded as one of `db`
/// ([`backup_owner`]), so the privilege on one database never reads another
/// database's data.
pub(crate) fn may_restore_from(users: &UserStore, user: &str, db: &str, source: &Path) -> bool {
    user == ROOT_USER || users.is_superuser(user) || backup_owner(source).as_deref() == Some(db)
}

/// The database a path component named like upstream's backups
/// (`<db>.db_<YYYYmmddHHMMSS>`, or that with `.zip`) names, if it is one.
pub(crate) fn backup_name_db(component: &str) -> Option<&str> {
    let name = component.strip_suffix(".zip").unwrap_or(component);
    let (db, ts) = name.rsplit_once(&format!("{}_", crate::DB_SUFFIX))?;
    (ts.len() == 14 && ts.bytes().all(|b| b.is_ascii_digit()) && !db.is_empty()).then_some(db)
}

/// The server-local file a `/build?db_path=` may read: what the server's LOAD
/// policy allows (the same rules as SPARQL `LOAD`), and only a regular file —
/// never a device or FIFO that would never end.
fn build_source(policy: &LoadPolicy, path: &str) -> Result<PathBuf, Resp> {
    let refused = || {
        Resp::fail(
            403,
            &format!(
                "db_path '{path}' is not a readable file inside the directories the server's \
                 load policy allows (see gserver --load-dir)"
            ),
        )
    };
    match policy.resolve(path) {
        Ok(LoadSource::Local(file)) if fs::metadata(&file).is_ok_and(|m| m.is_file()) => Ok(file),
        Ok(LoadSource::Local(_)) | Err(_) => Err(refused()),
        Ok(LoadSource::Remote(_)) => Err(Resp::fail(
            400,
            &format!("db_path '{path}': building from a remote source is not supported"),
        )),
    }
}

/// Resolve a client-supplied path against `base`: a relative path is taken
/// relative to `base`, `..` components are refused, and the result — its
/// deepest existing ancestor canonicalized, so symlinks are resolved — must
/// lie strictly inside the canonical `base`.
fn confined_path(base: &Path, requested: &str) -> Result<PathBuf, String> {
    let mut joined = base.to_path_buf();
    let mut any = false;
    for c in Path::new(requested).components() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                joined = PathBuf::from(c.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => return Err("'..' is not allowed in a path".into()),
            Component::Normal(part) => {
                joined.push(part);
                any = true;
            }
        }
    }
    if !any {
        return Err("empty path".into());
    }
    let base = fs::canonicalize(base).map_err(|e| e.to_string())?;
    // Canonicalize the deepest existing ancestor; re-append the rest (plain
    // names, so nothing left to resolve).
    let mut rest = Vec::new();
    let mut existing = joined.as_path();
    let mut target = loop {
        match fs::canonicalize(existing) {
            Ok(c) => break c,
            Err(_) => match (existing.file_name(), existing.parent()) {
                (Some(f), Some(p)) => {
                    rest.push(f.to_os_string());
                    existing = p;
                }
                _ => return Err("unresolvable path".into()),
            },
        }
    };
    for part in rest.iter().rev() {
        target.push(part);
    }
    if target.starts_with(&base) && target != base {
        Ok(target)
    } else {
        Err("outside the directory".into())
    }
}

/// Where a `/backup` of database `name` may write: `requested` (default
/// `<name>.backup`) confined to `backup_dir`, not nested inside a directory
/// holding a database (another backup), and either new, empty, or an earlier
/// backup of `name` itself (see [`BACKUP_OWNER_FILE`]).
fn backup_target(backup_dir: &Path, name: &str, requested: Option<&str>) -> Result<PathBuf, Resp> {
    if let Err(e) = fs::create_dir_all(backup_dir) {
        return Err(Resp::fail(
            500,
            &format!("backup failed: cannot create the backup directory: {e}"),
        ));
    }
    let default = format!("{name}.backup");
    let requested = requested.unwrap_or(&default);
    let target = confined_path(backup_dir, requested).map_err(|why| {
        Resp::fail(
            403,
            &format!(
                "backup path '{requested}' refused ({why}): backups are written inside the \
                 server's backup directory '{}' (a relative path is taken relative to it)",
                backup_dir.display()
            ),
        )
    })?;
    let conflict = |what: &str| {
        Resp::fail(
            409,
            &format!("backup path '{}' {what}; choose another path", target.display()),
        )
    };
    let base = fs::canonicalize(backup_dir).unwrap_or_default();
    // A name of the form `<db>.db_<timestamp>` is where the `/api` layer
    // looks for `<db>`'s backups: none may be named after another database.
    if let Ok(rel) = target.strip_prefix(&base) {
        for c in rel.components() {
            let c = c.as_os_str().to_string_lossy();
            if backup_name_db(&c).is_some_and(|db| db != name) {
                return Err(conflict(&format!(
                    "is named like a backup of database '{}'",
                    backup_name_db(&c).unwrap_or_default()
                )));
            }
        }
    }
    let mut ancestor = target.parent();
    while let Some(dir) = ancestor.filter(|d| d.starts_with(&base) && *d != base) {
        if Database::storage_kind(dir).is_some() || dir.join(BACKUP_OWNER_FILE).exists() {
            return Err(conflict("lies inside another backup"));
        }
        ancestor = dir.parent();
    }
    match fs::symlink_metadata(&target) {
        Err(_) => Ok(target),
        Ok(m) if m.is_dir() => {
            let owner = fs::read_to_string(target.join(BACKUP_OWNER_FILE)).ok();
            let empty = fs::read_dir(&target).map(|mut d| d.next().is_none()).unwrap_or(false);
            if empty || owner.as_deref() == Some(name) {
                Ok(target)
            } else {
                Err(conflict(&format!("already holds data that is not a backup of '{name}'")))
            }
        }
        Ok(_) => Err(conflict("exists and is not a directory")),
    }
}

/// Where a `/restore` may read its backup from: `requested` inside
/// `backup_dir` (relative paths resolve there), or a directory the server's
/// LOAD policy lets clients read. Denials do not say whether the path exists.
fn restore_source(
    backup_dir: &Path,
    policy: &LoadPolicy,
    requested: &str,
) -> Result<PathBuf, Resp> {
    if let Ok(path) = confined_path(backup_dir, requested) {
        if path.is_dir() {
            return Ok(path);
        }
    }
    if Path::new(requested).is_absolute() {
        if let Some(path) = policy.allowed_local_dir(Path::new(requested)) {
            return Ok(path);
        }
    }
    Err(Resp::fail(
        403,
        &format!(
            "restore path '{requested}' refused: a backup is restored from inside the server's \
             backup directory '{}' (a relative path is taken relative to it) or from a directory \
             the server's load policy allows (see gserver --load-dir)",
            backup_dir.display()
        ),
    ))
}

/// Append a statement to `query.log` (no lock needed: `O_APPEND` keeps each
/// line whole, and the text may be large, so callers do it off the state lock).
pub(crate) fn log_query(root: &Path, db: &str, user: &str, sparql: &str) {
    append_log(
        root,
        "query.log",
        &format!("{}\t{}\t{}\t{}", now_millis(), db, user, sparql.replace('\n', " ")),
    );
}

/// The database a REST `/user/*` request grants on: `db_name` (validated, and
/// never the system database), or `None` — on every database.
fn grant_db_param(req: &Request) -> Result<Option<String>, Resp> {
    match req.param("db_name").filter(|s| !s.trim().is_empty()) {
        None => Ok(None),
        Some(db) if valid_db_name(&db) && db != crate::sysdb::SYSTEM_DB => Ok(Some(db)),
        Some(_) => Err(Resp::fail(400, "invalid db_name")),
    }
}

/// The request's `db_name`/`db` parameter, if any (see [`pick_db_name`]).
fn db_param(req: &Request) -> Option<String> {
    req.param("db_name").or_else(|| req.param("db"))
}

/// The target database of a request: the named one (`param`, validated), else
/// the only loaded one.
fn pick_db_name(st: &ApiState, param: Option<String>) -> Option<String> {
    if let Some(n) = param {
        return valid_db_name(&n).then_some(n);
    }
    if st.dbs.len() == 1 {
        return st.dbs.keys().next().cloned();
    }
    None
}

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
        QueryResult::Update { changed } => update_resp(*changed, &None),
    }
}

/// The error message of an UPDATE that failed after applying part of its
/// operations (which stay applied — and were saved, unless `warning` says the
/// save failed).
fn partial_update_msg(e: &GStoreError, warning: Option<String>) -> String {
    match warning {
        Some(w) => format!("{e} (operations before the failing one were applied; {w})"),
        None => format!("{e} (operations before the failing one were applied)"),
    }
}

/// The JSON answer to an applied UPDATE, with an optional `"warning"`.
fn update_resp(changed: usize, warning: &Option<String>) -> Resp {
    Resp::new(
        200,
        JSON,
        format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"success\",\"changed\":{changed}{}}}",
            warn_field(warning)
        )
        .into_bytes(),
    )
}

/// Parse a `/batchInsert` / `/batchRemove` payload into triples, returning the
/// RDF format actually used. Only RDF parsers run here — never the SPARQL
/// parser — so the payload can only ever denote ground triples.
///
/// * An explicit `format=` names the parser (`nt`/`ntriples`, `ttl`/`turtle`,
///   or another RDF format [`parse_rdf`] knows; bare `json` — which clients
///   commonly send as a *response* format — is ignored).
/// * Otherwise a payload that declares `@prefix`/`@base`/`PREFIX`/`BASE` is
///   Turtle, and anything else N-Triples — falling back to Turtle (a superset)
///   when the line-based N-Triples parse fails, e.g. for several statements on
///   one line. When both fail the N-Triples error is reported.
fn parse_batch_payload(
    format: Option<&str>,
    payload: &str,
) -> Result<(&'static str, Vec<Triple>), String> {
    let explicit = format
        .map(|f| f.trim().to_ascii_lowercase())
        .filter(|f| !f.is_empty() && f != "json");
    if let Some(f) = explicit {
        let canonical = match f.as_str() {
            "nt" | "ntriples" | "n-triples" => "ntriples",
            "ttl" | "turtle" => "turtle",
            "nq" | "nquads" | "n-quads" => "nquads",
            "rdfxml" | "rdf" | "xml" => "rdfxml",
            "trig" => "trig",
            "jsonld" | "json-ld" => "jsonld",
            other => return Err(format!("(unknown format '{other}'; use ntriples or turtle)")),
        };
        return parse_rdf(canonical, payload)
            .map(|ts| (canonical, ts))
            .map_err(|e| format!("({canonical}): {e}"));
    }
    if declares_turtle_prefixes(payload) {
        return crate::parser::turtle::parse_str(payload)
            .map(|ts| ("turtle", ts))
            .map_err(|e| format!("(turtle): {e}"));
    }
    match crate::parser::ntriples::parse_str(payload) {
        Ok(ts) => Ok(("ntriples", ts)),
        Err(nt_err) => crate::parser::turtle::parse_str(payload)
            .map(|ts| ("turtle", ts))
            .map_err(|_| format!("(ntriples): {nt_err}")),
    }
}

/// Whether an RDF payload declares Turtle prefixes/base (`@prefix`, `@base`,
/// or the SPARQL-style `PREFIX` / `BASE`) at the start of some line.
fn declares_turtle_prefixes(payload: &str) -> bool {
    payload.lines().any(|line| {
        let t = line.trim_start();
        let word = t
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(word.as_str(), "@prefix" | "@base")
            || (matches!(word.as_str(), "prefix" | "base") && t.len() > word.len())
    })
}

/// Whether a parsed triple is valid RDF data: an IRI or blank-node subject and
/// an IRI predicate (the RDF parsers accept any term in any position).
fn valid_ground_triple(t: &Triple) -> bool {
    matches!(t.subject, Term::Iri(_) | Term::Blank(_)) && matches!(t.predicate, Term::Iri(_))
}

/// Whether a SPARQL request is an UPDATE (mutating) statement, skipping any
/// leading `PREFIX`/`BASE` declarations.
pub(crate) fn is_update_sparql(s: &str) -> bool {
    // Classify by the *parser*, not a whitespace token scan. A leading
    // `# comment` line (the lexer skips comments) or unusual whitespace must not
    // be able to hide an UPDATE keyword from this check — otherwise a query-only
    // user could smuggle `# x\nINSERT DATA { … }` past the privilege gate and
    // have the query job execute it. A parse failure is treated as non-update:
    // it will fail to parse again when the job runs and therefore never
    // execute.
    matches!(
        crate::parser::sparql::parse(s),
        Ok(crate::parser::sparql::ast::Query::Update(_))
    )
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
        "jsonld" | "json" | "json-ld" => crate::parser::jsonld::parse_str(content),
        "rdfa" | "html" | "xhtml" => crate::parser::rdfa::parse_str(content),
        other => Err(crate::error::GStoreError::RdfParse {
            line: 0,
            msg: format!(
                "unknown RDF format '{other}' (expected turtle, ntriples, nquads, rdfxml, trig, jsonld, or rdfa)"
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

/// Validate a database name: 1 to 64 characters, each a letter or digit (of
/// any script, so `测试库` is a name, as upstream), `_`, `-` or `.`, with no
/// leading or trailing `.` and no `..` — a safe single path component that
/// never needs quoting in the user table (`query@<db>`, comma separated) or
/// the tab-separated logs. See [`DB_NAME_RULE`].
pub(crate) fn valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// The client address as the IP checks see it (`"unknown"` without one).
pub(crate) fn peer_ip(peer: Option<SocketAddr>) -> String {
    peer.map_or_else(|| "unknown".to_string(), |a| a.ip().to_string())
}

/// The rule [`valid_db_name`] checks, for error messages.
pub(crate) const DB_NAME_RULE: &str = "the database name may only contain letters, digits, '_', \
                                       '-' and inner '.'s (at most 64 characters)";

/// Append one line (with a trailing newline) to `root/file`, best effort.
pub(crate) fn append_log(root: &Path, file: &str, line: &str) {
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(file))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Milliseconds since the Unix epoch (for log timestamps).
pub(crate) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A fresh random password for the `system` user (decimal digits, like
/// upstream's `to_string(IdUtil::randNum())`).
fn new_system_password() -> String {
    let mut b = [0u8; 8];
    crate::http_users::random_bytes(&mut b);
    u64::from_le_bytes(b).to_string()
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
        // A leading comment must not hide an UPDATE from the privilege gate:
        // the lexer skips `#…` comments, so the query job would execute this.
        assert!(is_update_sparql("# harmless?\nINSERT DATA { <a> <b> <c> }"));
        assert!(is_update_sparql("  # x\n  DROP ALL"));
        assert!(!is_update_sparql("SELECT * WHERE { ?s ?p ?o }"));
        assert!(!is_update_sparql("ASK { ?s ?p ?o }"));
        // A SELECT with a trailing smuggled UPDATE must not parse (so it is
        // neither classified as, nor executed as, an update).
        assert!(!is_update_sparql("SELECT * WHERE { ?s ?p ?o } ; DROP ALL"));
        assert!(!is_update_sparql(
            "PREFIX : <http://x/> SELECT * WHERE { ?s ?p ?o }"
        ));
        // The SPARQL 1.1 update forms of Phase 3 are updates.
        for u in [
            "ADD <http://g/1> TO <http://g/2>",
            "MOVE SILENT DEFAULT TO GRAPH <http://g/2>",
            "COPY <http://g/1> TO DEFAULT",
            "WITH <http://g/1> DELETE { ?s ?p ?o } INSERT { ?o ?p ?s } WHERE { ?s ?p ?o }",
            "WITH <http://g/1> DELETE WHERE { ?s ?p ?o }",
            "DELETE { ?s ?p ?o } USING NAMED <http://g/1> WHERE { GRAPH ?g { ?s ?p ?o } }",
            "LOAD <file:///x.nt> INTO GRAPH <http://g/1>",
            "INSERT DATA { <a> <b> [ <c> ( 1 2 ) ] }",
            "CREATE GRAPH <http://g/3> ; COPY DEFAULT TO <http://g/3>",
        ] {
            assert!(is_update_sparql(u), "{u}");
        }
        // Graph-function and other Phase 2/3 SELECTs are reads.
        for q in [
            "SELECT (shortestPath(<a>, <b>, true, {<p>, !<q>}) AS ?v) WHERE {}",
            "SELECT (PR(true, {}, 3, alpha=0.85, maxIter=10, tol=0.01) AS ?r) WHERE {}",
            "SELECT (rs:SSSPLEN(?a, ?b) AS ?l) WHERE { ?a <p> ?b }",
            "SELECT ?x WHERE { ?x <p> ?y FILTER(?y IN (1, 2)) } VALUES ?x { <a> }",
            "CONSTRUCT WHERE { ?s <p> [ <q> ?o ] }",
        ] {
            assert!(!is_update_sparql(q), "{q}");
        }
    }

    #[test]
    fn validates_db_names() {
        assert!(valid_db_name("my_db-1"));
        assert!(valid_db_name("lubm.v2"));
        assert!(valid_db_name("测试库"));
        assert!(!valid_db_name(""));
        assert!(!valid_db_name("../etc"));
        assert!(!valid_db_name("a b"));
        assert!(!valid_db_name(".x"));
        assert!(!valid_db_name("x."));
        assert!(!valid_db_name("a..b"));
        assert!(!valid_db_name("a@b"));
        assert!(!valid_db_name("a,b"));
        assert!(!valid_db_name("*"));
    }

    /// A transaction's read snapshot divides integers in the database's mode.
    #[test]
    fn read_snapshot_keeps_the_integer_division_mode() {
        let q = "SELECT (7/2 AS ?x) WHERE {}";
        let cell = |snap: &ReadSnapshot| match snap.query(q, None).unwrap() {
            QueryResult::Select(rs) => rs.rows[0][0].clone().unwrap_or_default(),
            other => panic!("expected Select, got {other:?}"),
        };
        let mut db = Database::new("snap_div");
        assert_eq!(
            cell(&ReadSnapshot::capture(&db)),
            "\"3.5\"^^<http://www.w3.org/2001/XMLSchema#decimal>"
        );
        db.set_integer_division(IntegerDivision::Truncate);
        assert_eq!(
            cell(&ReadSnapshot::capture(&db)),
            "\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
    }

    /// A transaction's read snapshot answers like the database itself:
    /// registered IRI functions resolve (an unregistered IRI call is a query
    /// error since the engine's graph-function integration), and the graph
    /// functions share one CSR — the database's cached one when it had one.
    #[test]
    fn read_snapshot_sees_user_functions_and_shares_one_csr() {
        const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                            <http://ex/b> <http://ex/p> <http://ex/c> .\n";
        const SP: &str = "SELECT (shortestPathLen(<http://ex/a>, <http://ex/c>, true, {}) AS ?p) \
                          WHERE {}";
        const SP_JSON: &str =
            r#""{\"paths\":[{\"src\":\"<http://ex/a>\",\"dst\":\"<http://ex/c>\",\"length\":2}]}""#;
        fn cell(r: crate::Result<QueryResult>) -> String {
            match r.unwrap() {
                QueryResult::Select(rs) => rs.rows[0][0].clone().unwrap_or_default(),
                other => panic!("expected Select, got {other:?}"),
            }
        }
        let mut db = Database::build_from_str("snap_gfun", DATA).unwrap();
        db.register_function("http://ex/fn/double", |args| {
            Some(crate::query::Value::Int(args.first()?.as_f64()? as i64 * 2))
        });

        // No CSR cached at capture: the first graph-function read builds one,
        // later reads reuse it.
        let snap = ReadSnapshot::capture(&db);
        assert!(snap.csr.peek(0).is_none());
        let v = cell(snap.query("SELECT (<http://ex/fn/double>(21) AS ?v) WHERE {}", None));
        assert!(v.starts_with("\"42\""), "{v}");
        assert!(
            snap.csr.peek(0).is_none(),
            "built only when a query needs it"
        );
        assert_eq!(cell(snap.query(SP, None)), SP_JSON);
        let built = snap
            .csr
            .peek(0)
            .expect("built by the first graph-function read");
        assert_eq!(cell(snap.query(SP, None)), SP_JSON);
        assert!(Arc::ptr_eq(&built, &snap.csr.peek(0).unwrap()));

        // A CSR the database already cached is shared, not rebuilt.
        db.preload_csr().unwrap();
        let snap = ReadSnapshot::capture(&db);
        let seeded = snap.csr.peek(0).expect("seeded from the database");
        assert!(Arc::ptr_eq(&seeded, &db.cached_csr().unwrap()));
        assert_eq!(cell(snap.query(SP, None)), SP_JSON);
        assert!(Arc::ptr_eq(&seeded, &snap.csr.peek(0).unwrap()));
    }

    // --- in-process HTTP client tests: upload, export download, console,
    // task registry (mirrors the pattern in tests/dt_http_parity.rs) --------

    use std::net::TcpStream;

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
        std::thread::spawn(move || s2.serve_forever());
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

    #[test]
    fn cancel_enforces_task_ownership() {
        // #4 (LOW): a non-admin may cancel only tasks they started; another
        // non-admin is 403; an admin may cancel anyone's. The registry is
        // populated directly (a real query completes synchronously, leaving no
        // residue), then driven through the real served /cancel path.
        use crate::http_users::Privilege;
        use std::collections::BTreeSet;
        let (server, addr) = start("cancel_own");
        {
            let mut st = server.inner.state.lock().unwrap();
            st.users.add("alice", "pw", BTreeSet::from([Privilege::Update])).unwrap();
            st.users.add("bob", "pw", BTreeSet::from([Privilege::Update])).unwrap();
        }
        let (id, cancel) = server.inner.tasks.register("query", "d", "q", "alice");
        let target = format!("/cancel?id={id}");
        let cancel_as = |user: &str, pass: &str| -> u16 {
            let auth = basic(user, pass);
            let (status, _) =
                split_status_body(&raw_request(&addr, "POST", &target, Some(&auth), ""));
            status
        };
        let flag = || cancel.load(Ordering::Relaxed);
        let reset = || cancel.store(false, Ordering::Relaxed);
        assert!(Arc::ptr_eq(&server.task_cancel_flag(id).unwrap(), &cancel));

        assert_eq!(cancel_as("bob", "pw"), 403, "non-owner non-admin must be forbidden");
        assert!(!flag(), "a forbidden cancel must not set the flag");
        assert_eq!(cancel_as("alice", "pw"), 200, "the owner may cancel");
        assert!(flag(), "owner's cancel sets the flag");
        // Admin can cancel anyone's — reset and cancel alice's task as root.
        reset();
        assert_eq!(cancel_as("root", "root"), 200, "admin may cancel any task");
        assert!(flag(), "admin's cancel sets the flag");
    }

    /// Extract the first `"id":<n>` from a `/tasklist` JSON body.
    /// Like `raw_request`, but gives up after `timeout` (`None`) instead of
    /// waiting forever for a wedged server.
    /// Bad credentials cost a deliberately slow password hash (PBKDF2, also
    /// for an unknown user). That hash must run with the state lock released:
    /// otherwise every client sending bad credentials — no account needed —
    /// holds the server-wide lock for the whole hash, and every other request
    /// (`/cancel`, `/tasklist`, …) queues behind it. Sampled directly: while
    /// clients hammer bad credentials through Basic auth, `/login`, `/shutdown`
    /// and the `/api` protocol, the state lock must be free nearly always.
    #[test]
    fn bad_credentials_never_hash_under_the_state_lock() {
        let (server, addr) = start("pbkdf2_lock");
        let stop = Arc::new(AtomicBool::new(false));
        let bad = basic("nobody", "x");
        let wrong = basic("root", "wrong");
        let mut attackers = Vec::new();
        for i in 0..3 {
            let stop = Arc::clone(&stop);
            let (bad, wrong) = (bad.clone(), wrong.clone());
            attackers.push(std::thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let (s, _) = match (i + n) % 5 {
                        0 => split_status_body(&raw_request(&addr, "GET", "/status", Some(&bad), "")),
                        1 => split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&wrong), "")),
                        2 => split_status_body(&raw_request(
                            &addr,
                            "POST",
                            "/login",
                            None,
                            "username=root&password=wrong",
                        )),
                        3 => split_status_body(&raw_request(
                            &addr,
                            "POST",
                            "/shutdown",
                            None,
                            "username=root&password=wrong",
                        )),
                        _ => split_status_body(&raw_request(
                            &addr,
                            "POST",
                            "/api",
                            None,
                            r#"{"operation":"testConnect","username":"nobody","password":"x"}"#,
                        )),
                    };
                    // 403: the address got locked out (failed logins).
                    assert!(s == 401 || s == 200 || s == 403, "unexpected status {s}");
                    n += 1;
                }
                n
            }));
        }
        // Let the attackers get going, then sample the lock for a while.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (mut samples, mut busy) = (0u32, 0u32);
        let until = Instant::now() + std::time::Duration::from_millis(1500);
        while Instant::now() < until {
            samples += 1;
            if server.inner.state.try_lock().is_err() {
                busy += 1;
            }
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        // Authenticated requests are still served meanwhile (refused, 403,
        // once the failed logins from this same address locked it out).
        let root = basic("root", "root");
        let (s, _) = split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
        assert!(s == 200 || s == 403, "{s}");
        stop.store(true, Ordering::Relaxed);
        let attempts: usize = attackers.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(attempts > 0);
        assert!(
            busy * 4 < samples,
            "the state lock was held in {busy} of {samples} samples while bad credentials \
             were being checked"
        );
        assert!(
            server.inner.state.try_lock().is_ok(),
            "the lock is free once the attackers stop"
        );
        server.shutdown();
    }

    fn request_within(
        addr: &SocketAddr,
        method: &str,
        target: &str,
        auth: Option<&str>,
        body: &str,
        timeout: std::time::Duration,
    ) -> Option<(u16, String)> {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(timeout)).unwrap();
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
        s.read_to_string(&mut resp).ok()?;
        Some(split_status_body(&resp))
    }

    /// The `"TID"` of a `/begin` response.
    fn tid_of(begin_body: &str) -> String {
        let i = begin_body.find("\"TID\":\"").expect("TID") + "\"TID\":\"".len();
        begin_body[i..].chars().take_while(|c| c.is_ascii_digit()).collect()
    }

    /// No endpoint may wait for a database's lock while holding the server's
    /// state lock: while a (simulated) long query holds database `w`'s lock,
    /// a request to any endpoint touching `w` waits — but `/tasklist` (which
    /// needs the state lock, as do `/cancel`, other databases and a
    /// shutdown's cancellation step) still answers at once.
    #[test]
    fn a_busy_database_never_wedges_the_state_lock() {
        let (srv, addr) = start("wedge");
        let root = basic("root", "root");
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        let (s, b) = split_status_body(&raw_request(&addr, "POST", "/build?db_name=w", Some(&root), data));
        assert_eq!(s, 200, "build: {b}");
        // Inside the server's backup directory (the only place `/backup` writes).
        let backup = srv.root_dir().join("backups").join("w.bak");
        let backup = backup.display().to_string();

        // Run `target` while the db lock is held; `/tasklist` must stay live.
        let probe = |method: &'static str, target: String, body: &'static str| -> (u16, String) {
            let handle = locked(&srv.inner.state)
                .dbs
                .get("w")
                .cloned()
                .expect("w is loaded");
            let guard = locked(&handle);
            let auth = root.clone();
            let t = target.clone();
            let req = std::thread::spawn(move || {
                split_status_body(&raw_request(&addr, method, &t, Some(&auth), body))
            });
            std::thread::sleep(std::time::Duration::from_millis(150));
            let started = std::time::Instant::now();
            let tasklist = request_within(
                &addr,
                "GET",
                "/tasklist",
                Some(&root),
                "",
                std::time::Duration::from_secs(3),
            );
            let waited = started.elapsed();
            drop(guard);
            let resp = req.join().unwrap();
            assert!(
                matches!(tasklist, Some((200, _))),
                "/tasklist wedged ({waited:?}) behind {method} {target}"
            );
            resp
        };

        let w = "?db_name=w";
        let nt = "<http://ex/n> <http://ex/p> <http://ex/m> .";
        assert_eq!(probe("POST", format!("/batchInsert{w}"), nt).0, 200);
        assert_eq!(probe("POST", format!("/batchRemove{w}"), nt).0, 200);
        assert_eq!(probe("POST", format!("/upload{w}&format=ntriples"), nt).0, 200);
        assert_eq!(probe("POST", format!("/checkpoint{w}"), "").0, 200);
        assert_eq!(probe("POST", format!("/backup{w}&path={backup}"), "").0, 200);
        assert_eq!(probe("GET", format!("/export{w}"), "").0, 200);
        let (s, b) = probe("POST", format!("/begin{w}"), "");
        assert_eq!(s, 200, "{b}");
        let tid = tid_of(&b);
        let sel = "SELECT * WHERE { ?s ?p ?o }";
        assert_eq!(probe("POST", format!("/tquery?tid={tid}"), sel).0, 200);
        assert_eq!(probe("POST", format!("/commit?tid={tid}"), "").0, 200);
        let (_, b) = split_status_body(&raw_request(&addr, "POST", &format!("/begin{w}"), Some(&root), ""));
        let tid = tid_of(&b);
        assert_eq!(probe("POST", format!("/rollback?tid={tid}"), "").0, 200);
        assert_eq!(probe("POST", format!("/build{w}"), data).0, 200);
        assert_eq!(probe("POST", format!("/restore{w}&path={backup}"), "").0, 200);
        assert_eq!(probe("POST", format!("/unload{w}"), "").0, 200);
        let (s, b) = split_status_body(&raw_request(&addr, "POST", &format!("/load{w}"), Some(&root), ""));
        assert_eq!(s, 200, "load: {b}");
        assert_eq!(probe("POST", format!("/drop{w}"), "").0, 200);
        srv.shutdown();
    }

    /// A write whose request is still being read when a graceful shutdown
    /// finishes (check_point off) must not be acknowledged unless it was also
    /// saved: the shutdown retires every slot after its final save, so such a
    /// late write is refused instead of applied and silently lost.
    #[test]
    fn a_write_arriving_after_the_shutdown_save_is_never_acked_and_lost() {
        let root_dir = tmp_root("late_write");
        let server = Arc::new(
            ApiServer::bind(&root_dir, "root", "127.0.0.1:0")
                .unwrap()
                .with_check_point(false)
                .with_shutdown_grace(std::time::Duration::from_millis(50)),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = std::thread::spawn(move || s2.serve_forever());
        let root = basic("root", "root");
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        let (s, b) = split_status_body(&raw_request(&addr, "POST", "/build?db_name=s", Some(&root), data));
        assert_eq!(s, 200, "build: {b}");

        // A client that has sent its headers but not yet its body.
        let update = "INSERT DATA { <http://ex/late> <http://ex/p> <http://ex/x> }";
        let mut late = TcpStream::connect(addr).unwrap();
        late.write_all(
            format!(
                "POST /update?db_name=s HTTP/1.1\r\nHost: x\r\nAuthorization: {root}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                update.len()
            )
            .as_bytes(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        server.shutdown();
        let report = serving.join().unwrap();
        assert!(report.saved.is_empty(), "nothing was dirty: {report:?}");

        // The shutdown is over; now the body arrives.
        late.write_all(update.as_bytes()).unwrap();
        let mut resp = String::new();
        late.read_to_string(&mut resp).unwrap();
        let (status, body) = split_status_body(&resp);
        let mut db = Database::open(root_dir.join("s.db")).unwrap();
        let durable = db
            .query("ASK { <http://ex/late> <http://ex/p> <http://ex/x> }")
            .unwrap()
            == QueryResult::Ask(true);
        assert!(status != 200 || durable, "acknowledged ({body}) but not saved");
        assert_ne!(status, 200, "a write after the final save is refused: {body}");
        let _ = fs::remove_dir_all(&root_dir);
    }

    /// An UPDATE that fails part-way leaves its earlier operations applied;
    /// the database is then dirty like after any write — saved under
    /// check_point — so what readers see is what a restart finds. Same for a
    /// failing `/tquery` statement followed by `/commit`.
    #[test]
    fn a_partially_applied_update_is_saved() {
        let (srv, addr) = start("partial_update");
        let root = basic("root", "root");
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        for db in ["x", "y"] {
            let (s, b) = split_status_body(&raw_request(&addr, "POST", &format!("/build?db_name={db}"), Some(&root), data));
            assert_eq!(s, 200, "build: {b}");
        }
        // LOAD is refused by the server's restricted policy: the update fails
        // after its INSERT DATA was applied.
        let failing = |s: &str| {
            format!("INSERT DATA {{ <http://ex/{s}> <http://ex/p> <http://ex/d> }} ; LOAD <file:///nonexistent/x.ttl>")
        };
        let (s, b) = split_status_body(&raw_request(&addr, "POST", "/update?db_name=x", Some(&root), &failing("c")));
        assert_eq!(s, 400, "{b}");
        assert!(b.contains("were applied"), "the client is told: {b}");
        let on_disk = |db: &str, s: &str| {
            let mut d = Database::open(srv.root_dir().join(format!("{db}.db"))).unwrap();
            d.query(&format!("ASK {{ <http://ex/{s}> <http://ex/p> <http://ex/d> }}")).unwrap()
                == QueryResult::Ask(true)
        };
        assert!(on_disk("x", "c"), "the applied part was saved (check_point)");

        let (_, b) = split_status_body(&raw_request(&addr, "POST", "/begin?db_name=y", Some(&root), ""));
        let tid = tid_of(&b);
        let (s, _) = split_status_body(&raw_request(&addr, "POST", &format!("/tquery?tid={tid}"), Some(&root), &failing("t")));
        assert_eq!(s, 400);
        let (s, b) = split_status_body(&raw_request(&addr, "POST", &format!("/commit?tid={tid}"), Some(&root), ""));
        assert_eq!(s, 200, "{b}");
        assert!(on_disk("y", "t"), "the committed part was saved");
        srv.shutdown();
    }

    fn first_task_id(tasklist_json: &str) -> Option<u64> {
        let i = tasklist_json.find("\"id\":")? + "\"id\":".len();
        tasklist_json[i..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }

    #[test]
    fn tquery_runs_in_the_transaction_and_rollback_discards_it() {
        // #3: a /tquery statement must execute inside its transaction (so a
        // rollback discards it), not non-transactionally. Executing it
        // synchronously under the state lock also closes the commit/rollback
        // TOCTOU; here we assert the transactional semantics directly.
        let (_srv, addr) = start("tq");
        let root = basic("root", "root");
        let post = |target: &str, body: &str| -> (u16, String) {
            split_status_body(&raw_request(&addr, "POST", target, Some(&root), body))
        };
        assert_eq!(post("/build?db_name=t", "").0, 200);
        let (s, body) = post("/begin?db_name=t", "");
        assert_eq!(s, 200);
        let tid = body
            .split("\"TID\":\"")
            .nth(1)
            .and_then(|x| x.split('"').next())
            .expect("TID in begin response")
            .to_string();
        // Insert within the transaction, then roll back.
        assert_eq!(
            post(
                &format!("/tquery?tid={tid}"),
                "INSERT DATA { <http://a> <http://b> <http://c> }"
            )
            .0,
            200
        );
        assert_eq!(post(&format!("/rollback?tid={tid}"), "").0, 200);
        // The rolled-back insert must not persist.
        let (s, body) = split_status_body(&raw_request(
            &addr,
            "GET",
            "/sparql?db_name=t",
            Some(&root),
            "SELECT ?s WHERE { ?s ?p ?o }",
        ));
        assert_eq!(s, 200);
        assert!(
            !body.contains("http://a"),
            "a rolled-back tquery insert must not persist: {body}"
        );
    }

    /// A `/tquery` is a task like any other query: `/tasklist` lists it and
    /// `/cancel` aborts it (and so does a shutdown past its grace period, which
    /// cancels every registered task) — it must not hold the database lock
    /// unreachably until it finishes on its own.
    #[test]
    fn a_running_tquery_is_listed_and_cancellable() {
        let (_srv, addr) = start("tq_cancel");
        let root = basic("root", "root");
        let post = |target: &str, body: &str| -> (u16, String) {
            split_status_body(&raw_request(&addr, "POST", target, Some(&root), body))
        };
        let mut ttl = String::from("@prefix : <http://ex/> .\n");
        for i in 0..600 {
            ttl.push_str(&format!(":s{i} :p :o{i} .\n"));
        }
        assert_eq!(post("/build?db_name=t", &ttl).0, 200);
        let (s, body) = post("/begin?db_name=t", "");
        assert_eq!(s, 200, "{body}");
        let tid = tid_of(&body);
        // A FILTER over a cross product: checked for cancellation per row.
        let slow = "SELECT (COUNT(*) AS ?n) WHERE { ?a ?p1 ?b . ?c ?p2 ?d \
                    FILTER(REGEX(CONCAT(STR(?a), STR(?d), STR(?b), STR(?c)), \"(x+x+)+y\")) }";
        let (addr_q, root_q, tid_q) = (addr, root.clone(), tid.clone());
        let query = std::thread::spawn(move || {
            split_status_body(&raw_request(
                &addr_q,
                "POST",
                &format!("/tquery?tid={tid_q}"),
                Some(&root_q),
                slow,
            ))
        });
        let mut task = None;
        for _ in 0..5000 {
            let (_, body) = split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
            if let Some(id) = first_task_id(&body) {
                assert!(body.contains("\"db\":\"t\"") && body.contains("\"user\":\"root\""), "{body}");
                task = Some(id);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let task = task.expect("the running /tquery must be listed by /tasklist");
        let (s, body) = post(&format!("/cancel?id={task}"), "");
        assert_eq!(s, 200, "{body}");
        let (s, body) = query.join().unwrap();
        assert_eq!(s, 400, "a cancelled /tquery fails: {body}");
        assert!(body.to_lowercase().contains("cancel"), "{body}");
        // The task is gone and the transaction is still usable.
        let (_, body) = split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
        assert_eq!(first_task_id(&body), None, "{body}");
        let (s, body) = post(&format!("/tquery?tid={tid}"), "ASK { ?s ?p ?o }");
        assert_eq!(s, 200, "{body}");
        assert_eq!(post(&format!("/rollback?tid={tid}"), "").0, 200);
    }

    /// Classifying a SPARQL statement (a full parse) and parsing a batch
    /// payload — up to the request body cap — run with the state lock
    /// released, so a big request cannot stall `/cancel` and every other
    /// client behind it. Sampled directly while such requests are processed.
    #[test]
    fn statements_and_batch_payloads_are_parsed_off_the_state_lock() {
        let (server, addr) = start("parse_off_lock");
        let root = basic("root", "root");
        assert_eq!(
            split_status_body(&raw_request(&addr, "POST", "/build?db_name=b", Some(&root), "")).0,
            200
        );
        let mut nt = String::new();
        for i in 0..150_000 {
            nt.push_str(&format!("<http://ex/s{i}> <http://ex/p> \"value number {i}\" .\n"));
        }
        let mut values = String::from("SELECT * WHERE { VALUES ?x { ");
        for i in 0..150_000 {
            values.push_str(&format!("<http://ex/s{i}> "));
        }
        values.push_str("} }");
        let requests: Vec<(&str, String)> = vec![
            ("/batchRemove?db_name=b", nt.clone()),
            ("/batchRemove?db_name=b&format=ttl", nt),
            ("/sparql?db_name=b", values.clone()),
            ("/update?db_name=b", values),
        ];
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (stop, root) = (Arc::clone(&stop), root.clone());
            std::thread::spawn(move || {
                for (target, body) in requests {
                    let (s, b) = split_status_body(&raw_request(&addr, "POST", target, Some(&root), &body));
                    assert!(s == 200 || s == 400, "{target}: {s} {b}");
                }
                stop.store(true, Ordering::Relaxed);
            })
        };
        let (mut samples, mut busy) = (0u32, 0u32);
        while !stop.load(Ordering::Relaxed) {
            samples += 1;
            if server.inner.state.try_lock().is_err() {
                busy += 1;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        worker.join().unwrap();
        assert!(samples > 20, "the requests took too little time to sample: {samples}");
        // Parsing under the lock shows up as ~35% here; what remains without
        // it (copying the body out of the request) is ~5%.
        assert!(
            busy * 8 < samples,
            "the state lock was held in {busy} of {samples} samples while requests were parsed"
        );
        server.shutdown();
    }

    #[test]
    fn concurrent_cancel_aborts_running_query() {
        // THE core multi-threading goal: while one worker runs a slow query, a
        // concurrent POST /cancel (on another worker) must be deliverable and
        // must abort the query. This is only possible because the query runs
        // with the ApiState lock released — proven here by /tasklist being
        // answerable (it needs that lock) *while* the query is in flight.
        let (_srv, addr) = start("cancel_live");
        let root = basic("root", "root");

        // 100 triples -> the unconstrained 3-way self-join materializes ~1e6
        // rows, which takes long enough for the cancel to land mid-flight.
        let mut ttl = String::from("@prefix : <http://ex/> .\n");
        for i in 0..100 {
            ttl.push_str(&format!(":s{i} :p :o{i} .\n"));
        }
        let (status, body) =
            split_status_body(&raw_request(&addr, "POST", "/build?db_name=slow", Some(&root), &ttl));
        assert_eq!(status, 200, "build: {body}");

        // Kick off the slow query on its own thread.
        let addr_q = addr;
        let root_q = root.clone();
        let query_thread = std::thread::spawn(move || {
            raw_request(
                &addr_q,
                "POST",
                "/sparql?db_name=slow",
                Some(&root_q),
                "SELECT * WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }",
            )
        });

        // Concurrently poll /tasklist until the running task appears (proving the
        // ApiState lock is free during execution), then cancel it.
        let mut task_id = None;
        for _ in 0..3000 {
            let (_s, body) =
                split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
            if let Some(id) = first_task_id(&body) {
                task_id = Some(id);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let task_id = task_id.expect("the running query must be visible in /tasklist mid-flight");

        let (cstatus, cbody) = split_status_body(&raw_request(
            &addr,
            "POST",
            &format!("/cancel?id={task_id}"),
            Some(&root),
            "",
        ));
        assert_eq!(cstatus, 200, "the concurrent cancel must be accepted: {cbody}");

        // The query, whose cancel flag was flipped while it ran, returns the
        // cancellation error (HTTP 400 + "query cancelled").
        let (qstatus, qbody) = split_status_body(&query_thread.join().unwrap());
        assert_eq!(qstatus, 400, "a cancelled query returns 400: {qbody}");
        assert!(
            qbody.to_lowercase().contains("cancel"),
            "expected a cancellation error, got: {qbody}"
        );
    }

    #[test]
    fn handler_panic_returns_500_and_the_server_keeps_serving() {
        let (server, addr) = start("panic");
        let root = basic("root", "root");
        let req = |method: &str, target: &str, body: &str| {
            split_status_body(&raw_request(&addr, method, target, Some(&root), body))
        };
        assert_eq!(req("POST", "/build?db_name=p", "<http://ex/a> <http://ex/p> <http://ex/b> .").0, 200);

        // Panics while holding BOTH the state lock and db `p`'s lock.
        let (status, body) = req("GET", "/__test/panic?db_name=p", "");
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("\"StatusCode\":500") && body.contains("panicked"), "{body}");
        assert!(server.inner.state.is_poisoned(), "the test route poisoned the state lock");

        // Both poisoned locks are recovered: the server keeps answering.
        let (status, body) = req("POST", "/sparql?db_name=p", "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("http://ex/b"), "{body}");
        assert_eq!(req("POST", "/update?db_name=p", "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }").0, 200);
        assert_eq!(req("GET", "/status", "").0, 200);
        assert_eq!(req("GET", "/show", "").0, 200);
        // The access log still records requests (written outside the state lock).
        let (_, log) = req("GET", "/accesslog", "");
        assert!(log.contains("/__test/panic\t-\t500"), "{log}");
    }

    #[test]
    fn query_job_panic_unregisters_its_task() {
        let (_srv, addr) = start("panic_job");
        let root = basic("root", "root");
        let req = |method: &str, target: &str, body: &str| {
            split_status_body(&raw_request(&addr, method, target, Some(&root), body))
        };
        assert_eq!(req("POST", "/build?db_name=q", "").0, 200);
        // The test hook panics inside the deferred query job, holding the db lock.
        let (status, body) = req(
            "POST",
            "/sparql?db_name=q",
            "SELECT * WHERE { ?s ?p ?o } # __gstore_test_panic__",
        );
        assert_eq!(status, 500, "{body}");
        let (_, tasks) = req("GET", "/tasklist", "");
        assert!(tasks.contains("\"tasks\":[]"), "the panicked task is unregistered: {tasks}");
        let (status, _) = req("POST", "/sparql?db_name=q", "SELECT * WHERE { ?s ?p ?o }");
        assert_eq!(status, 200, "the poisoned db lock is recovered");
    }

    #[test]
    fn isolevel_parsing_maps_upstream_levels() {
        assert_eq!(TxnIsolation::parse("1"), Some(TxnIsolation::Serializable));
        assert_eq!(TxnIsolation::parse(" 2 "), Some(TxnIsolation::Snapshot));
        assert_eq!(TxnIsolation::parse("3"), Some(TxnIsolation::ReadCommitted));
        assert_eq!(TxnIsolation::parse("read_committed"), Some(TxnIsolation::ReadCommitted));
        assert_eq!(TxnIsolation::parse("Read-Committed"), Some(TxnIsolation::ReadCommitted));
        assert_eq!(TxnIsolation::parse("SNAPSHOT"), Some(TxnIsolation::Snapshot));
        assert_eq!(TxnIsolation::parse("serializable"), Some(TxnIsolation::Serializable));
        for bad in ["0", "4", "-1", "", "abc", "12"] {
            assert_eq!(TxnIsolation::parse(bad), None, "{bad:?}");
        }
        for level in [TxnIsolation::Serializable, TxnIsolation::Snapshot, TxnIsolation::ReadCommitted] {
            assert_eq!(TxnIsolation::parse(&level.code().to_string()), Some(level));
            assert_eq!(TxnIsolation::parse(level.name()), Some(level));
        }
    }

    #[test]
    fn batch_payload_is_parsed_as_rdf_never_as_sparql() {
        // The original injection payload is a parse error in every mode.
        let inject = "<a> <b> <c> } ; DROP ALL ; INSERT DATA { <x> <y> <z>";
        for fmt in [None, Some("nt"), Some("ttl"), Some("json")] {
            assert!(parse_batch_payload(fmt, inject).is_err(), "{fmt:?}");
        }
        // N-Triples by default, one statement per line.
        let (f, ts) = parse_batch_payload(None, "<http://a> <http://b> <http://c> .\n<http://a> <http://b> \"x\" .\n").unwrap();
        assert_eq!((f, ts.len()), ("ntriples", 2));
        // Several statements on one line fall back to Turtle (a superset).
        let (f, ts) = parse_batch_payload(None, "<http://a> <http://b> <http://c> . <http://d> <http://b> <http://c> .").unwrap();
        assert_eq!((f, ts.len()), ("turtle", 2));
        // @prefix / PREFIX select Turtle.
        let (f, ts) = parse_batch_payload(None, "@prefix : <http://ex/> .\n:a :p :b , :c .").unwrap();
        assert_eq!((f, ts.len()), ("turtle", 2));
        let (f, _) = parse_batch_payload(None, "PREFIX : <http://ex/>\n:a :p :b .").unwrap();
        assert_eq!(f, "turtle");
        // An explicit format wins; bare `json` (a response format) is ignored.
        let (f, _) = parse_batch_payload(Some("TTL"), "<http://a> <http://b> <http://c> .").unwrap();
        assert_eq!(f, "turtle");
        let (f, _) = parse_batch_payload(Some("json"), "<http://a> <http://b> <http://c> .").unwrap();
        assert_eq!(f, "ntriples");
        assert!(parse_batch_payload(Some("nt"), "@prefix : <http://ex/> .\n:a :p :b .").is_err());
        assert!(parse_batch_payload(Some("csv"), "a,b,c").unwrap_err().contains("unknown format"));
        // A SPARQL update is not RDF.
        assert!(parse_batch_payload(None, "INSERT DATA { <http://a> <http://b> <http://c> }").is_err());
        assert!(parse_batch_payload(None, "DROP ALL").is_err());
    }

    /// `/api` `loadcsr` (and `load` with `csr=1`) build the database's CSR
    /// ahead of time, and the graph-function queries that follow run on that
    /// very CSR (the same `Arc`, never rebuilt) until a write invalidates it.
    #[test]
    fn loadcsr_builds_the_csr_that_graph_function_queries_then_use() {
        let (server, addr) = start("loadcsr");
        let api = |body: &str| split_status_body(&raw_request(&addr, "POST", "/api", None, body));
        let op = |extra: &str| {
            format!(r#"{{"username":"root","password":"root","db_name":"gfn",{extra}}}"#)
        };
        let ttl = "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n";
        let file = tmp_root("loadcsr_data");
        fs::create_dir_all(&file).unwrap();
        let file = file.join("g.nt");
        fs::write(&file, ttl).unwrap();
        // Builds go through the server's directory confinement: allow ours.
        locked(&server.inner.state).load_policy =
            LoadPolicy::restricted().with_allowed_dir(file.parent().unwrap());
        let (s, b) = api(&op(&format!(
            r#""operation":"build","db_path":"{}""#,
            file.display()
        )));
        assert_eq!(s, 200, "{b}");
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        let (_, b) = api(&op(r#""operation":"load""#));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        let cached = || {
            let h = Arc::clone(locked(&server.inner.state).dbs.get("gfn").unwrap());
            let slot = locked(&h);
            slot.db.cached_csr()
        };
        assert!(cached().is_none(), "nothing built before loadcsr");
        let (_, b) = api(&op(r#""operation":"loadcsr""#));
        assert!(b.contains("CSR loaded successfully."), "{b}");
        let preloaded = cached().expect("loadcsr built the CSR");
        let (_, b) = api(&op(r#""operation":"loadcsr""#));
        assert!(b.contains("CSR already loaded."), "{b}");
        let (_, b) = api(&op(r#""operation":"show""#));
        assert!(b.contains("\"csr\":true"), "{b}");

        let sp = r#"SELECT (shortestPathLen(<http://ex/a>, <http://ex/c>, true, {}) AS ?l) WHERE {}"#;
        let query = op(&format!(r#""operation":"query","sparql":{}"#, json_str(sp)));
        let (_, b) = api(&query);
        assert!(b.contains("length"), "{b}");
        assert!(
            Arc::ptr_eq(&preloaded, &cached().unwrap()),
            "the graph-function query ran on the preloaded CSR"
        );
        // A write invalidates it; the next graph-function query rebuilds it,
        // and `load csr=1` on the loaded database builds it again too.
        let insert = r#"INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }"#;
        let (_, b) = api(&op(&format!(r#""operation":"query","sparql":{}"#, json_str(insert))));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        assert!(cached().is_none(), "a write drops the CSR");
        let (_, b) = api(&op(r#""operation":"show""#));
        assert!(b.contains("\"csr\":false"), "{b}");
        let (_, b) = api(&op(r#""operation":"load","csr":"1""#));
        assert!(b.contains("The database already load yet."), "{b}");
        let reloaded = cached().expect("load csr=1 built it");
        assert!(!Arc::ptr_eq(&preloaded, &reloaded));
        let (_, b) = api(&query);
        assert!(b.contains("length"), "{b}");
        assert!(Arc::ptr_eq(&reloaded, &cached().unwrap()));
        server.shutdown();
    }

    /// The server's query-evaluation settings (`conf.ini` `[query]`) reach
    /// every database it serves, whichever way it got loaded — REST build /
    /// load / upload-create / restore, `/api` load / restore, a dropped and
    /// recreated name — and a transaction's read snapshot; installing new
    /// settings (as `refreshconf` does) re-applies them to loaded databases.
    #[test]
    fn query_settings_reach_every_served_database() {
        use crate::query::{DpbMode, IntegerDivision, TopKOptions};
        let custom = QuerySettings {
            integer_division: IntegerDivision::Truncate,
            query_cache_bytes: 12_345,
            topk: TopKOptions {
                early_termination: false,
                bounded_heap: true,
                dpb: DpbMode::Off,
            },
            gfun_max_output_bytes: 777,
        };
        let root = tmp_root("qsettings");
        let server = Arc::new(
            ApiServer::bind(&root, "root", "127.0.0.1:0")
                .expect("bind")
                .with_query_settings(custom),
        );
        assert_eq!(server.query_settings(), custom);
        let addr = server.local_addr().expect("addr");
        let s2 = Arc::clone(&server);
        std::thread::spawn(move || s2.serve_forever());
        let auth = basic("root", "root");
        let rest = |method: &str, target: &str, body: &str| {
            split_status_body(&raw_request(&addr, method, target, Some(&auth), body))
        };
        let api = |body: String| split_status_body(&raw_request(&addr, "POST", "/api", None, &body));
        let op = |op: &str, db: &str| {
            format!(r#"{{"username":"root","password":"root","operation":"{op}","db_name":"{db}"}}"#)
        };
        let settings_of = |name: &str| {
            let h = Arc::clone(locked(&server.inner.state).dbs.get(name).expect("loaded"));
            let slot = locked(&h);
            slot.db.query_settings()
        };
        const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        let div = "SELECT (7/2 AS ?x) WHERE {}";

        // REST build.
        assert_eq!(rest("POST", "/build?db_name=one", DATA).0, 200);
        assert_eq!(settings_of("one"), custom);
        let (_, b) = rest("POST", "/sparql?db_name=one", div);
        assert!(b.contains("\"value\":\"3\""), "{b}");
        // REST unload + load.
        assert_eq!(rest("POST", "/unload?db_name=one", "").0, 200);
        assert_eq!(rest("POST", "/load?db_name=one", "").0, 200);
        assert_eq!(settings_of("one"), custom);
        // /api unload + load.
        let (_, b) = api(op("unload", "one"));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        let (_, b) = api(op("load", "one"));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        assert_eq!(settings_of("one"), custom);
        // REST upload into a database that does not exist yet (created).
        assert_eq!(rest("POST", "/upload?db_name=fresh&format=nt", DATA).0, 200);
        assert_eq!(settings_of("fresh"), custom);
        // REST backup + restore, then /api backup + restore.
        let (s, b) = rest("POST", "/backup?db_name=one", "");
        assert_eq!(s, 200, "{b}");
        let path = b.split("\"path\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        let (s, b) = rest("POST", &format!("/restore?db_name=one&path={path}"), "");
        assert_eq!(s, 200, "{b}");
        assert_eq!(settings_of("one"), custom);
        let (_, b) = api(op("backup", "one"));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        let (_, b) = api(op("restore", "one"));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        assert_eq!(settings_of("one"), custom);
        // Drop and recreate the same name.
        let (_, b) = api(op("drop", "fresh"));
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        assert_eq!(rest("POST", "/build?db_name=fresh", DATA).0, 200);
        assert_eq!(settings_of("fresh"), custom);
        // A transaction's read snapshot divides like the database.
        let (s, b) = rest("POST", "/begin?db_name=one", "");
        assert_eq!(s, 200, "{b}");
        let tid = b.split("\"TID\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        let (_, b) = rest("POST", "/sparql?db_name=one", div);
        assert!(b.contains("\"value\":\"3\""), "snapshot read: {b}");
        let (_, b) = rest("POST", &format!("/tquery?tid={tid}"), div);
        assert!(b.contains("\"value\":\"3\""), "transactional read: {b}");
        assert_eq!(rest("POST", &format!("/rollback?tid={tid}"), "").0, 200);

        // New settings (what refreshconf installs) reach the loaded ones.
        let other = QuerySettings::default();
        server.inner.apply_query_settings(other);
        assert_eq!(settings_of("one"), other);
        assert_eq!(settings_of("fresh"), other);
        let (_, b) = rest("POST", "/sparql?db_name=one", div);
        assert!(b.contains("\"value\":\"3.5\""), "{b}");
        server.shutdown();
    }

    #[test]
    fn invalid_ground_triples_are_not_applied() {
        let t = |s: Term, p: Term| Triple::new(s, p, Term::iri("http://o"));
        assert!(valid_ground_triple(&t(Term::iri("http://s"), Term::iri("http://p"))));
        assert!(valid_ground_triple(&t(Term::Blank("b".into()), Term::iri("http://p"))));
        assert!(!valid_ground_triple(&t(Term::plain_literal("lit"), Term::iri("http://p"))));
        assert!(!valid_ground_triple(&t(Term::iri("http://s"), Term::Blank("b".into()))));
    }
}
