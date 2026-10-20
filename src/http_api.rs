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
//!   mapping to [`Database::begin`]/[`commit`](Database::commit)/
//!   [`rollback`](Database::rollback). See *Transactions* below.
//! * **Database lifecycle** — `/build`, `/load`, `/unload`, `/drop`, `/show`.
//! * **Backup / restore / export** — `/backup`, `/restore`, `/export`.
//! * **Batch operations** — `/batchInsert`, `/batchRemove`: the payload is
//!   parsed as RDF (N-Triples, or Turtle) and the ground triples are applied
//!   directly — no SPARQL text is ever built from user input.
//! * **Logging & monitoring** — query / access / transaction logs and a live
//!   `/monitor` (and `/status`) JSON snapshot.
//!
//! **Durability.** Each loaded database tracks whether it has unsaved changes.
//! Under gStore's `check_point` policy (on by default,
//! [`ApiServer::with_check_point`]) every successful write — `/update`, an
//! update via `/sparql`, `/batchInsert`, `/batchRemove`, `/upload`, a `/commit`
//! that changed data — is saved to `<root>/<name>.db` before it is
//! acknowledged. With the policy off, changes stay in memory until
//! `/checkpoint` (which always forces a save), `/unload` (which saves a dirty
//! database before dropping it) or a graceful shutdown.
//!
//! **Graceful shutdown.** [`ApiServer::shutdown`] (or a
//! [`ShutdownHandle`] — e.g. wired to `SIGINT`/`SIGTERM` with
//! [`crate::server::shutdown_on_signals`]) stops accepting, lets in-flight
//! requests finish (bounded; queries still running after the grace period are
//! cancelled), rolls back open transactions, saves every dirty database, and
//! makes [`serve_forever`](ApiServer::serve_forever) return a
//! [`ShutdownReport`].
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
//! [`TxnIsolation`]); with a single writer every level is honoured. Unknown
//! transaction ids are `404`; committing or rolling back a finished one is
//! `409`.
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
//!
//! As with [`crate::server`], real TLS is out of scope — terminate HTTPS in a
//! reverse proxy and do not send credentials over plain `http://` in production.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::db::{Database, LoadPolicy};
use crate::dict::Dictionary;
use crate::error::GStoreError;
use crate::http_users::{Privilege, UserStore, ROOT_USER};
use crate::model::{Term, Triple};
use crate::query::{Evaluator, QueryResult};
use crate::server::{
    err_json, locked, ntriples_body, parse_basic_auth, run_accept_loop, spawn_worker,
    write_response_ext, write_response_maybe_gzip, Lifecycle, Request, ShutdownHandle,
    ShutdownReport, DEFAULT_SHUTDOWN_GRACE, PANIC_MSG,
};
use crate::sparql_results::{self, json_str, ResultFormat};
use crate::store::{TripleSource, TripleStore};

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
struct Txn {
    /// The database it operates on.
    db: String,
    /// The user who began it: only they (or `root`) may `/tquery`, `/commit` or
    /// `/rollback` it.
    owner: String,
    isolation: TxnIsolation,
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
/// in-memory footprint until it commits or rolls back. User-defined functions
/// and the VS-tree are not part of the snapshot (the VS-tree is only a
/// candidate-pruning accelerator, so results are unchanged).
struct ReadSnapshot {
    dict: Dictionary,
    store: TripleStore,
    named: BTreeMap<u32, TripleStore>,
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
        let mut eval = Evaluator::new(&self.dict, &self.store);
        if !self.named.is_empty() {
            eval = eval.with_named(&self.named);
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
struct OpenTxn {
    tid: u64,
    /// The committed state at `/begin` (see [`ReadSnapshot`]).
    snapshot: Arc<ReadSnapshot>,
    /// Whether a `/tquery` update changed data — if so, the commit marks the
    /// database dirty (and saves it under the check_point policy).
    changed: bool,
}

/// One loaded database plus its HTTP-layer bookkeeping, behind the per-db
/// mutex.
struct DbSlot {
    db: Database,
    /// Where this database is persisted (`<root>/<name>.db`).
    dir: PathBuf,
    /// The in-memory database has changes not yet saved to [`dir`](Self::dir).
    dirty: bool,
    /// The open HTTP transaction on this database, if any.
    txn: Option<OpenTxn>,
    /// Set once the database was unloaded, dropped or replaced while a request
    /// still held this handle: the slot then holds an empty placeholder that
    /// must never be queried, written or saved (it would overwrite `dir`).
    retired: bool,
}

/// A shared handle to one loaded database.
type DbHandle = Arc<Mutex<DbSlot>>;

impl DbSlot {
    /// A freshly loaded / built / restored database: clean when it matches
    /// what is on disk at `dir`. The server's `LOAD` policy is installed on it
    /// (clients must not make the server read arbitrary local files).
    fn handle(mut db: Database, dir: PathBuf, dirty: bool, policy: &LoadPolicy) -> DbHandle {
        db.set_load_policy(policy.clone());
        Arc::new(Mutex::new(DbSlot {
            db,
            dir,
            dirty,
            txn: None,
            retired: false,
        }))
    }

    /// Retire this slot (its database is being unloaded, dropped or replaced):
    /// drop the database now — closing its store files before anything on disk
    /// replaces them — and leave an empty placeholder that refuses to be saved.
    /// A request that cloned the handle earlier then sees [`retired`](Self::retired).
    fn retire(&mut self) {
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
    fn save(&mut self) -> crate::Result<()> {
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
    fn wrote(&mut self, check_point: bool) -> Option<String> {
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
    fn read(&mut self, sparql: &str) -> crate::Result<QueryResult> {
        match &self.txn {
            Some(open) => open.snapshot.query(sparql, None),
            None => self.db.query(sparql),
        }
    }

    /// The committed triple count (the snapshot's while a transaction is open).
    fn committed_triple_num(&self) -> u64 {
        match &self.txn {
            Some(open) => open.snapshot.triple_num(),
            None => self.db.triple_num(),
        }
    }
}

/// The `409` answer to a non-transactional write while a transaction is open
/// on the target database. Writes never silently join someone's transaction.
fn txn_conflict(name: &str, tid: u64) -> Resp {
    Resp::fail(
        409,
        &format!(
            "database '{name}' has an open transaction (TID {tid}); non-transactional writes \
             are rejected until it is committed or rolled back (send the statement to /tquery \
             with that tid instead)"
        ),
    )
}

/// One in-flight query/update task, tracked so `/tasklist` can list it and
/// `/cancel` can flag it for cancellation.
///
/// `cancel` is a live cooperative-cancellation flag: it is cloned into the
/// [`QueryJob`] that runs the query (off the [`ApiState`] lock) and threaded
/// into [`Database::query_cancellable`], which polls it and aborts an in-flight
/// read with [`GStoreError::Cancelled`](crate::GStoreError::Cancelled). A
/// concurrent `POST /cancel` sets it while the query is still executing.
struct TaskEntry {
    db: String,
    /// A short, log-safe snippet of the query text (newlines flattened,
    /// truncated) — not necessarily the full statement.
    query: String,
    user: String,
    started: Instant,
    cancel: Arc<AtomicBool>,
}

/// Mutable server state guarded by a single mutex. The registries here are all
/// cheap to touch, so this lock is only ever held briefly; the *databases* live
/// behind their own per-db locks ([`ApiState::dbs`]) so a long query holds only
/// that db's lock and leaves this one free — which is what lets a concurrent
/// `POST /cancel` reach the task registry while a query runs.
///
/// Lock order: this lock first, then a db lock — never the reverse.
struct ApiState {
    root: PathBuf,
    users: UserStore,
    /// Loaded, in-memory databases by name. Each is behind its own `Mutex` (and
    /// an `Arc` so a worker can clone the handle and run a query without holding
    /// the outer `ApiState` lock).
    dbs: HashMap<String, DbHandle>,
    /// Open transactions by id.
    txns: HashMap<u64, Txn>,
    /// The single open transaction per database (single-writer), if any.
    db_txn: HashMap<String, u64>,
    /// Recently finished transactions (`tid`, outcome), newest last, bounded by
    /// [`ENDED_TXN_MEMORY`].
    ended_txns: VecDeque<(u64, &'static str)>,
    /// In-flight query/update tasks by id (see [`TaskEntry`]).
    tasks: HashMap<u64, TaskEntry>,
    started: Instant,
    requests: u64,
    /// The `LOAD` policy installed on every database this server serves
    /// (default [`LoadPolicy::restricted`]: SPARQL `LOAD` reads nothing).
    load_policy: LoadPolicy,
}

impl ApiState {
    /// Unload database `name`, if loaded, *without* saving: its slot is retired
    /// (see [`DbSlot::retire`]) so its files are closed before the caller
    /// replaces or deletes them, and a request still holding the handle can
    /// neither use nor save it. Returns whether it was loaded.
    fn evict(&mut self, name: &str) -> bool {
        match self.dbs.remove(name) {
            Some(h) => {
                locked(&h).retire();
                true
            }
            None => false,
        }
    }

    fn remember_ended(&mut self, tid: u64, outcome: &'static str) {
        if self.ended_txns.len() >= ENDED_TXN_MEMORY {
            self.ended_txns.pop_front();
        }
        self.ended_txns.push_back((tid, outcome));
    }

    fn ended_outcome(&self, tid: u64) -> Option<&'static str> {
        self.ended_txns
            .iter()
            .rev()
            .find(|(t, _)| *t == tid)
            .map(|(_, o)| *o)
    }

    /// The on-disk directory of database `name`.
    fn db_dir(&self, name: &str) -> PathBuf {
        self.root.join(crate::db_dir_for(name))
    }
}

/// The parity HTTP API server. Construct with [`ApiServer::bind`], then drive it
/// with [`serve_forever`](ApiServer::serve_forever) (e.g. in a background
/// thread) until [`shutdown`](ApiServer::shutdown). Each accepted connection is
/// handled on its own worker thread.
pub struct ApiServer {
    /// Shared with every connection worker (behind an `Arc`).
    inner: Arc<ApiInner>,
    /// The listening socket; taken (closed) once the server has shut down.
    listener: Mutex<Option<TcpListener>>,
    addr: SocketAddr,
    life: Arc<Lifecycle>,
}

/// The shared, thread-safe part of an [`ApiServer`]: everything a connection
/// worker needs. Held behind an `Arc` so a worker only clones a pointer.
struct ApiInner {
    state: Mutex<ApiState>,
    /// The root directory (also in [`ApiState::root`]); kept here so the access
    /// log can be written even when a panicking handler poisoned the state lock.
    root: PathBuf,
    /// gStore's `check_point` policy: save a database after every successful
    /// write (update, batch, upload, commit). On by default.
    check_point: bool,
    /// Upper bound on how long a graceful shutdown waits for in-flight requests.
    grace: Duration,
}

/// The outcome of routing a request: either a fully-built response, or a query
/// deferred to run *after* the [`ApiState`] lock is released — so a concurrent
/// `POST /cancel` can reach the task registry while the query executes.
enum Dispatch {
    Done(Resp),
    Query(QueryJob),
    /// A response whose (possibly db-lock-contended) work runs with the
    /// `ApiState` lock released — the closure captures cloned [`DbHandle`]s and
    /// does the locking itself. Used by the read-only monitoring endpoints so a
    /// slow query holding one db's lock cannot wedge the state lock and make
    /// `POST /cancel` undeliverable.
    Deferred(Box<dyn FnOnce() -> Resp + Send>),
}

/// A query handed out of [`ApiInner::dispatch`] to run without the `ApiState`
/// lock held. It carries its own database handle and cancel flag.
struct QueryJob {
    /// The target database (its own lock, independent of `ApiState`).
    db: DbHandle,
    name: String,
    /// Shared with the [`TaskEntry`] in [`ApiState::tasks`]; a concurrent
    /// `/cancel` flips it and [`Database::query_cancellable`] observes it.
    cancel: Arc<AtomicBool>,
    /// The registry id to unregister once the query finishes.
    task_id: u64,
    sparql: String,
    /// Whether `sparql` is an UPDATE (classified by the parser at dispatch).
    is_update: bool,
    fmt: ResultFormat,
}

/// Unregisters a task when dropped — also when the query panics — so the
/// `/tasklist` registry never keeps a dead entry.
struct TaskGuard<'a> {
    inner: &'a ApiInner,
    id: u64,
}

impl Drop for TaskGuard<'_> {
    fn drop(&mut self) {
        locked(&self.inner.state).tasks.remove(&self.id);
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
        let users = UserStore::open(root.join("users.db"), root_password)?;
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        Ok(ApiServer {
            inner: Arc::new(ApiInner {
                state: Mutex::new(ApiState {
                    root: root.clone(),
                    users,
                    dbs: HashMap::new(),
                    txns: HashMap::new(),
                    db_txn: HashMap::new(),
                    ended_txns: VecDeque::new(),
                    tasks: HashMap::new(),
                    started: Instant::now(),
                    requests: 0,
                    load_policy: LoadPolicy::restricted(),
                }),
                root,
                check_point: true,
                grace: DEFAULT_SHUTDOWN_GRACE,
            }),
            listener: Mutex::new(Some(listener)),
            addr,
            life: Lifecycle::new(addr),
        })
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
    /// re-allow a directory with [`LoadPolicy::with_allowed_dir`].
    pub fn with_load_policy(self, policy: LoadPolicy) -> ApiServer {
        locked(&self.inner.state).load_policy = policy;
        self
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
        self.life.request_stop();
    }

    /// The live cancel flag for an in-flight task, if it still exists. The
    /// engine's cooperative-cancellation hook ([`Database::query_cancellable`])
    /// already consults this flag: `run_query` clones it into the [`QueryJob`]
    /// that runs the query, `POST /cancel` sets it, and `/tasklist` reports it.
    /// This accessor lets an embedder reach the same flag directly.
    pub fn task_cancel_flag(&self, task_id: u64) -> Option<Arc<AtomicBool>> {
        locked(&self.inner.state)
            .tasks
            .get(&task_id)
            .map(|t| Arc::clone(&t.cancel))
    }

    /// Accept connections until [`shutdown`](Self::shutdown), handling each on
    /// its own worker thread. A long-running query on one connection holds only
    /// its database's lock, so concurrent requests — crucially `POST /cancel` —
    /// proceed on other workers. Returns what the shutdown did.
    pub fn serve_forever(&self) -> ShutdownReport {
        let drained = run_accept_loop(&self.listener, &self.life, self.inner.grace, |stream, guard| {
            let inner = Arc::clone(&self.inner);
            spawn_worker(move || {
                let _in_flight = guard;
                let mut stream = stream;
                let _ = catch_unwind(AssertUnwindSafe(|| inner.handle(&mut stream)));
            });
        });
        self.finish_shutdown(drained)
    }

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS in-process
    /// using `config` (see [`crate::server::load_tls_config`]). Requires the
    /// `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        let drained = run_accept_loop(&self.listener, &self.life, self.inner.grace, |stream, guard| {
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

    /// The tail of a graceful shutdown, after the listener closed and in-flight
    /// requests had their grace period: cancel queries that are still running,
    /// roll back open transactions (uncommitted work is never persisted), and
    /// save every database with unsaved changes.
    fn finish_shutdown(&self, mut drained: bool) -> ShutdownReport {
        if !drained {
            for t in locked(&self.inner.state).tasks.values() {
                t.cancel.store(true, Ordering::Relaxed);
            }
            drained = self.life.wait_idle(Duration::from_secs(2));
        }
        let mut report = ShutdownReport {
            drained,
            ..ShutdownReport::default()
        };
        let mut st = locked(&self.inner.state);
        let mut tids: Vec<u64> = st.txns.keys().copied().collect();
        tids.sort_unstable();
        for tid in tids {
            let Some(txn) = st.txns.remove(&tid) else {
                continue;
            };
            st.db_txn.remove(&txn.db);
            if let Some(h) = st.dbs.get(&txn.db) {
                let mut slot = locked(h);
                let _ = slot.db.rollback();
                slot.txn = None;
            }
            st.remember_ended(tid, "rolled back");
            append_log(
                &st.root,
                "txn.log",
                &format!("{}\trollback\t{}\t{}\t(shutdown)", now_millis(), txn.db, tid),
            );
            report.rolled_back += 1;
        }
        let mut names: Vec<String> = st.dbs.keys().cloned().collect();
        names.sort();
        for name in names {
            let h = Arc::clone(&st.dbs[&name]);
            let mut slot = locked(&h);
            if slot.dirty {
                let dir = slot.dir.clone();
                match slot.save() {
                    Ok(()) => report.saved.push(dir),
                    Err(e) => report.failed.push((dir, e.to_string())),
                }
            }
        }
        report
    }
}

impl ApiInner {
    /// Handle one connection (one request). Generic over the transport so the
    /// same path serves plain TCP and `rustls` TLS streams. Everything between
    /// parsing and writing runs under `catch_unwind`: a panicking handler
    /// answers `500` (JSON) and the locks it poisoned are recovered by
    /// [`locked`], so the server keeps serving.
    fn handle<S: Read + Write>(&self, stream: &mut S) -> std::io::Result<()> {
        let req = match Request::parse(stream) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(_) => return write_response_ext(stream, 400, JSON, &[], &err_json("bad request")),
        };

        let (resp, user_disp) = catch_unwind(AssertUnwindSafe(|| self.respond(&req)))
            .unwrap_or_else(|_| (Resp::fail(500, PANIC_MSG), "-".to_string()));

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
        write_response_maybe_gzip(stream, resp.status, &resp.ctype, &extra, &resp.body, enc)
    }

    /// Route and execute one request, returning the response and the caller's
    /// display name for the access log.
    fn respond(&self, req: &Request) -> (Resp, String) {
        // Short critical section: count the request, resolve the caller, route.
        // A query endpoint returns `Dispatch::Query` so the actual (possibly
        // long) execution happens below with the `ApiState` lock released.
        let (dispatch, user_disp) = {
            let mut st = locked(&self.state);
            st.requests += 1;
            let user = self.resolve_user(&mut st, req);
            let user_disp = user.clone().unwrap_or_else(|| "-".to_string());
            let dispatch = self.dispatch(&mut st, req, user);
            (dispatch, user_disp)
        };

        // Run any deferred query with NO `ApiState` lock held: this is what
        // makes a concurrent `POST /cancel` (which needs that lock) deliverable
        // while the query runs.
        let resp = match dispatch {
            Dispatch::Done(r) => r,
            Dispatch::Query(job) => self.execute_query_job(job),
            Dispatch::Deferred(f) => f(),
        };
        (resp, user_disp)
    }

    /// Run a deferred [`QueryJob`] with only the target database's lock held
    /// (the `ApiState` lock is free), then unregister the task.
    ///
    /// The open-transaction check happens here, under the db lock, so it cannot
    /// race a concurrent `/begin`: a write is rejected while a transaction is
    /// open, and a read is answered from the transaction's begin-time snapshot
    /// (released from the db lock first, so it does not stall the owner's
    /// `/tquery`s). A write that changed data marks the database dirty and is
    /// saved before the lock is released under the check_point policy.
    fn execute_query_job(&self, job: QueryJob) -> Resp {
        // Declared before the db guard so it drops *after* it: the registry is
        // touched (state lock) only once the db lock is released.
        let _task = TaskGuard {
            inner: self,
            id: job.task_id,
        };
        let mut slot = locked(&job.db);
        #[cfg(test)]
        if job.sparql.contains("__gstore_test_panic__") {
            panic!("test-triggered panic inside a query job");
        }
        if slot.retired {
            return Resp::fail(
                404,
                &format!("database '{}' was unloaded while this request waited", job.name),
            );
        }
        if let Some(open) = &slot.txn {
            if job.is_update {
                return txn_conflict(&job.name, open.tid);
            }
            let snapshot = Arc::clone(&open.snapshot);
            drop(slot);
            return match snapshot.query(&job.sparql, Some(job.cancel)) {
                Ok(result) => render_result(job.fmt, &result),
                Err(e) => Resp::new(400, SPARQL_JSON, err_json(&e.to_string())),
            };
        }
        let result = slot.db.query_cancellable(&job.sparql, job.cancel);
        match result {
            Ok(QueryResult::Update { changed }) => {
                let warning = if changed > 0 {
                    slot.wrote(self.check_point)
                } else {
                    None
                };
                drop(slot);
                update_resp(changed, &warning)
            }
            Ok(result) => {
                drop(slot);
                render_result(job.fmt, &result)
            }
            Err(e) => Resp::new(400, SPARQL_JSON, err_json(&e.to_string())),
        }
    }

    /// Resolve the caller's identity from Basic auth or a session token.
    fn resolve_user(&self, st: &mut ApiState, req: &Request) -> Option<String> {
        if let Some((u, p)) = parse_basic_auth(req) {
            // Also migrates a legacy password hash on first successful use,
            // so Basic-auth-only clients get upgraded like `/login` users.
            if st.users.verify_and_upgrade(&u, &p) {
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

    fn dispatch(&self, st: &mut ApiState, req: &Request, user: Option<String>) -> Dispatch {
        let method = req.method.as_str();
        let path = req.path.as_str();

        // `/login`, and the static web console, are the only fully public
        // endpoints — the console itself only ever *reads* by calling back
        // into the (still auth-gated) `/sparql` from the browser.
        if path == "/login" {
            return Dispatch::Done(self.handle_login(st, req));
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

        // Query endpoints defer execution (`Dispatch::Query`) so the query runs
        // with the `ApiState` lock released; everything else runs under it here.
        match (method, path) {
            ("GET" | "POST", "/sparql") => return self.handle_query(st, req, &user),
            ("POST", "/update") => {
                return if st.users.has_priv(&user, Privilege::Update) {
                    self.handle_update(st, req, &user)
                } else {
                    Dispatch::Done(self.forbidden())
                };
            }
            ("POST", "/tquery") => return self.handle_tquery(st, req, &user),
            // Read-only monitoring endpoints defer their db reads off the state
            // lock (see Dispatch::Deferred) so they never wedge POST /cancel.
            ("GET", "/status") => return self.handle_status(st),
            ("GET", "/monitor") => return self.handle_monitor(st),
            ("GET" | "POST", "/show") => return self.handle_show(st),
            _ => {}
        }

        let resp = match (method, path) {
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
            ("POST", "/begin") => self.guarded(st, &user, Privilege::Update, |s, st| {
                s.handle_begin(st, req, &user)
            }),
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
                // Either privilege can cancel: Update (only your *own* long-running
                // work) or Admin (an operator cancelling anyone's).
                let is_admin = st.users.has_priv(&user, Privilege::Admin);
                if is_admin || st.users.has_priv(&user, Privilege::Update) {
                    self.handle_cancel(st, req, &user, is_admin)
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

    fn handle_query(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(sparql) = req.param("sparql").or_else(|| req.query_string()) else {
            return Dispatch::Done(Resp::fail(400, "missing sparql/query parameter"));
        };
        // A read endpoint: an UPDATE-shaped statement additionally needs Update.
        let is_update = is_update_sparql(&sparql);
        let need = if is_update {
            Privilege::Update
        } else {
            Privilege::Query
        };
        if !st.users.has_priv(user, need) {
            return Dispatch::Done(self.forbidden());
        }
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        self.run_query(st, &name, &sparql, is_update, req, user)
    }

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
        let is_update = is_update_sparql(&sparql);
        self.run_query(st, &name, &sparql, is_update, req, user)
    }

    /// Set up a SPARQL request against loaded database `name`: register the task,
    /// clone the target database handle + cancel flag, and return a
    /// [`Dispatch::Query`] so [`execute_query_job`](Self::execute_query_job) can
    /// run it *without* the `ApiState` lock held (letting a concurrent
    /// `POST /cancel` reach the task). Content negotiation is captured now; the
    /// query log is appended here too.
    fn run_query(
        &self,
        st: &mut ApiState,
        name: &str,
        sparql: &str,
        is_update: bool,
        req: &Request,
        user: &str,
    ) -> Dispatch {
        append_log(
            &st.root,
            "query.log",
            &format!("{}\t{}\t{}\t{}", now_millis(), name, user, sparql.replace('\n', " ")),
        );
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        // Clone the target database handle — it has its own lock, independent of
        // the `ApiState` lock, so the query runs without blocking `/cancel`.
        let Some(db) = st.dbs.get(name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        let (task_id, cancel) = self.register_task(st, name, sparql, user);
        Dispatch::Query(QueryJob {
            db,
            name: name.to_string(),
            cancel,
            task_id,
            sparql: sparql.to_string(),
            is_update,
            fmt,
        })
    }

    // ---- task registry / cancellation --------------------------------------

    /// Register a new in-flight task for `db`/`sparql`/`user`, returning its id
    /// and cancel flag. The [`TaskGuard`] in
    /// [`execute_query_job`](Self::execute_query_job) unregisters it once the
    /// query completes (success, error, or panic).
    fn register_task(
        &self,
        st: &mut ApiState,
        db: &str,
        sparql: &str,
        user: &str,
    ) -> (u64, Arc<AtomicBool>) {
        let mut id = new_id();
        while st.tasks.contains_key(&id) {
            id = new_id();
        }
        let snippet: String = sparql.replace(['\n', '\r', '\t'], " ").chars().take(200).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        st.tasks.insert(
            id,
            TaskEntry {
                db: db.to_string(),
                query: snippet,
                user: user.to_string(),
                started: Instant::now(),
                cancel: Arc::clone(&cancel),
            },
        );
        (id, cancel)
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

    fn handle_cancel(&self, st: &mut ApiState, req: &Request, user: &str, is_admin: bool) -> Resp {
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
                // A non-admin may only cancel tasks they started; admins cancel any.
                if !is_admin && t.user != user {
                    return Resp::fail(403, &format!("task {id} belongs to another user"));
                }
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
        let dir = st.db_dir(&name);
        // A rebuilt database replaces the loaded one: close its files first.
        st.evict(&name);
        if let Err(e) = db.save(&dir) {
            return Resp::fail(500, &format!("save failed: {e}"));
        }
        let triples = db.triple_num();
        let slot = DbSlot::handle(db, dir, false, &st.load_policy);
        st.dbs.insert(name.clone(), slot);
        Resp::ok(&format!("database '{name}' built ({triples} triples)"))
    }

    fn handle_load(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        // Re-loading an already-loaded database would silently discard its
        // unsaved in-memory changes; it is a no-op instead (as in gStore).
        if st.dbs.contains_key(&name) {
            return Resp::ok(&format!("database '{name}' already loaded"));
        }
        let dir = st.db_dir(&name);
        if !dir.is_dir() {
            return Resp::fail(404, &format!("database '{name}' not found on disk"));
        }
        // Any layout (snapshot, on-disk `kvstore.kv`, RocksDB): an on-disk /
        // RocksDB database keeps writing its updates through to `dir`.
        match Database::open(&dir) {
            Ok(db) => {
                let slot = DbSlot::handle(db, dir, false, &st.load_policy);
                st.dbs.insert(name.clone(), slot);
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
    /// gStore's `LoadFile`, which may target a not-yet-built db). The result is
    /// saved under the check_point policy.
    fn handle_upload(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if let Some(&tid) = st.db_txn.get(&name) {
            return txn_conflict(&name, tid);
        }
        let format = upload_format(req);
        let content = req.body_str();
        let triples = match parse_rdf(&format, &content) {
            Ok(ts) => ts,
            Err(e) => return Resp::fail(400, &format!("upload parse failed ({format}): {e}")),
        };
        let parsed = triples.len();

        if !st.dbs.contains_key(&name) {
            let dir = st.db_dir(&name);
            let (opened, fresh) = if dir.is_dir() {
                (Database::open(&dir), false)
            } else {
                (Database::build_from_str(name.clone(), ""), true)
            };
            match opened {
                // A fresh database exists only in memory so far: dirty, so that
                // it is persisted even if nothing ends up being inserted.
                Ok(db) => {
                    let slot = DbSlot::handle(db, dir, fresh, &st.load_policy);
                    st.dbs.insert(name.clone(), slot);
                }
                Err(e) => return Resp::fail(500, &format!("upload failed to open database: {e}")),
            }
        }
        let (inserted, warning) = {
            let dbh = st
                .dbs
                .get(&name)
                .expect("just inserted or already loaded above");
            let mut slot = locked(dbh);
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
                slot.wrote(self.check_point)
            } else {
                None
            };
            if let Some(e) = failure {
                return Resp::fail(
                    500,
                    &format!("upload stopped after inserting {inserted} triple(s): {e}"),
                );
            }
            (inserted, warning)
        };
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
    }

    /// `POST /unload?db_name=…` — save the database if it has unsaved changes,
    /// then drop it from memory. If the save fails it stays loaded (nothing is
    /// lost) and the error is returned.
    fn handle_unload(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if let Some(&tid) = st.db_txn.get(&name) {
            return Resp::fail(
                409,
                &format!("database '{name}' is busy in transaction {tid}; commit or roll back first"),
            );
        }
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        let saved = {
            let mut slot = locked(&dbh);
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
            saved
        };
        st.dbs.remove(&name);
        if saved {
            Resp::ok(&format!("database '{name}' saved and unloaded"))
        } else {
            Resp::ok(&format!("database '{name}' unloaded"))
        }
    }

    fn handle_drop(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.req_db_name(req) else {
            return Resp::fail(400, "missing or invalid db_name");
        };
        if st.db_txn.contains_key(&name) {
            return Resp::fail(409, "database is busy in a transaction");
        }
        // Retire (closing its store files) before the directory is deleted.
        let was_loaded = st.evict(&name);
        let dir = st.db_dir(&name);
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

    fn handle_show(&self, st: &ApiState) -> Dispatch {
        // Snapshot the loaded-db handles under the state lock; the directory
        // scan and per-db counts run with the state lock released (see
        // Dispatch::Deferred).
        let root = st.root.clone();
        let loaded: HashMap<String, DbHandle> =
            st.dbs.iter().map(|(n, d)| (n.clone(), Arc::clone(d))).collect();
        Dispatch::Deferred(Box::new(move || {
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
    fn handle_begin(&self, st: &mut ApiState, req: &Request, user: &str) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let isolation = match req.param("isolevel").filter(|s| !s.trim().is_empty()) {
            None => TxnIsolation::Serializable,
            Some(v) => match TxnIsolation::parse(&v) {
                Some(level) => level,
                None => {
                    return Resp::fail(
                        400,
                        &format!(
                            "invalid isolevel '{v}': the isolation level's value can only be \
                             1 (SERIALIZABLE), 2 (SNAPSHOT) or 3 (READ_COMMITTED)"
                        ),
                    )
                }
            },
        };
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        if let Some(&open) = st.db_txn.get(&name) {
            return Resp::fail(
                409,
                &format!("a transaction ({open}) is already open on database '{name}'"),
            );
        }
        let mut tid = new_id();
        while st.txns.contains_key(&tid) || st.ended_outcome(tid).is_some() {
            tid = new_id();
        }
        {
            let mut slot = locked(&dbh);
            let snapshot = Arc::new(ReadSnapshot::capture(&slot.db));
            if let Err(e) = slot.db.begin() {
                return Resp::fail(500, &format!("begin failed: {e}"));
            }
            slot.txn = Some(OpenTxn {
                tid,
                snapshot,
                changed: false,
            });
        }
        st.txns.insert(
            tid,
            Txn {
                db: name.clone(),
                owner: user.to_string(),
                isolation,
            },
        );
        st.db_txn.insert(name.clone(), tid);
        append_log(
            &st.root,
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
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":\"transaction started\",\"TID\":{},\"isolevel\":{},\"IsolationLevel\":{}}}",
            json_str(&tid.to_string()),
            isolation.code(),
            json_str(isolation.name()),
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    /// Look up open transaction `tid` for `user`: unknown ids are `404`, already
    /// finished ones `409`, and someone else's transaction `403` (only the
    /// owner or `root` may touch it).
    fn txn_for(&self, st: &ApiState, tid: u64, user: &str) -> Result<String, Resp> {
        let Some(txn) = st.txns.get(&tid) else {
            return Err(match st.ended_outcome(tid) {
                Some(outcome) => Resp::fail(
                    409,
                    &format!("transaction {tid} is not running (already {outcome})"),
                ),
                None => Resp::fail(404, &format!("no open transaction {tid}")),
            });
        };
        if txn.owner != user && user != ROOT_USER {
            return Err(Resp::fail(
                403,
                &format!("transaction {tid} belongs to another user"),
            ));
        }
        Ok(txn.db.clone())
    }

    fn handle_tquery(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(tid) = self.txn_id(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or malformed tid/txn parameter"));
        };
        let name = match self.txn_for(st, tid, user) {
            Ok(name) => name,
            Err(resp) => return Dispatch::Done(resp),
        };
        let Some(sparql) = req
            .param("sparql")
            .or_else(|| req.query_string())
            .or_else(|| Some(req.body_str()).filter(|s| !s.trim().is_empty()))
        else {
            return Dispatch::Done(Resp::fail(400, "missing sparql parameter"));
        };
        let need = if is_update_sparql(&sparql) {
            Privilege::Update
        } else {
            Privilege::Query
        };
        if !st.users.has_priv(user, need) {
            return Dispatch::Done(self.forbidden());
        }
        // Execute the transactional statement SYNCHRONOUSLY, still holding the
        // ApiState lock, so a concurrent /commit or /rollback cannot slip between
        // the transaction lookup above and this execution — a TOCTOU that would
        // otherwise apply the statement non-transactionally (after a rollback that
        // already returned OK) or record it in a different transaction's undo log.
        // Unlike a read query it is NOT deferred: transactional statements are
        // bounded writes, so the brief lock hold is an acceptable trade for
        // atomicity. (A deferred query still cannot be cancelled mid-flight here,
        // but a bounded write does not need /cancel the way a long SELECT does.)
        append_log(
            &st.root,
            "query.log",
            &format!("{}\t{}\t{}\t{}", now_millis(), name, user, sparql.replace('\n', " ")),
        );
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        let mut slot = locked(&dbh);
        if slot.txn.as_ref().map(|t| t.tid) != Some(tid) {
            return Dispatch::Done(Resp::fail(
                409,
                &format!("transaction {tid} is not open on database '{name}'"),
            ));
        }
        let result = slot.db.query(&sparql);
        if let Ok(QueryResult::Update { changed }) = &result {
            if *changed > 0 {
                if let Some(open) = slot.txn.as_mut() {
                    open.changed = true;
                }
            }
        }
        drop(slot);
        Dispatch::Done(match result {
            Ok(r) => render_result(fmt, &r),
            Err(e) => Resp::new(400, SPARQL_JSON, err_json(&e.to_string())),
        })
    }

    fn handle_txn_end(&self, st: &mut ApiState, req: &Request, user: &str, commit: bool) -> Resp {
        let Some(tid) = self.txn_id(req) else {
            return Resp::fail(400, "missing or malformed tid/txn parameter");
        };
        if let Err(resp) = self.txn_for(st, tid, user) {
            return resp;
        }
        let txn = st.txns.remove(&tid).expect("checked by txn_for");
        st.db_txn.remove(&txn.db);
        let verb = if commit { "commit" } else { "rollback" };
        st.remember_ended(tid, if commit { "committed" } else { "rolled back" });
        let result = st.dbs.get(&txn.db).map(|dbh| {
            let mut slot = locked(dbh);
            let open = slot.txn.take();
            if commit {
                // `Database::commit` keeps the writes even when flushing its
                // redo log fails, so the data changed either way.
                let committed = slot.db.commit();
                let warning = if open.is_some_and(|o| o.changed) {
                    slot.wrote(self.check_point)
                } else {
                    None
                };
                committed.map(|()| warning)
            } else {
                slot.db.rollback().map(|()| None)
            }
        });
        append_log(
            &st.root,
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
        match result {
            Some(Ok(warning)) => Resp::ok_warn(&format!("transaction {tid} {verb} done"), &warning),
            Some(Err(e)) => Resp::fail(500, &format!("{verb} failed: {e}")),
            None => Resp::fail(404, &format!("database '{}' is not loaded", txn.db)),
        }
    }

    /// `POST /checkpoint?db_name=…` — force a save now (whatever the
    /// check_point policy). Refused while a transaction is open, since the live
    /// database then holds uncommitted writes.
    fn handle_checkpoint(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let Some(dbh) = st.dbs.get(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        let mut slot = locked(dbh);
        if let Some(open) = &slot.txn {
            return Resp::fail(
                409,
                &format!(
                    "database '{name}' has an open transaction (TID {}); commit or roll back \
                     before checkpointing",
                    open.tid
                ),
            );
        }
        match slot.save() {
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
        let Some(dbh) = st.dbs.get(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        let slot = locked(dbh);
        // A backup of the live database would capture uncommitted writes.
        if let Some(open) = &slot.txn {
            return Resp::fail(
                409,
                &format!(
                    "database '{name}' has an open transaction (TID {}); commit or roll back \
                     before backing up",
                    open.tid
                ),
            );
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
        // Validate before touching the live database: the backup must hold one.
        if Database::storage_kind(&path).is_none() {
            return Resp::fail(
                500,
                &format!("restore failed: '{path}' does not contain a database backup"),
            );
        }
        let dir = st.db_dir(&name);
        // The restored copy replaces the loaded database (whose unsaved changes
        // a restore discards): close its files before they are replaced.
        st.evict(&name);
        // Copy the backup (any layout) into the database's own directory and
        // open it there — never open the backup in place, or later writes
        // would land in the backup.
        if let Err(e) = Database::restore_into(&path, &dir) {
            return Resp::fail(500, &format!("restore failed: {e}"));
        }
        match Database::open(&dir) {
            Ok(db) => {
                let slot = DbSlot::handle(db, dir, false, &st.load_policy);
                st.dbs.insert(name.clone(), slot);
                Resp::ok(&format!("database '{name}' restored"))
            }
            Err(e) => Resp::fail(500, &format!("restore failed to open the restored copy: {e}")),
        }
    }

    /// `GET /export?db_name=<name>[&download=0]` — the whole database as
    /// N-Triples (its committed state, while a transaction is open). Sent as a
    /// file download (`Content-Disposition: attachment`) by default; pass
    /// `download=0`/`false`/`no`/`off` for an inline (browser-rendered)
    /// response instead.
    fn handle_export(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let Some(dbh) = st.dbs.get(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        let exported = locked(dbh).read("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }");
        match exported {
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
    fn handle_batch(&self, st: &mut ApiState, req: &Request, user: &str, insert: bool) -> Resp {
        let Some(name) = self.pick_db(st, req) else {
            return Resp::fail(400, "missing or ambiguous db parameter");
        };
        let payload = req.param("triples").unwrap_or_else(|| req.body_str());
        if payload.trim().is_empty() {
            return Resp::fail(400, "no triples supplied");
        }
        let op = if insert { "batchInsert" } else { "batchRemove" };
        let (format, triples) = match parse_batch_payload(req.param("format").as_deref(), &payload)
        {
            Ok(parsed) => parsed,
            Err(e) => return Resp::fail(400, &format!("{op} payload is not valid RDF {e}")),
        };
        append_log(
            &st.root,
            "query.log",
            &format!(
                "{}\t{}\t{}\t{op} {} triple(s) ({format})",
                now_millis(),
                name,
                user,
                triples.len()
            ),
        );
        let Some(dbh) = st.dbs.get(&name) else {
            return Resp::fail(404, &format!("database '{name}' is not loaded"));
        };
        let mut slot = locked(dbh);
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
            slot.wrote(self.check_point)
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
        Dispatch::Deferred(Box::new(move || {
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

    fn handle_monitor(&self, st: &ApiState) -> Dispatch {
        let mut entries: Vec<(String, DbHandle)> = st
            .dbs
            .iter()
            .map(|(n, d)| (n.clone(), Arc::clone(d)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let uptime = st.started.elapsed().as_secs();
        let nreq = st.requests;
        let nusers = st.users.count();
        let ntxns = st.txns.len();
        Dispatch::Deferred(Box::new(move || {
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
fn is_update_sparql(s: &str) -> bool {
    // Classify by the *parser*, not a whitespace token scan. A leading
    // `# comment` line (the lexer skips comments) or unusual whitespace must not
    // be able to hide an UPDATE keyword from this check — otherwise a query-only
    // user could smuggle `# x\nINSERT DATA { … }` past the privilege gate and
    // have `run_query` execute it. A parse failure is treated as non-update: it
    // will fail to parse again in `run_query` and therefore never execute.
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
        // A leading comment must not hide an UPDATE from the privilege gate:
        // the lexer skips `#…` comments, so `run_query` would execute this.
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
            st.tasks.insert(
                42,
                TaskEntry {
                    db: "d".into(),
                    query: "q".into(),
                    user: "alice".into(),
                    started: Instant::now(),
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        let cancel_as = |user: &str, pass: &str| -> u16 {
            let auth = basic(user, pass);
            let (status, _) =
                split_status_body(&raw_request(&addr, "POST", "/cancel?id=42", Some(&auth), ""));
            status
        };
        let flag = || server.inner.state.lock().unwrap().tasks[&42].cancel.load(Ordering::Relaxed);
        let reset = || server
            .inner
            .state
            .lock()
            .unwrap()
            .tasks
            .get_mut(&42)
            .unwrap()
            .cancel
            .store(false, Ordering::Relaxed);

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

    #[test]
    fn invalid_ground_triples_are_not_applied() {
        let t = |s: Term, p: Term| Triple::new(s, p, Term::iri("http://o"));
        assert!(valid_ground_triple(&t(Term::iri("http://s"), Term::iri("http://p"))));
        assert!(valid_ground_triple(&t(Term::Blank("b".into()), Term::iri("http://p"))));
        assert!(!valid_ground_triple(&t(Term::plain_literal("lit"), Term::iri("http://p"))));
        assert!(!valid_ground_triple(&t(Term::iri("http://s"), Term::Blank("b".into()))));
    }
}
