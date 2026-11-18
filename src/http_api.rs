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
//!   directory (`/api` `drop` keeps a `<db>.db.bak` by default). `/build`
//!   and `/api` `build` take a `layout` (`snapshot`, `disk` or `rocksdb`;
//!   default `[storage] default_layout`, else `snapshot`): the on-disk and
//!   RocksDB layouts are built in bulk ([`crate::bulk::build`]), RocksDB
//!   only by a server built with the `rocksdb` feature.
//! * **Backup / restore / export** — `/backup`, `/restore`, `/export`. Backups
//!   are written only inside the server's backup directory (`<root>/backups`,
//!   or `conf.ini`'s `backup_path`) and never over another database's files;
//!   `/restore` reads from there, or from a directory the [`LoadPolicy`]
//!   allows. `/export` shares the `/api` `export`'s serializer (N-Quads when
//!   a named graph holds a triple) and streams it as a cancellable task.
//! * **Errors** — one classification for every protocol layer
//!   (`OpErrorKind::of`, review design#3): a lock another process holds
//!   is `409` on every endpoint, a storage failure `500`, a cancelled or
//!   timed-out statement `503`, a syntax error `400`; `/api` keeps HTTP 200
//!   and its `StatusCode`s.
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
//! are saved like any other change. A write whose save (or, with the policy
//! off, whose commit point) fails is never acknowledged: it stays applied in
//! memory but is answered as a failure, `"applied in memory but not durable:
//! <cause>"` — HTTP 500 on the REST endpoints, HTTP 200 with `StatusCode` 1005
//! on `/api`. An in-memory snapshot database saves it with its next successful
//! save; an on-disk database whose commit point failed is broken until it is
//! reopened (`/unload` + `/load`), which discards those writes
//! ([`Database::storage_failure`]). With the policy off,
//! changes stay in memory until `/checkpoint` (which always forces a save),
//! `/unload` (which saves a dirty database before dropping it) or a graceful
//! shutdown.
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
//! **Locking.** Every database has its own reader-writer lock (`DbCell`), and
//! no request ever waits for one while holding the server-wide state lock
//! (see `ApiState`): a long query blocks only *writers* of its database,
//! never other readers of it, `/cancel`, `/tasklist` or other databases. Nor
//! does slow per-request work run under the state lock: password hashing
//! (also for bad credentials), parsing a SPARQL statement (once, to route it
//! to a shared or an exclusive guard), and parsing a batch payload all happen
//! with it released. Every query — `/tquery` included — is a task that
//! `/tasklist` lists and `/cancel` (or a shutdown past its grace period) can
//! cancel.
//!
//! The rules (contract C8 of the Phase 6 plan):
//!
//! * **Shared** guards (`DbCell::read`): statement reads (a read statement
//!   by the transaction's owner included; a read of a transaction's
//!   snapshot takes it only to find the snapshot, see `ReadSnapshot`),
//!   export, backup, `loadcsr`, `monitor` / `schema` of a database whose
//!   statistics were never computed (later ones are refreshed by a
//!   background thread on a shared guard, off the request path, see
//!   `gapi::ops_db`), and the reasoning rules' Check. Everything they call
//!   on the [`Database`] takes `&self` and is synchronized inside it (the
//!   query cache, the CSR cache, the string-index cache, the PFN registry,
//!   the logs).
//! * **Exclusive** guards (`DbCell::write`): updates, begin / commit /
//!   rollback, checkpoint / save / `wrote`, upload, batch, unload / drop /
//!   restore / retire, the shutdown's final pass, reasoning Execute /
//!   Disable, and installing new query settings (only with
//!   `DbCell::try_write`: a database busy meanwhile takes them the next
//!   time a request finds it free, see `ApiInner::read_synced`), and
//!   publishing the figures of a write section that unwound (also only with
//!   `DbCell::try_write`, see `DbCell::figures`).
//! * **No guard** — monitoring: `/status`, `/monitor`, `/show`, `/api`
//!   `show` and `stat`, and `/api` `monitor` / `schema` once statistics
//!   were computed for the database (the last ones are reported while a
//!   refresh for newer data runs in the background), read counters each
//!   database publishes at the end of every exclusive section and after it
//!   is loaded (see `server::DbCounters`; `diskUsed` is a size measured
//!   there too, never a directory walk on the request path), so they
//!   answer while a long query, a write or a save's fsync holds the
//!   database.
//! * Lock order: `ApiState` (never wait for a database while holding it) →
//!   a database's guard → its rule directory's lock → the database's
//!   internals (`persist_lock` → disk sink → `persist_error` → pager) → the
//!   `DiskTriples` gate → the pager. Never the reverse. A non-transactional
//!   read of a transaction's on-disk snapshot takes the database's guard
//!   only to find the snapshot and releases it before it enters the gate
//!   (so a `/commit` — exclusive guard, then the gate's drain — never waits
//!   for it: the drain cancels it, see `ReadSnapshot`).
//! * Leaf locks, held for a few instructions with nothing called under
//!   them: the cancel bridge's registry (forwarding a request's cancel flag
//!   to an on-disk snapshot read's gate flag) and `gapi::ops_db`'s registry
//!   of running statistics refreshes.
//! * No thread holds two database guards, takes a guard of a database it
//!   already holds one of, or upgrades a shared guard to an exclusive one
//!   without dropping it first. A thread-local registry enforces the second
//!   rule in every build: a re-entrant `DbCell::read` / `DbCell::write`
//!   returns an error instead of deadlocking.
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
//! the user who began it: only that user (or a superuser — `root`, or a user
//! with the `admin` privilege) may `/tquery`, `/commit` or `/rollback` it,
//! and the owner may roll it back whatever their current privileges. One
//! idle (no statement running) for `txn_idle_timeout` seconds (default 600,
//! `0` never), or whose owner was deleted, is rolled back and logged as
//! aborted — by the next `/begin` on its database, the next request, or a
//! background pass (see `ApiInner::reap_idle_txns`). A commit is answered
//! as what happened: committed, rolled back when its commit point failed,
//! or applied but not durable when it could no longer be undone
//! ([`Database::commit_checked`]). While it is open, every non-transactional write to that
//! database (`/update`, `/batch*`, `/upload`, …) is rejected with `409` rather
//! than silently joining the transaction, and every non-transactional read is
//! answered from a snapshot of the committed state taken at `/begin`, so no
//! client ever observes uncommitted data. `/begin` copies no triple (see
//! `ReadSnapshot`): an in-memory database's snapshot shares its
//! copy-on-write indexes, which the transaction's first write copies once
//! (an open transaction that wrote holds the begin-time copy until it ends);
//! a Stream-mode on-disk database's snapshot is a read view of its store,
//! which the commit expires (reads in flight are cancelled and re-run on the
//! committed state); a RocksDB database is still copied whole, under the
//! shared guard (its readers go on meanwhile). `/begin`
//! accepts gStore's `isolevel`
//! (`1` SERIALIZABLE — the default, `2` SNAPSHOT, `3` READ_COMMITTED, see
//! [`TxnIsolation`]); with a single writer every level is honoured. Like
//! upstream, transaction ids are numbered per loaded database, starting at 1
//! after each load; `/begin` answers the database (`db_name`) with the id,
//! and `/tquery`, `/commit` and `/rollback` take it to tell apart an id open
//! on several databases (without it the id must be open on exactly one and
//! not recently ended on another — a retried `/commit` — else `409`).
//! Unknown transaction ids are `404`; committing or rolling back a finished
//! one is `409` (also when it ended in an earlier load of the database). The
//! transaction log's `<begin>_<tid>` form is accepted too and names only the
//! transaction that began then.
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
//! `X-Session-Token` header, or a `session=`/`token=` parameter of the URL —
//! never of the body). Root's default password is accepted from local
//! clients only (see `server::peer_is_trusted_local`). Lacking a
//! valid identity is `401`; lacking the required privilege is `403`.
//! Privileges are those of the system store ([`crate::sysdb`]) and decided
//! by the same rules as the `/api` layer: each endpoint counts as the
//! upstream operation it performs and is checked on the database it targets
//! (`/user/create`, `/user/grant` and `/user/revoke` grant on one database
//! with `db_name=`, else on every database); `/build` and `/drop` are
//! reserved to `root` (and `admin` users), as upstream.
//!
//! **HTTPS.** As with [`crate::server`], a build with the optional `tls`
//! feature terminates TLS in-process: `ApiServer::serve_forever_tls` with
//! a `rustls::ServerConfig` from `crate::server::load_tls_config`
//! (`gserver --tls-cert FILE --tls-key FILE`; a `gserver` built without the
//! feature refuses those options). The default, dependency-free build
//! speaks plain HTTP only: terminate HTTPS in front of it with a reverse
//! proxy, and do not send credentials over plain `http://` in production.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{
    Arc, Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError,
    Weak,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::analytics::csr::{CsrCache, CsrGraph};
use crate::config::GStoreConfig;
use crate::db::dirlock::DirLock;
use crate::db::{Database, LoadPolicy, LoadSource, QuerySettings, Statement, StorageKind};
use crate::dict::Dictionary;
use crate::error::GStoreError;
use crate::gapi::apilog::{ApiLogs, TxnState};
use crate::gapi::ipfilter::IpFilter;
use crate::gapi::tasks::{TaskConfig, TaskManager};
use crate::gapi::GapiConfig;
use crate::http_users::{
    CheckedCredentials, CredentialCheck, NewPassword, PasswordForm, Privilege, UserStore, ROOT_USER,
};
use crate::kvstore::{DiskReadView, DiskSource, ViewError};
use crate::model::{Term, Triple};
use crate::query::engine::EvalProviders;
use crate::query::{Evaluator, QueryResult};
// The tests below name it through `use super::*`.
#[cfg(test)]
use crate::query::IntegerDivision;
use crate::reason::rules::RULE_DIR;
use crate::reason::store::RuleStore;
use crate::server::{
    err_json, locked, ntriples_body, parse_basic_auth, run_accept_loop, serve_connection,
    unix_millis, write_response_ext, write_response_maybe_gzip, write_response_streamed,
    BadRequest, ConnMeta, CounterSnapshot, DbCounters, HttpOptions, Lifecycle, Request,
    ShutdownHandle, ShutdownReport, DEFAULT_SHUTDOWN_GRACE, PANIC_MSG,
};
use crate::sparql_results::{self, json_str, ResultFormat};
use crate::store::read_scope::ReadScope;
use crate::store::{TripleSource, TripleStore};
use crate::sysdb::SysDb;
use crate::zipio::ExportFormat;

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
    /// A body streamed from a file instead of `body` (a download): copied
    /// to the connection in bounded chunks by [`ApiInner::handle`].
    stream: Option<crate::gapi::envelope::FileBody>,
}

impl Resp {
    fn new(status: u16, ctype: &str, body: Vec<u8>) -> Resp {
        Resp {
            status,
            ctype: ctype.to_string(),
            body,
            extra: Vec::new(),
            stream: None,
        }
    }

    /// A `{"StatusCode":0,"StatusMsg":…}` success envelope (gStore-style).
    fn ok(msg: &str) -> Resp {
        Resp::new(200, JSON, status_json(0, msg))
    }

    /// A success that carries a `"warning"`: HTTP 200 `{"StatusCode":0,
    /// "StatusMsg":msg,"warning":…}` — for an operation that succeeded but
    /// has something to report (an unload or reopen of a database whose
    /// on-disk store failed discards its uncommitted changes). Never for a
    /// write whose save failed: that is [`ok_warn`](Self::ok_warn)'s failure.
    fn ok_with_warning(msg: &str, warning: &str) -> Resp {
        let body = format!(
            "{{\"StatusCode\":0,\"StatusMsg\":{}{}}}",
            json_str(msg),
            warn_field(&Some(warning.to_string()))
        );
        Resp::new(200, JSON, body.into_bytes())
    }

    /// The answer to a write: [`ok`](Self::ok) when it is durable, or — when
    /// `warning` reports that persisting it failed ([`DbSlot::wrote`]) — the
    /// failure of [`write_outcome`](Self::write_outcome).
    fn ok_warn(msg: &str, warning: &Option<String>) -> Resp {
        Resp::write_outcome(msg, "", warning)
    }

    /// The answer to a write with extra JSON `fields` (`,"key":value…`, or
    /// empty). Durable (`warning` is `None`): HTTP 200 `{"StatusCode":0,
    /// "StatusMsg":msg<fields>}`. Applied in memory but not durable (`warning`
    /// is the [`DbSlot::wrote`] text): a failure, never an acknowledgement —
    /// HTTP 500 `{"StatusCode":500,"StatusMsg":"applied in memory but not
    /// durable: <cause>"<fields>,"warning":…}`, the warning kept as the detail
    /// field earlier clients looked for (review durability#2).
    fn write_outcome(msg: &str, fields: &str, warning: &Option<String>) -> Resp {
        let (status, code, msg) = match warning {
            None => (200, 0, msg),
            Some(w) => (500, 500, w.as_str()),
        };
        let body = format!(
            "{{\"StatusCode\":{code},\"StatusMsg\":{}{fields}{}}}",
            json_str(msg),
            warn_field(warning)
        );
        Resp::new(status, JSON, body.into_bytes())
    }

    /// A `{"StatusCode":code,"StatusMsg":…}` error envelope at HTTP `status`.
    fn fail(status: u16, msg: &str) -> Resp {
        Resp::new(status, JSON, status_json(status as i64, msg))
    }
}

impl From<crate::gapi::GResp> for Resp {
    /// An `/api` response: a JSON envelope is always HTTP 200 (see
    /// [`crate::gapi::envelope`]); a file download stays streamed.
    fn from(g: crate::gapi::GResp) -> Resp {
        use crate::gapi::envelope::HttpBody;
        let (status, ctype, body, extra) = g.into_http();
        let mut resp = match body {
            HttpBody::Bytes(body) => Resp {
                status,
                ctype,
                body,
                extra: Vec::new(),
                stream: None,
            },
            HttpBody::File(file) => Resp::stream(status, &ctype, file),
        };
        resp.extra = extra;
        resp
    }
}

impl Resp {
    /// A response whose body is streamed from `file` (its `stream` field):
    /// a download, sent in bounded chunks.
    fn stream(status: u16, ctype: &str, file: crate::gapi::envelope::FileBody) -> Resp {
        let mut resp = Resp::new(status, ctype, Vec::new());
        resp.stream = Some(file);
        resp
    }
}

/// Build a `{"StatusCode":code,"StatusMsg":"msg"}` JSON body.
fn status_json(code: i64, msg: &str) -> Vec<u8> {
    format!("{{\"StatusCode\":{code},\"StatusMsg\":{}}}", json_str(msg)).into_bytes()
}

/// How every write that was applied in memory but whose save or commit point
/// failed is reported (REST, `/api` and the single-database server): the
/// message starts with this, followed by `": <cause>"`.
pub(crate) const NOT_DURABLE: &str = "applied in memory but not durable";

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
        let norm: String = if s
            .chars()
            .all(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '-' | ' '))
        {
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
    /// The user who began it: only they (or a superuser — `root`, or a user
    /// with the `admin` privilege) may `/tquery`, `/commit` or `/rollback`
    /// it; they may roll it back whatever their current privileges.
    pub(crate) owner: String,
    pub(crate) isolation: TxnIsolation,
    /// When it began (Unix milliseconds): the `<begin>` of the
    /// `<begin>_<tid>` form the transaction log shows (and the Workbench
    /// sends back), checked against it (see [`TidRef`]).
    pub(crate) begun: u128,
    /// The load generation of its database ([`DbSlot::load_gen`]): ids
    /// restart at 1 with every load, so it tells the transactions of two
    /// loads apart.
    pub(crate) generation: u64,
    /// The transaction clock ([`TxnReapState::now_ms`]) when its last
    /// statement ended (or it began): idle from then on.
    pub(crate) last_active: u64,
    /// Its statements in flight (counted from before they take the
    /// database's guard until they release it): a running statement is
    /// activity, so the transaction is never idle meanwhile.
    pub(crate) running: u32,
}

/// How many finished transaction ids are remembered so that a repeated
/// `/commit` or `/rollback` gets a precise "already committed" error.
const ENDED_TXN_MEMORY: usize = 1024;

/// A finished transaction ([`ApiState::ended_txns`]). Kept across reloads of
/// its database: the load generation and the begin time tell a retried
/// request for it apart from the transaction of the same number a later
/// load opened.
pub(crate) struct EndedTxn {
    pub(crate) db: String,
    pub(crate) tid: u64,
    /// Its database's load generation, when known.
    pub(crate) generation: Option<u64>,
    /// Its begin time (Unix milliseconds), when known.
    pub(crate) begun: Option<u128>,
    /// How it ended ("committed", "rolled back", …).
    pub(crate) outcome: &'static str,
}

/// How a transaction that a failed commit rolled back ended.
const ENDED_COMMIT_ROLLED_BACK: &str = "rolled back after its commit failed";
/// How a transaction ended whose commit failed after it could no longer be
/// undone.
const ENDED_NOT_DURABLE: &str = "left applied but not durable by a failed commit";
/// How an idle transaction ended.
const ENDED_IDLE: &str = "rolled back after txn_idle_timeout";
/// How the transaction of a deleted user ended.
const ENDED_ORPHAN: &str = "rolled back because its owner was deleted";

/// A transaction id as clients send it: a positive integer, or the
/// `<begin>_<tid>` form of the transaction log (upstream's Workbench sends
/// it back), whose begin time must then match the transaction's. One parser
/// for the REST endpoints and `/api` ([`parse_tid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TidRef {
    pub(crate) tid: u64,
    /// The `<begin>` part (Unix milliseconds), when given.
    pub(crate) begun: Option<u128>,
}

impl TidRef {
    /// A bare id.
    pub(crate) fn plain(tid: u64) -> TidRef {
        TidRef { tid, begun: None }
    }
}

/// Parse a transaction id ([`TidRef`]): digits only (surrounding
/// whitespace aside) — no sign, no other separator — and not 0.
pub(crate) fn parse_tid(raw: &str) -> Option<TidRef> {
    let raw = raw.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (begun, tid) = match raw.split_once('_') {
        Some((b, t)) if digits(b) => (Some(b.parse::<u128>().ok()?), t),
        Some(_) => return None,
        None => (None, raw),
    };
    if !digits(tid) {
        return None;
    }
    let tid = tid.parse::<u64>().ok().filter(|&t| t != 0)?;
    Some(TidRef { tid, begun })
}

/// Whether an open transaction may go on, or why it no longer counts as
/// running (see [`ApiInner::txn_liveness`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxnLiveness {
    /// Running (or idle for less than `txn_idle_timeout`).
    Live,
    /// Idle for `txn_idle_timeout` or longer, no statement in flight.
    Idle,
    /// Its owner no longer exists.
    Orphaned,
}

impl TxnLiveness {
    /// How it ends when it is reaped (`None` while live).
    fn ended_as(self) -> Option<&'static str> {
        match self {
            TxnLiveness::Live => None,
            TxnLiveness::Idle => Some(ENDED_IDLE),
            TxnLiveness::Orphaned => Some(ENDED_ORPHAN),
        }
    }
}

/// The idle-transaction reaper's state, one per server
/// ([`ApiInner::txn_reap`]): the earliest idle deadline (read with no lock
/// on every request), the server's transaction clock, and the periodic
/// reaper's wake-ups and passes.
pub(crate) struct TxnReapState {
    /// The earliest time ([`now_ms`](Self::now_ms)) at which an open
    /// transaction may have become reapable — idle past `txn_idle_timeout`,
    /// or found without an owner — `u64::MAX` when none is known. Lowered
    /// (never raised, see [`arm`](Self::arm)) under the state lock by
    /// whoever stamps activity, re-computed by each reap pass; a value in
    /// the past makes the next request run a pass.
    pub(crate) at: AtomicU64,
    /// A test-only shift of the transaction clock
    /// ([`debug_advance_txn_clock`]).
    skew_ms: AtomicU64,
    /// The periodic reaper's (wake-ups asked for, the wake-up count seen
    /// at the start of its last finished pass).
    beat: Mutex<(u64, u64)>,
    beat_changed: std::sync::Condvar,
    /// Whether this server already marked the transactions a previous
    /// process left `RUNNING` in the transaction log as `ABORTED`.
    swept: AtomicBool,
}

impl Default for TxnReapState {
    fn default() -> TxnReapState {
        TxnReapState {
            at: AtomicU64::new(u64::MAX),
            skew_ms: AtomicU64::new(0),
            beat: Mutex::new((0, 0)),
            beat_changed: std::sync::Condvar::new(),
            swept: AtomicBool::new(false),
        }
    }
}

impl TxnReapState {
    /// The transaction clock: monotonic milliseconds since the process
    /// first read it (plus the test skew).
    pub(crate) fn now_ms(&self) -> u64 {
        static BASE: OnceLock<Instant> = OnceLock::new();
        let base = BASE.get_or_init(Instant::now);
        (base.elapsed().as_millis() as u64).saturating_add(self.skew_ms.load(Ordering::SeqCst))
    }

    /// Make a pass due no later than `at` (called under the state lock, so
    /// a pass that re-computes the deadline never loses it).
    pub(crate) fn arm(&self, at: u64) {
        self.at.fetch_min(at, Ordering::SeqCst);
    }

    fn beat(&self) -> std::sync::MutexGuard<'_, (u64, u64)> {
        self.beat.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wake the periodic reaper now; returns the wake-up's number.
    fn wake(&self) -> u64 {
        let mut beat = self.beat();
        beat.0 += 1;
        self.beat_changed.notify_all();
        beat.0
    }

    /// A periodic pass that started after wake-up `seen` finished: tell
    /// whoever waits for one.
    fn finished_pass(&self, seen: u64) {
        let mut beat = self.beat();
        beat.1 = beat.1.max(seen);
        self.beat_changed.notify_all();
    }
}

/// The servers whose transaction lifecycle the hidden test hooks reach, by
/// root directory (registered when they start serving).
static TXN_SERVERS: Mutex<Vec<(PathBuf, Weak<ApiInner>)>> = Mutex::new(Vec::new());

/// The serving [`ApiServer`] whose root is `root` (the path it was bound
/// with), if any.
fn txn_server(root: &Path) -> Option<Arc<ApiInner>> {
    let servers = TXN_SERVERS.lock().unwrap_or_else(PoisonError::into_inner);
    servers
        .iter()
        .rev()
        .filter(|(r, _)| r == root)
        .find_map(|(_, w)| w.upgrade())
}

/// Test hook: move the transaction clock of the server serving `root`
/// forward by `by` — what `txn_idle_timeout` is measured on. Returns whether
/// such a server is serving.
#[doc(hidden)]
pub fn debug_advance_txn_clock(root: &Path, by: Duration) -> bool {
    match txn_server(root) {
        Some(inner) => {
            let ms = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
            inner.txn_reap.skew_ms.fetch_add(ms, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

/// Test hook: wake the periodic idle-transaction reaper of the server
/// serving `root` now (its explicit trigger; it also runs every
/// `min(txn_idle_timeout / 4, 30 s)`). Returns the wake-up's number for
/// [`debug_wait_txn_reaper_pass`], `None` when no such server serves.
#[doc(hidden)]
pub fn debug_wake_txn_reaper(root: &Path) -> Option<u64> {
    txn_server(root).map(|inner| inner.txn_reap.wake())
}

/// Test hook: wait until the periodic reaper of the server serving `root`
/// finished a pass that started after wake-up `wake` (from
/// [`debug_wake_txn_reaper`]) — so it read the transaction clock after
/// everything the test did before waking it. `hang_guard` bounds the wait;
/// `false` when it ran out (or no such server serves).
#[doc(hidden)]
pub fn debug_wait_txn_reaper_pass(root: &Path, wake: u64, hang_guard: Duration) -> bool {
    let Some(inner) = txn_server(root) else {
        return false;
    };
    let state = &inner.txn_reap;
    let (beat, _) = state
        .beat_changed
        .wait_timeout_while(state.beat(), hang_guard, |b| b.1 < wake)
        .unwrap_or_else(PoisonError::into_inner);
    beat.1 >= wake
}

/// Where a test can park a step of a transaction or a statement
/// ([`debug_park_txn`]; the single-database [`Server`](crate::server::Server)
/// knows the two statement points too).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnParkPoint {
    /// A statement (inside a transaction or not) is about to wait for its
    /// database's guard; its `query_timeout` has not started.
    StatementQueued,
    /// A statement (inside a transaction or not) holds its database's guard
    /// and its `query_timeout` has started; it is about to be evaluated.
    StatementHolding,
    /// A `/tquery` statement has ended (its activity is stamped) but still
    /// holds its database's guard.
    StatementEnded,
    /// A RocksDB `/begin` is copying the database for its read snapshot,
    /// holding the shared guard.
    BeginCapture,
    /// A RocksDB `/begin` has its copy and holds no guard yet (a write may
    /// slip in before it registers the transaction).
    BeginCaptured,
    /// A RocksDB `/begin` found another `/begin` of the same database
    /// copying it, and is about to wait for that one to finish (it then
    /// sees the transaction that one opened, and copies nothing).
    BeginQueued,
    /// A `/commit` or `/rollback` was accepted (its transaction was live,
    /// and it now counts as activity) and is about to wait for the
    /// database's guard.
    EndQueued,
}

/// A parked step: [`wait_parked`](Self::wait_parked) until a thread reaches
/// it, [`release`](Self::release) to let it (and later ones) go on.
/// Released and unregistered when dropped.
#[doc(hidden)]
pub struct TxnPark {
    db: String,
    point: TxnParkPoint,
    gate: Arc<ParkGate>,
}

#[derive(Default)]
struct ParkGate {
    /// (threads that reached the point, released)
    state: Mutex<(u64, bool)>,
    changed: std::sync::Condvar,
}

/// The parks armed by tests (normally none: one atomic read per point).
static TXN_PARKS: Mutex<Vec<(String, TxnParkPoint, Arc<ParkGate>)>> = Mutex::new(Vec::new());
static TXN_PARKS_ARMED: AtomicBool = AtomicBool::new(false);

/// Test hook: park every thread that reaches `point` for database `db`
/// until the returned handle is released or dropped.
#[doc(hidden)]
pub fn debug_park_txn(db: &str, point: TxnParkPoint) -> TxnPark {
    let gate = Arc::new(ParkGate::default());
    let mut parks = TXN_PARKS.lock().unwrap_or_else(PoisonError::into_inner);
    parks.push((db.to_string(), point, Arc::clone(&gate)));
    TXN_PARKS_ARMED.store(true, Ordering::SeqCst);
    TxnPark {
        db: db.to_string(),
        point,
        gate,
    }
}

impl TxnPark {
    /// Wait (at most `hang_guard`) until a thread is parked here.
    pub fn wait_parked(&self, hang_guard: Duration) -> bool {
        self.wait_parked_n(1, hang_guard)
    }

    /// Wait (at most `hang_guard`) until `n` threads have reached this
    /// point (parked, or passed it once released).
    pub fn wait_parked_n(&self, n: u64, hang_guard: Duration) -> bool {
        let g = self
            .gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (g, _) = self
            .gate
            .changed
            .wait_timeout_while(g, hang_guard, |(reached, _)| *reached < n)
            .unwrap_or_else(PoisonError::into_inner);
        g.0 >= n
    }

    /// How many threads have reached this point so far (parked, or passed
    /// it once released).
    pub fn parked(&self) -> u64 {
        self.gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// Let the parked thread (and every later one) go on.
    pub fn release(&self) {
        let mut g = self
            .gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        g.1 = true;
        self.gate.changed.notify_all();
    }
}

impl Drop for TxnPark {
    fn drop(&mut self) {
        self.release();
        let mut parks = TXN_PARKS.lock().unwrap_or_else(PoisonError::into_inner);
        parks
            .retain(|(d, p, g)| !(d == &self.db && *p == self.point && Arc::ptr_eq(g, &self.gate)));
        if parks.is_empty() {
            TXN_PARKS_ARMED.store(false, Ordering::SeqCst);
        }
    }
}

/// Park here if a test armed `point` for `db` ([`debug_park_txn`]).
pub(crate) fn txn_park(db: &str, point: TxnParkPoint) {
    if !TXN_PARKS_ARMED.load(Ordering::SeqCst) {
        return;
    }
    let gate = {
        let parks = TXN_PARKS.lock().unwrap_or_else(PoisonError::into_inner);
        parks
            .iter()
            .find(|(d, p, _)| d == db && *p == point)
            .map(|(_, _, g)| Arc::clone(g))
    };
    let Some(gate) = gate else {
        return;
    };
    let mut g = gate.state.lock().unwrap_or_else(PoisonError::into_inner);
    g.0 += 1;
    gate.changed.notify_all();
    while !g.1 {
        g = gate.changed.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
}

/// The periodic idle-transaction reaper of a serving [`ApiServer`] (see
/// [`ApiInner::start_txn_reaper`]): stopped and joined (bounded) when
/// dropped.
pub(crate) struct TxnReaper {
    inner: Arc<ApiInner>,
    stop: Arc<AtomicBool>,
    done: std::sync::mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TxnReaper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.inner.txn_reap.wake();
        // Bounded: a pass never waits for a database (it skips busy ones),
        // so it ends promptly; a rollback it is in the middle of finishes
        // on its own if this gives up (the thread holds the server). Once
        // the loop has ended, the thread is joined, so no reaper outlives
        // `serve_forever` (as its connection workers do not, review
        // conc#14).
        if !matches!(
            self.done.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            if let Some(thread) = self.thread.take() {
                crate::exit_park::joining(thread.thread());
                let _ = thread.join();
            }
        }
    }
}

/// A statement of an open transaction in flight (see
/// [`ApiInner::txn_statement`]): it counts as activity until it is
/// [`end`](Self::end)ed (or dropped), which restarts the transaction's idle
/// clock.
pub(crate) struct TxnStatement<'a> {
    inner: &'a ApiInner,
    db: String,
    tid: u64,
    /// The registry entry's load generation when the statement started
    /// (`None`: no such transaction, or already ended).
    generation: Option<u64>,
}

impl TxnStatement<'_> {
    /// The statement ended: stamp its transaction's activity.
    fn end(mut self) {
        self.stamp_end();
    }

    fn stamp_end(&mut self) {
        if let Some(generation) = self.generation.take() {
            let mut st = locked(&self.inner.state);
            self.inner
                .txn_activity(&mut st, &self.db, self.tid, generation, false);
        }
    }
}

impl Drop for TxnStatement<'_> {
    fn drop(&mut self) {
        self.stamp_end();
    }
}

/// The transaction open on a database, as monitoring shows it (see
/// [`ApiInner::txn_summary`]).
pub(crate) struct TxnSummary {
    pub(crate) tid: u64,
    /// Its begin time (Unix milliseconds).
    pub(crate) begun: u128,
    pub(crate) owner: String,
    pub(crate) isolation: TxnIsolation,
    /// Seconds since its last statement ended (or it began); `0` while a
    /// statement runs.
    pub(crate) idle_secs: u64,
    /// Its statements in flight.
    pub(crate) running: u32,
}

impl TxnSummary {
    /// As a JSON object.
    pub(crate) fn to_json(&self) -> crate::json::Json {
        crate::json::Json::obj()
            .with("TID", self.tid.to_string())
            .with("txnlog_TID", format!("{}_{}", self.begun, self.tid))
            .with("user", self.owner.as_str())
            .with("isolation", self.isolation.name())
            .with("begin_time", self.begun.to_string())
            .with("idle_seconds", self.idle_secs)
            .with("running_statements", u64::from(self.running))
    }

    /// `,"transaction":{…}`, spliced into a REST JSON object.
    fn json_field(&self) -> String {
        format!(",\"transaction\":{}", self.to_json())
    }
}

/// How [`ApiInner::end_txn`] ended a transaction.
struct TxnEnd {
    /// For [`ApiState::ended_txns`] ("committed", "rolled back", …).
    outcome: &'static str,
    /// For the transaction log.
    state: TxnState,
    /// For `txn.log`.
    logged: &'static str,
    /// The answer (see [`ApiInner::end_txn`]).
    result: Result<Option<String>, OpError>,
}

/// A RocksDB `/begin`'s copy of the database, taken under the shared guard,
/// with the state it was taken at (see [`ApiInner::begin_txn`]).
struct CapturedSnapshot {
    snapshot: ReadSnapshot,
    mutations: u64,
    settings_gen: u64,
}

/// How many times a RocksDB `/begin` copies the database under the shared
/// guard before it copies it under the exclusive one (see
/// [`ApiInner::begin_txn`]).
const BEGIN_CAPTURE_ATTEMPTS: usize = 3;

/// A database's committed state, taken at `/begin`. While a transaction is
/// open every *non-transactional* read of that database is answered from
/// here, so other clients never observe its uncommitted writes (no dirty
/// reads).
///
/// Cost (see [`SnapStore`]): `/begin` copies no triple of an in-memory or a
/// Stream-mode on-disk database. The dictionary is shared (its interner is
/// copy-on-write; an on-disk dictionary is shared with the database: terms
/// a transaction interns are visible to it, but no snapshot triple uses
/// them), the default graph is a copy-on-write clone or an on-disk read
/// view, and each named graph's store is a copy-on-write clone. The first
/// write of the transaction to an in-memory store copies its indexes once,
/// so an open transaction that wrote holds the begin-time copy until it
/// ends; a RocksDB database is still copied whole at `/begin`. The VS-tree
/// is not part of the snapshot (it is only a candidate-pruning accelerator,
/// so results are unchanged). The user-defined functions registered at
/// `/begin` are shared with it (the registry is reference-counted), and the
/// advanced graph functions run on one CSR of the snapshot — the
/// database's cached one when it had one for this data, otherwise built on
/// the first graph-function read — shared by every read until the
/// transaction ends.
pub(crate) struct ReadSnapshot {
    dict: Dictionary,
    store: SnapStore,
    named: BTreeMap<u32, TripleStore>,
    /// The committed triple count at `/begin`.
    triples: u64,
    csr: CsrCache,
    /// The database's evaluation settings at `/begin`
    /// ([`Database::eval_settings`]), applied to every read of the snapshot
    /// in one call ([`configure`](Self::configure); review design#9): the
    /// `SERVICE` policy (a server's restricted policy refuses it,
    /// `--allow-service` refuses host-local endpoints — a read inside a
    /// transaction cannot bypass it), the integer-division mode, the Top-K
    /// options, the graph-function bound, the user-defined functions and
    /// the PFN limits and own caller (a served database has none — the
    /// server passes the user per read, see [`read_as`](Self::read_as)).
    settings: crate::query::engine::EvalSettings,
    /// The snapshot's dictionary substring index: the database's cached one
    /// when there was one at `/begin`, else built on the first substring
    /// query and kept for the next (review design#9: rebuilt per query).
    string_index: crate::query::engine::StringIndexCell,
}

impl ReadSnapshot {
    /// The subject count, class statistics and schema graph of the
    /// snapshot's default graph (`/api monitor` and `schema` while a
    /// transaction is open), in a read scope: `Err` on a read error.
    pub(crate) fn class_stats(&self) -> Result<crate::db::ClassStats, String> {
        let scope = ReadScope::enter();
        let stats = match &self.store {
            SnapStore::Mem(store) => crate::db::class_stats_of(&self.dict, store),
            SnapStore::Disk(view) => view
                .with_base(|base, _| crate::db::class_stats_of(&self.dict, &DiskSource::new(base)))
                .map_err(|e| format!("reading the transaction's snapshot: {e:?}"))?,
        };
        match scope.take_error() {
            Some(e) => Err(e),
            None => Ok(stats),
        }
    }
}

impl ReadSnapshot {
    /// Write the snapshot to `out` as `format` (`None`: N-Quads when a named
    /// graph holds a triple, else N-Triples — what
    /// [`ExportFormat::for_database`] picks for a database), as
    /// [`crate::zipio::write_export`] writes a database: the default graph's
    /// statements, then (N-Quads) each named graph's, every term through
    /// the escaping serializer. Returns the format written and the
    /// statement count. The REST `/export` of a database whose transaction
    /// is open (its committed state; review http#11). Inside a
    /// [`ReadScope`]: a read error fails it (C2). The caller holds the
    /// database's shared guard, so the transaction cannot commit (and an
    /// on-disk view expire) meanwhile.
    fn write_export(
        &self,
        format: Option<ExportFormat>,
        out: &mut dyn Write,
    ) -> crate::Result<(ExportFormat, u64)> {
        let format = format.unwrap_or(if self.named.values().any(|g| g.triple_count() > 0) {
            ExportFormat::NQuads
        } else {
            ExportFormat::NTriples
        });
        let scope = ReadScope::enter();
        let mut write = |default: Vec<crate::model::IdTriple>| -> crate::Result<u64> {
            let mut count = export_triples(&self.dict, &default, None, out)?;
            if format == ExportFormat::NQuads {
                for (&gid, graph) in &self.named {
                    let Some(g) = self.dict.term(gid) else {
                        continue;
                    };
                    let g = crate::model::term::export_key(&g).into_owned();
                    let triples: Vec<_> = graph.iter_all().collect();
                    count += export_triples(&self.dict, &triples, Some(&g), out)?;
                }
            }
            Ok(count)
        };
        let count = match &self.store {
            SnapStore::Mem(store) => write(store.iter_all().collect())?,
            // The dictionary of an on-disk snapshot is read inside the
            // view's gate only.
            SnapStore::Disk(view) => view
                .with_base(|base, _| write(DiskSource::new(base).iter_all()))
                .map_err(|e| {
                    GStoreError::Database(format!(
                        "the transaction's read snapshot could not be exported: {e}"
                    ))
                })??,
        };
        if let Some(e) = scope.take_error() {
            return Err(crate::store::read_scope::read_failure(
                "exporting the transaction's read snapshot",
                e,
            ));
        }
        drop(scope);
        out.flush()?;
        Ok((format, count))
    }
}

/// Write the id triples `triples` as N-Triples lines — or N-Quads lines of
/// graph `graph` (its N-Triples form) — resolving each term in `dict`
/// (a statement whose term does not resolve is skipped, as
/// [`crate::zipio::write_export`] does). Returns the number written.
fn export_triples(
    dict: &Dictionary,
    triples: &[crate::model::IdTriple],
    graph: Option<&str>,
    out: &mut dyn Write,
) -> crate::Result<u64> {
    use crate::model::term::export_key;
    let mut count = 0;
    for t in triples {
        let (Some(s), Some(p), Some(o)) = (dict.term(t.sub), dict.pred(t.pred), dict.term(t.obj))
        else {
            continue;
        };
        let (s, p, o) = (export_key(&s), export_key(&p), export_key(&o));
        match graph {
            Some(g) => writeln!(out, "{s} {p} {o} {g} .")?,
            None => writeln!(out, "{s} {p} {o} .")?,
        }
        count += 1;
    }
    Ok(count)
}

/// The default graph of a [`ReadSnapshot`].
pub(crate) enum SnapStore {
    /// An in-memory store (also a Materialize-mode on-disk database's
    /// working set, and a RocksDB database's materialized copy): a
    /// copy-on-write clone, O(1) at `/begin`.
    Mem(TripleStore),
    /// A Stream-mode on-disk store: a read view of its committed state at
    /// `/begin` (no triple copied). Each read runs inside
    /// [`DiskReadView::with_base`], under the store's writer gate: the
    /// transaction's commit (or any other write to the pages) cancels the
    /// reads in flight and expires the view, and a read that finds it
    /// expired answers [`SnapshotReadError::Expired`] — its caller re-runs
    /// it on the live database (see `ApiInner::run_query_job`).
    Disk(DiskReadView),
}

/// Why a [`ReadSnapshot`] could not answer a read.
#[derive(Debug)]
pub(crate) enum SnapshotReadError {
    /// The database's committed state changed since `/begin` (the
    /// transaction committed, or the view was otherwise invalidated): the
    /// read must run on the live database (or a newer snapshot) instead.
    Expired,
    /// The on-disk store was closed (the database was unloaded).
    Closed,
    /// The read itself failed (a query error, a read error, a cancel).
    Failed(GStoreError),
}

impl From<GStoreError> for SnapshotReadError {
    fn from(e: GStoreError) -> SnapshotReadError {
        SnapshotReadError::Failed(e)
    }
}

impl std::fmt::Display for SnapshotReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotReadError::Failed(e) => write!(f, "{e}"),
            SnapshotReadError::Expired => f.write_str("the snapshot expired"),
            SnapshotReadError::Closed => f.write_str("the database was closed"),
        }
    }
}

impl SnapshotReadError {
    /// As a [`GStoreError`], for a caller that holds the database's guard
    /// (the transaction cannot end under it, so the snapshot does not
    /// expire; see [`DbSlot::read_cancellable`]).
    fn into_error(self) -> GStoreError {
        match self {
            SnapshotReadError::Failed(e) => e,
            SnapshotReadError::Expired => GStoreError::Database(
                "the transaction's read snapshot expired: the database changed".into(),
            ),
            SnapshotReadError::Closed => {
                GStoreError::Database("the database is no longer loaded".into())
            }
        }
    }
}

impl ReadSnapshot {
    /// Take `db`'s committed state (see [`SnapStore`]). A persistent backend
    /// with no in-RAM store and no read view (RocksDB) is materialized
    /// inside a [`ReadScope`]: a read error it reports fails the capture
    /// (and so the `/begin`) instead of yielding a partial copy.
    fn capture(db: &Database) -> crate::Result<ReadSnapshot> {
        let store = if let Some(mem) = db.backend().as_memory() {
            SnapStore::Mem(mem.clone())
        } else if let Some(disk) = db.backend().as_disk() {
            SnapStore::Disk(disk.read_view())
        } else {
            // A persistent backend with no in-RAM store: materialize it.
            let scope = ReadScope::enter();
            let mut s = TripleStore::new();
            s.bulk_load(db.backend().iter_all());
            if let Some(e) = scope.take_error() {
                return Err(GStoreError::Database(format!(
                    "a read from the store failed while copying the database for a \
                     transaction's read snapshot: {e}"
                )));
            }
            SnapStore::Mem(s)
        };
        // The CSR generation: the view's epoch (an on-disk snapshot), else
        // one generation for the snapshot's lifetime.
        let generation = match &store {
            SnapStore::Disk(view) => view.begin_epoch(),
            SnapStore::Mem(_) => 0,
        };
        Ok(ReadSnapshot {
            // An on-disk snapshot shares the database's dictionary and reads
            // it only inside the view's gate (`read_as`), which a writer
            // drains — expiring the view — before it frees any id: such a
            // clone need not keep freed ids from being reused, so it takes
            // no pin (a pinned clone would defer every free of a Stream-mode
            // database while the transaction is open). An in-memory
            // dictionary's clone is copy-on-write: it keeps the terms it was
            // taken with, whatever the live dictionary frees and reuses.
            dict: if matches!(store, SnapStore::Disk(_)) {
                db.dict().clone_unpinned()
            } else {
                db.dict().clone()
            },
            store,
            // Each graph's store is shared (copy-on-write).
            named: db
                .named_graphs()
                .iter()
                .map(|(&g, s)| (g, s.clone()))
                .collect(),
            triples: db.triple_num(),
            csr: db
                .cached_csr()
                .map_or_else(CsrCache::new, |g| CsrCache::seeded(generation, g)),
            settings: db.eval_settings(),
            // The database's index serves the snapshot's dictionary (a
            // clone at the same generation) while it is current.
            string_index: db.cached_string_index().map_or_else(
                crate::query::engine::StringIndexCell::new,
                |index| {
                    crate::query::engine::StringIndexCell::seeded(db.dict().generation(), index)
                },
            ),
        })
    }

    /// The default graph.
    #[cfg(test)]
    pub(crate) fn store(&self) -> &SnapStore {
        &self.store
    }

    /// Run a read query (SELECT/ASK/CONSTRUCT/DESCRIBE). Updates are refused:
    /// the snapshot is read-only by construction.
    fn query(
        &self,
        sparql: &str,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<QueryResult, SnapshotReadError> {
        self.read_as(&Statement::parse(sparql)?, cancel, None)
    }

    /// Run the parsed read `stmt` on behalf of `pfn` (the requesting user's
    /// PFN functions, under the snapshot's PFN limits; `None` falls back to
    /// the database's own caller, if any). The evaluation runs inside a
    /// [`ReadScope`]: a read error recorded there fails the statement. On an
    /// on-disk snapshot it runs inside the view's gate, with the gate's
    /// cancel flag wired into the evaluator (and `cancel` forwarded to it):
    /// a writer that needs the pages stops it, and it answers
    /// [`SnapshotReadError::Expired`] (or fails with
    /// [`GStoreError::Cancelled`] when `cancel` was raised).
    fn read_as(
        &self,
        stmt: &Statement,
        cancel: Option<Arc<AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult, SnapshotReadError> {
        if stmt.is_update() {
            return Err(SnapshotReadError::Failed(GStoreError::Query(
                "updates cannot run against a transaction's read snapshot".into(),
            )));
        }
        let q = stmt.query();
        match &self.store {
            SnapStore::Mem(store) => {
                // The snapshot never changes, so its CSR is cached under one
                // generation for the transaction's lifetime.
                let flag = cancel.clone();
                let csr = || {
                    self.csr.get_or_build_cancellable(0, flag.as_deref(), || {
                        CsrGraph::build_from_memory(store, flag.as_deref())
                    })
                };
                let string_index = || self.string_index.get(&self.dict, flag.as_deref());
                let mut eval = self.configure(
                    Evaluator::new(&self.dict, store),
                    EvalProviders {
                        csr: Some(&csr),
                        string_index: Some(&string_index),
                    },
                    pfn,
                );
                if let Some(c) = cancel {
                    eval = eval.with_cancel(c);
                }
                Ok(evaluate_in_scope(eval, q)?)
            }
            SnapStore::Disk(view) => {
                let epoch = view.begin_epoch();
                let outcome = view.with_base(|base, gate_cancel| {
                    // A cancel of the request reaches the evaluator through
                    // the gate's flag (the one a writer raises).
                    let _forward = cancel
                        .as_ref()
                        .map(|c| cancel_bridge::forward(c, gate_cancel));
                    let src = DiskSource::new(base);
                    let preds = crate::dict::Space::Predicate.first_id()
                        ..self.dict.id_limit(crate::dict::Space::Predicate);
                    // The graph functions' CSR, built once for the snapshot:
                    // a commit's drain stops the build (the gate's flag) as
                    // it stops the evaluation, and a read error fails it —
                    // nothing is cached then (C2), the statement fails.
                    let csr = || {
                        self.csr.get_or_build(epoch, || {
                            let scope = ReadScope::enter();
                            let graph =
                                CsrGraph::build_cancellable(&src, preds.clone(), gate_cancel);
                            match scope.take_error() {
                                Some(e) => Err(GStoreError::Database(format!(
                                    "a read from the store failed while building the CSR: {e}"
                                ))),
                                None => graph,
                            }
                        })
                    };
                    let string_index = || self.string_index.get(&self.dict, Some(&**gate_cancel));
                    let eval = self
                        .configure(
                            Evaluator::new(&self.dict, &src),
                            EvalProviders {
                                csr: Some(&csr),
                                string_index: Some(&string_index),
                            },
                            pfn,
                        )
                        .with_cancel(Arc::clone(gate_cancel));
                    evaluate_in_scope(eval, q)
                });
                let cancelled = cancel.is_some_and(|c| c.load(Ordering::SeqCst));
                match outcome {
                    Ok(result) => Ok(result?),
                    // Stopped by the request's own flag (forwarded to the
                    // gate's): a cancel, not an expiry.
                    Err(ViewError::Expired) if cancelled => {
                        Err(SnapshotReadError::Failed(GStoreError::Cancelled))
                    }
                    Err(ViewError::Expired) => Err(SnapshotReadError::Expired),
                    Err(ViewError::Closed) => Err(SnapshotReadError::Closed),
                    Err(ViewError::Io(e)) => Err(SnapshotReadError::Failed(GStoreError::Database(
                        format!("a read from the store failed while evaluating the query: {e}"),
                    ))),
                }
            }
        }
    }

    /// The snapshot's evaluation settings and named graphs, applied to
    /// `eval` in one call ([`Evaluator::with_settings`], review design#9),
    /// with the snapshot's caches (`providers`) and the requesting user's
    /// PFN caller (`pfn`, under the snapshot's PFN limits; `None` falls
    /// back to the database's own caller).
    fn configure<'a, S: TripleSource>(
        &'a self,
        eval: Evaluator<'a, S>,
        providers: EvalProviders<'a>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Evaluator<'a, S> {
        let eval = eval.with_settings(self.settings.clone(), providers, pfn);
        if self.named.is_empty() {
            eval
        } else {
            eval.with_named(&self.named)
        }
    }

    fn triple_num(&self) -> u64 {
        self.triples
    }
}

/// Evaluate `q` with `eval` inside a [`ReadScope`]: a read error recorded
/// there fails the statement.
fn evaluate_in_scope<S: TripleSource>(
    eval: Evaluator<'_, S>,
    q: &crate::parser::sparql::ast::Query,
) -> crate::Result<QueryResult> {
    let scope = ReadScope::enter();
    let result = eval.evaluate(q);
    if let Some(e) = scope.take_error() {
        return Err(GStoreError::Database(format!(
            "a read from the store failed while evaluating the query: {e}"
        )));
    }
    result
}

/// Forwarding one cancel flag to another, for an evaluator that watches
/// only one: an on-disk snapshot read watches its gate's flag (which a
/// writer raises), and the request's own flag (`/cancel`, a shutdown past
/// its grace period) is forwarded to it by one shared, lazily started
/// thread. It polls the registered pairs every few milliseconds and ends
/// once none is registered (the next registration starts another), so a
/// stopped server can join it ([`quiesce`](cancel_bridge::quiesce)). Its
/// lock is a leaf: nothing is called under it.
mod cancel_bridge {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    use std::time::Duration;

    /// How often the registered flags are polled.
    const POLL: Duration = Duration::from_millis(5);

    /// One registered forwarding: its id, the watched flag, the raised one.
    type Pair = (u64, Arc<AtomicBool>, Arc<AtomicBool>);

    #[derive(Default)]
    struct Links {
        next: u64,
        pairs: Vec<Pair>,
        /// The thread runs (it ends, under the lock, once no pair is left).
        started: bool,
        /// The latest thread started (joined by [`quiesce`]).
        thread: Option<std::thread::JoinHandle<()>>,
    }

    fn bridge() -> &'static Mutex<Links> {
        static BRIDGE: OnceLock<Mutex<Links>> = OnceLock::new();
        BRIDGE.get_or_init(|| Mutex::new(Links::default()))
    }

    fn run() {
        let links = bridge();
        let mut g = links.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            // Idle: end (the next `forward` starts another), so no thread
            // outlives the reads that needed it (review conc#14).
            if g.pairs.is_empty() {
                g.started = false;
                return;
            }
            for (_, from, to) in &g.pairs {
                if from.load(Ordering::SeqCst) {
                    to.store(true, Ordering::SeqCst);
                }
            }
            drop(g);
            std::thread::sleep(POLL);
            g = links.lock().unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Raise `to` (within a few milliseconds) once `from` is raised, until
    /// the returned guard is dropped.
    pub(super) fn forward(from: &Arc<AtomicBool>, to: &Arc<AtomicBool>) -> Forward {
        if from.load(Ordering::SeqCst) {
            to.store(true, Ordering::SeqCst);
        }
        let mut g = bridge().lock().unwrap_or_else(PoisonError::into_inner);
        if !g.started {
            // Without the thread a cancel is honoured only when the read
            // ends (the caller checks its flag then): later, not wrong.
            match std::thread::Builder::new()
                .name("gstore-cancel-bridge".into())
                .spawn(run)
            {
                Ok(t) => {
                    g.started = true;
                    // An earlier thread has ended (or is about to: it gave
                    // up the lock with `started` cleared).
                    g.thread = Some(t);
                }
                Err(_) => g.started = false,
            }
        }
        let id = g.next;
        g.next += 1;
        g.pairs.push((id, Arc::clone(from), Arc::clone(to)));
        Forward { id }
    }

    /// Join the bridge thread if it has nothing left to forward (it ends at
    /// its next poll then), waiting at most a few polls. A thread still
    /// forwarding for someone — another server's reads in this process,
    /// also ones registered meanwhile — is left running.
    pub(super) fn quiesce() {
        let thread = {
            let mut g = bridge().lock().unwrap_or_else(PoisonError::into_inner);
            if !g.pairs.is_empty() {
                return;
            }
            g.thread.take()
        };
        let Some(t) = thread else {
            return;
        };
        let deadline = std::time::Instant::now() + 20 * POLL;
        while !t.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(POLL / 5);
        }
        if t.is_finished() {
            let _ = t.join();
        } else {
            // Busy again: give the handle back (unless a newer one is there).
            let mut g = bridge().lock().unwrap_or_else(PoisonError::into_inner);
            if g.thread.is_none() {
                g.thread = Some(t);
            }
        }
    }

    /// A registered forwarding, removed when dropped.
    pub(super) struct Forward {
        id: u64,
    }

    impl Drop for Forward {
        fn drop(&mut self) {
            let mut g = bridge().lock().unwrap_or_else(PoisonError::into_inner);
            g.pairs.retain(|(id, _, _)| *id != self.id);
        }
    }
}

/// Snapshot reads that expired and were re-run (see [`expired_snapshot_reads`]).
static EXPIRED_SNAPSHOT_READS: AtomicU64 = AtomicU64::new(0);

/// How many reads of a transaction's on-disk snapshot this process saw
/// expire under them (the transaction committed meanwhile) and re-ran on the
/// live database. For tests and diagnostics.
#[doc(hidden)]
pub fn expired_snapshot_reads() -> u64 {
    EXPIRED_SNAPSHOT_READS.load(Ordering::Relaxed)
}

/// How many times a non-transactional read tries a transaction's on-disk
/// snapshot that expired under it before it reads holding the database's
/// guard (see [`ApiInner::read_committed`]).
const SNAPSHOT_READ_ATTEMPTS: usize = 3;

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
    /// The database's figures and mutation count at `/begin` — what
    /// monitoring reports while the transaction is open (its writes are
    /// visible to no other client).
    pub(crate) begin_stats: crate::db::DbStats,
    pub(crate) begin_generation: u64,
}

/// One loaded database plus its HTTP-layer bookkeeping, behind the per-db
/// reader-writer lock ([`DbCell`]).
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
    /// This load's generation: unique per process for every database
    /// served (a reload gets a new one). Finished transactions are
    /// remembered with it ([`EndedTxn`]), so ids reused by a later load are
    /// told apart from theirs.
    pub(crate) load_gen: u64,
    /// Set once the database was unloaded, dropped or replaced while a request
    /// still held this handle: the slot then holds an empty placeholder that
    /// must never be queried, written or saved (it would overwrite `dir`).
    pub(crate) retired: bool,
    /// The [`ApiInner::settings_gen`] whose `[query]` settings the database
    /// has (`0`: those it was opened with — see
    /// [`ApiInner::sync_query_settings`]).
    pub(crate) settings_gen: u64,
    /// Unix milliseconds of the last successful [`save`](Self::save); `0`
    /// when none happened since it was loaded (published for monitoring).
    pub(crate) last_save: u64,
}

/// One loaded database's lock: a reader-writer lock over its [`DbSlot`]
/// (taken with [`read`](Self::read), [`write`](Self::write) or
/// [`try_write`](Self::try_write), see the module's *Locking* rules), with
/// what may be read without taking it.
pub(crate) struct DbCell {
    slot: RwLock<DbSlot>,
    /// The database's name (as it was loaded), for messages and monitoring.
    pub(crate) name: String,
    /// Whether the database's CSR is built for its current data
    /// ([`Database::csr_is_cached`]), readable while a statement, a CSR
    /// build or a batch holds the lock — `show` reports it without waiting
    /// for them, as upstream reads `db_ptr->csr` without a lock. It reads
    /// `false` once the slot is retired (its database dropped).
    pub(crate) csr: crate::analytics::csr::CsrStatus,
    /// The database's figures as of the end of its last exclusive section
    /// (and its load), for monitoring without the lock.
    pub(crate) counters: DbCounters,
    /// A write section unwound (a panic) without publishing: the figures
    /// may predate a change it applied. [`figures`](Self::figures)
    /// publishes them the next time the database is free.
    unpublished: AtomicBool,
    /// The bytes of the database's directory, measured when it was loaded
    /// and after each exclusive section that changed the data or saved it
    /// (once its guard is released: never while readers wait), so `/api
    /// monitor`'s `diskUsed` never walks the directory on its request path
    /// (where it would stall behind a save's fsync). A change no write
    /// makes (a RocksDB background compaction) shows after the next one.
    disk_bytes: AtomicU64,
    /// Which state of the slot `disk_bytes` was last asked to measure (see
    /// [`publish`](Self::publish)).
    disk_measured: Mutex<DiskMeasure>,
    /// The layout the database persists as (fixed for the slot's life).
    pub(crate) layout: StorageKind,
    /// How an on-disk database answers queries (`None` for the other
    /// layouts; fixed for the slot's life).
    pub(crate) query_mode: Option<crate::db::DiskQueryMode>,
    /// Held by a RocksDB `/begin` from before its whole-store copy until it
    /// registered its transaction (or failed): one copy per database at a
    /// time ([`ApiInner::begin_txn`]).
    begin_copy: Mutex<()>,
}

/// What a [`DbCell`]'s directory size was measured for.
struct DiskMeasure {
    /// The slot's mutation count and last save when the latest measurement
    /// was asked for.
    asked: (u64, u64),
    /// The number of that request (they are numbered in the order asked).
    seq: u64,
    /// The number of the request whose result `disk_bytes` holds.
    stored: u64,
}

/// The total size of the files under `dir`, in bytes (`0` when it cannot
/// be read). A walk (a readdir and a stat per file): never called with the
/// server-wide state lock held (review http#13/conc#7), nor with a
/// database's guard held while others wait for it.
pub(crate) fn dir_size(dir: &Path) -> u64 {
    dir_size_park(dir);
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    total
}

/// The directory-size parks armed by tests (normally none: one atomic read
/// per measurement).
static DIR_SIZE_PARKS: Mutex<Vec<(PathBuf, Arc<ParkGate>)>> = Mutex::new(Vec::new());
static DIR_SIZE_PARKS_ARMED: AtomicBool = AtomicBool::new(false);

/// A parked directory measurement ([`debug_park_dir_size`]): released and
/// unregistered when dropped.
#[doc(hidden)]
pub struct DirSizePark {
    dir: PathBuf,
    gate: Arc<ParkGate>,
}

/// Test hook: park every thread that starts measuring the size of `dir`
/// (the walk behind a served database's `diskUsed`, taken when its handle
/// is created and after a write) until the returned handle is released or
/// dropped — so a test can prove what does not wait for the walk.
#[doc(hidden)]
pub fn debug_park_dir_size(dir: &Path) -> DirSizePark {
    let gate = Arc::new(ParkGate::default());
    let mut parks = DIR_SIZE_PARKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    parks.push((dir.to_path_buf(), Arc::clone(&gate)));
    DIR_SIZE_PARKS_ARMED.store(true, Ordering::SeqCst);
    DirSizePark {
        dir: dir.to_path_buf(),
        gate,
    }
}

impl DirSizePark {
    /// Wait (at most `hang_guard`) until a thread is parked here.
    pub fn wait_parked(&self, hang_guard: Duration) -> bool {
        let g = self
            .gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (g, _) = self
            .gate
            .changed
            .wait_timeout_while(g, hang_guard, |(reached, _)| *reached < 1)
            .unwrap_or_else(PoisonError::into_inner);
        g.0 >= 1
    }

    /// Let the parked thread (and every later one) go on.
    pub fn release(&self) {
        let mut g = self
            .gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        g.1 = true;
        self.gate.changed.notify_all();
    }
}

impl Drop for DirSizePark {
    fn drop(&mut self) {
        self.release();
        let mut parks = DIR_SIZE_PARKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        parks.retain(|(d, g)| !(d == &self.dir && Arc::ptr_eq(g, &self.gate)));
        if parks.is_empty() {
            DIR_SIZE_PARKS_ARMED.store(false, Ordering::SeqCst);
        }
    }
}

/// Park here if a test armed a measurement of `dir` ([`debug_park_dir_size`]).
fn dir_size_park(dir: &Path) {
    if !DIR_SIZE_PARKS_ARMED.load(Ordering::SeqCst) {
        return;
    }
    let gate = {
        let parks = DIR_SIZE_PARKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        parks
            .iter()
            .find(|(d, _)| d == dir)
            .map(|(_, g)| Arc::clone(g))
    };
    let Some(gate) = gate else {
        return;
    };
    let mut g = gate.state.lock().unwrap_or_else(PoisonError::into_inner);
    g.0 += 1;
    gate.changed.notify_all();
    while !g.1 {
        g = gate.changed.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
}

thread_local! {
    /// The [`DbCell`]s this thread holds a guard of (their addresses): what
    /// turns a re-entrant [`DbCell::read`] / [`DbCell::write`] into an error
    /// instead of a deadlock, in every build.
    static HELD_CELLS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// This thread's claim on one [`DbCell`], registered in `HELD_CELLS` and
/// released when dropped. `!Send`, like the guard it accompanies.
struct HeldCell {
    key: usize,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl HeldCell {
    /// Register `cell` as held by this thread, or `None` when it already is.
    fn claim(cell: &DbCell) -> Option<HeldCell> {
        let key = std::ptr::from_ref(cell) as usize;
        HELD_CELLS.with(|held| {
            let mut held = held.borrow_mut();
            if held.contains(&key) {
                return None;
            }
            held.push(key);
            Some(HeldCell {
                key,
                _not_send: std::marker::PhantomData,
            })
        })
    }
}

impl Drop for HeldCell {
    fn drop(&mut self) {
        let _ = HELD_CELLS.try_with(|held| {
            let mut held = held.borrow_mut();
            if let Some(i) = held.iter().rposition(|&k| k == self.key) {
                held.swap_remove(i);
            }
        });
    }
}

/// A shared guard of a [`DbSlot`] ([`DbCell::read`]).
pub(crate) struct SlotRead<'a> {
    guard: RwLockReadGuard<'a, DbSlot>,
    _held: HeldCell,
}

impl std::ops::Deref for SlotRead<'_> {
    type Target = DbSlot;

    fn deref(&self) -> &DbSlot {
        &self.guard
    }
}

/// The exclusive guard of a [`DbSlot`] ([`DbCell::write`]). When it is
/// released, the database's figures are published for monitoring
/// ([`DbCell::counters`]): every write section ends with a publish.
pub(crate) struct SlotWrite<'a> {
    /// `Some` until dropped (released before the directory is measured).
    guard: Option<RwLockWriteGuard<'a, DbSlot>>,
    cell: &'a DbCell,
    _held: HeldCell,
}

impl std::ops::Deref for SlotWrite<'_> {
    type Target = DbSlot;

    fn deref(&self) -> &DbSlot {
        self.guard.as_ref().expect("held until dropped")
    }
}

impl std::ops::DerefMut for SlotWrite<'_> {
    fn deref_mut(&mut self) -> &mut DbSlot {
        self.guard.as_mut().expect("held until dropped")
    }
}

impl Drop for SlotWrite<'_> {
    fn drop(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        // Not while unwinding: the slot may be half-way through a change
        // (the guard is poisoned) and reading it here could panic again. It
        // is marked instead: the next exclusive holder — the next writer, or
        // monitoring once the database is free — publishes.
        if std::thread::panicking() {
            self.cell.unpublished.store(true, Ordering::SeqCst);
            return;
        }
        let remeasure = self.cell.publish(&guard);
        self.cell.unpublished.store(false, Ordering::SeqCst);
        // The directory walk (a readdir and a stat per file) runs with the
        // guard released.
        drop(guard);
        if let Some((dir, seq)) = remeasure {
            self.cell.measure_dir(&dir, seq);
        }
    }
}

impl DbCell {
    /// The cell of `slot`, whose directory measured `disk_bytes` (by
    /// [`DbSlot::prepare`], before the state lock was taken).
    fn new(slot: DbSlot, disk_bytes: u64) -> DbCell {
        let counters = DbCounters::default();
        publish_slot(&counters, &slot);
        DbCell {
            name: slot.db.name().to_string(),
            csr: slot.db.csr_status(),
            counters,
            unpublished: AtomicBool::new(false),
            disk_bytes: AtomicU64::new(disk_bytes),
            disk_measured: Mutex::new(DiskMeasure {
                asked: (slot.db.mutation_count(), slot.last_save),
                seq: 0,
                stored: 0,
            }),
            layout: slot.db.storage(),
            query_mode: slot.db.disk_query_mode(),
            begin_copy: Mutex::new(()),
            slot: RwLock::new(slot),
        }
    }

    /// The bytes of the database's directory as last measured (see the
    /// field's docs): read without the lock, never walks the directory.
    pub(crate) fn disk_bytes(&self) -> u64 {
        self.disk_bytes.load(Ordering::SeqCst)
    }

    /// The error a re-entrant lock request gets (instead of a deadlock).
    fn reentrant(&self) -> OpError {
        OpError::new(
            OpErrorKind::Failed,
            format!(
                "internal error: this request already holds database '{}' and asked for it \
                 again (refused instead of deadlocking)",
                self.name
            ),
        )
    }

    /// A shared guard: any number of readers at once, none while a writer
    /// holds (or waits for) the exclusive guard. Poison-tolerant; an error
    /// when this thread already holds a guard of this database.
    pub(crate) fn read(&self) -> Result<SlotRead<'_>, OpError> {
        let held = HeldCell::claim(self).ok_or_else(|| self.reentrant())?;
        Ok(SlotRead {
            guard: self.slot.read().unwrap_or_else(PoisonError::into_inner),
            _held: held,
        })
    }

    /// The exclusive guard (publishes the figures when released).
    /// Poison-tolerant; an error when this thread already holds a guard of
    /// this database.
    pub(crate) fn write(&self) -> Result<SlotWrite<'_>, OpError> {
        let held = HeldCell::claim(self).ok_or_else(|| self.reentrant())?;
        Ok(SlotWrite {
            guard: Some(self.slot.write().unwrap_or_else(PoisonError::into_inner)),
            cell: self,
            _held: held,
        })
    }

    /// The exclusive guard if it is free right now (`None` while anyone
    /// holds a guard, or when this thread holds one).
    pub(crate) fn try_write(&self) -> Option<SlotWrite<'_>> {
        let held = HeldCell::claim(self)?;
        let guard = match self.slot.try_write() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(SlotWrite {
            guard: Some(guard),
            cell: self,
            _held: held,
        })
    }

    /// Publish `slot`'s figures (its holder is exclusive, see
    /// [`DbCounters`]). Returns the directory to measure again (and what
    /// for) when the data changed or was saved since it was last measured:
    /// the caller walks it once the guard is released.
    fn publish(&self, slot: &DbSlot) -> Option<(PathBuf, u64)> {
        publish_slot(&self.counters, slot);
        if slot.retired {
            return None;
        }
        let key = (slot.db.mutation_count(), slot.last_save);
        let mut m = self
            .disk_measured
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if m.asked == key {
            return None;
        }
        m.asked = key;
        m.seq += 1;
        Some((slot.dir.clone(), m.seq))
    }

    /// Measure `dir` for request `seq` (see [`publish`](Self::publish)); a
    /// measurement a later request's overtook is not stored.
    fn measure_dir(&self, dir: &Path, seq: u64) {
        let bytes = dir_size(dir);
        let mut m = self
            .disk_measured
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if seq > m.stored {
            m.stored = seq;
            self.disk_bytes.store(bytes, Ordering::SeqCst);
        }
    }

    /// The figures published at the end of the last exclusive section (or
    /// the load): read without the lock, never waits for a statement. After
    /// a write section that unwound without publishing they are published
    /// first when the database is free right now (else the last ones are
    /// returned, and a later call or the next writer publishes).
    pub(crate) fn figures(&self) -> CounterSnapshot {
        if self.unpublished.load(Ordering::SeqCst) {
            // Dropping the exclusive guard publishes (and clears the mark).
            drop(self.try_write());
        }
        self.counters.read()
    }

    /// Whether the published figures may predate the data (a write section
    /// unwound since the last publish).
    #[cfg(test)]
    pub(crate) fn unpublished(&self) -> bool {
        self.unpublished.load(Ordering::SeqCst)
    }
}

/// Publish `slot`'s figures into `counters` (see [`DbCell::publish`]).
fn publish_slot(counters: &DbCounters, slot: &DbSlot) {
    if slot.retired {
        counters.store(CounterSnapshot::default());
        return;
    }
    let s = slot.committed_stats();
    counters.store(CounterSnapshot {
        triples: slot.committed_triple_num(),
        entities: s.entity_num as u64,
        literals: s.literal_num as u64,
        predicates: s.predicate_num as u64,
        generation: slot.committed_generation(),
        last_save: slot.last_save,
        index_valid: s.index_valid,
        in_transaction: slot.txn.is_some(),
        dirty: slot.dirty,
        live: true,
    });
}

/// A shared handle to one loaded database.
pub(crate) type DbHandle = Arc<DbCell>;

/// A database made ready to be served ([`DbSlot::prepare`]) — its
/// directory locked and measured — that [`serve`](Self::serve) registers
/// under the state lock.
pub(crate) struct PreparedSlot {
    db: Database,
    dir: PathBuf,
    dirty: bool,
    disk_bytes: u64,
}

impl PreparedSlot {
    /// The handle that serves the prepared database, with the server's
    /// `LOAD` policy and query-evaluation settings installed. Cheap (no
    /// I/O): it runs under the state lock the handle is registered under,
    /// so policy and settings installed meanwhile reach it (see the
    /// `/upload` path) and a request sees the database only once it is
    /// fully set up.
    pub(crate) fn serve(self, policy: &LoadPolicy, query: &QuerySettings) -> DbHandle {
        let PreparedSlot {
            mut db,
            dir,
            dirty,
            disk_bytes,
        } = self;
        db.set_load_policy(policy.clone());
        db.apply_query_settings(query);
        Arc::new(DbCell::new(
            DbSlot {
                db,
                dir,
                dirty,
                txn: None,
                next_tid: 1,
                load_gen: {
                    static LOAD_GEN: AtomicU64 = AtomicU64::new(1);
                    LOAD_GEN.fetch_add(1, Ordering::Relaxed)
                },
                retired: false,
                settings_gen: 0,
                last_save: 0,
            },
            disk_bytes,
        ))
    }
}

impl DbSlot {
    /// The handle of a freshly loaded / built / created / restored database
    /// in one step: [`prepare`](Self::prepare), then
    /// [`PreparedSlot::serve`] (for tests; the server calls the two itself,
    /// `prepare` with the state lock released).
    #[cfg(test)]
    pub(crate) fn handle(
        db: Database,
        dir: PathBuf,
        dirty: bool,
        policy: &LoadPolicy,
        query: &QuerySettings,
    ) -> DbHandle {
        Self::prepare(db, dir, dirty).serve(policy, query)
    }

    /// Make a freshly loaded / built / created / restored database ready to
    /// be served from `dir` (clean when it matches what is on disk there):
    /// every database the server serves passes through here and then
    /// [`PreparedSlot::serve`], which installs the server's `LOAD` policy
    /// (clients must not make the server read arbitrary local files) and
    /// its query-evaluation settings (`conf.ini`'s `[query]` section:
    /// integer division, query cache, Top-K, graph-function output bound).
    ///
    /// This part does the I/O — locking `dir` against other processes and
    /// measuring its size (a readdir and a stat per file: thousands of
    /// files for a RocksDB store) — and needs none of the server's state:
    /// it runs with the state lock released, so no other request
    /// (`/status`, `/cancel`, a login) waits for it (review
    /// http#13/conc#7). `serve` finishes the handle under the lock it is
    /// registered under.
    pub(crate) fn prepare(mut db: Database, dir: PathBuf, dirty: bool) -> PreparedSlot {
        // Its reasoning rules (`reasonManage`) are those of the directory it
        // is served from — also for a database built or created in memory,
        // which has no home directory — so that its own backups (REST
        // `/backup`, `Database::backup`) carry them.
        if db.reason_db_dir().as_deref() != Some(dir.as_path()) {
            db.set_reason_db_dir(dir.clone());
        }
        // Hold `dir`'s inter-process lock while serving it (`crate::db::dirlock`):
        // a database opened from `dir` already does; one built in memory and
        // saved there takes it now. Best effort — a database not on disk yet
        // has nothing to lock (it is locked when it is next opened).
        // Every layout, RocksDB included: `<dir>/LOCK` sits beside
        // `rocksdb/` (whose own lock RocksDB takes), so a bulk build or
        // another server's lifecycle operation sees the directory in use.
        if dir.is_dir() {
            let _ = db.hold_dir_lock(&dir);
        }
        let disk_bytes = dir_size(&dir);
        PreparedSlot {
            db,
            dir,
            dirty,
            disk_bytes,
        }
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
        Self::save_db(&mut self.db, &self.dir)?;
        self.dirty = false;
        self.last_save = unix_millis();
        Ok(())
    }

    /// Save served database `db` to its directory `dir` (what
    /// [`save`](Self::save) does once it has checked the slot), for a caller
    /// that holds the slot's database borrowed.
    pub(crate) fn save_db(db: &mut Database, dir: &Path) -> crate::Result<()> {
        // Never write a directory another process holds (`crate::db::dirlock`):
        // a database created in memory (`/upload` into a new name) takes the
        // lock before its first save, and keeps it while it is served.
        if !db.holds_dir_lock() {
            // Created durably: the save below finds the directory there and
            // syncs only what is inside it (review tests-b#3: the fsync
            // audit found that a database `/upload` created could lose its
            // directory entry to a power cut after the success answer).
            crate::db::fsutil::create_dir_durable(dir)?;
            db.hold_dir_lock(dir)?;
        }
        db.save(dir)
    }

    /// Record that a committed write changed data: mark dirty and, under the
    /// check_point policy, save now. Returns a warning when that save failed
    /// (the write itself stays applied in memory: an in-memory snapshot
    /// database retries it on unload / shutdown / `/checkpoint`, while an
    /// on-disk database whose commit point failed is broken until reopened,
    /// which discards it): `"applied in memory but not durable: <cause>"`
    /// ([`NOT_DURABLE`]). The caller must answer such a write as a
    /// failure — REST HTTP 500, `/api` `StatusCode` 1005 — never as a success
    /// carrying a warning (review durability#2).
    ///
    /// With check_point off an on-disk / RocksDB database still gets a commit
    /// point ([`Database::sync`], a no-op for an in-memory snapshot): its
    /// direct-API writes (`/batch*`, `/upload`) are already written through
    /// and only need committing, so there is no reason to leave them volatile.
    pub(crate) fn wrote(&mut self, check_point: bool) -> Option<String> {
        self.dirty = true;
        let warning = if !check_point {
            self.db.sync().err().map(|e| {
                format!(
                    "{NOT_DURABLE}: the commit point of '{}' failed: {e}",
                    self.dir.display()
                )
            })
        } else {
            self.save().err().map(|e| {
                format!(
                    "{NOT_DURABLE}: saving to '{}' failed: {e}",
                    self.dir.display()
                )
            })
        };
        if let Some(w) = &warning {
            crate::gs_error!("{w}");
        }
        warning
    }

    /// Run a read on behalf of a *non-transactional* caller, under the
    /// cancel flag `cancel`: against the begin-time snapshot while a
    /// transaction is open (never its uncommitted writes), else against the
    /// live database — on a shared guard (an UPDATE is refused).
    pub(crate) fn read_cancellable(
        &self,
        sparql: &str,
        cancel: Option<Arc<AtomicBool>>,
    ) -> crate::Result<QueryResult> {
        match &self.txn {
            // The caller holds this slot's guard: the transaction cannot end
            // (nor its snapshot expire) meanwhile.
            Some(open) => open
                .snapshot
                .query(sparql, cancel)
                .map_err(SnapshotReadError::into_error),
            None => self.db.query_read(sparql, cancel, None),
        }
    }

    /// The committed triple count (the snapshot's while a transaction is open).
    pub(crate) fn committed_triple_num(&self) -> u64 {
        match &self.txn {
            Some(open) => open.snapshot.triple_num(),
            None => self.db.triple_num(),
        }
    }

    /// The database's figures ([`Database::stats`]) of its committed state:
    /// as of `/begin` while a transaction is open (whose writes no other
    /// client may observe).
    pub(crate) fn committed_stats(&self) -> crate::db::DbStats {
        match &self.txn {
            Some(open) => crate::db::DbStats {
                in_transaction: true,
                ..open.begin_stats.clone()
            },
            None => self.db.stats(),
        }
    }

    /// The mutation count ([`Database::mutation_count`]) of the committed
    /// state: as of `/begin` while a transaction is open — what statistics
    /// of the committed state are cached under.
    pub(crate) fn committed_generation(&self) -> u64 {
        match &self.txn {
            Some(open) => open.begin_generation,
            None => self.db.mutation_count(),
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
/// `/api` as an upstream `StatusCode` (see `crate::gapi::ops_query`). An
/// operation that failed with a [`GStoreError`] gets its kind from
/// [`OpErrorKind::of`], the one place the protocol layers classify one
/// (review design#3).
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
    /// The SPARQL statement failed (syntax, evaluation) — REST `400` with a
    /// `{"error":…}` body.
    Query,
    /// An internal failure (I/O, a failed read, corrupt data, save,
    /// begin/commit) — REST `500`.
    Failed,
    /// A write was applied in memory but persisting it failed (an UPDATE that
    /// failed part-way and whose applied part could not be saved) — REST
    /// `500`, `/api` `1005`; the message starts with [`NOT_DURABLE`].
    Persist,
    /// The statement ran past `query_timeout` and was stopped (review
    /// sec#10) — REST `503` (the SPARQL Protocol's refused query) with a
    /// `{"error":…}` body, `/api` `1005`.
    Timeout,
    /// The serialized answer passed `max_result_bytes` (review sec#10) —
    /// REST `503` with a `{"error":…}` body, `/api` `1005`.
    ResultTooLarge,
    /// The statement or export was stopped by a cancellation request
    /// (`/cancel`, `operationTaskCancel`, a shutdown) — REST `503` with a
    /// `{"error":…}` body (it was a `400` query error: review design#3),
    /// `/api` `1005` as before.
    Cancelled,
}

impl OpErrorKind {
    /// The kind of an operation that failed with `e` — the one place the
    /// protocol layers classify a [`GStoreError`] (review design#3), by its
    /// [`kind`](GStoreError::kind): a syntax or evaluation error is
    /// [`Query`](Self::Query) (REST `400`); a lock another process holds
    /// (or a transaction conflict) [`Conflict`](Self::Conflict), `409` on
    /// every endpoint; a cancellation [`Cancelled`](Self::Cancelled) and a
    /// time limit [`Timeout`](Self::Timeout), `503`; a storage failure — an
    /// I/O error, a failed read, a broken store, corrupt data —
    /// [`Failed`](Self::Failed), `500` (as is a write applied but not made
    /// durable, [`Persist`](Self::Persist), which the callers that save
    /// report themselves). An untyped error
    /// ([`ErrorKind::Other`](crate::error::ErrorKind::Other)) is `untyped`:
    /// what the endpoint answered such an error as before errors were typed.
    pub(crate) fn of(e: &GStoreError, untyped: OpErrorKind) -> OpErrorKind {
        use crate::error::ErrorKind;
        match e.kind() {
            ErrorKind::Parse | ErrorKind::Query => OpErrorKind::Query,
            ErrorKind::Locked | ErrorKind::Conflict => OpErrorKind::Conflict,
            ErrorKind::Cancelled => OpErrorKind::Cancelled,
            ErrorKind::Timeout => OpErrorKind::Timeout,
            ErrorKind::Storage | ErrorKind::Corrupt => OpErrorKind::Failed,
            _ => untyped,
        }
    }

    /// The REST endpoints' (and the single-database server's) HTTP status
    /// for this kind.
    pub(crate) fn http(self) -> u16 {
        match self {
            OpErrorKind::NotLoaded | OpErrorKind::TxnUnknown => 404,
            OpErrorKind::TxnEnded | OpErrorKind::Conflict => 409,
            OpErrorKind::NotOwner => 403,
            OpErrorKind::ShuttingDown => 503,
            OpErrorKind::Timeout | OpErrorKind::ResultTooLarge | OpErrorKind::Cancelled => 503,
            OpErrorKind::Query => 400,
            OpErrorKind::Failed | OpErrorKind::Persist => 500,
        }
    }
}

impl OpError {
    pub(crate) fn new(kind: OpErrorKind, msg: impl Into<String>) -> OpError {
        OpError {
            kind,
            msg: msg.into(),
        }
    }

    /// The error `msg` of an operation that failed with `e`, of the kind
    /// [`OpErrorKind::of`] gives it (`untyped`: the kind of an unclassified
    /// error).
    pub(crate) fn of(e: &GStoreError, untyped: OpErrorKind, msg: impl Into<String>) -> OpError {
        OpError::new(OpErrorKind::of(e, untyped), msg)
    }

    /// A failed SPARQL statement: `e`, as its message, of the kind
    /// [`OpErrorKind::of`] gives it — an unclassified error is a
    /// [`Query`](OpErrorKind::Query) error, as every statement error was
    /// before errors were typed.
    pub(crate) fn statement(e: &GStoreError) -> OpError {
        OpError::of(e, OpErrorKind::Query, e.to_string())
    }

    /// The REST endpoints' HTTP status for this error.
    pub(crate) fn http(&self) -> u16 {
        self.kind.http()
    }

    /// The REST rendering: `{"StatusCode":<http>,"StatusMsg":…}`, or for a
    /// failed SPARQL statement the SPARQL endpoints' `{"error":…}` body.
    fn rest(&self) -> Resp {
        match self.kind {
            OpErrorKind::Query
            | OpErrorKind::Timeout
            | OpErrorKind::ResultTooLarge
            | OpErrorKind::Cancelled => Resp::new(self.http(), SPARQL_JSON, err_json(&self.msg)),
            _ => Resp::fail(self.http(), &self.msg),
        }
    }
}

/// The REST failure `msg` of a lifecycle operation that failed with `e`:
/// `{"StatusCode":<http>,"StatusMsg":msg}` at the status of
/// [`OpErrorKind::of`] — `409` for a lock another process holds, `500` for
/// a storage failure, … — or of `untyped` for an unclassified error (the
/// endpoint's status for it before errors were typed). Review design#3.
fn fail_with(e: &GStoreError, untyped: OpErrorKind, msg: &str) -> Resp {
    Resp::fail(OpErrorKind::of(e, untyped).http(), msg)
}

/// The [`OpErrorKind::Timeout`] error of a job whose `query_timeout` stopped
/// it (`clock`; `e`: what the stopped job reported — for an UPDATE stopped
/// part-way, which operations stayed applied).
fn timed_out_err(e: OpError, clock: &StatementClock) -> OpError {
    let msg = if e.msg.contains("applied") {
        format!("{} ({})", clock.message(), e.msg)
    } else {
        clock.message()
    };
    OpError::new(OpErrorKind::Timeout, msg)
}

/// The `query_timeout` clock of one statement (review sec#10, robust#5): it
/// measures the statement's evaluation only. It [`start`](Self::start)s once
/// the statement holds its database's guard — time spent queued behind
/// another holder of the guard (a long write, a save, a backup) is not the
/// statement's, so a trivial statement that waited is never answered as
/// timed out — and [`stop`](Self::stop)s as soon as the evaluation returned,
/// before the save of what an UPDATE applied, so a deadline passing during
/// that save never relabels the statement's real outcome. A job is
/// [`timed_out`](Self::timed_out) when the deadline had raised its cancel
/// flag by the time the evaluation returned.
pub(crate) struct StatementClock {
    cancel: Arc<AtomicBool>,
    timeout: Duration,
    deadline: RefCell<Option<crate::deadline::Deadline>>,
    started: std::cell::Cell<bool>,
    timed_out: std::cell::Cell<bool>,
}

impl StatementClock {
    /// The clock of a statement whose cancel flag is `cancel` (also raised
    /// by `/cancel` and a shutdown), allowed `timeout` (zero: no limit).
    pub(crate) fn new(cancel: &Arc<AtomicBool>, timeout: Duration) -> StatementClock {
        StatementClock {
            cancel: Arc::clone(cancel),
            timeout,
            deadline: RefCell::new(None),
            started: std::cell::Cell::new(false),
            timed_out: std::cell::Cell::new(false),
        }
    }

    /// The statement holds its database's guard: its time starts now (only
    /// the first call counts — a read retried on the live database keeps
    /// the time it started with).
    fn start(&self) {
        if !self.started.replace(true) {
            *self.deadline.borrow_mut() = crate::deadline::arm(&self.cancel, self.timeout);
        }
    }

    /// The evaluation returned: disarm the deadline, remembering whether it
    /// had fired (and so stopped the evaluation).
    fn stop(&self) {
        if let Some(d) = self.deadline.borrow_mut().take() {
            if d.fired() {
                self.timed_out.set(true);
            }
        }
    }

    /// Whether the deadline stopped the evaluation.
    fn timed_out(&self) -> bool {
        self.timed_out.get()
    }

    /// The statement's cancel flag.
    fn cancel(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// "query timed out after … (query_timeout)".
    fn message(&self) -> String {
        crate::deadline::timeout_message(self.timeout)
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
    /// Recently finished transactions, newest last, bounded by
    /// [`ENDED_TXN_MEMORY`] — across reloads of their databases (see
    /// [`EndedTxn`]).
    pub(crate) ended_txns: VecDeque<EndedTxn>,
    pub(crate) started: Instant,
    pub(crate) requests: u64,
    /// The `LOAD` policy installed on every database this server serves
    /// (default [`LoadPolicy::restricted`]: SPARQL `LOAD` reads nothing).
    pub(crate) load_policy: LoadPolicy,
    /// The query-evaluation settings installed on every database this server
    /// serves ([`PreparedSlot::serve`]; `conf.ini`'s `[query]` section, default
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

    /// Remember that transaction `tid` of database `db` ended with
    /// `outcome` (its load generation and begin time unknown: a retry gives
    /// the outcome to a bare id only).
    pub(crate) fn remember_ended(&mut self, db: &str, tid: u64, outcome: &'static str) {
        self.push_ended(EndedTxn {
            db: db.to_string(),
            tid,
            generation: None,
            begun: None,
            outcome,
        });
    }

    /// Remember how the registry entry `txn` (transaction `tid`) ended.
    pub(crate) fn remember_ended_txn(&mut self, txn: &Txn, tid: u64, outcome: &'static str) {
        self.push_ended(EndedTxn {
            db: txn.db.clone(),
            tid,
            generation: Some(txn.generation),
            begun: Some(txn.begun),
            outcome,
        });
    }

    fn push_ended(&mut self, ended: EndedTxn) {
        if self.ended_txns.len() >= ENDED_TXN_MEMORY {
            self.ended_txns.pop_front();
        }
        self.ended_txns.push_back(ended);
    }

    /// How transaction `tid` of database `db` (any database when `None`)
    /// ended, if it is remembered (the newest, for an id several loads
    /// used).
    pub(crate) fn ended_outcome(&self, db: Option<&str>, tid: u64) -> Option<&'static str> {
        self.ended_record(db, TidRef::plain(tid)).map(|e| e.outcome)
    }

    /// The newest remembered end of transaction `tid` of database `db` (any
    /// database when `None`); with a begin time, only the transaction that
    /// began then.
    fn ended_record(&self, db: Option<&str>, tid: TidRef) -> Option<&EndedTxn> {
        self.ended_txns.iter().rev().find(|e| {
            e.tid == tid.tid
                && db.is_none_or(|db| e.db == db)
                && tid.begun.is_none_or(|b| e.begun == Some(b))
        })
    }

    /// The database of open transaction `tid` (see
    /// [`open_txn_db_at`](Self::open_txn_db_at)).
    pub(crate) fn open_txn_db(&self, db: Option<&str>, tid: u64) -> Result<String, OpError> {
        self.open_txn_db_at(db, TidRef::plain(tid))
    }

    /// The database of open transaction `tid`: on `db` when named, else the
    /// one database where `tid` is open. Ids are numbered per database, so
    /// without a name an id is ambiguous when it is open on several databases
    /// — or open on one and recently ended on another: a client retrying the
    /// `/commit` of a transaction that already ended must not end another
    /// database's transaction of the same number. A `<begin>_<tid>` id names
    /// only the transaction that began then (review gap-txn-lifecycle#2): a
    /// stale one — of an earlier load or an earlier server process — is
    /// "not running" when its end is remembered, else unknown, and never
    /// ends the transaction that has the number now.
    pub(crate) fn open_txn_db_at(&self, db: Option<&str>, tid: TidRef) -> Result<String, OpError> {
        let begun_ok = |name: &String| {
            tid.begun.is_none_or(|b| {
                self.txns
                    .get(&(name.clone(), tid.tid))
                    .is_some_and(|t| t.begun == b)
            })
        };
        let id = tid.tid;
        if let Some(db) = db {
            let name = db.to_string();
            if self.txns.contains_key(&(name.clone(), id)) && begun_ok(&name) {
                return Ok(name);
            }
        } else {
            let mut on: Vec<&String> = self
                .txns
                .keys()
                .filter(|(_, t)| *t == id)
                .map(|(d, _)| d)
                .filter(|d| begun_ok(d))
                .collect();
            on.sort();
            if let [one] = on.as_slice() {
                let ended_elsewhere = self
                    .ended_txns
                    .iter()
                    .find(|e| e.tid == id && e.db != **one && tid.begun.is_none());
                if let Some(other) = ended_elsewhere {
                    return Err(OpError::new(
                        OpErrorKind::Conflict,
                        format!(
                            "transaction {id} is open on database '{one}' but transaction {id} \
                             of database '{}' was {} recently: name the database (db_name)",
                            other.db, other.outcome
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
                            "transaction {id} is open on several databases ({}): name the \
                             database (db_name)",
                            names.join(", ")
                        ),
                    ));
                }
            }
        }
        let outcome = match tid.begun {
            None => self.ended_outcome(db, id),
            Some(_) => self.ended_record(db, tid).map(|e| e.outcome),
        };
        Err(match outcome {
            Some(outcome) => {
                // The same number may be open again in a later load.
                let earlier = self.ended_record(db, tid).is_some_and(|e| {
                    self.txns.iter().any(|((d, t), open)| {
                        *d == e.db && *t == id && Some(open.generation) != e.generation
                    })
                });
                let when = if earlier {
                    " in an earlier load of the database"
                } else {
                    ""
                };
                OpError::new(
                    OpErrorKind::TxnEnded,
                    format!("transaction {id} is not running (already {outcome}{when})"),
                )
            }
            None if tid.begun.is_some() => OpError::new(
                OpErrorKind::TxnUnknown,
                format!(
                    "no open transaction {id} began at that time (a transaction of an earlier \
                     load or server process)"
                ),
            ),
            None => OpError::new(OpErrorKind::TxnUnknown, format!("no open transaction {id}")),
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
    /// The idle-transaction reaper's state: the earliest idle deadline the
    /// request path checks with no lock, the transaction clock (see
    /// [`reap_idle_txns`](Self::reap_idle_txns), `txn_idle_timeout`).
    pub(crate) txn_reap: TxnReapState,
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
    /// Counts the `[query]` settings installed
    /// ([`apply_query_settings`](Self::apply_query_settings)); advanced
    /// with [`ApiState::query_settings`], under the state lock. A database
    /// whose [`DbSlot::settings_gen`] lags takes the current settings the
    /// next time it is used ([`sync_query_settings`](Self::sync_query_settings)).
    settings_gen: AtomicU64,
    /// Whether `refreshconf` re-configures the process-wide diagnostic log
    /// ([`crate::logging`]) from the re-read `conf.ini`, as upstream's
    /// `Util::configure` re-runs log4cplus' `doConfigure(log_mode)`: the
    /// configuration last installed, or `None` when this server does not own
    /// the log (an embedded server; `gserver` opts in with
    /// [`ApiServer::with_log_refresh`]).
    log_refresh: Mutex<Option<crate::logging::LogConfig>>,
    /// Whether `refreshconf` re-installs the re-read `conf.ini`'s `[storage]`
    /// section as the process's default open options
    /// ([`crate::db::set_default_open_options`]) — for the process that owns
    /// them (`gserver`, which installed them at startup; see
    /// [`ApiServer::with_storage_refresh`]). An embedded server leaves the
    /// process's defaults alone (the default).
    storage_refresh: AtomicBool,
    /// The options this server opens databases with
    /// ([`ApiServer::with_open_options`]); `None`: the process defaults
    /// ([`crate::db::default_open_options`]), the environment included.
    open_options: RwLock<Option<crate::db::OpenOptions>>,
    /// Every user's PFN functions (`funquery` / `funcudb` / `funreview`,
    /// `PFN.name(…)` in queries): under `conf.ini`'s `pfn_base_path` when it
    /// is set, else `<root>/pfn`. A query calls the functions of the user it
    /// runs for ([`pfn_caller`](Self::pfn_caller)).
    pub(crate) pfn: Arc<crate::pfn::PfnRegistry>,
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
    /// the IP allow / deny lists, the failed-login lockout
    /// (`upstream_login_lockout`, `login_lockout_failures`,
    /// `login_lockout_max_seconds`; the counts are kept), and the `/api`
    /// layer's `max_output_size` and
    /// `licensetype`, the `[query]` evaluation settings (applied to the
    /// loaded databases too), the log's `log_mode` / `[log]` configuration
    /// for a server that owns the diagnostic log
    /// ([`ApiServer::with_log_refresh`], as upstream), and the `[storage]`
    /// section for a server that owns the process's open options
    /// ([`ApiServer::with_storage_refresh`]: databases loaded from then on
    /// use it; loaded ones keep the mode they were opened in). The address,
    /// worker pool, connection settings,
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
        let fresh = Arc::new(fresh);
        // Nothing below waits for a database (a busy one takes the new
        // `[query]` settings when next used), so the whole file is in effect
        // when the request is answered.
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&fresh);
        self.ipfilter
            .reconfigure(fresh.ip_allow_path.clone(), fresh.ip_deny_path.clone());
        // The lockout keys (`upstream_login_lockout`, `login_lockout_failures`,
        // `login_lockout_max_seconds`) take effect too, keeping the counts
        // (round-2 review: they used to read back new values while the
        // startup policy stayed in force).
        let (policy, upstream) = crate::gapi::ipfilter::IpFilter::lockout_of(&fresh);
        if upstream && !self.ipfilter.upstream_lockout() {
            crate::gs_warn!("upstream-compat: upstream_login_lockout=on (applied by refreshconf)");
        }
        self.ipfilter.set_lockout(policy, upstream);
        {
            let mut st = locked(&self.state);
            st.gapi.max_output_size = fresh.max_output_size;
            st.gapi.license_type = fresh.licensetype.clone();
        }
        self.apply_query_settings(fresh.query_settings());
        self.refresh_log(&fresh);
        self.refresh_storage(&fresh);
        Ok(true)
    }

    /// `refreshconf`'s part for the `[storage]` section (see
    /// [`ApiServer::with_storage_refresh`]): install it as the process's
    /// default open options, which every later `Database::open` of this
    /// process uses (the environment still overrides them), together with
    /// the storage defaults that are not open options (`[system]
    /// id_reclaim`, the RocksDB tuning keys:
    /// [`GStoreConfig::install_storage_defaults`]).
    fn refresh_storage(&self, fresh: &GStoreConfig) {
        if !self.storage_refresh.load(Ordering::SeqCst) {
            return;
        }
        crate::db::set_default_open_options(fresh.open_options());
        fresh.install_storage_defaults();
        let effective = crate::db::default_open_options();
        crate::gs_info!(
            "refreshconf: on-disk databases loaded from now on open in {} mode ({} MiB page \
             cache); databases loaded from now on use id reclamation {}",
            effective.disk_query_mode,
            (effective.disk_cache_pages * crate::kvstore::PAGE_SIZE) >> 20,
            crate::db::default_id_reclaim()
        );
    }

    /// `refreshconf`'s part for the diagnostic log (see
    /// [`ApiServer::with_log_refresh`]): re-install it when the re-read
    /// configuration describes another log than the one installed — which
    /// lacks the file when it could not be opened, so the next `refreshconf`
    /// tries the file again.
    fn refresh_log(&self, fresh: &GStoreConfig) {
        let mut last = locked(&self.log_refresh);
        let Some(current) = last.as_mut() else {
            return;
        };
        let (mut cfg, warnings) = crate::logging::LogConfig::from_gstore_config(fresh);
        let env_warning = cfg.apply_env();
        for w in warnings.iter().chain(env_warning.as_ref()) {
            crate::gs_warn!("log configuration: {w}");
        }
        if cfg == *current {
            return;
        }
        match crate::logging::init(&cfg) {
            Ok(_) => {
                crate::gs_info!(
                    "refreshconf: diagnostic log reconfigured (level {}, file {})",
                    crate::logging::filter_name(cfg.level),
                    cfg.file
                        .as_ref()
                        .map_or_else(|| "none".to_string(), |f| format!("'{}'", f.path.display()))
                );
                *current = cfg;
            }
            Err(e) => {
                crate::gs_error!(
                    "refreshconf: cannot open the log file '{}': {e}; logging to standard error only",
                    cfg.file
                        .as_ref()
                        .map(|f| f.path.display().to_string())
                        .unwrap_or_default()
                );
                // What `init` installed instead.
                *current = crate::logging::LogConfig { file: None, ..cfg };
            }
        }
    }

    /// Install `settings` as the query-evaluation settings of every database
    /// this server serves: recorded for the databases it opens from now on
    /// ([`PreparedSlot::serve`]) and applied to those already loaded. Never waits
    /// for a database (nor holds the state lock while locking one): a
    /// database busy with a statement, a CSR build or a batch takes them
    /// the next time it is used ([`sync_query_settings`](Self::sync_query_settings)).
    pub(crate) fn apply_query_settings(&self, settings: QuerySettings) {
        // The PFN registry checks the compile-time limits itself.
        self.pfn.set_limits(settings.pfn);
        let loaded: Vec<DbHandle> = {
            let mut st = locked(&self.state);
            st.query_settings = settings;
            self.settings_gen.fetch_add(1, Ordering::SeqCst);
            st.dbs.values().map(Arc::clone).collect()
        };
        for h in loaded {
            // A database a statement holds (shared or exclusive) is skipped:
            // its next request syncs them (`read_synced`).
            if let Some(mut slot) = h.try_write() {
                self.sync_query_settings(&mut slot);
            }
        }
    }

    /// `user` calling the server's PFN functions (under the registry's
    /// limits; a database replaces them with its own when the query runs).
    pub(crate) fn pfn_caller(&self, user: &str) -> crate::pfn::PfnCaller {
        crate::pfn::PfnCaller::new(Arc::clone(&self.pfn), user)
    }

    /// Bring an exclusively held database's `[query]` settings up to date,
    /// if new ones were installed since it got its own (a database that was
    /// busy then). Called whenever a database is held exclusively to run a
    /// statement or begin a transaction; readers go through
    /// [`read_synced`](Self::read_synced). The settings are read now, under
    /// the state lock (which a db-guard holder may take, see [`ApiState`]),
    /// so of two settings installed one after the other the later one always
    /// wins.
    pub(crate) fn sync_query_settings(&self, slot: &mut DbSlot) {
        if !self.settings_stale(slot) {
            return;
        }
        let (gen, settings) = {
            let st = locked(&self.state);
            (self.settings_gen.load(Ordering::SeqCst), st.query_settings)
        };
        slot.db.apply_query_settings(&settings);
        slot.settings_gen = gen;
    }

    /// Whether `slot` lacks the `[query]` settings installed last.
    fn settings_stale(&self, slot: &DbSlot) -> bool {
        !slot.retired && slot.settings_gen != self.settings_gen.load(Ordering::SeqCst)
    }

    /// A shared guard of `cell`, whose database has the `[query]` settings
    /// installed last whenever it could sync them: when they are stale, the
    /// shared guard is dropped, the exclusive one taken once to sync them,
    /// and a shared guard taken again (never an upgrade while holding the
    /// shared guard). Settings installed in between wait for the next
    /// request.
    ///
    /// The exclusive guard is only taken when it is free right now: waiting
    /// for it would wait for every statement in flight (the longest query)
    /// and, the lock preferring writers, make every later reader of the
    /// database wait behind it too. A database busy with other readers runs
    /// this statement with the settings it has, and takes the new ones with
    /// a later request or its next write section.
    pub(crate) fn read_synced<'a>(&self, cell: &'a DbCell) -> Result<SlotRead<'a>, OpError> {
        let slot = cell.read()?;
        if !self.settings_stale(&slot) {
            return Ok(slot);
        }
        drop(slot);
        if let Some(mut slot) = cell.try_write() {
            self.sync_query_settings(&mut slot);
        }
        cell.read()
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

    /// Open the database at `dir` with this server's open options (see
    /// [`ApiServer::with_open_options`]).
    pub(crate) fn open_db(&self, dir: &Path) -> crate::Result<Database> {
        let options = *self
            .open_options
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match options {
            Some(o) => Database::open_with(dir, &o),
            None => Database::open(dir),
        }
    }

    /// The layout of a build that names none: `[storage] default_layout`
    /// of this server's configuration, else of the process's open options
    /// (`gserver` installs its `[storage]` section), else the snapshot
    /// layout (the default disk query mode is Materialize, so the plan's
    /// rule keeps the snapshot layout).
    pub(crate) fn default_build_layout(&self) -> StorageKind {
        self.config()
            .default_layout
            .or_else(|| crate::db::default_open_options().default_layout)
            .unwrap_or(StorageKind::Snapshot)
    }

    /// Stop serving `name` (if loaded) *without* saving: take its handle out
    /// of the registry, then — with the state lock released — wait for any
    /// request still using it and retire the slot (see [`DbSlot::retire`]), so
    /// its files are closed before the caller replaces or deletes them and a
    /// request still holding the handle can neither use nor save it. Returns
    /// whether it was loaded. Must not be called with the state lock held
    /// (nor a guard of the database).
    pub(crate) fn retire_loaded(&self, name: &str) -> Result<bool, OpError> {
        let handle = locked(&self.state).dbs.remove(name);
        match handle {
            Some(h) => {
                h.write()?.retire();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// [`retire_loaded`](Self::retire_loaded), saving the loaded copy first
    /// when it holds writes not yet saved (check_point off): a rebuild that
    /// then fails serves it again, from its directory, with every write it
    /// acknowledged. A failed save keeps it loaded (and fails the request).
    /// A copy whose on-disk store failed cannot be saved: it is retired
    /// without a commit point (logged; review gap-txn-lifecycle#4).
    fn retire_loaded_saved(&self, name: &str) -> Result<bool, Resp> {
        let Some(h) = locked(&self.state).dbs.remove(name) else {
            return Ok(false);
        };
        let mut slot = match h.write() {
            Ok(slot) => slot,
            Err(e) => return Err(e.rest()),
        };
        let failure = (!slot.retired).then(|| slot.db.storage_failure()).flatten();
        if let Some(failure) = failure {
            crate::gs_warn!(
                "database '{name}' was retired without a commit point: its on-disk store had \
                 failed ({failure}); those changes are discarded"
            );
        } else if slot.dirty && !slot.retired {
            if let Err(e) = slot.save() {
                drop(slot);
                locked(&self.state).dbs.insert(name.to_string(), h);
                return Err(Resp::fail(
                    500,
                    &format!("build failed: saving the loaded database first failed: {e}"),
                ));
            }
        }
        slot.retire();
        Ok(true)
    }
}

/// The registry's bookkeeping for a database a REST `/build` built under
/// `name` for `creator` (`vanished`: a registered name whose directory was
/// removed outside the server — the new database must not inherit its
/// grants). Best effort: the registry is reconciled with the database
/// directories at every start anyway.
fn register_built(st: &mut ApiState, name: &str, vanished: bool, creator: &str) {
    if vanished {
        let _ = st.users.forget_database(name);
    }
    let _ = st.sysdb.register(name, creator);
}

/// Test-only: work a test arms to run inside the REST bulk build of a
/// directory, right before the bulk loader starts (the request has sampled
/// whether the directory existed and retired a loaded copy).
#[cfg(test)]
pub(crate) mod build_hooks {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};

    type Hook = Box<dyn FnOnce() + Send>;

    static HOOKS: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    /// Run `hook` in the next REST bulk build of `dir`.
    pub(crate) fn set(dir: &Path, hook: impl FnOnce() + Send + 'static) {
        HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((dir.to_path_buf(), Box::new(hook)));
    }

    pub(super) fn run(dir: &Path) {
        let hook = {
            let mut hooks = HOOKS.lock().unwrap_or_else(PoisonError::into_inner);
            hooks
                .iter()
                .position(|(d, _)| d == dir)
                .map(|i| hooks.remove(i).1)
        };
        if let Some(hook) = hook {
            hook();
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
/// lock held. It carries its own database handle and cancel flag, and the
/// statement parsed once: what routes it to a shared guard (a read) or the
/// exclusive one (an UPDATE).
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
    /// `sparql`, parsed at dispatch (with no lock held); `None` when it does
    /// not parse — the job then answers the syntax error (taking a shared
    /// guard only to check that the database is still served).
    pub(crate) stmt: Option<Statement>,
    pub(crate) fmt: ResultFormat,
    /// The user the statement runs for: its `PFN.name(…)` calls reach this
    /// user's functions (see [`ApiInner::pfn_caller`]).
    pub(crate) user: String,
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
        let pfn = Arc::new(
            crate::pfn::PfnRegistry::new(root.join("pfn"))
                .with_limits(GStoreConfig::default().query_settings().pfn),
        );
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
                txn_reap: TxnReapState::default(),
                life: Arc::clone(&life),
                ipfilter: IpFilter::from_config(&GStoreConfig::default()),
                apilogs: ApiLogs::new(),
                config: RwLock::new(Arc::new(GStoreConfig::default())),
                system_password: new_system_password(),
                tasks: TaskManager::default(),
                self_ref: OnceLock::new(),
                settings_gen: AtomicU64::new(0),
                log_refresh: Mutex::new(None),
                storage_refresh: AtomicBool::new(false),
                open_options: RwLock::new(None),
                pfn,
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
            if config.is_set("pfn_base_path") {
                inner.pfn = Arc::new(
                    crate::pfn::PfnRegistry::new(config.pfn_base_path.clone())
                        .with_limits(config.query_settings().pfn),
                );
            }
            *inner
                .config
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner) = Arc::new(config);
        }
        self
    }

    /// The `conf.ini` settings this server runs with.
    pub fn config(&self) -> Arc<GStoreConfig> {
        self.inner.config()
    }

    /// The IP access control and failed-login lockout in force (for
    /// monitoring and tests).
    #[doc(hidden)]
    pub fn ip_filter(&self) -> &IpFilter {
        &self.inner.ipfilter
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

    /// Whether `root` still has upstream's default password
    /// ([`DEFAULT_ROOT_PASSWORD`](crate::config::DEFAULT_ROOT_PASSWORD)) —
    /// as stored, whatever `conf.ini` says now (it seeds a new store only).
    pub fn root_has_default_password(&self) -> bool {
        locked(&self.inner.state)
            .users
            .verify(ROOT_USER, crate::config::DEFAULT_ROOT_PASSWORD)
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

    /// Open the databases this server loads (`/load`, `/api load`, a
    /// restore, a build in the disk or RocksDB layout) with `options`, as
    /// given — instead of the process defaults
    /// ([`crate::db::default_open_options`], which the environment can
    /// override). For an embedded server that needs one disk query mode
    /// whatever the process defaults say.
    pub fn with_open_options(self, options: crate::db::OpenOptions) -> ApiServer {
        *self
            .inner
            .open_options
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(options);
        self
    }

    /// Let `refreshconf` re-install the re-read `conf.ini`'s `[storage]`
    /// section (`disk_query_mode`, `disk_cache_mb`, `dict_cache_mb`,
    /// `vstree_on_open`, `default_layout`) as the process's default open
    /// options ([`crate::db::set_default_open_options`], contract C7) — for
    /// the process that owns them (`gserver`, which installed the same
    /// section at startup). Databases loaded afterwards open with them (the
    /// environment, `GSTORE_DISK_QUERY_MODE` / `GSTORE_DISK_CACHE_MB`, still
    /// overrides them); loaded ones keep the mode they were opened in. An
    /// embedded server leaves the process's defaults alone (the default).
    pub fn with_storage_refresh(self) -> ApiServer {
        self.inner.storage_refresh.store(true, Ordering::SeqCst);
        self
    }

    /// Let `refreshconf` re-configure the process-wide diagnostic log
    /// ([`crate::logging`]) from the re-read `conf.ini` — its `log_mode`
    /// properties file and `[log]` keys, then `GSTORE_LOG` — as upstream's
    /// `refreshconf` does (`Util::configure` re-runs log4cplus'
    /// `doConfigure`). For the process that owns the log (`gserver`, which
    /// installed it from the same configuration): the log is re-installed
    /// only when the re-read configuration differs from what it was. An
    /// embedded server leaves the log alone (the default).
    pub fn with_log_refresh(self) -> ApiServer {
        let (mut current, _) = crate::logging::LogConfig::from_gstore_config(&self.inner.config());
        let _ = current.apply_env();
        // The configured file could not be opened at startup (`logging::init`
        // then installs the rest without it): the next refreshconf tries it.
        let installed_file =
            crate::logging::current().and_then(|l| l.file_path().map(Path::to_path_buf));
        if current.file.as_ref().map(|f| &f.path) != installed_file.as_ref() {
            current.file = None;
        }
        *locked(&self.inner.log_refresh) = Some(current);
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

    /// The registry of every user's PFN functions (see [`crate::pfn`]).
    pub fn pfn_registry(&self) -> Arc<crate::pfn::PfnRegistry> {
        Arc::clone(&self.inner.pfn)
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
    /// already consults this flag: `prepare_query` clones it into the `QueryJob`
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

    /// Test hook: the default failed-login lockout's `policy` (after
    /// [`with_config`](Self::with_config), which sets it from `conf.ini`;
    /// `refreshconf` sets it from the file again) — e.g. a long first lock,
    /// so a test's assertion that an account is locked never depends on
    /// how fast the test runs (round-2 review).
    #[doc(hidden)]
    pub fn with_lockout_policy(self, policy: crate::gapi::ipfilter::LockoutPolicy) -> ApiServer {
        let upstream = self.inner.ipfilter.upstream_lockout();
        self.inner.ipfilter.set_lockout(policy, upstream);
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
        let _txn_reaper = self.inner.start_txn_reaper();
        // The body caps, budget and pre-body hook of this run (see
        // `crate::preauth`).
        let ingest = crate::preauth::api_ingest(&self.http, &self.inner, self.addr);
        let (drained, workers) = run_accept_loop(
            &self.listener,
            &self.life,
            self.inner.grace,
            &self.http,
            |acc| {
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
            },
        );
        let report = self.finish_shutdown(drained);
        self.join_threads(workers);
        report
    }

    /// Like [`serve_forever`](Self::serve_forever) but terminates TLS in-process
    /// using `config` (see [`crate::server::load_tls_config`]). Requires the
    /// `tls` feature.
    #[cfg(feature = "tls")]
    pub fn serve_forever_tls(&self, config: Arc<rustls::ServerConfig>) -> ShutdownReport {
        let _ = self.inner.self_ref.set(Arc::downgrade(&self.inner));
        let _txn_reaper = self.inner.start_txn_reaper();
        // The body caps, budget and pre-body hook of this run (see
        // `crate::preauth`).
        let ingest = crate::preauth::api_ingest(&self.http, &self.inner, self.addr);
        let (drained, workers) = run_accept_loop(
            &self.listener,
            &self.life,
            self.inner.grace,
            &self.http,
            |acc| {
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
            },
        );
        let report = self.finish_shutdown(drained);
        self.join_threads(workers);
        report
    }

    /// After a shutdown: join the connection workers, the cancel bridge (if
    /// no other server uses it) and the asynchronous operations' runner and
    /// callback threads, bounded, so a program that stops this server and
    /// starts another in the same process is not left with this one's
    /// threads — nor with the database handles they hold (review conc#14;
    /// the statistics refreshes were joined by `finish_shutdown`, the
    /// idle-transaction reaper is joined when `serve_forever` drops it). A
    /// runner still running an operation that ignored its cancel past
    /// `TaskConfig::shutdown_wait` (or a callback still being delivered,
    /// bounded by `callback_timeout`) is not waited for again: it finishes
    /// on its own.
    fn join_threads(&self, workers: crate::server::WorkerThreads) {
        crate::server::join_workers(workers);
        cancel_bridge::quiesce();
        let tasks = &self.inner.tasks;
        let bound = if tasks.async_idle() {
            tasks.config().shutdown_wait
        } else {
            Duration::ZERO
        };
        let _ = tasks.join_threads(bound);
    }

    /// The tail of a graceful shutdown, after the listener closed and in-flight
    /// requests had their grace period: cancel queries that are still running,
    /// roll back open transactions (uncommitted work is never persisted), save
    /// every database with unsaved changes, and retire every slot so a request
    /// that is still queued on one can no longer apply a write nobody would
    /// save. From the start, every write still in flight saves itself before it
    /// is acknowledged (see [`ApiInner::closing`]). The databases nobody holds
    /// are done first; one a cancelled statement still holds is waited for
    /// afterwards (it stops at its next cancellation check), so it never holds
    /// up the others' saves.
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
        let _ = inner
            .tasks
            .wait_async_idle(inner.tasks.config().shutdown_wait);
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
        // A background statistics refresh holds a shared guard for one of
        // its passes: stop them first (they end at their next pass; review
        // conc#9) — joined, bounded, once the databases are saved.
        let refreshes = crate::gapi::stop_stats_refreshes(
            &handles
                .iter()
                .map(|(_, h)| Arc::clone(h))
                .collect::<Vec<_>>(),
        );
        // Roll back, save and retire one database, its guard held.
        let mut finish = |name: String, mut slot: SlotWrite<'_>| {
            if slot.retired {
                return;
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
                    &format!(
                        "{}\trollback\t{}\t{}\t(shutdown)",
                        now_millis(),
                        name,
                        open.tid
                    ),
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
        };
        // Every database nobody holds first; then wait for the ones a
        // statement still reads (it was cancelled above and stops at its
        // next check — an index build included), so a database kept busy
        // delays only its own save, never the others' (review conc#5).
        let mut busy = Vec::new();
        for (name, h) in &handles {
            match h.try_write() {
                Some(slot) => finish(name.clone(), slot),
                None => busy.push((name, h)),
            }
        }
        for (name, h) in busy {
            match h.write() {
                Ok(slot) => finish(name.clone(), slot),
                Err(e) => crate::gs_error!("shutdown: {}", e.msg),
            }
        }
        let lingering = refreshes.join(crate::server::THREAD_JOIN_WAIT);
        if lingering > 0 {
            crate::gs_warn!(
                "shutdown: {lingering} statistics refresh thread(s) did not stop in time and \
                 were left to finish on their own"
            );
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
        self.reap_idle_txns_if_due();

        let started = Instant::now();
        let (resp, user_disp) = catch_unwind(AssertUnwindSafe(|| self.respond(&req, peer, meta)))
            .unwrap_or_else(|_| {
                crate::gs_error!("request handler panicked: {} {}", req.method, req.path);
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
        // concurrently. The client's IP closes the line (after the fields
        // earlier versions wrote, so readers of those keep working).
        let ip = peer_ip(peer);
        let cfg = self.config();
        if cfg.accesslog_mode {
            append_daily_log(
                &self.root,
                "access.log",
                &format!(
                    "{ts}\t{method}\t{path}\t{user}\t{status}\t{ip}",
                    ts = now_millis(),
                    method = req.method,
                    path = req.path,
                    user = user_disp,
                    status = resp.status,
                ),
                cfg.accesslog_days,
            );
        }
        crate::gs_debug!(
            "{ip} {} {} -> {} ({} ms, user {user_disp})",
            req.method,
            req.path,
            resp.status,
            started.elapsed().as_millis()
        );

        let enc = req.header("accept-encoding");
        let extra: Vec<(&str, &str)> = resp
            .extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let headers = meta.headers(&extra);
        if let Some(file) = &resp.stream {
            // A download: copied in bounded chunks, never buffered (review
            // gapi#4). A file that cannot be positioned is answered as
            // upstream's `FileReadError`; one that ends early cuts the
            // connection (its `Content-Length` cannot be met).
            match file.reader() {
                Ok(reader) => write_response_streamed(
                    stream,
                    resp.status,
                    &resp.ctype,
                    &headers,
                    reader,
                    file.len(),
                )?,
                Err(_) => {
                    let (status, ctype, body, _) =
                        crate::gapi::GResp::error(crate::gapi::Status::FileReadError, "")
                            .into_parts();
                    write_response_ext(stream, status, &ctype, &meta.headers(&[]), &body)?;
                }
            }
            return Ok(true);
        }
        write_response_maybe_gzip(stream, resp.status, &resp.ctype, &headers, &resp.body, enc)?;
        Ok(true)
    }

    /// Route and execute one request, returning the response and the caller's
    /// display name for the access log. `meta` carries the verdict of the
    /// connection's browser policy ([`HttpOptions`]): a refused `Host` /
    /// `Origin`, whether the client counts as local, `allow_get_update`.
    fn respond(&self, req: &Request, peer: Option<SocketAddr>, meta: &ConnMeta) -> (Resp, String) {
        let upstream = crate::gapi::wants(req);
        // A foreign Host on a loopback connection (DNS rebinding) or a
        // foreign Origin on a state-changing request (CSRF), refused before
        // anything else — `/api` the upstream way (HTTP 200, 1101).
        if let Some(why) = meta.refusal() {
            let resp = if upstream {
                Resp::from(crate::gapi::GResp::error(
                    crate::gapi::Status::IpBlocked,
                    why,
                ))
            } else {
                Resp::fail(403, why)
            };
            return (resp, "-".to_string());
        }
        // The upstream-compatible `/api` protocol (`/api`, `POST /`,
        // `GET /?operation=…`, its path routes): per-request credentials and a
        // JSON envelope, served by `crate::gapi` with no lock held.
        if upstream {
            let (resp, user_disp) = crate::gapi::serve(self, req, peer, meta.local_trusted());
            return (Resp::from(resp), user_disp);
        }
        // The IP checks of the `/api` layer hold for every endpoint on this
        // listener: the allow / deny lists, and — under
        // `upstream_login_lockout` — upstream's per-address lockout for
        // everything but `/login` (as `/api` exempts its `login`). The
        // default lockout refuses a locked account's password check instead
        // (`/login`'s and Basic credentials' below), before the hash runs.
        let ip = peer_ip(peer);
        if !self.ipfilter.permits(&ip) {
            return (
                Resp::fail(403, crate::gapi::ipfilter::BLOCKED_MSG),
                "-".to_string(),
            );
        }
        if req.path != "/login" && self.ipfilter.locked_out(&ip) {
            return (
                Resp::fail(403, crate::gapi::ipfilter::LOCKED_MSG),
                "-".to_string(),
            );
        }
        // Root's default password is a loopback privilege: refused from any
        // client that is not local (a remote one, or one forwarded by a
        // reverse proxy on this machine), whether or not root still has it.
        // Not a failed login for the lockout: the fixed refusal reveals
        // nothing, and counting it against a proxy's address would lock out
        // every client behind the proxy.
        if !meta.local_trusted() && presents_default_root_password(req) {
            return (Resp::fail(401, DEFAULT_PASSWORD_REFUSED), "-".to_string());
        }
        // The SPARQL Protocol's method and body rules (see
        // `HttpOptions::allow_get_update`): an UPDATE over `GET /sparql` —
        // classified by a parse of the (request-line sized) statement, before
        // anything runs — and an `/update` body of another type.
        if !meta.allow_get_update() {
            if req.method == "GET" && req.path == "/sparql" {
                let get_update = req
                    .param("sparql")
                    .or_else(|| req.query_string())
                    .is_some_and(|q| crate::server::is_update_text(&q));
                if get_update {
                    return (
                        Resp::fail(400, crate::server::GET_UPDATE_REFUSED),
                        "-".to_string(),
                    );
                }
            }
            // By the body's type alone: the body of a caller who is not
            // authenticated yet is not copied (`update_text` would copy it).
            if req.method == "POST"
                && req.path == "/update"
                && req.body_kind() == crate::server::BodyKind::Simple
            {
                return (
                    Resp::fail(415, crate::server::UPDATE_BODY_REFUSED),
                    "-".to_string(),
                );
            }
        }
        // `/login` is public and reads its credentials from a body nobody has
        // authenticated yet: its parameter lookups run with no `ApiState` lock
        // held (http#4), and its own credentials are all it takes (a Basic
        // header on it is not looked at).
        if req.path == "/login" {
            locked(&self.state).requests += 1;
            let resp = match self.handle_login(req) {
                Dispatch::Done(r) => r,
                Dispatch::Deferred(f) => f(self),
            };
            return (resp, "-".to_string());
        }
        // Basic credentials are checked with the state lock held only to look
        // the user up: the deliberately slow password hash runs with no lock
        // held, so a client sending bad credentials (no account needed) cannot
        // stall every other request — `/cancel` included — behind it. An
        // account the lockout refuses at this address is answered `429`
        // before any hash runs (review http#6 / sec#7).
        let basic = match parse_basic_auth(req) {
            None => None,
            Some((u, p)) => {
                if let Some(refusal) = self.ipfilter.auth_refusal_detail(&u, &ip) {
                    return (lockout_refused(refusal), "-".to_string());
                }
                let attempt = locked(&self.state)
                    .users
                    .begin_check(&u, &p, PasswordForm::Plain);
                Some((u, attempt.verify()))
            }
        };
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
    /// endpoints (see [`run_query_job`](Self::run_query_job)), recording it in
    /// the query log (`qlog`).
    fn execute_query_job(&self, job: QueryJob, qlog: RestQueryLog) -> Resp {
        let fmt = job.fmt;
        let out = self.run_query_job(job);
        // An update applied in memory but not saved is logged as the failure
        // it is answered as (HTTP 500), not as a success (review
        // durability#2).
        let not_durable = match &out {
            Ok(JobOutput::Updated {
                warning: Some(w), ..
            }) => Some(OpError::new(OpErrorKind::Persist, w.clone())),
            _ => None,
        };
        // A read is rendered before it is logged: an answer over the result
        // byte budget is logged as the failure it is answered as.
        let rendered = match &out {
            Ok(JobOutput::Read(result)) => Some(render_result_within(
                fmt,
                result,
                self.config().max_result_bytes,
            )),
            _ => None,
        };
        qlog.finish(
            self,
            match (&out, &not_durable, &rendered) {
                (_, Some(e), _) => Err(e),
                (_, None, Some(Err(e))) => Err(e),
                (Ok(JobOutput::Read(result)), None, _) => Ok(answer_count(result)),
                (Ok(JobOutput::Updated { changed, .. }), None, _) => Ok(*changed),
                (Err(e), None, _) => Err(e),
            },
        );
        match (out, rendered) {
            (_, Some(Ok(resp))) => resp,
            (_, Some(Err(e))) => e.rest(),
            (Ok(JobOutput::Updated { changed, warning }), None) => update_resp(changed, &warning),
            (Ok(JobOutput::Read(result)), None) => render_result(fmt, &result),
            (Err(e), None) => e.rest(),
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
    ///
    /// The statement runs under `query_timeout` (review sec#10, robust#5): a
    /// deadline raises the job's cancel flag — the one `/cancel` raises —
    /// once its time is up, the evaluator stops at its next cancellation
    /// check and releases the database (a writer queued behind a runaway
    /// read is no longer blocked for good), and the job fails as
    /// [`OpErrorKind::Timeout`]. Only the evaluation counts towards it
    /// ([`StatementClock`]): not the wait for the database's guard, nor the
    /// save after an UPDATE.
    pub(crate) fn run_query_job(&self, job: QueryJob) -> Result<JobOutput, OpError> {
        // Ends the task (also on a panic) once the job — and so its db lock —
        // is done.
        let mut task = TaskGuard::new(self, job.task_id);
        let clock = StatementClock::new(&job.cancel, self.config().query_timeout);
        let out = self.run_query_job_locked(job, &clock);
        let out = match out {
            Err(e) if clock.timed_out() && e.kind != OpErrorKind::Persist => {
                Err(timed_out_err(e, &clock))
            }
            out => out,
        };
        task.set_ok(out.is_ok());
        out
    }

    fn run_query_job_locked(
        &self,
        job: QueryJob,
        clock: &StatementClock,
    ) -> Result<JobOutput, OpError> {
        #[cfg(test)]
        if job.sparql.contains("__gstore_test_panic__") {
            let _slot = job.db.write();
            panic!("test-triggered panic inside a query job");
        }
        let unloaded = || {
            OpError::new(
                OpErrorKind::NotLoaded,
                format!(
                    "database '{}' was unloaded while this request waited",
                    job.name
                ),
            )
        };
        let query_err = |e: GStoreError| OpError::statement(&e);
        let caller = self.pfn_caller(&job.user);
        let Some(stmt) = job.stmt else {
            // A syntax error: answered once the database is known to be
            // served, as for any statement (a shared guard suffices).
            if job.db.read()?.retired {
                return Err(unloaded());
            }
            return Err(match Statement::parse(&job.sparql) {
                Err(e) => query_err(e),
                Ok(_) => OpError::new(OpErrorKind::Failed, "the statement was not parsed"),
            });
        };
        if !stmt.is_update() {
            let out = self.read_committed(&job.db, &job.name, &stmt, clock, &caller);
            clock.stop();
            return out.map(JobOutput::Read);
        }
        txn_park(&job.name, TxnParkPoint::StatementQueued);
        let mut slot = job.db.write()?;
        if slot.retired {
            return Err(unloaded());
        }
        if let Some(open) = &slot.txn {
            return Err(txn_conflict_err(&job.name, open.tid));
        }
        self.sync_query_settings(&mut slot);
        let before = slot.db.mutation_count();
        clock.start();
        txn_park(&job.name, TxnParkPoint::StatementHolding);
        let result = slot
            .db
            .execute_as(stmt, Some(clock.cancel()), Some(&caller));
        clock.stop();
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
                let warning = if wrote {
                    slot.wrote(self.save_writes())
                } else {
                    None
                };
                // A failed save of the applied part is a persistence failure
                // (REST 500), not a query error (400): review durability#2.
                // Otherwise the statement's own error decides (design#3): a
                // read or storage failure is a 500, a cancellation a 503.
                let kind = if warning.is_some() {
                    OpErrorKind::Persist
                } else {
                    OpErrorKind::of(&e, OpErrorKind::Query)
                };
                let msg = if wrote {
                    partial_update_msg(&e, warning)
                } else {
                    e.to_string()
                };
                Err(OpError::new(kind, msg))
            }
        }
    }

    /// Run the non-transactional read `stmt` of database `dbh` (its task's
    /// `cancel` flag, the user's PFN `caller`): on a shared guard of the
    /// live database, or — while a transaction is open — on its begin-time
    /// snapshot, released from the guard first (the snapshot is immutable,
    /// and the owner's `/tquery`s and `/commit` must not wait for it).
    ///
    /// An on-disk snapshot read that ends [`Expired`](SnapshotReadError::Expired)
    /// (the transaction committed meanwhile: its commit cancelled the read)
    /// is re-run on the database as it is then — re-checking under the
    /// guard that it is still served and whether a transaction is open —
    /// so it answers the committed state, never an uncommitted one. The last
    /// of [`SNAPSHOT_READ_ATTEMPTS`] runs keeps the guard while it reads,
    /// which no commit can pass. `Closed` (and a database no longer served)
    /// is answered as `Closed`: the caller reports it as not loaded.
    ///
    /// The statement's `query_timeout` `clock` starts once it first holds
    /// the guard (the caller stops it).
    pub(crate) fn read_committed(
        &self,
        dbh: &DbHandle,
        name: &str,
        stmt: &Statement,
        clock: &StatementClock,
        caller: &crate::pfn::PfnCaller,
    ) -> Result<QueryResult, OpError> {
        let unloaded = || {
            OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' was unloaded while this request waited"),
            )
        };
        let query_err = |e: GStoreError| OpError::statement(&e);
        for attempt in 1..=SNAPSHOT_READ_ATTEMPTS {
            txn_park(name, TxnParkPoint::StatementQueued);
            let slot = self.read_synced(dbh)?;
            if slot.retired {
                return Err(unloaded());
            }
            clock.start();
            txn_park(name, TxnParkPoint::StatementHolding);
            let Some(open) = &slot.txn else {
                return slot
                    .db
                    .read_as(stmt, Some(clock.cancel()), Some(caller))
                    .map_err(query_err);
            };
            let snapshot = Arc::clone(&open.snapshot);
            let last = attempt == SNAPSHOT_READ_ATTEMPTS;
            let held = if last {
                Some(slot)
            } else {
                drop(slot);
                None
            };
            let read = snapshot.read_as(stmt, Some(clock.cancel()), Some(caller));
            if matches!(read, Err(SnapshotReadError::Expired)) {
                EXPIRED_SNAPSHOT_READS.fetch_add(1, Ordering::Relaxed);
            }
            match read {
                Ok(result) => return Ok(result),
                Err(SnapshotReadError::Expired) if !last => {}
                Err(SnapshotReadError::Failed(e)) => return Err(query_err(e)),
                Err(SnapshotReadError::Closed) => return Err(unloaded()),
                Err(SnapshotReadError::Expired) => {
                    drop(held);
                    return Err(OpError::new(
                        OpErrorKind::Failed,
                        format!(
                            "database '{name}': the transaction's read snapshot kept expiring \
                             while it was read"
                        ),
                    ));
                }
            }
        }
        unreachable!("the last attempt returns")
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
            let ip = peer_ip(crate::server::current_peer());
            // Also migrates a legacy password hash on first successful use,
            // so Basic-auth-only clients get upgraded like `/login` users.
            if st.users.finish_check(checked) == CredentialCheck::Valid {
                self.ipfilter.note_auth_success(&u, &ip);
                return Some(u);
            }
            // Counts towards the lockout, as a failed `/api` login.
            self.ipfilter.note_auth_failure(&u, &ip);
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
            ("POST", "/batchInsert") => Some(("batchInsert", Self::handle_batch_insert as Handler)),
            ("POST", "/batchRemove") => Some(("batchRemove", Self::handle_batch_remove as Handler)),
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
            // The privilege check stays here, under the state lock; the
            // password hash and the users.db write run deferred, with no
            // lock held (review http#7).
            ("POST", "/user/create") => {
                if !crate::sysdb::allowed(&st.users, &user, "usermanage", None) {
                    return Dispatch::Done(self.forbidden());
                }
                return self.handle_user_create(st, req);
            }
            ("POST", "/user/drop") => {
                self.guarded(st, &user, "usermanage", |s, st| s.handle_user_drop(st, req))
            }
            ("POST", "/user/password") => return self.handle_user_password(st, req, &user),
            ("POST", "/user/grant") => self.guarded(st, &user, "userprivilegemanage", |s, st| {
                s.handle_user_grant(st, req, true)
            }),
            ("POST", "/user/revoke") => self.guarded(st, &user, "userprivilegemanage", |s, st| {
                s.handle_user_grant(st, req, false)
            }),
            ("GET", "/user/show") => {
                self.guarded(st, &user, "showuser", |s, st| s.handle_user_show(st))
            }
            ("GET", "/querylog") => {
                self.guarded(st, &user, "querylog", |s, st| s.read_log(st, "query.log"))
            }
            ("GET", "/accesslog") => {
                self.guarded(st, &user, "accesslog", |s, st| s.read_log(st, "access.log"))
            }
            ("GET", "/txnlog") => {
                self.guarded(st, &user, "txnlog", |s, st| s.read_log(st, "txn.log"))
            }
            ("GET", "/tasklist") => self.guarded(st, &user, "tasklist", |s, _| s.handle_tasklist()),
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
                let _guard = held.as_ref().map(|h| h.write());
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
        let (Some(username), Some(password)) = (req.param("username"), req.param("password"))
        else {
            return Dispatch::Done(Resp::fail(400, "missing username or password"));
        };
        Dispatch::Deferred(Box::new(move |inner| {
            let ip = peer_ip(crate::server::current_peer());
            // A locked account is refused before the slow hash (review
            // http#6): guessing through `/login` gets no further.
            if let Some(refusal) = inner.ipfilter.auth_refusal_detail(&username, &ip) {
                return lockout_refused(refusal);
            }
            let attempt =
                locked(&inner.state)
                    .users
                    .begin_check(&username, &password, PasswordForm::Plain);
            let checked = attempt.verify();
            let mut st = locked(&inner.state);
            if st.users.finish_check(checked) != CredentialCheck::Valid {
                inner.ipfilter.note_auth_failure(&username, &ip);
                return Resp::fail(401, "invalid username or password");
            }
            // As `/api`'s `login`: a successful login clears the account's
            // failed-login count at this address (upstream: the address's).
            inner.ipfilter.note_login_success(&username, &ip);
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
        // `sparql=` / `query=` of the URL or a form body, else a raw body
        // (never parameters read out of one, see `Request`).
        let Some(sparql) = req.param("sparql").or_else(|| req.query_string()) else {
            return Dispatch::Done(Resp::fail(400, "missing sparql/query parameter"));
        };
        if let Some(p) = req.first_param_of(crate::server::QUERY_DATASET_PARAMS) {
            return Dispatch::Done(Resp::fail(400, &crate::server::dataset_param_refusal(p)));
        }
        let db = db_param(req);
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            // Parsed once, with no lock held: the parse classifies the
            // statement (privilege, shared or exclusive guard) and is what
            // the job runs.
            let stmt = Statement::parse(&sparql).ok();
            let is_update = stmt.as_ref().is_some_and(Statement::is_update);
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
            log_query(&inner.config(), &inner.root, &name, &user, &sparql);
            let qlog = RestQueryLog::start(inner, &name, &sparql, fmt);
            let prepared = {
                let mut st = locked(&inner.state);
                inner.prepare_query(&mut st, &name, sparql, stmt, fmt, &user)
            };
            let job = match prepared {
                Ok(job) => job,
                Err(r) => return r,
            };
            inner.execute_query_job(job, qlog)
        }))
    }

    /// `/update` (the caller holds the Update privilege): like
    /// [`handle_query`](Self::handle_query), the statement is classified with
    /// the state lock released.
    fn handle_update(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(sparql) = req
            .param("sparql")
            .or_else(|| req.param("update"))
            .or_else(|| req.raw_statement())
        else {
            return Dispatch::Done(Resp::fail(400, "missing update statement"));
        };
        if let Some(p) = req.first_param_of(crate::server::UPDATE_DATASET_PARAMS) {
            return Dispatch::Done(Resp::fail(400, &crate::server::dataset_param_refusal(p)));
        }
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        if !crate::sysdb::allowed(&st.users, user, "update", Some(&name)) {
            return Dispatch::Done(self.forbidden());
        }
        let fmt = ResultFormat::negotiate(req.param("format").as_deref(), req.header("accept"));
        let user = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let stmt = Statement::parse(&sparql).ok();
            log_query(&inner.config(), &inner.root, &name, &user, &sparql);
            let qlog = RestQueryLog::start(inner, &name, &sparql, fmt);
            let prepared = {
                let mut st = locked(&inner.state);
                inner.prepare_query(&mut st, &name, sparql, stmt, fmt, &user)
            };
            let job = match prepared {
                Ok(job) => job,
                Err(r) => return r,
            };
            inner.execute_query_job(job, qlog)
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
        stmt: Option<Statement>,
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
            stmt,
            fmt,
            user: user.to_string(),
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
        let vanished =
            st.sysdb.contains(&name) && !st.dbs.contains_key(&name) && !st.db_dir(&name).exists();
        if !exists && st.database_limit_reached(max_dbs) {
            return Dispatch::Done(Resp::fail(
                409,
                &format!(
                    "cannot build '{name}': the server already holds max_database_num \
                     ({max_dbs}) databases"
                ),
            ));
        }
        let layout = match build_layout(req.param("layout").as_deref(), self.default_build_layout())
        {
            Ok(layout) => layout,
            Err(e) => return Dispatch::Done(Resp::fail(400, &e)),
        };
        if let Err(r) = st.claim(&name) {
            return Dispatch::Done(r);
        }
        // Build from a server-local file path, or from the request body (RDF).
        let path = req.param("db_path").or_else(|| req.param("path"));
        let body = if path.is_none() {
            req.body_str()
        } else {
            String::new()
        };
        let dir = st.db_dir(&name);
        let policy = st.load_policy.clone();
        let creator = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let _busy = BusyGuard {
                inner,
                name: name.clone(),
            };
            let source = match path {
                Some(path) => match build_source(&policy, &path) {
                    Ok(file) => RestBuildSource::File(file),
                    Err(r) => return r,
                },
                None => RestBuildSource::Text(body),
            };
            if layout != StorageKind::Snapshot {
                return inner.rest_build_bulk(&name, &dir, &source, layout, vanished, &creator);
            }
            let built = match &source {
                RestBuildSource::File(file) => Database::build_from_files(name.clone(), &[file]),
                RestBuildSource::Text(body) => Database::build_from_str(name.clone(), body),
            };
            let db = match built {
                Ok(db) => db,
                Err(e) => return fail_with(&e, OpErrorKind::Query, &format!("build failed: {e}")),
            };
            // No other process may have the directory open while it is
            // rewritten (shares the lock a loaded copy holds in this process).
            let _dir_lock = match DirLock::acquire_existing(&dir) {
                Ok(lock) => lock,
                Err(e) => {
                    return fail_with(&e, OpErrorKind::Conflict, &format!("build failed: {e}"))
                }
            };
            // A rebuilt database replaces the loaded one: retire it (closing
            // its files and releasing its handle on the directory) before its
            // directory is rewritten, as a drop does — saved first when it
            // holds acknowledged writes not yet saved (check_point off); the
            // new one is served below.
            let was_loaded = match inner.retire_loaded_saved(&name) {
                Ok(loaded) => loaded,
                Err(r) => return r,
            };
            // No `reasonManage` file operation is half-way through the rule
            // directory meanwhile. A rebuild replaces the database, as a drop
            // and a build do: the old one's rules (executed, with ledgers
            // naming triples the new data asserts) go — once the new
            // database is saved, not before (review http#10: a failed save
            // served the previous database again without its rules and
            // ledgers). As the bulk layouts' install: a crash between the
            // save and the removal leaves them (docs/REFACTOR_BACKLOG.md).
            let rules = RuleStore::dir(&dir);
            let rules_lock = rules.lock();
            if let Err(e) = db.save(&dir) {
                drop(rules_lock);
                // The snapshot set is replaced atomically: the directory
                // holds the previous database (saved above), with its rules,
                // served again with every write it acknowledged.
                if was_loaded {
                    inner.serve_again(&name, &dir);
                }
                return fail_with(&e, OpErrorKind::Failed, &format!("save failed: {e}"));
            }
            let replaced = crate::reason::store::remove_rule_dir_locked(&dir, &rules_lock);
            drop(rules_lock);
            if let Err(e) = replaced {
                // Built, but the replaced rules are still there: not served
                // (they would reason over the new data), as a bulk build
                // whose install did not finish.
                register_built(&mut locked(&inner.state), &name, vanished, &creator);
                return fail_with(
                    &e,
                    OpErrorKind::Failed,
                    &format!(
                        "database '{name}' was built, but the rules of the database it replaced \
                         could not be removed ({e}); it is not loaded: remove its \
                         {RULE_DIR} directory, then load it"
                    ),
                );
            }
            inner.serve_built(&name, db, dir, vanished, &creator)
        }))
    }

    /// Serve database `db` (just built at `dir`) under `name` and register
    /// it for `creator` (`vanished`: forget the grants of a dropped
    /// database of that name first). The REST `/build` answer.
    fn serve_built(
        &self,
        name: &str,
        db: Database,
        dir: PathBuf,
        vanished: bool,
        creator: &str,
    ) -> Resp {
        let triples = db.triple_num();
        // The directory is locked and measured before the state lock is
        // taken (review http#13/conc#7).
        let prepared = DbSlot::prepare(db, dir, false);
        let mut st = locked(&self.state);
        let slot = prepared.serve(&st.load_policy, &st.query_settings);
        st.dbs.insert(name.to_string(), slot);
        register_built(&mut st, name, vanished, creator);
        Resp::ok(&format!("database '{name}' built ({triples} triples)"))
    }

    /// Serve the database at `dir` again under `name`: a rebuild that failed
    /// after it retired the loaded copy left the previous database there
    /// (saved before it was retired). Best effort: one that does not open
    /// stays unloaded.
    fn serve_again(&self, name: &str, dir: &Path) {
        if let Ok(db) = self.open_db(dir) {
            let prepared = DbSlot::prepare(db, dir.to_path_buf(), false);
            let mut st = locked(&self.state);
            let slot = prepared.serve(&st.load_policy, &st.query_settings);
            st.dbs.insert(name.to_string(), slot);
        }
    }

    /// REST `/build` in the on-disk or RocksDB `layout`, through the bulk
    /// loader ([`crate::bulk::build`]), which installs the new database
    /// crash-safely over `dir` and takes the directory's lock itself. So the
    /// loaded copy is retired first (closing its files and releasing its
    /// handle on the directory) — saved first when it holds acknowledged
    /// writes not yet saved (check_point off) — and this request's probe of
    /// the lock — refusing a directory another process holds — is released
    /// before the build starts. No rule-directory lock is held while it
    /// loads (a `reasonManage` request would wait for the whole build): the
    /// install takes it to remove the replaced rules. The result is then
    /// opened and served.
    ///
    /// A build that fails leaves the previous database in place (a loaded
    /// one is served again, with every write it acknowledged) or, for a new
    /// name, no directory (removed under its lock, and only when it holds no
    /// database: one another process created meanwhile is left alone). A
    /// build whose install committed but could not finish its cleanup
    /// ([`BulkReport::warnings`](crate::bulk::BulkReport) — the replaced
    /// layout or rules still there) is reported as failed and not served:
    /// opening it might read what was left behind (the registry is updated
    /// all the same). A staging area left behind is only logged: no open
    /// reads it.
    fn rest_build_bulk(
        &self,
        name: &str,
        dir: &Path,
        source: &RestBuildSource,
        layout: StorageKind,
        vanished: bool,
        creator: &str,
    ) -> Resp {
        let sources = match source {
            RestBuildSource::File(file) => match crate::parser::import::sources_from_paths(&[file])
            {
                Ok(s) => s,
                Err(e) => return fail_with(&e, OpErrorKind::Query, &format!("build failed: {e}")),
            },
            RestBuildSource::Text(_) => Vec::new(),
        };
        let input = match source {
            RestBuildSource::File(_) => crate::bulk::BulkInput::Sources(&sources),
            RestBuildSource::Text(text) => crate::bulk::BulkInput::Text {
                text,
                format: crate::parser::import::RdfFormat::Turtle,
                name: "request body",
            },
        };
        // A body keeps its blank-node labels, as the snapshot layout's
        // `Database::build_from_str` does (a file is scoped per document in
        // both layouts): the same request stores the same terms whatever
        // the layout.
        let opts = match source {
            RestBuildSource::File(_) => crate::parser::import::ImportOptions::strict(),
            RestBuildSource::Text(_) => crate::parser::import::ImportOptions {
                blank_nodes: crate::parser::import::BlankNodes::Keep,
                ..crate::parser::import::ImportOptions::strict()
            },
        };
        match DirLock::acquire_existing(dir) {
            Ok(probe) => drop(probe),
            Err(e) => return fail_with(&e, OpErrorKind::Conflict, &format!("build failed: {e}")),
        }
        let existed = dir.exists();
        let was_loaded = match self.retire_loaded_saved(name) {
            Ok(loaded) => loaded,
            Err(r) => return r,
        };
        #[cfg(test)]
        build_hooks::run(dir);
        let built = crate::bulk::build(
            dir,
            name,
            input,
            &opts,
            &mut (),
            bulk_target(layout),
            &self.config().bulk_options(),
        );
        let report = match built {
            Ok(report) => report,
            Err(e) => {
                if was_loaded {
                    // The previous database is intact: serve it again.
                    self.serve_again(name, dir);
                } else if !existed {
                    // A new name: the build made the directory (and its
                    // lock file); a failed build leaves none behind, as the
                    // snapshot layout's and `/api build` do. Under its lock
                    // (the build released its own), and only when it holds
                    // no database: another process (a `gbuild` of the same
                    // name) may have created it meanwhile — the reason this
                    // build failed, maybe — and is left alone.
                    if let Ok(Some(_lock)) = DirLock::acquire_existing(dir) {
                        if Database::storage_kind(dir).is_none() {
                            let _ = fs::remove_dir_all(dir);
                        }
                    }
                }
                return fail_with(&e, OpErrorKind::Query, &format!("build failed: {e}"));
            }
        };
        if !report.warnings.is_empty() {
            let warnings = report.warnings.join("; ");
            crate::gs_warn!("REST /build of '{name}': the install did not finish: {warnings}");
        }
        if !report.opens_as_built() {
            let warnings = report.warnings.join("; ");
            // Not served, but built: the registry's bookkeeping still runs
            // (a dropped database's grants must not reach the new data).
            register_built(&mut locked(&self.state), name, vanished, creator);
            return Resp::fail(
                500,
                &format!(
                    "database '{name}' was built, but its install did not finish ({warnings}); \
                     it is not loaded: remove what was left behind, then load it"
                ),
            );
        }
        match self.open_db(dir) {
            Ok(db) => self.serve_built(name, db, dir.to_path_buf(), vanished, creator),
            Err(e) => {
                register_built(&mut locked(&self.state), name, vanished, creator);
                fail_with(
                    &e,
                    OpErrorKind::Failed,
                    &format!("built, but opening it failed: {e}"),
                )
            }
        }
    }

    fn handle_load(&self, st: &mut ApiState, req: &Request, _user: &str) -> Dispatch {
        let Some(name) = self.req_db_name(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or invalid db_name"));
        };
        // Re-loading an already-loaded database would silently discard its
        // unsaved in-memory changes; it is a no-op instead (as in gStore) —
        // unless its on-disk store failed (review gap-txn-lifecycle#4): such
        // a database can never reach a commit point, so it is reopened in
        // place, without one, from its directory (consistent through its
        // write-ahead log), and the answer's warning names what is discarded.
        if let Some(h) = st.dbs.get(&name).map(Arc::clone) {
            let already = Resp::ok(&format!("database '{name}' already loaded"));
            if st.db_txn.contains_key(&name) || st.busy.contains(&name) {
                return Dispatch::Done(already);
            }
            return Dispatch::Deferred(Box::new(move |inner| {
                // Whether it failed is checked without waiting and without
                // reserving its name (and off the state lock: dropping the
                // guard may measure the directory), so loading a healthy
                // loaded database stays a no-op: it never waits behind a
                // running statement and never makes the database busy for a
                // concurrent begin / unload. A database busy right now is
                // answered as loaded; a later load reopens it.
                let failed_now = h.try_write().is_some_and(|slot| {
                    !slot.retired && slot.txn.is_none() && slot.db.storage_failure().is_some()
                });
                if !failed_now {
                    return already;
                }
                {
                    let mut st = locked(&inner.state);
                    if st.db_txn.contains_key(&name)
                        || !st.dbs.get(&name).is_some_and(|x| Arc::ptr_eq(x, &h))
                        || st.claim(&name).is_err()
                    {
                        return already;
                    }
                }
                let _busy = BusyGuard {
                    inner,
                    name: name.clone(),
                };
                let mut slot = match h.write() {
                    Ok(slot) => slot,
                    Err(e) => return e.rest(),
                };
                let failure = (!slot.retired && slot.txn.is_none())
                    .then(|| slot.db.storage_failure())
                    .flatten();
                let Some(failure) = failure else {
                    return already;
                };
                // The directory the served copy was opened from.
                let dir = slot.dir.clone();
                slot.retire();
                {
                    let mut st = locked(&inner.state);
                    if st.dbs.get(&name).is_some_and(|x| Arc::ptr_eq(x, &h)) {
                        st.dbs.remove(&name);
                    }
                }
                drop(slot);
                let warning = format!(
                    "database '{name}' was reopened without a commit point: its on-disk store \
                     had failed ({failure}); those changes are discarded (a commit whose \
                     write-ahead log was complete is replayed by the reopen)"
                );
                crate::gs_warn!("{warning}");
                match inner.open_db(&dir) {
                    Ok(db) => {
                        let prepared = DbSlot::prepare(db, dir, false);
                        let mut st = locked(&inner.state);
                        let slot = prepared.serve(&st.load_policy, &st.query_settings);
                        st.dbs.insert(name.clone(), slot);
                        Resp::ok_with_warning(&format!("database '{name}' reopened"), &warning)
                    }
                    Err(e) => fail_with(
                        &e,
                        OpErrorKind::Failed,
                        &format!("load failed: {e} ({warning}; it is unloaded now)"),
                    ),
                }
            }));
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
            match inner.open_db(&dir) {
                Ok(db) => {
                    // Locked and measured before the state lock is taken
                    // (review http#13/conc#7): no request waits for the walk.
                    let prepared = DbSlot::prepare(db, dir, false);
                    let mut st = locked(&inner.state);
                    let slot = prepared.serve(&st.load_policy, &st.query_settings);
                    st.dbs.insert(name.clone(), slot);
                    Resp::ok(&format!("database '{name}' loaded"))
                }
                Err(e) => fail_with(&e, OpErrorKind::Failed, &format!("load failed: {e}")),
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
    ///
    /// A database the upload creates is registered for the caller, as
    /// `/build` registers one — forgetting the grants of a registered name
    /// whose directory was removed outside the server, so the new data does
    /// not inherit them (review http#16).
    fn handle_upload(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
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
        // Still registered, but neither loaded nor on disk: dropped outside
        // the server (`gdrop`). A database created here must not inherit
        // its grants (as `/build`).
        let vanished = loaded.is_none() && st.sysdb.contains(&name) && !dir.exists();
        let creator = user.to_string();
        Dispatch::Deferred(Box::new(move |inner| {
            let busy = loaded.is_none().then(|| BusyGuard {
                inner,
                name: name.clone(),
            });
            let triples = match parse_rdf(&format, &content) {
                Ok(ts) => ts,
                Err(e) => {
                    return fail_with(
                        &e,
                        OpErrorKind::Query,
                        &format!("upload parse failed ({format}): {e}"),
                    )
                }
            };
            let parsed = triples.len();
            let dbh = match loaded {
                Some(h) => h,
                None => {
                    let (opened, fresh) = if dir.is_dir() {
                        (inner.open_db(&dir), false)
                    } else {
                        (Database::build_from_str(name.clone(), ""), true)
                    };
                    match opened {
                        // A fresh database exists only in memory so far: dirty,
                        // so it is persisted even if nothing ends up inserted.
                        Ok(db) => {
                            // The directory is locked and measured first,
                            // with the state lock released (review
                            // http#13/conc#7); the policy and settings in
                            // effect now are read under the lock the slot
                            // is registered under: ones installed while the
                            // body was parsed reach it (`apply_query_settings`).
                            let prepared = DbSlot::prepare(db, dir, fresh);
                            let mut st = locked(&inner.state);
                            let h = prepared.serve(&st.load_policy, &st.query_settings);
                            st.dbs.insert(name.clone(), Arc::clone(&h));
                            if fresh {
                                register_built(&mut st, &name, vanished, &creator);
                            }
                            h
                        }
                        Err(e) => {
                            return fail_with(
                                &e,
                                OpErrorKind::Failed,
                                &format!("upload failed to open database: {e}"),
                            )
                        }
                    }
                }
            };
            drop(busy);
            let mut slot = match dbh.write() {
                Ok(slot) => slot,
                Err(e) => return e.rest(),
            };
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
            // Inserted but not saved: a failure (HTTP 500), review durability#2.
            let fields = format!(
                ",\"db_name\":{},\"format\":{},\"triples_parsed\":{parsed},\"triples_inserted\":{inserted}",
                json_str(&name),
                json_str(&format),
            );
            Resp::write_outcome("upload complete", &fields, &warning)
        }))
    }

    /// `POST /unload?db_name=…` — save the database if it has unsaved changes,
    /// then drop it from memory. If the save fails it stays loaded (nothing is
    /// lost) and the error is returned — unless its on-disk store failed (no
    /// commit point can succeed then): it is unloaded without one, and the
    /// answer (`200`) carries a `warning` naming the discarded changes
    /// (review gap-txn-lifecycle#4).
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
            let mut slot = match dbh.write() {
                Ok(slot) => slot,
                Err(e) => return e.rest(),
            };
            if slot.retired {
                return Resp::fail(404, &format!("database '{name}' is not loaded"));
            }
            if let Some(open) = &slot.txn {
                return unload_busy(&name, open.tid);
            }
            if let Some(failure) = slot.db.storage_failure() {
                // Its file is consistent through its write-ahead log; what
                // it could not commit was never acknowledged as durable.
                let warning = format!(
                    "database '{name}' was unloaded without a commit point: its on-disk store \
                     had failed ({failure}); those changes are discarded (a commit whose \
                     write-ahead log was complete is replayed when it is loaded again)"
                );
                crate::gs_warn!("{warning}");
                slot.retire();
                {
                    let mut st = locked(&inner.state);
                    if st.dbs.get(&name).is_some_and(|h| Arc::ptr_eq(h, &dbh)) {
                        st.dbs.remove(&name);
                    }
                }
                drop(slot);
                return Resp::ok_with_warning(&format!("database '{name}' unloaded"), &warning);
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
                        &format!(
                            "unload aborted: committing '{name}' failed ({e}); it stays loaded"
                        ),
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
                Err(e) => {
                    return fail_with(&e, OpErrorKind::Conflict, &format!("drop failed: {e}"))
                }
            };
            // Retire (closing its store files) before the directory is deleted.
            let was_loaded = match inner.retire_loaded(&name) {
                Ok(was) => was,
                Err(e) => return e.rest(),
            };
            let existed_on_disk = dir.is_dir();
            if existed_on_disk {
                // A `reasonManage` file operation (which does not wait for
                // the database) is never half-way through its rule directory
                // meanwhile, as for the `/api` drop.
                let _rules = RuleStore::dir(&dir).lock();
                // Renamed aside durably, then deleted: a crash or an error
                // part-way never leaves a half-deleted database under its
                // name (review durability#10).
                if let Err(e) = crate::db::fsutil::discard_db_dir(&dir) {
                    return fail_with(&e, OpErrorKind::Failed, &format!("drop failed: {e}"));
                }
            } else {
                // What an interrupted drop of this name left behind (no
                // database scan reads it) goes now.
                let _ = crate::db::fsutil::remove_drop_leftover(&dir);
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
        let loaded: HashMap<String, DbHandle> = st
            .dbs
            .iter()
            .map(|(n, d)| (n.clone(), Arc::clone(d)))
            .collect();
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
            let txns: HashMap<String, TxnSummary> = {
                let st = locked(&inner.state);
                names.retain(|n| crate::sysdb::allowed(&st.users, &user, "query", Some(n)));
                names
                    .iter()
                    .filter_map(|n| Some((n.clone(), inner.txn_summary(&st, n)?)))
                    .collect()
            };
            let mut items = Vec::new();
            for n in &names {
                // The figures each database published at the end of its last
                // write: no database lock is taken, so the list never waits
                // for a statement.
                let (is_loaded, triples, in_transaction, dirty) = match loaded.get(n) {
                    Some(d) => {
                        let f = d.figures();
                        (true, f.triples.to_string(), f.in_transaction, f.dirty)
                    }
                    None => (false, "null".to_string(), false, false),
                };
                items.push(format!(
                    "{{\"name\":{},\"loaded\":{is_loaded},\"in_transaction\":{in_transaction},\"dirty\":{dirty},\"triples\":{triples}{}}}",
                    json_str(n),
                    txns.get(n).map_or_else(String::new, TxnSummary::json_field)
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
        let dbh = match self.begin_target(st, &name) {
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
    /// database's handle, or why no transaction can begin on it now. A
    /// transaction open on it that is idle past `txn_idle_timeout` or whose
    /// owner was deleted does not refuse the begin: [`begin_txn`](Self::begin_txn)
    /// reaps it.
    pub(crate) fn begin_target(&self, st: &ApiState, name: &str) -> Result<DbHandle, OpError> {
        let Some(dbh) = st.dbs.get(name).map(Arc::clone) else {
            return Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' is not loaded"),
            ));
        };
        if let Some(&open) = st.db_txn.get(name) {
            if self.txn_liveness(st, name, open) == TxnLiveness::Live {
                return Err(txn_already_open_err(name, open));
            }
        }
        if st.busy.contains(name) {
            return Err(OpError::new(
                OpErrorKind::Conflict,
                format!("database '{name}' is busy"),
            ));
        }
        Ok(dbh)
    }

    /// Whether open transaction `tid` of `db` may go on (see
    /// [`TxnLiveness`]); `Live` when it is not open. Under the state lock.
    pub(crate) fn txn_liveness(&self, st: &ApiState, db: &str, tid: u64) -> TxnLiveness {
        match st.txns.get(&(db.to_string(), tid)) {
            Some(txn) => Self::liveness_of(
                st,
                txn,
                self.config().txn_idle_timeout,
                self.txn_reap.now_ms(),
            ),
            None => TxnLiveness::Live,
        }
    }

    /// [`txn_liveness`](Self::txn_liveness) of registry entry `txn` at
    /// transaction-clock time `now`, with `txn_idle_timeout` = `timeout`
    /// seconds (`0`: never idle). A statement in flight keeps it live.
    fn liveness_of(st: &ApiState, txn: &Txn, timeout: u64, now: u64) -> TxnLiveness {
        if txn.running > 0 {
            return TxnLiveness::Live;
        }
        if txn.owner != ROOT_USER && st.users.get(&txn.owner).is_none() {
            return TxnLiveness::Orphaned;
        }
        if timeout > 0 && now >= Self::idle_deadline(txn, timeout) {
            return TxnLiveness::Idle;
        }
        TxnLiveness::Live
    }

    /// When registry entry `txn` becomes idle past `timeout` seconds.
    fn idle_deadline(txn: &Txn, timeout: u64) -> u64 {
        txn.last_active.saturating_add(timeout.saturating_mul(1000))
    }

    /// What monitoring shows of the transaction open on `db`, if any (REST
    /// `/show` and `/monitor`, `/api` `show` and `monitor`): its id (also in
    /// the transaction log's `<begin>_<tid>` form), owner, and how long it
    /// has been idle. Under the state lock.
    pub(crate) fn txn_summary(&self, st: &ApiState, db: &str) -> Option<TxnSummary> {
        let tid = *st.db_txn.get(db)?;
        let txn = st.txns.get(&(db.to_string(), tid))?;
        let idle_ms = if txn.running > 0 {
            0
        } else {
            self.txn_reap.now_ms().saturating_sub(txn.last_active)
        };
        Some(TxnSummary {
            tid,
            begun: txn.begun,
            owner: txn.owner.clone(),
            isolation: txn.isolation,
            idle_secs: idle_ms / 1000,
            running: txn.running,
        })
    }

    /// Begin a transaction owned by `user` on loaded database `name` (handle
    /// `dbh`, from [`begin_target`](Self::begin_target)). Must be called with
    /// no lock held: it locks the database, snapshots its committed state,
    /// and registers the transaction while still holding the exclusive guard
    /// (so the registry and the slot always agree). Returns the new
    /// transaction id.
    ///
    /// A transaction still open on the database that is idle past
    /// `txn_idle_timeout`, or whose owner was deleted, is rolled back first,
    /// under the exclusive guard (the one reaping home that waits for a
    /// database, see [`reap_idle_txns`](Self::reap_idle_txns)).
    ///
    /// A RocksDB database has no copy-on-write store to snapshot, so it is
    /// copied whole (REFACTOR_BACKLOG D4): under the *shared* guard, so its
    /// readers go on meanwhile (review gap-rocksdb-exec#3). The exclusive
    /// guard is taken only to register the transaction, after checking that
    /// the committed state did not change since the copy (a write that
    /// slipped in makes it copy again; after [`BEGIN_CAPTURE_ATTEMPTS`] it
    /// copies under the exclusive guard). The other layouts' snapshots cost
    /// O(1) and are taken under the exclusive guard.
    ///
    /// Concurrent RocksDB begins of one database copy it one at a time: each
    /// holds the database's begin lock from before its copy until it
    /// registered its transaction (or failed), so a begin that arrives
    /// meanwhile waits, then finds that transaction open and is refused
    /// without copying anything — never several whole-store copies at once
    /// that all but one throw away.
    pub(crate) fn begin_txn(
        &self,
        dbh: &DbHandle,
        name: &str,
        isolation: TxnIsolation,
        user: &str,
    ) -> Result<u64, OpError> {
        let mut copied: Option<CapturedSnapshot> = None;
        let mut copies = 0;
        let rocks = dbh.layout == StorageKind::RocksDb;
        let _copying = rocks.then(|| match dbh.begin_copy.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                txn_park(name, TxnParkPoint::BeginQueued);
                dbh.begin_copy
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            }
        });
        let (mut slot, snapshot) = loop {
            if rocks && copied.is_none() && copies < BEGIN_CAPTURE_ATTEMPTS {
                copied = self.capture_shared(dbh, name)?;
                copies += usize::from(copied.is_some());
            }
            let mut slot = dbh.write()?;
            if slot.retired {
                return Err(OpError::new(
                    OpErrorKind::NotLoaded,
                    format!("database '{name}' is not loaded"),
                ));
            }
            if let Some(open) = slot.txn.as_ref().map(|o| o.tid) {
                if !self.reap_locked(&mut slot, dbh, name, open) {
                    return Err(txn_already_open_err(name, open));
                }
                if copied.is_none() && Self::copies_for_snapshot(&slot.db) {
                    // Copy it with the shared guard (the reaped
                    // transaction's rollback is in the committed state).
                    drop(slot);
                    continue;
                }
            }
            // Checked under the db lock, which the shutdown takes after setting
            // the flag: no transaction can begin after its rollback pass.
            if self.closing.load(Ordering::SeqCst) {
                return Err(OpError::new(
                    OpErrorKind::ShuttingDown,
                    "the server is shutting down",
                ));
            }
            self.sync_query_settings(&mut slot);
            let snapshot = match copied.take() {
                Some(c)
                    if c.mutations == slot.db.mutation_count()
                        && c.settings_gen == slot.settings_gen =>
                {
                    c.snapshot
                }
                Some(_) if copies < BEGIN_CAPTURE_ATTEMPTS => {
                    // A write (or new settings) slipped in after the copy.
                    drop(slot);
                    continue;
                }
                stale => {
                    if stale.is_some() {
                        crate::gs_info!(
                            "begin on '{name}': the database changed during each of \
                             {BEGIN_CAPTURE_ATTEMPTS} copies for the transaction's read \
                             snapshot; copying it under the exclusive guard"
                        );
                    }
                    ReadSnapshot::capture(&slot.db).map_err(|e| {
                        OpError::new(OpErrorKind::Failed, format!("begin failed: {e}"))
                    })?
                }
            };
            break (slot, Arc::new(snapshot));
        };
        let begin_stats = slot.db.stats();
        let begin_generation = slot.db.mutation_count();
        if let Err(e) = slot.db.begin() {
            return Err(OpError::new(
                OpErrorKind::Failed,
                format!("begin failed: {e}"),
            ));
        }
        let begun = unix_millis() as u128;
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
            // `DbSlot::next_tid`). How the ids of earlier loads ended stays
            // remembered, with their load generation (review
            // gap-txn-lifecycle#2).
            let tid = slot.next_tid.max(1);
            slot.next_tid = tid.checked_add(1).unwrap_or(1);
            let now = self.txn_reap.now_ms();
            let txn = Txn {
                db: name.to_string(),
                owner: user.to_string(),
                isolation,
                begun,
                generation: slot.load_gen,
                last_active: now,
                running: 0,
            };
            let timeout = self.config().txn_idle_timeout;
            if timeout > 0 {
                self.txn_reap.arm(Self::idle_deadline(&txn, timeout));
            }
            st.txns.insert((name.to_string(), tid), txn);
            st.db_txn.insert(name.to_string(), tid);
            tid
        };
        slot.txn = Some(OpenTxn {
            tid,
            snapshot,
            changed: false,
            begin_stats,
            begin_generation,
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
        self.apilogs
            .txn_begin_at(&self.root, name, tid, user, begun);
        Ok(tid)
    }

    /// Whether `db`'s read snapshot is a copy of the whole store (RocksDB:
    /// no in-RAM store, no on-disk read view; see [`ReadSnapshot::capture`]).
    fn copies_for_snapshot(db: &Database) -> bool {
        db.backend().as_memory().is_none() && db.backend().as_disk().is_none()
    }

    /// For a `/begin` on a database whose snapshot is a copy (RocksDB): the
    /// copy, taken under the shared guard, with the state it was taken at.
    /// `None` for the other layouts, and while a transaction is open or the
    /// database is retired (the exclusive step deals with those).
    fn capture_shared(
        &self,
        dbh: &DbHandle,
        name: &str,
    ) -> Result<Option<CapturedSnapshot>, OpError> {
        let slot = self.read_synced(dbh)?;
        if slot.retired || slot.txn.is_some() || !Self::copies_for_snapshot(&slot.db) {
            return Ok(None);
        }
        txn_park(name, TxnParkPoint::BeginCapture);
        let mutations = slot.db.mutation_count();
        let settings_gen = slot.settings_gen;
        let snapshot = ReadSnapshot::capture(&slot.db)
            .map_err(|e| OpError::new(OpErrorKind::Failed, format!("begin failed: {e}")))?;
        drop(slot);
        txn_park(name, TxnParkPoint::BeginCaptured);
        Ok(Some(CapturedSnapshot {
            snapshot,
            mutations,
            settings_gen,
        }))
    }

    /// Look up open transaction `tid` of database `db` (when not named: the one
    /// database where `tid` is open, see [`ApiState::open_txn_db_at`]) for
    /// `user`: unknown ids are `404`, already finished ones `409`, an id open
    /// on several databases `409`, and someone else's transaction `403` (only
    /// the owner or a superuser — `root`, or a user with the `admin`
    /// privilege — may touch it). Returns the transaction's database.
    ///
    /// A transaction idle past `txn_idle_timeout` (no statement in flight),
    /// or whose owner was deleted, is answered as ended ("rolled back after
    /// txn_idle_timeout"): this runs under the state lock and so cannot roll
    /// it back, but it makes the next request's reaping pass due.
    pub(crate) fn txn_for(
        &self,
        st: &ApiState,
        db: Option<&str>,
        tid: TidRef,
        user: &str,
    ) -> Result<String, OpError> {
        let name = st.open_txn_db_at(db, tid)?;
        let id = tid.tid;
        let txn = &st.txns[&(name.clone(), id)];
        if txn.owner != user && user != ROOT_USER && !st.users.is_superuser(user) {
            return Err(OpError::new(
                OpErrorKind::NotOwner,
                format!("transaction {id} belongs to another user"),
            ));
        }
        if let Some(outcome) = self.txn_liveness(st, &name, id).ended_as() {
            self.txn_reap.arm(0);
            return Err(OpError::new(
                OpErrorKind::TxnEnded,
                format!("transaction {id} is not running ({outcome})"),
            ));
        }
        Ok(txn.db.clone())
    }

    fn handle_tquery(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(tid) = self.txn_ref(req) else {
            return Dispatch::Done(Resp::fail(400, "missing or malformed tid/txn parameter"));
        };
        let name = match self.txn_for(st, self.req_db_name(req).as_deref(), tid, user) {
            Ok(name) => name,
            Err(e) => return Dispatch::Done(e.rest()),
        };
        let tid = tid.tid;
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
        // Classified (parsed, once) with the state lock released, like
        // `/sparql`.
        Dispatch::Deferred(Box::new(move |inner| {
            let stmt = Statement::parse(&sparql);
            let op = if stmt.as_ref().is_ok_and(Statement::is_update) {
                "update"
            } else {
                "query"
            };
            if !crate::sysdb::allowed(&locked(&inner.state).users, &user, op, Some(&name)) {
                return inner.forbidden();
            }
            log_query(&inner.config(), &inner.root, &name, &user, &sparql);
            let qlog = RestQueryLog::start(inner, &name, &sparql, fmt);
            let out = inner.run_in_txn_task(&dbh, &name, tid, &sparql, stmt, &user);
            // Rendered under `max_result_bytes` before it is logged, as
            // `/sparql` does: an answer over the budget is logged as the
            // failure it is answered as (review sec#10).
            let rendered = match out {
                Ok(r) => render_result_within(fmt, &r, inner.config().max_result_bytes)
                    .map(|resp| (resp, answer_count(&r))),
                Err(e) => Err(e),
            };
            qlog.finish(inner, rendered.as_ref().map(|(_, n)| *n));
            match rendered {
                Ok((resp, _)) => resp,
                Err(e) => e.rest(),
            }
        }))
    }

    /// [`run_in_txn`](Self::run_in_txn) as a registered task of `user`: listed
    /// by `/tasklist`, and cancellable — by `/cancel`, and by a shutdown past
    /// its grace period — like any other query. `stmt` is `sparql` parsed
    /// (with no lock held). Must be called with no lock held. The statement
    /// counts as the transaction's activity from here until it releases the
    /// database (see [`TxnStatement`]).
    ///
    /// Like every other statement it runs under `query_timeout` (review
    /// sec#10, robust#5; see [`run_query_job`](Self::run_query_job)): the
    /// deadline raises the task's cancel flag, and the statement then fails
    /// as [`OpErrorKind::Timeout`]; only its evaluation counts
    /// ([`StatementClock`]). The transaction stays open — with the
    /// operations a multi-operation UPDATE applied before it was stopped,
    /// which the answer names (see [`run_in_txn`](Self::run_in_txn)).
    pub(crate) fn run_in_txn_task(
        &self,
        dbh: &DbHandle,
        name: &str,
        tid: u64,
        sparql: &str,
        stmt: crate::Result<Statement>,
        user: &str,
    ) -> Result<QueryResult, OpError> {
        let (task_id, cancel, activity) = {
            let mut st = locked(&self.state);
            let (task_id, cancel) = self.register_task(&mut st, name, sparql, user);
            let activity = self.txn_statement(&mut st, name, tid);
            (task_id, cancel, activity)
        };
        let mut task = TaskGuard::new(self, task_id);
        let clock = StatementClock::new(&cancel, self.config().query_timeout);
        let out = self.run_in_txn(dbh, name, tid, stmt, &clock, user, activity);
        let out = match out {
            Err(e) if clock.timed_out() && e.kind != OpErrorKind::Persist => {
                Err(timed_out_err(e, &clock))
            }
            out => out,
        };
        task.set_ok(out.is_ok());
        out
    }

    /// A statement of open transaction `tid` of `db` starts: it counts as
    /// activity until the returned value ends (see [`TxnStatement`]). Under
    /// the state lock (`st`).
    pub(crate) fn txn_statement(&self, st: &mut ApiState, db: &str, tid: u64) -> TxnStatement<'_> {
        let generation = st
            .txns
            .get(&(db.to_string(), tid))
            .map(|txn| txn.generation);
        if let Some(generation) = generation {
            self.txn_activity(st, db, tid, generation, true);
        }
        TxnStatement {
            inner: self,
            db: db.to_string(),
            tid,
            generation,
        }
    }

    /// Stamp a statement of open transaction `tid` of `db` (load
    /// `generation`) starting (`start`) or ending: a statement in flight is
    /// activity, and the idle clock restarts when the last one ends. Under
    /// the state lock (`st`).
    fn txn_activity(&self, st: &mut ApiState, db: &str, tid: u64, generation: u64, start: bool) {
        let now = self.txn_reap.now_ms();
        let Some(txn) = st
            .txns
            .get_mut(&(db.to_string(), tid))
            .filter(|t| t.generation == generation)
        else {
            return;
        };
        if start {
            txn.running += 1;
        } else {
            txn.running = txn.running.saturating_sub(1);
        }
        txn.last_active = now;
        let timeout = self.config().txn_idle_timeout;
        if txn.running == 0 && timeout > 0 {
            self.txn_reap.arm(Self::idle_deadline(txn, timeout));
        }
    }

    /// Run `stmt` (a parsed statement, or the error parsing it gave) inside
    /// open transaction `tid` on database `name` (handle `dbh`), cancellable
    /// through `clock`'s flag, whose `query_timeout` starts once the
    /// statement holds the guard. Must be called with no lock held. The
    /// transaction is re-checked under the db guard — `/commit` and
    /// `/rollback` take the exclusive guard before they end it — so the
    /// statement either runs inside transaction `tid` or is refused, never
    /// applied outside it. A read takes a shared guard (it sees the
    /// transaction's own writes), an UPDATE the exclusive one. `activity`
    /// ends (stamping the transaction's activity) before the guard is
    /// released.
    ///
    /// A multi-operation UPDATE that fails (or is stopped) part-way keeps
    /// the operations before the failing one applied inside the
    /// transaction, as it does outside one: the error says so ("operations
    /// before the failing one were applied"), so the client knows to roll
    /// the transaction back rather than commit a half-applied statement.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_in_txn(
        &self,
        dbh: &DbHandle,
        name: &str,
        tid: u64,
        stmt: crate::Result<Statement>,
        clock: &StatementClock,
        user: &str,
        activity: TxnStatement<'_>,
    ) -> Result<QueryResult, OpError> {
        let not_open = || {
            OpError::new(
                OpErrorKind::TxnEnded,
                format!("transaction {tid} is not open on database '{name}'"),
            )
        };
        let open_here =
            |slot: &DbSlot| !slot.retired && slot.txn.as_ref().map(|t| t.tid) == Some(tid);
        let query_err = |e: GStoreError| OpError::statement(&e);
        let caller = self.pfn_caller(user);
        let stmt = match stmt {
            Ok(stmt) => stmt,
            Err(e) => {
                // A syntax error, answered once the transaction is known
                // to be open (as for any statement).
                if !open_here(&*self.read_synced(dbh)?) {
                    return Err(not_open());
                }
                return Err(query_err(e));
            }
        };
        if !stmt.is_update() {
            txn_park(name, TxnParkPoint::StatementQueued);
            let slot = self.read_synced(dbh)?;
            if !open_here(&slot) {
                return Err(not_open());
            }
            clock.start();
            txn_park(name, TxnParkPoint::StatementHolding);
            let out = slot
                .db
                .read_as(&stmt, Some(clock.cancel()), Some(&caller))
                .map_err(query_err);
            clock.stop();
            activity.end();
            txn_park(name, TxnParkPoint::StatementEnded);
            drop(slot);
            return out;
        }
        txn_park(name, TxnParkPoint::StatementQueued);
        let mut slot = dbh.write()?;
        if !open_here(&slot) {
            return Err(not_open());
        }
        self.sync_query_settings(&mut slot);
        let before = slot.db.mutation_count();
        clock.start();
        txn_park(name, TxnParkPoint::StatementHolding);
        let result = slot
            .db
            .execute_as(stmt, Some(clock.cancel()), Some(&caller));
        clock.stop();
        // Also a failing UPDATE may have applied part of its operations:
        // they stay in the transaction, and the answer says so (as outside
        // one, see `run_query_job_locked`).
        let wrote = slot.db.mutation_count() != before;
        if wrote {
            if let Some(open) = slot.txn.as_mut() {
                open.changed = true;
            }
        }
        activity.end();
        txn_park(name, TxnParkPoint::StatementEnded);
        drop(slot);
        result.map_err(|e| {
            if wrote {
                OpError::of(&e, OpErrorKind::Query, partial_update_msg(&e, None))
            } else {
                query_err(e)
            }
        })
    }

    fn handle_commit(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        self.handle_txn_end(st, req, user, true)
    }

    fn handle_rollback(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        self.handle_txn_end(st, req, user, false)
    }

    fn handle_txn_end(
        &self,
        st: &mut ApiState,
        req: &Request,
        user: &str,
        commit: bool,
    ) -> Dispatch {
        let Some(tid) = self.txn_ref(req) else {
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
                Ok(warning) => {
                    Resp::ok_warn(&format!("transaction {} {verb} done", tid.tid), &warning)
                }
                Err(e) => e.rest(),
            }
        }))
    }

    /// Commit (or roll back) open transaction `tid` of database `db` on behalf
    /// of `user`; `dbh` is the handle of `db` (looked up beforehand, `None`
    /// when it is no longer loaded). Must be called with no lock held. Returns
    /// the warning of a failed check_point save after a commit that changed
    /// data (the commit stands in memory but is not durable: callers answer
    /// it as a failure — REST 500, `/api` 1005; review durability#2).
    ///
    /// The outcome is recorded — in the registry of ended transactions, the
    /// transaction logs — only once the commit or rollback returned, and as
    /// what happened (review http#18). A commit whose commit point failed
    /// is answered as such (review gap-txn-lifecycle#3, see
    /// [`Database::commit_checked`]): rolled back (REST 500, `/api` 1005,
    /// "commit failed and the transaction was rolled back"), or, when it
    /// could no longer be undone, applied but not durable ([`NOT_DURABLE`],
    /// [`OpErrorKind::Persist`]; the database needs a reload). Either way
    /// the transaction log records it as aborted.
    ///
    /// A commit or rollback that was accepted while its transaction was live
    /// counts as the transaction's activity until it has ended it, like a
    /// statement in flight ([`TxnStatement`]): the wait for the database's
    /// guard behind another holder (a save, a long read, a CSR build) can
    /// therefore not turn it into an idle rollback, nor can a reaper roll
    /// the transaction back meanwhile.
    pub(crate) fn end_txn(
        &self,
        dbh: Option<&DbHandle>,
        db: &str,
        tid: TidRef,
        user: &str,
        commit: bool,
    ) -> Result<Option<String>, OpError> {
        let id = tid.tid;
        let activity = {
            let mut st = locked(&self.state);
            self.txn_for(&st, Some(db), tid, user)?;
            self.txn_statement(&mut st, db, id)
        };
        txn_park(db, TxnParkPoint::EndQueued);
        let mut slot = match dbh {
            Some(h) => Some(h.write()?),
            None => None,
        };
        // Checked while the db lock is held (race-free with /begin, /tquery,
        // a concurrent /commit of the same tid and the reapers, which all
        // take the guard before they touch it). The registry entry is
        // removed once the outcome is known, still under the guard.
        {
            let st = locked(&self.state);
            self.txn_for(&st, Some(db), tid, user)?;
            // `dbh` was looked up before this lock was taken: if the database
            // has been unloaded and loaded again since (which restarts its
            // ids), (`db`, `tid`) is now another transaction on the new
            // handle, and this request must not end it.
            if let Some(h) = dbh {
                if !st.dbs.get(db).is_some_and(|cur| Arc::ptr_eq(cur, h)) {
                    return Err(OpError::new(
                        OpErrorKind::TxnEnded,
                        format!("transaction {id} is not open on database '{db}'"),
                    ));
                }
            }
        }
        let ended = match slot.as_mut().filter(|s| !s.retired) {
            Some(slot) => self.finish_txn(slot, id, commit),
            None => TxnEnd {
                outcome: "rolled back",
                state: TxnState::RolledBack,
                logged: if commit { "commit" } else { "rollback" },
                result: Err(OpError::new(
                    OpErrorKind::NotLoaded,
                    format!("database '{db}' is not loaded"),
                )),
            },
        };
        let txn = {
            let mut st = locked(&self.state);
            let txn = st.txns.remove(&(db.to_string(), id));
            if let Some(txn) = &txn {
                st.db_txn.remove(db);
                st.remember_ended_txn(txn, id, ended.outcome);
            }
            txn
        };
        // The registry entry is gone: nothing left to stamp.
        activity.end();
        drop(slot);
        if let Some(txn) = txn {
            self.log_txn_end(&txn, id, user, ended.logged, ended.state);
        }
        ended.result
    }

    /// End the open transaction `id` of `slot` (exclusive guard held):
    /// commit or roll it back and say how it ended (see
    /// [`end_txn`](Self::end_txn)).
    fn finish_txn(&self, slot: &mut DbSlot, id: u64, commit: bool) -> TxnEnd {
        // Let the begin-time snapshot go first: an in-memory one shares the
        // dictionary's interners, and a commit that frees a term would copy
        // a shared interner whole just to drop the copy right after (readers
        // holding the snapshot keep their own reference).
        let changed = slot.txn.take().is_some_and(|o| o.changed);
        if !commit {
            return TxnEnd {
                outcome: "rolled back",
                state: TxnState::RolledBack,
                logged: "rollback",
                result: slot.db.rollback().map(|()| None).map_err(|e| {
                    OpError::new(OpErrorKind::Failed, format!("rollback failed: {e}"))
                }),
            };
        }
        let committed = |slot: &mut DbSlot| TxnEnd {
            outcome: "committed",
            state: TxnState::Committed,
            logged: "commit",
            result: Ok(if changed {
                slot.wrote(self.save_writes())
            } else {
                None
            }),
        };
        let rolled_back = |e: String| TxnEnd {
            outcome: ENDED_COMMIT_ROLLED_BACK,
            state: TxnState::Aborted,
            logged: "commit-failed-rolled-back",
            result: Err(OpError::new(
                OpErrorKind::Failed,
                format!("commit failed and transaction {id} was rolled back: {e}"),
            )),
        };
        match slot.db.commit_checked() {
            Ok(()) => committed(slot),
            Err(crate::db::CommitFailure::CommittedWithError(e)) => {
                crate::gs_warn!("transaction {id} committed, but: {e}");
                committed(slot)
            }
            Err(crate::db::CommitFailure::NotCommitted(e)) => {
                // Refused before anything was taken: it is still open in the
                // database, which must agree with the registry it leaves.
                let _ = slot.db.rollback();
                rolled_back(e.to_string())
            }
            Err(crate::db::CommitFailure::RolledBack(e)) => {
                let broken = slot.db.storage_failure().map_or_else(String::new, |f| {
                    format!(
                        "; the on-disk store failed ({f}): unload and load the database \
                         before writing to it again"
                    )
                });
                rolled_back(format!("{e}{broken}"))
            }
            Err(crate::db::CommitFailure::NotDurable(e)) => {
                slot.dirty = true;
                let msg = format!(
                    "{NOT_DURABLE}: the commit point of transaction {id} failed: {e}; the \
                     database needs a reload (unload, then load) — it then holds what was made \
                     durable"
                );
                crate::gs_error!("{msg}");
                TxnEnd {
                    outcome: ENDED_NOT_DURABLE,
                    state: TxnState::Aborted,
                    logged: "commit-failed-not-durable",
                    result: Err(OpError::new(OpErrorKind::Persist, msg)),
                }
            }
        }
    }

    /// Append the end of transaction `id` (registry entry `txn`, ended by
    /// `user`) to `txn.log` (as `logged`) and the transaction log (`state`).
    fn log_txn_end(&self, txn: &Txn, id: u64, user: &str, logged: &str, state: TxnState) {
        append_log(
            &self.root,
            "txn.log",
            &format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                now_millis(),
                logged,
                txn.db,
                id,
                user,
                txn.isolation.name()
            ),
        );
        self.apilogs
            .txn_end(&self.root, &txn.db, id, &txn.owner, state);
    }

    /// One pass of the idle-transaction reaper (review gap-txn-lifecycle#1,
    /// gap-txn-lifecycle#5): roll back every open transaction idle past
    /// `txn_idle_timeout` with no statement in flight, or whose owner was
    /// deleted, recorded as aborted ("rolled back after txn_idle_timeout").
    ///
    /// It never waits for a database: under the state lock it only collects
    /// the candidates (removing nothing); then it takes each candidate's
    /// exclusive guard with [`DbCell::try_write`] and skips a database that
    /// is busy (a long read, a save, an export…) — the deadline stays due, so
    /// the next request or pass tries again. Holding the guard it re-checks
    /// everything ([`reap_locked`](Self::reap_locked)), so the registry and
    /// the slot agree at every step. Runs from three homes: lazily inside
    /// [`begin_txn`](Self::begin_txn) (the one that may wait for the guard
    /// it takes anyway), on the request path when the earliest deadline has
    /// passed ([`reap_idle_txns_if_due`](Self::reap_idle_txns_if_due)), and
    /// periodically ([`start_txn_reaper`](Self::start_txn_reaper)).
    pub(crate) fn reap_idle_txns(&self) {
        let now = self.txn_reap.now_ms();
        let timeout = self.config().txn_idle_timeout;
        let candidates: Vec<(String, u64, DbHandle)> = {
            let st = locked(&self.state);
            // Re-computed from the registry (under the lock that every arm
            // is made under, so none is lost).
            self.txn_reap.at.store(u64::MAX, Ordering::SeqCst);
            let mut out = Vec::new();
            for ((db, tid), txn) in &st.txns {
                match Self::liveness_of(&st, txn, timeout, now) {
                    TxnLiveness::Live => {
                        if timeout > 0 && txn.running == 0 {
                            self.txn_reap.arm(Self::idle_deadline(txn, timeout));
                        }
                    }
                    _ => {
                        if let Some(h) = st.dbs.get(db) {
                            out.push((db.clone(), *tid, Arc::clone(h)));
                        }
                    }
                }
            }
            out
        };
        for (db, tid, dbh) in candidates {
            if self.closing.load(Ordering::SeqCst) {
                break;
            }
            match dbh.try_write() {
                Some(mut slot) => {
                    self.reap_locked(&mut slot, &dbh, &db, tid);
                }
                // Busy: tried again by the next request or pass.
                None => {
                    let _st = locked(&self.state);
                    self.txn_reap.arm(now);
                }
            }
        }
    }

    /// The request path's reaping (one line at the top of
    /// [`handle`](Self::handle)): a [`reap_idle_txns`](Self::reap_idle_txns)
    /// pass once the earliest idle deadline has passed — one atomic read
    /// otherwise. Never waits for a database guard (so `/cancel` stays
    /// immediate); a panic in it is contained.
    pub(crate) fn reap_idle_txns_if_due(&self) {
        let due = self.txn_reap.at.load(Ordering::SeqCst);
        if due != u64::MAX && due <= self.txn_reap.now_ms() {
            let _ = catch_unwind(AssertUnwindSafe(|| self.reap_idle_txns()));
        }
    }

    /// The reaping step, under `slot`'s exclusive guard (`slot` is `dbh`'s,
    /// database `name`): when its open transaction `tid` is still idle past
    /// `txn_idle_timeout` or orphaned — re-checked under the state lock,
    /// with the registry entry still on this handle — remove it from the
    /// registry and remember how it ended, then roll it back as
    /// [`end_txn`](Self::end_txn) would and log it as aborted. Returns
    /// whether the slot has no open transaction now.
    fn reap_locked(&self, slot: &mut DbSlot, dbh: &DbHandle, name: &str, tid: u64) -> bool {
        if slot.retired || slot.txn.as_ref().map(|o| o.tid) != Some(tid) {
            return slot.txn.is_none();
        }
        let (txn, outcome) = {
            let mut st = locked(&self.state);
            if !st.dbs.get(name).is_some_and(|cur| Arc::ptr_eq(cur, dbh)) {
                return false;
            }
            let Some(outcome) = self.txn_liveness(&st, name, tid).ended_as() else {
                return false;
            };
            let Some(txn) = st.txns.remove(&(name.to_string(), tid)) else {
                return false;
            };
            st.db_txn.remove(name);
            st.remember_ended_txn(&txn, tid, outcome);
            (txn, outcome)
        };
        // As `end_txn`: the begin-time snapshot goes first.
        slot.txn = None;
        if let Err(e) = slot.db.rollback() {
            crate::gs_error!("rolling back transaction {tid} of '{name}' ({outcome}) failed: {e}");
        }
        crate::gs_warn!(
            "transaction {tid} of '{}' on database '{name}' {outcome}",
            txn.owner
        );
        self.log_txn_end(&txn, tid, "-", "aborted", TxnState::Aborted);
        true
    }

    /// Start serving's transaction housekeeping — one line at the top of
    /// [`ApiServer::serve_forever`] / `serve_forever_tls`, whose end drops
    /// the returned guard: mark the transactions a previous server process
    /// left `RUNNING` in the transaction log as `ABORTED` (once per server;
    /// review gap-txn-lifecycle#2), and start the periodic idle-transaction
    /// reaper — a thread that runs a [`reap_idle_txns`](Self::reap_idle_txns)
    /// pass every `min(txn_idle_timeout / 4, 30 s)` (none when
    /// `txn_idle_timeout` is 0), so an idle server too frees abandoned
    /// transactions (and a RocksDB database's copy for one). It stops once
    /// the server is closing, and when the guard drops (joined, bounded).
    pub(crate) fn start_txn_reaper(self: &Arc<Self>) -> Option<TxnReaper> {
        {
            let mut servers = TXN_SERVERS.lock().unwrap_or_else(PoisonError::into_inner);
            servers.retain(|(_, w)| w.strong_count() > 0);
            servers.push((self.root.clone(), Arc::downgrade(self)));
        }
        if !self.txn_reap.swept.swap(true, Ordering::SeqCst) {
            self.apilogs.abort_stale_running(&self.root);
        }
        let timeout = self.config().txn_idle_timeout;
        if timeout == 0 {
            return None;
        }
        let every = Duration::from_millis(timeout.saturating_mul(250).clamp(1, 30_000));
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done) = std::sync::mpsc::channel::<()>();
        let inner = Arc::clone(self);
        let halt = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("gstore-txn-reaper".into())
            .spawn(move || {
                let state = &inner.txn_reap;
                let mut seen = state.beat().0;
                loop {
                    let (beat, _) = state
                        .beat_changed
                        .wait_timeout_while(state.beat(), every, |b| {
                            b.0 == seen && !halt.load(Ordering::SeqCst)
                        })
                        .unwrap_or_else(PoisonError::into_inner);
                    seen = beat.0;
                    drop(beat);
                    if halt.load(Ordering::SeqCst) || inner.closing.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = catch_unwind(AssertUnwindSafe(|| inner.reap_idle_txns()));
                    state.finished_pass(seen);
                }
                drop(done_tx);
                crate::exit_park::exiting();
            });
        match spawned {
            Ok(thread) => Some(TxnReaper {
                inner: Arc::clone(self),
                stop,
                done,
                thread: Some(thread),
            }),
            Err(e) => {
                crate::gs_warn!("the idle-transaction reaper could not start: {e}");
                None
            }
        }
    }

    /// Lock a loaded database for a non-transactional maintenance request
    /// that only reads it (a backup, an export): a shared guard, or the
    /// response refusing it (`404` once it is no longer served; `409` naming
    /// `what` while a transaction is open).
    fn lock_for<'a>(dbh: &'a DbHandle, name: &str, what: &str) -> Result<SlotRead<'a>, Resp> {
        Self::lock_for_op_read(dbh, name, what).map_err(|e| e.rest())
    }

    /// Why a maintenance request may not use `slot` now: no longer served,
    /// or a transaction is open.
    fn maintenance_refusal(slot: &DbSlot, name: &str, what: &str) -> Option<OpError> {
        if slot.retired {
            return Some(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' is not loaded"),
            ));
        }
        slot.txn.as_ref().map(|open| {
            OpError::new(
                OpErrorKind::Conflict,
                format!(
                    "database '{name}' has an open transaction (TID {}); commit or roll back \
                     before {what}",
                    open.tid
                ),
            )
        })
    }

    /// [`lock_for`](Self::lock_for) with an [`OpError`]: a shared guard.
    pub(crate) fn lock_for_op_read<'a>(
        dbh: &'a DbHandle,
        name: &str,
        what: &str,
    ) -> Result<SlotRead<'a>, OpError> {
        let slot = dbh.read()?;
        match Self::maintenance_refusal(&slot, name, what) {
            Some(e) => Err(e),
            None => Ok(slot),
        }
    }

    /// [`lock_for_op_read`](Self::lock_for_op_read) for a request that
    /// changes the slot (a checkpoint): the exclusive guard.
    pub(crate) fn lock_for_op_write<'a>(
        dbh: &'a DbHandle,
        name: &str,
        what: &str,
    ) -> Result<SlotWrite<'a>, OpError> {
        let slot = dbh.write()?;
        match Self::maintenance_refusal(&slot, name, what) {
            Some(e) => Err(e),
            None => Ok(slot),
        }
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
        let mut slot = Self::lock_for_op_write(dbh, name, "checkpointing")?;
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
            // Claimed before it is checked and before the database's guard
            // is taken: backups into one directory run one after the other.
            let (path, _claim) =
                match backup_target(&inner.backup_dir(), &name, requested.as_deref()) {
                    Ok(target) => target,
                    Err(r) => return r,
                };
            // A backup of the live database would capture uncommitted writes.
            let slot = match Self::lock_for(&dbh, &name, "backing up") {
                Ok(slot) => slot,
                Err(r) => return r,
            };
            // Recorded as a backup of this database once complete, as the
            // `/api` `backup` does (review design#4): while it is written the
            // directory is marked as a backup of it in progress, so
            // `/restore` never takes it for a complete backup — also when a
            // crash or a panic skips the cleanup below — and a later backup
            // of the database may write there again (round-3 review: it
            // was refused for good). `backup_target` allows only a new
            // directory, an empty one or one of this database's (complete
            // or not) here.
            let created = !path.exists();
            let earlier = !created
                && fs::read_to_string(path.join(BACKUP_OWNER_FILE))
                    .ok()
                    .as_deref()
                    == Some(name.as_str());
            // The new directory's entry (and the backups directory's)
            // durably (review tests-b#3: the fsync audit found they were
            // not).
            let backed_up = crate::db::fsutil::create_dir_durable(&path)
                .and_then(|()| mark_backup_started(&path, &name))
                .and_then(|()| {
                    #[cfg(test)]
                    backup_test_hook::reached(&path);
                    slot.db.backup(&path)
                })
                .and_then(|()| mark_backup(&path, &name));
            drop(slot);
            match backed_up {
                Ok(()) => {
                    let body = format!(
                        "{{\"StatusCode\":0,\"StatusMsg\":\"backup success\",\"path\":{}}}",
                        json_str(&path.to_string_lossy())
                    );
                    Resp::new(200, JSON, body.into_bytes())
                }
                Err(e) => {
                    // A directory this backup made goes again (as the `/api`
                    // backup's), and an empty one it wrote into is emptied
                    // again: neither is left holding data a later backup
                    // there would refuse as "not a backup" (409). An earlier
                    // backup it was replacing stays marked as a backup in
                    // progress: its rules may already be this backup's
                    // while its store is still the earlier one's, so
                    // `/restore` refuses it until a backup into it succeeds
                    // (a retry is allowed).
                    let note = if created {
                        let _ = fs::remove_dir_all(&path);
                        ""
                    } else if earlier {
                        "; the earlier backup in that directory may be partly replaced, so it \
                         is marked incomplete until a backup into it succeeds"
                    } else {
                        empty_dir(&path);
                        ""
                    };
                    fail_with(
                        &e,
                        OpErrorKind::Failed,
                        &format!("backup failed: {e}{note}"),
                    )
                }
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
    /// from before the backup is staged until the swap is done
    /// ([`Database::lock_restore_target`]; `409` while another process has
    /// the database open), as the `/api` `restore` does.
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
            // A backup that did not complete is restored for nobody — and is
            // reported as such to whoever restores the database it was a
            // backup of (round-3 review): its owner record no longer counts
            // while it is incomplete, so the ownership check below would
            // call it "not a backup of" the database.
            let incomplete = backup_in_progress(&path);
            let refuse_incomplete = || {
                Resp::fail(
                    409,
                    &format!(
                        "restore refused: the backup in '{}' is incomplete (a backup into it \
                         did not finish); back the database up there again, or restore \
                         another backup",
                        path.display()
                    ),
                )
            };
            if incomplete.as_deref() == Some(name.as_str()) {
                return refuse_incomplete();
            }
            if !may_restore_from(&locked(&inner.state).users, &user, &name, &path) {
                return Resp::fail(
                    403,
                    &format!(
                        "restore path refused: it is not a backup of database '{name}' (only \
                         root and admin users may restore a database from another one's backup)"
                    ),
                );
            }
            if incomplete.is_some() {
                return refuse_incomplete();
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
            // No other process may open the database while the backup is
            // staged beside it (its open would repair the staged copy away)
            // or swapped in: the directory is locked first and held through
            // the swap (as the `/api` restore; shares a loaded copy's lock).
            let probe = match Database::lock_restore_target(&dir) {
                Ok(lock) => lock,
                Err(e) => {
                    return fail_with(&e, OpErrorKind::Conflict, &format!("restore failed: {e}"))
                }
            };
            // Copy the backup beside the database's own directory and validate
            // the copy — never open the backup in place, or later writes would
            // land in the backup. The loaded database is untouched so far.
            let staged = match Database::stage_restore(&path, &dir) {
                Ok(staged) => staged,
                Err(e) => {
                    return fail_with(&e, OpErrorKind::Failed, &format!("restore failed: {e}"))
                }
            };
            let loaded = locked(&inner.state).dbs.get(&name).map(Arc::clone);
            if let Some(h) = &loaded {
                let mut slot = match h.write() {
                    Ok(slot) => slot,
                    Err(e) => {
                        Database::discard_staged_restore(&staged);
                        return e.rest();
                    }
                };
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
            let installed = {
                // As in the `/api` restore: no `reasonManage` file operation
                // is half-way through the rule directory swapped out.
                let _rules = RuleStore::dir(&dir).lock();
                Database::install_restore(&staged, &dir)
            };
            // Release the probe before reopening: the directory at this path
            // is a new one now, which the reopened database locks itself.
            drop(probe);
            // Serve whatever the directory now holds: the restored copy, or —
            // if the swap failed — the previous database (saved above).
            let reopened = inner.open_db(&dir);
            match (installed, reopened) {
                (Ok(()), Ok(db)) => {
                    let prepared = DbSlot::prepare(db, dir, false);
                    let mut st = locked(&inner.state);
                    let slot = prepared.serve(&st.load_policy, &st.query_settings);
                    st.dbs.insert(name.clone(), slot);
                    // A database restored under a new name joins the
                    // registry with the restoring user as its creator (best
                    // effort, as for /build; no privileges are granted, and
                    // none left from an earlier database of the name hold).
                    if fresh && st.sysdb.contains(&name) {
                        let _ = st.users.forget_database(&name);
                    }
                    // Built at the backup's time (its `<db>.db_<timestamp>`
                    // name), as the `/api` `restore` registers it (review
                    // design#4), else now.
                    if fresh || !st.sysdb.contains(&name) {
                        let built = backup_time(&path)
                            .unwrap_or_else(|| crate::sysdb::clock::now().datetime());
                        let _ = st.sysdb.register_at(&name, &user, &built);
                    }
                    Resp::ok(&format!("database '{name}' restored"))
                }
                (Ok(()), Err(e)) => fail_with(
                    &e,
                    OpErrorKind::Failed,
                    &format!("restore failed to open the restored copy: {e}"),
                ),
                (Err(e), reopened) => {
                    Database::discard_staged_restore(&staged);
                    if let (Ok(db), true) = (reopened, loaded.is_some()) {
                        let prepared = DbSlot::prepare(db, dir, false);
                        let mut st = locked(&inner.state);
                        let slot = prepared.serve(&st.load_policy, &st.query_settings);
                        st.dbs.insert(name.clone(), slot);
                    }
                    fail_with(&e, OpErrorKind::Failed, &format!("restore failed: {e}"))
                }
            }
        }))
    }

    /// `GET /export?db_name=<name>[&format=nt|nq][&download=0]` — the whole
    /// database: N-Triples, or N-Quads (the default graph, then every named
    /// graph) when it has a non-empty named graph, so nothing is lost —
    /// `format` (`nt` / `nq`, as the `/api` `export`'s) picks one. Its
    /// committed state while a transaction is open. Sent as a file download
    /// (`Content-Disposition: attachment; filename="<name>.nt|nq"`) by
    /// default; pass `download=0`/`false`/`no`/`off` for an inline
    /// (browser-rendered) response instead.
    ///
    /// The `/api` `export`'s serializer ([`export_slot`], review http#11 /
    /// design#4: the endpoint wrote only the default graph). It runs as a
    /// task — listed by `/tasklist`, stopped by `/cancel` at its next
    /// statement (`503`) — writing to an unnamed scratch file in the export
    /// directory under the database's shared guard; the guard released, the
    /// file is streamed in bounded chunks (never held in memory, never
    /// compressed) and is gone once sent.
    fn handle_export(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(name) = self.pick_db(st, req) else {
            return Dispatch::Done(Resp::fail(400, "missing or ambiguous db parameter"));
        };
        let format = match req.param("format").filter(|f| !f.trim().is_empty()) {
            None => None,
            Some(f) => match ExportFormat::parse(&f) {
                Some(format) => Some(format),
                None => {
                    return Dispatch::Done(Resp::fail(
                        400,
                        &format!(
                            "the export format '{f}' is not supported (use nt for N-Triples or \
                             nq for N-Quads)"
                        ),
                    ))
                }
            },
        };
        let Some(dbh) = st.dbs.get(&name).map(Arc::clone) else {
            return Dispatch::Done(Resp::fail(404, &format!("database '{name}' is not loaded")));
        };
        let inline = wants_inline(req);
        let scratch_dir = st
            .gapi
            .export_dir
            .clone()
            .unwrap_or_else(|| self.root.join("export"));
        let (task_id, cancel) =
            self.tasks
                .register("export", &name, &format!("export {name}"), user);
        Dispatch::Deferred(Box::new(move |inner| {
            // Ends the task (also on a panic) once the export is written.
            let mut task = TaskGuard::new(inner, task_id);
            let written = rest_export(&dbh, &name, format, &scratch_dir, &cancel);
            task.set_ok(written.is_ok());
            match written {
                Ok((format, body)) => {
                    let (ctype, ext) = match format {
                        ExportFormat::NTriples => ("application/n-triples", "nt"),
                        ExportFormat::NQuads => ("application/n-quads", "nq"),
                    };
                    let mut resp = Resp::stream(200, ctype, body);
                    let disposition = if inline { "inline" } else { "attachment" };
                    resp.extra.push((
                        "Content-Disposition".to_string(),
                        format!("{disposition}; filename=\"{name}.{ext}\""),
                    ));
                    resp
                }
                Err(e) => e.rest(),
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
    /// through [`Database::try_insert_triple`] / [`Database::try_remove_triple`].
    /// No SPARQL text is ever built from the payload, so it cannot smuggle in
    /// other update operations: a malformed payload is a `400` parse error and
    /// nothing is applied. Reports `parsed`, `applied` (= `changed`) and
    /// `failed` (parsed but not applied: duplicates on insert, absent triples on
    /// remove, or a literal/blank in a position RDF does not allow), and the
    /// `/api` operation's `success_num` (= `applied`) and `failed_num` (the
    /// statements skipped as invalid). A triple
    /// that cannot be applied — an id space exhausted, a read error looking up
    /// its terms (C2), a broken store — stops the batch with a `500` naming
    /// how many were applied before it.
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
            let cfg = inner.config();
            if cfg.querylog_mode {
                append_daily_log(
                    &inner.root,
                    "query.log",
                    &format!(
                        "{}\t{}\t{}\t{op} {} triple(s) ({format})",
                        now_millis(),
                        name,
                        user,
                        triples.len()
                    ),
                    cfg.querylog_days,
                );
            }
            let Some(dbh) = dbh else {
                return Resp::fail(404, &format!("database '{name}' is not loaded"));
            };
            let mut slot = match dbh.write() {
                Ok(slot) => slot,
                Err(e) => return e.rest(),
            };
            if slot.retired {
                return Resp::fail(404, &format!("database '{name}' is not loaded"));
            }
            if let Some(open) = &slot.txn {
                return txn_conflict(&name, open.tid);
            }
            let parsed = triples.len();
            // Statements RDF does not allow (a literal or blank node out of
            // place), skipped: upstream's `failed_num`.
            let skipped = triples.iter().filter(|t| !valid_ground_triple(t)).count();
            let mut applied = 0usize;
            let mut failure = None;
            for t in triples.iter().filter(|t| valid_ground_triple(t)) {
                let changed = if insert {
                    slot.db.try_insert_triple(t)
                } else {
                    slot.db.try_remove_triple(t)
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
            // Applied but not saved: a failure (HTTP 500), review durability#2.
            // `success_num` / `failed_num` mean what the `/api` operation's
            // do (review design#4): statements applied, and statements
            // skipped as invalid — a duplicate (insert) or absent triple
            // (remove) is neither, as upstream counts them. `failed` keeps
            // counting every statement not applied.
            let fields = format!(
                ",\"format\":{},\"parsed\":{parsed},\"applied\":{applied},\"failed\":{failed},\"changed\":{applied},\"success_num\":{applied},\"failed_num\":{skipped}",
                json_str(format),
            );
            Resp::write_outcome(&format!("batch {verb}"), &fields, &warning)
        }))
    }

    // ---- monitoring -------------------------------------------------------

    fn handle_status(&self, st: &ApiState) -> Dispatch {
        // Snapshot registry scalars under the state lock; clone the db handles
        // so the figures are read with the state lock released. They are the
        // ones each database published at the end of its last write: no
        // database lock is taken.
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
                let f = d.figures();
                total_triples += f.triples;
                dirty_dbs += usize::from(f.dirty);
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
        let txns: HashMap<String, TxnSummary> = entries
            .iter()
            .filter_map(|(n, _)| Some((n.clone(), self.txn_summary(st, n)?)))
            .collect();
        Dispatch::Deferred(Box::new(move |_| {
            // The figures each database published at the end of its last
            // write (no database lock: monitoring never waits for a
            // statement).
            let mut dbs = Vec::new();
            for (n, d) in &entries {
                let f = d.figures();
                dbs.push(format!(
                    "{{\"name\":{},\"triples\":{},\"entities\":{},\"literals\":{},\"predicates\":{},\"index_valid\":{},\"in_transaction\":{},\"dirty\":{}{}}}",
                    json_str(&d.name),
                    f.triples,
                    f.entities,
                    f.literals,
                    f.predicates,
                    f.index_valid,
                    f.in_transaction,
                    f.dirty,
                    txns.get(n).map_or_else(String::new, TxnSummary::json_field),
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

    /// `/user/create` (the caller's `usermanage` privilege is checked by
    /// [`dispatch`](Self::dispatch)). The request is checked here, under the
    /// state lock; the password hash and the users.db write run deferred
    /// with no lock held (review http#7), and the user is created only if
    /// the name is still free then.
    fn handle_user_create(&self, st: &mut ApiState, req: &Request) -> Dispatch {
        let (Some(name), Some(password)) = (
            req.param("op_username").or_else(|| req.param("username")),
            req.param("op_password").or_else(|| req.param("password")),
        ) else {
            return Dispatch::Done(Resp::fail(400, "missing op_username or op_password"));
        };
        let privileges = req.param("privileges").unwrap_or_default();
        let Some(privs) = Privilege::parse_list(&privileges) else {
            return Dispatch::Done(Resp::fail(
                400,
                "invalid privileges (use names or codes 1..8)",
            ));
        };
        // With a `db_name` the privileges are granted on that database only;
        // without, on every database.
        let db = match grant_db_param(req) {
            Ok(db) => db,
            Err(r) => return Dispatch::Done(r),
        };
        let (global, on_db) = match db {
            Some(db) => (BTreeSet::new(), Some((db, privs))),
            None => (privs, None),
        };
        // The system user (`/shutdown`, `/restart`) is not a stored user.
        if name == self.config().system_username {
            return Dispatch::Done(Resp::fail(
                400,
                &format!("'{name}' is the system user's name (system_username)"),
            ));
        }
        if st.users.exists(&name) {
            return Dispatch::Done(Resp::fail(400, &format!("user '{name}' already exists")));
        }
        Dispatch::Deferred(Box::new(move |inner| {
            let hashed = match NewPassword::hash(&password) {
                Ok(h) => h,
                Err(e) => return Resp::fail(400, &e),
            };
            let on_db = on_db.as_ref().map(|(db, p)| (db.as_str(), p));
            let created = crate::http_users::write_off_lock(
                &inner.state,
                |st| &mut st.users,
                |users| users.prepare_add(&name, &hashed, global.clone(), on_db),
            );
            match created {
                Ok(()) => Resp::ok(&format!("user '{name}' created")),
                Err(e) => Resp::fail(400, &e),
            }
        }))
    }

    fn handle_user_drop(&self, st: &mut ApiState, req: &Request) -> Resp {
        let Some(name) = req.param("op_username").or_else(|| req.param("username")) else {
            return Resp::fail(400, "missing op_username");
        };
        match st.users.drop_user(&name) {
            // Its PFN functions go with it (see `PfnRegistry::drop_user`).
            Ok(()) => match self.pfn.drop_user(&name) {
                Ok(()) => Resp::ok(&format!("user '{name}' dropped")),
                Err(e) => {
                    crate::gs_error!(
                        "user '{name}' dropped, but removing its PFN functions failed: {e}"
                    );
                    Resp::fail(
                        500,
                        &format!(
                            "user '{name}' dropped, but removing its PFN functions failed: {e}"
                        ),
                    )
                }
            },
            Err(e) => Resp::fail(400, &e),
        }
    }

    /// `/user/password`: whether the caller may change the target's
    /// password is decided here, under the state lock; the password hash
    /// and the users.db write run deferred with no lock held (review
    /// http#7), and the password is set only if the user still exists then.
    fn handle_user_password(&self, st: &mut ApiState, req: &Request, user: &str) -> Dispatch {
        let Some(target) = req
            .param("username")
            .or_else(|| req.param("op_username"))
            .or_else(|| Some(user.to_string()))
        else {
            return Dispatch::Done(Resp::fail(400, "missing username"));
        };
        let Some(password) = req.param("op_password").or_else(|| req.param("password")) else {
            return Dispatch::Done(Resp::fail(400, "missing op_password"));
        };
        // A user may change their own password; only admins may change others'.
        let is_admin = st.users.is_superuser(user);
        if target != user && !is_admin {
            return Dispatch::Done(self.forbidden());
        }
        if !st.users.exists(&target) {
            return Dispatch::Done(Resp::fail(400, &format!("user '{target}' does not exist")));
        }
        Dispatch::Deferred(Box::new(move |inner| {
            let hashed = match NewPassword::hash(&password) {
                Ok(h) => h,
                Err(e) => return Resp::fail(400, &e),
            };
            let changed = crate::http_users::write_off_lock(
                &inner.state,
                |st| &mut st.users,
                |users| users.prepare_set_password(&target, &hashed),
            );
            match changed {
                Ok(()) => Resp::ok(&format!("password for '{target}' changed")),
                Err(e) => Resp::fail(400, &e),
            }
        }))
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

    /// The transaction id from `tid`/`txn` (its number: the Workbench's
    /// `beginTime_tid` form is accepted, see [`txn_ref`](Self::txn_ref), whose
    /// begin time the transaction handlers check).
    fn txn_id(&self, req: &Request) -> Option<u64> {
        self.txn_ref(req).map(|t| t.tid)
    }

    /// The `tid` (or `txn`) parameter, parsed as `/api` parses it
    /// ([`parse_tid`]): a positive integer or `<begin>_<tid>`.
    fn txn_ref(&self, req: &Request) -> Option<TidRef> {
        parse_tid(&req.param("tid").or_else(|| req.param("txn"))?)
    }
}

// --- free helpers ------------------------------------------------------------

// --- server-local paths named by clients --------------------------------------

/// Marker every backup this server writes (`/backup`, `/api`'s `backup`)
/// carries in its directory: the name of the database backed up there, so a
/// later backup of a *different* database cannot overwrite it, and a restore
/// knows whose data it holds ([`may_restore_from`]).
pub(crate) const BACKUP_OWNER_FILE: &str = "BACKUP_OF";

/// Marker a backup this server is writing carries in its directory until
/// it is complete (the name of the database backed up there): written
/// before anything is copied and replaced by [`BACKUP_OWNER_FILE`] once the
/// backup succeeded (round-3 review). A directory a crashed or panicked
/// backup left keeps it, so a later backup of the same database may write
/// there again (it used to be refused for good, `409` "already holds data
/// that is not a backup of …"), while a restore refuses it as incomplete —
/// also when it was replacing an earlier, complete backup.
pub(crate) const BACKUP_PARTIAL_FILE: &str = "BACKUP_OF.partial";

/// The database the backup at `dir` was recorded as a backup of (its
/// [`BACKUP_OWNER_FILE`]), if any — none while the directory is marked as a
/// backup in progress ([`BACKUP_PARTIAL_FILE`]): it is not complete.
pub(crate) fn backup_owner(dir: &Path) -> Option<String> {
    if backup_in_progress(dir).is_some() {
        return None;
    }
    fs::read_to_string(dir.join(BACKUP_OWNER_FILE))
        .ok()
        .map(|s| s.trim().to_string())
}

/// The database a backup into `dir` was started for and has not completed
/// ([`BACKUP_PARTIAL_FILE`]), if any.
pub(crate) fn backup_in_progress(dir: &Path) -> Option<String> {
    let path = dir.join(BACKUP_PARTIAL_FILE);
    match fs::read_to_string(&path) {
        Ok(s) => Some(s.trim().to_string()),
        // Present but unreadable: still not a complete backup.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    }
}

/// Mark `dir` as a backup of database `db` in progress, before anything is
/// copied into it ([`BACKUP_PARTIAL_FILE`]).
pub(crate) fn mark_backup_started(dir: &Path, db: &str) -> crate::Result<()> {
    crate::db::fsutil::atomic_write(&dir.join(BACKUP_PARTIAL_FILE), db.as_bytes())
}

/// Record the backup at `dir` as one of database `db`, complete: its owner
/// record is written, then its in-progress marker removed (durably).
pub(crate) fn mark_backup(dir: &Path, db: &str) -> crate::Result<()> {
    crate::db::fsutil::atomic_write(&dir.join(BACKUP_OWNER_FILE), db.as_bytes())?;
    match fs::remove_file(dir.join(BACKUP_PARTIAL_FILE)) {
        Ok(()) => crate::db::fsutil::sync_dir(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Best effort: remove everything in directory `dir`, keeping `dir` itself —
/// a REST `/backup` into a directory that was empty (perhaps a mount point)
/// leaves it as it found it when the backup fails.
fn empty_dir(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let _ = match entry.file_type() {
            Ok(t) if t.is_dir() => fs::remove_dir_all(&path),
            _ => fs::remove_file(&path),
        };
    }
}

/// One backup request's claim on its destination directory: backups into
/// one directory — REST `/backup`s of a database into its `<db>.backup`,
/// `/api backup`s of a database in the same second (one `<db>.db_<time>`
/// name), or one of each — run one after the other, from checking or
/// clearing the destination to writing and recording (and packing,
/// rotating) the backup. Backups hold only the database's shared guard, so
/// without the claim they wrote each other's files: one's owner record or
/// staged copy was replaced or removed under the other mid-write. Taken
/// before the database's guard and released after it (never the reverse,
/// so no guard holder waits for a claim). Keyed by the resolved path (its
/// deepest existing ancestor canonicalized), so two spellings of one
/// directory are one claim.
pub(crate) struct BackupClaim {
    dest: PathBuf,
}

impl BackupClaim {
    /// The backup directories claimed by the requests in flight.
    fn registry() -> &'static (Mutex<HashSet<PathBuf>>, std::sync::Condvar) {
        static CLAIMS: OnceLock<(Mutex<HashSet<PathBuf>>, std::sync::Condvar)> = OnceLock::new();
        CLAIMS.get_or_init(Default::default)
    }

    /// Claim `dest`, waiting while another request holds it.
    pub(crate) fn new(dest: &Path) -> BackupClaim {
        let dest = resolved_path(dest).unwrap_or_else(|| dest.to_path_buf());
        let (claims, freed) = Self::registry();
        let mut held = locked(claims);
        while held.contains(&dest) {
            held = freed
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        held.insert(dest.clone());
        BackupClaim { dest }
    }
}

impl Drop for BackupClaim {
    fn drop(&mut self) {
        let (claims, freed) = Self::registry();
        locked(claims).remove(&self.dest);
        freed.notify_all();
    }
}

/// `path` with its deepest existing ancestor canonicalized (so symlinks are
/// resolved) and the rest — plain names, nothing left to resolve —
/// re-appended; `None` when no ancestor resolves.
fn resolved_path(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut existing = path;
    let mut resolved = loop {
        match fs::canonicalize(existing) {
            Ok(c) => break c,
            Err(_) => {
                rest.push(existing.file_name()?.to_os_string());
                existing = existing.parent()?;
            }
        }
    };
    for part in rest.iter().rev() {
        resolved.push(part);
    }
    Some(resolved)
}

/// Whether `user` may restore database `db` from the backup at `source`:
/// `root` and `admin` users any backup; anyone else — who holds the restore
/// privilege on `db` only — only a backup this server recorded as one of `db`
/// ([`backup_owner`]), so the privilege on one database never reads another
/// database's data.
pub(crate) fn may_restore_from(users: &UserStore, user: &str, db: &str, source: &Path) -> bool {
    user == ROOT_USER || users.is_superuser(user) || backup_owner(source).as_deref() == Some(db)
}

/// The built time a backup's name records (`…_YYYYmmddHHMMSS` →
/// `YYYY-MM-DD HH:MM:SS`, upstream `Util::get_backup_time`): the registry's
/// built time of a database a restore from it creates (REST and `/api`).
pub(crate) fn backup_time(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let ts = name.rsplit_once('_')?.1;
    let ts = ts.strip_suffix(".zip").unwrap_or(ts);
    if ts.len() != 14 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{} {}:{}:{}",
        &ts[0..4],
        &ts[4..6],
        &ts[6..8],
        &ts[8..10],
        &ts[10..12],
        &ts[12..14]
    ))
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
/// Where REST `/build` reads its RDF: a server-local file (`db_path`) or
/// the request body.
enum RestBuildSource {
    File(PathBuf),
    Text(String),
}

/// A build's `layout` parameter (REST `/build`, `/api build`):
/// `snapshot` (also `memory`), `disk` (also `kvstore`) or `rocksdb`
/// (also `rocks`), case-insensitive; `default` when it is absent or empty.
/// The RocksDB layout needs a server built with the `rocksdb` feature.
pub(crate) fn build_layout(
    param: Option<&str>,
    default: StorageKind,
) -> Result<StorageKind, String> {
    let layout = match param.map(str::trim).filter(|v| !v.is_empty()) {
        None => default,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "snapshot" | "memory" => StorageKind::Snapshot,
            "disk" | "kvstore" => StorageKind::Disk,
            "rocksdb" | "rocks" => StorageKind::RocksDb,
            _ => {
                return Err(format!(
                    "invalid layout '{v}': it can only be snapshot, disk or rocksdb"
                ))
            }
        },
    };
    if layout == StorageKind::RocksDb && !cfg!(feature = "rocksdb") {
        return Err(
            "the rocksdb layout needs a server built with the rocksdb feature \
             (--features rocksdb)"
                .into(),
        );
    }
    Ok(layout)
}

/// The bulk loader's target for an on-disk or RocksDB build layout.
pub(crate) fn bulk_target(layout: StorageKind) -> crate::bulk::BulkTarget {
    match layout {
        StorageKind::RocksDb => crate::bulk::BulkTarget::RocksDb,
        _ => crate::bulk::BulkTarget::Disk,
    }
}

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
    let target = resolved_path(&joined).ok_or("unresolvable path")?;
    if target.starts_with(&base) && target != base {
        Ok(target)
    } else {
        Err("outside the directory".into())
    }
}

/// Where a `/backup` of database `name` may write: `requested` (default
/// `<name>.backup`) confined to `backup_dir`, not nested inside a directory
/// holding a database (another backup), and either new, empty, or an earlier
/// backup of `name` itself (see [`BACKUP_OWNER_FILE`]). The destination is
/// [claimed](BackupClaim) before it is checked, and the claim returned with
/// it: a concurrent backup into it must neither be seen half-written by the
/// check nor write beside this one.
fn backup_target(
    backup_dir: &Path,
    name: &str,
    requested: Option<&str>,
) -> Result<(PathBuf, BackupClaim), Resp> {
    // Durably (review tests-b#3: the fsync audit found the backup
    // directory's entry was not).
    if let Err(e) = crate::db::fsutil::create_dir_durable(backup_dir) {
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
    let claim = BackupClaim::new(&target);
    let conflict = |what: &str| {
        Resp::fail(
            409,
            &format!(
                "backup path '{}' {what}; choose another path",
                target.display()
            ),
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
        if Database::storage_kind(dir).is_some()
            || dir.join(BACKUP_OWNER_FILE).exists()
            || dir.join(BACKUP_PARTIAL_FILE).exists()
        {
            return Err(conflict("lies inside another backup"));
        }
        ancestor = dir.parent();
    }
    match fs::symlink_metadata(&target) {
        Err(_) => Ok((target, claim)),
        Ok(m) if m.is_dir() => {
            let owner = fs::read_to_string(target.join(BACKUP_OWNER_FILE)).ok();
            // A backup of `name` that did not complete (a crash or a panic
            // skipped its cleanup) may be written again.
            let partial = backup_in_progress(&target);
            let empty = fs::read_dir(&target)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            let ours = |record: &Option<String>| record.as_deref().map(str::trim) == Some(name);
            let foreign = |record: &Option<String>| record.is_some() && !ours(record);
            if empty || ((ours(&owner) || ours(&partial)) && !foreign(&owner) && !foreign(&partial))
            {
                Ok((target, claim))
            } else {
                Err(conflict(&format!(
                    "already holds data that is not a backup of '{name}'"
                )))
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
/// line whole, and the text may be large, so callers do it off the state
/// lock) — when `cfg`'s `querylog_mode` is on; the file rolls over by day,
/// keeping `querylog_days` days ([`append_daily_log`]).
pub(crate) fn log_query(cfg: &GStoreConfig, root: &Path, db: &str, user: &str, sparql: &str) {
    if !cfg.querylog_mode {
        return;
    }
    append_daily_log(
        root,
        "query.log",
        &format!(
            "{}\t{}\t{}\t{}",
            now_millis(),
            db,
            user,
            sparql.replace('\n', " ")
        ),
        cfg.querylog_days,
    );
}

/// The query-log record of a REST SPARQL request (`/sparql`, `/update`,
/// `/tquery`): the same JSON record, in the same daily file
/// (`querylog_path`), as the `/api` `query` operation writes
/// ([`crate::gapi::apilog`]) — when it started, the client's IP, the
/// statement, the number of answers (rows; `1` for ASK; triples for
/// CONSTRUCT / DESCRIBE; triples changed for an UPDATE), the result format,
/// the outcome (`statusCode` `0`, or the HTTP status of the error) and the
/// latency in ms. [`start`](Self::start) it before the statement runs,
/// [`finish`](Self::finish) it with the outcome. Written when
/// `querylog_mode` is on.
pub(crate) struct RestQueryLog {
    db: String,
    /// The statement (empty when the query log is off: not copied then).
    sparql: String,
    format: &'static str,
    ip: String,
    started: SystemTime,
    timer: Instant,
}

impl RestQueryLog {
    /// Must run on the request's worker thread (it reads the client's
    /// address from [`crate::server::current_peer`]).
    pub(crate) fn start(
        inner: &ApiInner,
        db: &str,
        sparql: &str,
        fmt: ResultFormat,
    ) -> RestQueryLog {
        let on = inner.config().querylog_mode;
        RestQueryLog {
            db: db.to_string(),
            sparql: if on {
                sparql.to_string()
            } else {
                String::new()
            },
            format: format_name(fmt),
            ip: peer_ip(crate::server::current_peer()),
            started: SystemTime::now(),
            timer: Instant::now(),
        }
    }

    /// Record the outcome: the number of answers, or the error.
    pub(crate) fn finish(self, inner: &ApiInner, outcome: Result<usize, &OpError>) {
        let config = inner.config();
        let query_time = self.timer.elapsed().as_millis() as u64;
        crate::gs_debug!(
            "query on '{}' from {}: {} in {query_time} ms",
            self.db,
            self.ip,
            match &outcome {
                Ok(n) => format!("{n} answer(s)"),
                Err(e) => format!("failed ({})", e.msg),
            }
        );
        if !config.querylog_mode {
            return;
        }
        let (ans_num, status_code, status_msg) = match outcome {
            Ok(n) => (
                i64::try_from(n).unwrap_or(i64::MAX),
                0,
                "success".to_string(),
            ),
            Err(e) => (0, i64::from(e.http()), e.msg.replace(['\r', '\n'], " ")),
        };
        let rec = crate::gapi::apilog::QueryLogRecord {
            query_date_time: crate::sysdb::clock::at(self.started).datetime_ms(),
            remote_ip: self.ip,
            sparql: self.sparql,
            ans_num,
            format: self.format.to_string(),
            file_name: String::new(),
            status_code,
            status_msg,
            query_time,
            db_name: self.db,
        };
        inner.apilogs.write_query(&config, &inner.root, &rec);
    }
}

/// The number of answers of a query result: rows of a SELECT, `1` for an
/// ASK, triples of a CONSTRUCT / DESCRIBE, triples changed by an UPDATE.
fn answer_count(result: &QueryResult) -> usize {
    match result {
        QueryResult::Select(rs) => rs.row_count(),
        QueryResult::Ask(_) => 1,
        QueryResult::Construct(ts) => ts.len(),
        QueryResult::Update { changed } => *changed,
    }
}

/// The query log's name of a result format.
fn format_name(fmt: ResultFormat) -> &'static str {
    match fmt {
        ResultFormat::Json => "json",
        ResultFormat::Xml => "xml",
        ResultFormat::Csv => "csv",
        ResultFormat::Tsv => "tsv",
    }
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

/// The answer to root's default password from a client that is not local.
const DEFAULT_PASSWORD_REFUSED: &str = "the default root password is accepted only from a \
     local client (not from a remote one, nor through a proxy): change root's password";

/// Whether `req` authenticates as root with the default password, in Basic
/// credentials or as `/login`'s `username` / `password` parameters. A
/// constant comparison: the stored hash is not consulted, so the check costs
/// nothing and tells nothing about root's actual password.
fn presents_default_root_password(req: &Request) -> bool {
    let default = crate::config::DEFAULT_ROOT_PASSWORD;
    if parse_basic_auth(req).is_some_and(|(u, p)| u == ROOT_USER && p == default) {
        return true;
    }
    req.path == "/login"
        && req.param("username").as_deref() == Some(ROOT_USER)
        && req.param("password").as_deref() == Some(default)
}

/// The answer to a password check the failed-login lockout refuses
/// (`crate::gapi::ipfilter`): `429 Too Many Requests` with `Retry-After`.
fn lockout_refused(refusal: crate::gapi::ipfilter::LockRefusal) -> Resp {
    let mut r = Resp::fail(429, &refusal.message());
    r.extra.push((
        "Retry-After".to_string(),
        crate::gapi::ipfilter::retry_after_secs(refusal.wait).to_string(),
    ));
    r
}

/// The message of an answer larger than `max_result_bytes` (`limit`).
pub(crate) fn result_too_large_msg(limit: u64) -> String {
    format!(
        "result too large: the answer exceeds max_result_bytes ({limit} bytes); ask for less \
         (project fewer variables, add a LIMIT and page with OFFSET) or raise max_result_bytes"
    )
}

/// A [`Write`] adapter that counts the bytes it passes on and fails the
/// write that would take them past `limit` (`0`: no limit) — the result
/// byte budget (`max_result_bytes`, review sec#10). Serialization stops at
/// that write, so no more than `limit` bytes of an oversized answer are
/// ever produced; [`exceeded`](Self::exceeded) tells the failure from an
/// I/O error of `inner`. Used around every result serializer of both
/// servers (REST answers here, the single-database server's buffered and
/// streamed ones).
pub(crate) struct CountingWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
    exceeded: bool,
}

impl<W: Write> CountingWriter<W> {
    /// Count what goes to `inner`, at most `limit` bytes (`0`: no limit).
    pub(crate) fn new(inner: W, limit: u64) -> CountingWriter<W> {
        CountingWriter {
            inner,
            written: 0,
            limit,
            exceeded: false,
        }
    }

    /// Whether a write was refused for passing the limit.
    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// The wrapped writer.
    pub(crate) fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.limit != 0 && self.written.saturating_add(buf.len() as u64) > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other(result_too_large_msg(self.limit)));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write `triples` as N-Triples ([`ntriples_body`], a slice at a time) to
/// `out`, stopping at the first failed write (a [`CountingWriter`]'s budget).
pub(crate) fn write_ntriples<W: Write>(
    triples: &[crate::model::Triple],
    out: &mut W,
) -> std::io::Result<()> {
    for chunk in triples.chunks(256) {
        out.write_all(&ntriples_body(chunk))?;
    }
    Ok(())
}

/// Render a [`QueryResult`] into an HTTP response using the negotiated format.
fn render_result(fmt: ResultFormat, result: &QueryResult) -> Resp {
    render_result_within(fmt, result, 0).unwrap_or_else(|e| e.rest())
}

/// [`render_result`] within the result byte budget `limit`
/// (`max_result_bytes`; `0`: none): an answer that would pass it is
/// [`OpErrorKind::ResultTooLarge`], found as soon as its serialization
/// passes the budget (a [`CountingWriter`]), never after the whole of it.
fn render_result_within(
    fmt: ResultFormat,
    result: &QueryResult,
    limit: u64,
) -> Result<Resp, OpError> {
    let too_large = || OpError::new(OpErrorKind::ResultTooLarge, result_too_large_msg(limit));
    let mut out = CountingWriter::new(Vec::new(), limit);
    let (ctype, written) = match result {
        QueryResult::Select(rs) => (
            fmt.content_type(),
            sparql_results::write_select(fmt, rs, &mut out),
        ),
        QueryResult::Ask(b) => (
            fmt.content_type(),
            sparql_results::write_ask(fmt, *b, &mut out),
        ),
        QueryResult::Construct(ts) => ("application/n-triples", write_ntriples(ts, &mut out)),
        QueryResult::Update { changed } => return Ok(update_resp(*changed, &None)),
    };
    if written.is_err() && out.exceeded() {
        return Err(too_large());
    }
    Ok(Resp::new(200, ctype, out.into_inner()))
}

/// Export served database `slot` (whose shared guard the caller holds) to
/// `out` as `format` — `None`: the format that loses nothing, N-Quads when it
/// has a non-empty named graph ([`ExportFormat::for_database`]) — and return
/// the format written and the statement count. The export of the REST
/// `/export` and the `/api` `export` alike (review http#11, design#4): its
/// live state ([`crate::zipio::write_export`]), or — while a transaction is
/// open — the transaction's begin-time snapshot (the `/api` caller refuses
/// that case first, as upstream). A raised `cancel` stops it at its next
/// statement ([`OpErrorKind::Cancelled`]); a read error fails it (contract
/// C2).
pub(crate) fn export_slot(
    slot: &DbSlot,
    format: Option<ExportFormat>,
    out: &mut dyn Write,
    cancel: Option<&AtomicBool>,
) -> Result<(ExportFormat, u64), OpError> {
    let mut out = CancellableWriter { out, cancel };
    let written = match &slot.txn {
        None => {
            let format = format.unwrap_or_else(|| ExportFormat::for_database(&slot.db));
            crate::zipio::write_export(&slot.db, format, &mut out).map(|n| (format, n))
        }
        Some(open) => open.snapshot.write_export(format, &mut out),
    };
    written.map_err(|e| {
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            OpError::new(OpErrorKind::Cancelled, "export cancelled")
        } else {
            OpError::of(&e, OpErrorKind::Failed, e.to_string())
        }
    })
}

/// A writer that fails once `cancel` is raised: how an export stops at its
/// next statement after a `/cancel`.
struct CancellableWriter<'a> {
    out: &'a mut dyn Write,
    cancel: Option<&'a AtomicBool>,
}

impl Write for CancellableWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            return Err(std::io::Error::other("export cancelled"));
        }
        self.out.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// The REST `/export` of loaded database `name` (`dbh`): [`export_slot`]
/// under a shared guard into an unnamed scratch file in `scratch_dir` (the
/// export directory), returned — with the guard released — as a streamed
/// body positioned at its start. The file is unlinked once open (where
/// the platform allows; elsewhere when the body is dropped), so neither a
/// failure nor a crash leaves it behind. `cancel` is the task's flag.
fn rest_export(
    dbh: &DbHandle,
    name: &str,
    format: Option<ExportFormat>,
    scratch_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(ExportFormat, crate::gapi::envelope::FileBody), OpError> {
    use std::io::{Seek, SeekFrom};
    /// Removes the scratch file once the body that reads it is dropped.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let failed = |e: std::io::Error| {
        OpError::new(
            OpErrorKind::Failed,
            format!("export failed: cannot write its scratch file: {e}"),
        )
    };
    fs::create_dir_all(scratch_dir).map_err(failed)?;
    static SCRATCH: AtomicU64 = AtomicU64::new(0);
    let path = scratch_dir.join(format!(
        ".rest-export-{}-{}-{}.tmp",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed),
        now_millis()
    ));
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(failed)?;
    let scratch = Scratch(path);
    // Unnamed from now on where the platform allows it.
    let _ = fs::remove_file(&scratch.0);
    let written = {
        let slot = dbh.read()?;
        if slot.retired {
            return Err(OpError::new(
                OpErrorKind::NotLoaded,
                format!("database '{name}' is not loaded"),
            ));
        }
        txn_park(name, TxnParkPoint::StatementHolding);
        let mut out = std::io::BufWriter::new(&mut file);
        let written = export_slot(&slot, format, &mut out, Some(cancel));
        drop(out);
        written
    };
    let (format, _) = written.map_err(|e| match e.kind {
        OpErrorKind::Cancelled => e,
        kind => OpError::new(kind, format!("export failed: {}", e.msg)),
    })?;
    let len = file.seek(SeekFrom::End(0)).map_err(failed)?;
    file.seek(SeekFrom::Start(0)).map_err(failed)?;
    let body = crate::gapi::envelope::FileBody::new(file, 0, len).with_permit(scratch);
    Ok((format, body))
}

/// The error message of an UPDATE that failed after applying part of its
/// operations (which stay applied — and were saved, unless `warning` says the
/// save failed: the message then starts with that warning, `"applied in
/// memory but not durable: …"`).
fn partial_update_msg(e: &GStoreError, warning: Option<String>) -> String {
    match warning {
        Some(w) => format!(
            "{w} (the update failed part-way: {e}; the operations before the failing one \
             were applied)"
        ),
        None => format!("{e} (operations before the failing one were applied)"),
    }
}

/// The JSON answer to an applied UPDATE: `{"StatusCode":0,"StatusMsg":
/// "success","changed":N}`, or — when its save failed (`warning`) — the HTTP
/// 500 failure of [`Resp::write_outcome`] (review durability#2).
fn update_resp(changed: usize, warning: &Option<String>) -> Resp {
    Resp::write_outcome("success", &format!(",\"changed\":{changed}"), warning)
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
            other => {
                return Err(format!(
                    "(unknown format '{other}'; use ntriples or turtle)"
                ))
            }
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
/// leading `PREFIX`/`BASE` declarations: the classification every endpoint
/// makes, from the one parse ([`Statement`]) the statement then runs as.
#[cfg(test)]
pub(crate) fn is_update_sparql(s: &str) -> bool {
    // Classify by the *parser*, not a whitespace token scan. A leading
    // `# comment` line (the lexer skips comments) or unusual whitespace must not
    // be able to hide an UPDATE keyword from this check — otherwise a query-only
    // user could smuggle `# x\nINSERT DATA { … }` past the privilege gate and
    // have the query job execute it. A parse failure is treated as non-update:
    // such a statement is never executed (the job answers the syntax error).
    Statement::parse(s).is_ok_and(|stmt| stmt.is_update())
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
        let ct = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
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

/// [`append_log`] to a plain-text log that rolls over by day, keeping
/// `days` days of rolled files ([`crate::logging::roll_daily`]).
pub(crate) fn append_daily_log(root: &Path, file: &str, line: &str, days: u32) {
    crate::logging::roll_daily(&root.join(file), days);
    append_log(root, file, line);
}

/// Append one line (with a trailing newline) to `root/file`, best effort:
/// one `write(2)` per line ([`crate::logging::append_line`]), so concurrent
/// requests' lines never interleave (review conc#6).
pub(crate) fn append_log(root: &Path, file: &str, line: &str) {
    let _ = crate::logging::append_line(&root.join(file), line);
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

/// Test hook: lets a test act on a REST `/backup` once it marked its
/// directory as a backup in progress and before it copies anything — e.g.
/// panic there, as a crash would stop it.
#[cfg(test)]
pub(crate) mod backup_test_hook {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    static HOOKS: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    /// Call `hook` when a backup into `dir` reaches the point.
    pub(crate) fn set(dir: &Path, hook: Hook) {
        HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((dir.to_path_buf(), hook));
    }

    /// Remove the hooks of `dir`.
    pub(crate) fn clear(dir: &Path) {
        HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(d, _)| d != dir);
    }

    pub(super) fn reached(dir: &Path) {
        let hooks: Vec<Hook> = HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(d, _)| d == dir)
            .map(|(_, h)| Arc::clone(h))
            .collect();
        for hook in hooks {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// review design#3: the status-mapping table — one classification for
    /// every protocol layer. A statement error and a lifecycle failure get
    /// the status of their kind; an unclassified one keeps the endpoint's
    /// own (`untyped`).
    #[test]
    fn errors_map_to_the_status_of_their_kind() {
        let io = |kind| GStoreError::Io(std::io::Error::new(kind, "x"));
        let read = crate::store::read_scope::read_failure("evaluating the query", "page 7");
        let table: Vec<(GStoreError, u16, u16, u16)> = vec![
            // (error, as a statement, as a lifecycle step whose unclassified
            // errors were 409, … whose unclassified errors were 500)
            (GStoreError::SparqlParse("x".into()), 400, 400, 400),
            (
                GStoreError::RdfParse {
                    line: 1,
                    msg: "x".into(),
                },
                400,
                400,
                400,
            ),
            (GStoreError::Query("x".into()), 400, 400, 400),
            (GStoreError::Locked("x".into()), 409, 409, 409),
            (GStoreError::Conflict("x".into()), 409, 409, 409),
            (io(std::io::ErrorKind::PermissionDenied), 500, 500, 500),
            (io(std::io::ErrorKind::Other), 500, 500, 500),
            (read.duplicate(), 500, 500, 500),
            (GStoreError::StorageFailed("x".into()), 500, 500, 500),
            (GStoreError::Serialize("x".into()), 500, 500, 500),
            (GStoreError::Cancelled, 503, 503, 503),
            (GStoreError::Timeout(Duration::from_secs(1)), 503, 503, 503),
            (GStoreError::Database("x".into()), 400, 409, 500),
        ];
        for (e, statement, conflict, failed) in table {
            assert_eq!(OpError::statement(&e).http(), statement, "{e}");
            assert_eq!(
                fail_with(&e, OpErrorKind::Conflict, "m").status,
                conflict,
                "{e}"
            );
            assert_eq!(
                fail_with(&e, OpErrorKind::Failed, "m").status,
                failed,
                "{e}"
            );
        }
        // A statement's refusals keep the SPARQL endpoints' body.
        let cancelled = OpError::statement(&GStoreError::Cancelled).rest();
        assert_eq!(
            (cancelled.status, cancelled.ctype.as_str()),
            (503, SPARQL_JSON)
        );
        let failed = OpError::statement(&read).rest();
        assert_eq!(failed.status, 500);
    }

    /// review sec#10: the result byte budget stops a serializer at the
    /// write that passes it — an oversized answer is never produced whole.
    #[test]
    fn the_counting_writer_stops_an_oversized_answer_at_the_budget() {
        let mut rs = crate::query::ResultSet::new(vec!["s".to_string()]);
        for i in 0..10_000 {
            rs.rows.push(vec![Some(format!("<http://ex/{i}>"))]);
        }
        let mut whole = Vec::new();
        sparql_results::write_select(ResultFormat::Json, &rs, &mut whole).unwrap();
        assert!(whole.len() > 200_000);
        let mut out = CountingWriter::new(Vec::new(), 4096);
        let r = sparql_results::write_select(ResultFormat::Json, &rs, &mut out);
        assert!(r.is_err() && out.exceeded());
        assert!(out.into_inner().len() <= 4096, "no more than the budget");
        // Within the budget (or with none) the bytes are those of the answer.
        let mut out = CountingWriter::new(Vec::new(), 0);
        sparql_results::write_select(ResultFormat::Json, &rs, &mut out).unwrap();
        assert!(!out.exceeded());
        assert_eq!(out.into_inner(), whole);
        let small = QueryResult::Ask(true);
        assert_eq!(
            render_result_within(ResultFormat::Json, &small, 4096)
                .unwrap()
                .status,
            200
        );
        let big = QueryResult::Select(rs);
        let Err(e) = render_result_within(ResultFormat::Json, &big, 4096) else {
            panic!("an oversized answer was rendered");
        };
        assert_eq!(e.kind, OpErrorKind::ResultTooLarge);
        assert_eq!(e.rest().status, 503);
    }

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
            cell(&ReadSnapshot::capture(&db).unwrap()),
            "\"3.5\"^^<http://www.w3.org/2001/XMLSchema#decimal>"
        );
        db.set_integer_division(IntegerDivision::Truncate);
        assert_eq!(
            cell(&ReadSnapshot::capture(&db).unwrap()),
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
        fn cell(r: Result<QueryResult, SnapshotReadError>) -> String {
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
        let snap = ReadSnapshot::capture(&db).unwrap();
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
        let snap = ReadSnapshot::capture(&db).unwrap();
        let seeded = snap.csr.peek(0).expect("seeded from the database");
        assert!(Arc::ptr_eq(&seeded, &db.cached_csr().unwrap()));
        assert_eq!(cell(snap.query(SP, None)), SP_JSON);
        assert!(Arc::ptr_eq(&seeded, &snap.csr.peek(0).unwrap()));
    }

    // --- in-process HTTP client tests: upload, export download, console,
    // task registry (mirrors the pattern in tests/dt_http_parity.rs) --------

    use std::net::TcpStream;

    /// The temp roots the current test thread made, removed when the thread
    /// ends — also when the test fails (a thread-local's destructor runs
    /// after the unwinding).
    struct TmpRoots(std::cell::RefCell<Vec<PathBuf>>);

    impl Drop for TmpRoots {
        fn drop(&mut self) {
            for p in self.0.borrow().iter() {
                let _ = fs::remove_dir_all(p);
            }
        }
    }

    thread_local! {
        static TMP_ROOTS: TmpRoots = const { TmpRoots(std::cell::RefCell::new(Vec::new())) };
    }

    /// A unique temp directory for one server's databases + logs + user
    /// store, removed when the test ends (see [`TmpRoots`]).
    fn tmp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static CTR: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        p.push(format!("gstore_api_unit_{tag}_{}_{n}", std::process::id()));
        TMP_ROOTS.with(|r| r.0.borrow_mut().push(p.clone()));
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
    fn raw_request(
        addr: &SocketAddr,
        method: &str,
        target: &str,
        auth: Option<&str>,
        body: &str,
    ) -> String {
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
            out.push(if chunk.len() > 1 {
                A[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[(n & 63) as usize] as char
            } else {
                '='
            });
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
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=up",
            Some(&root),
            "",
        ));
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
        assert!(
            body.contains("http://ex/b"),
            "uploaded triple must be queryable: {body}"
        );

        // Re-uploading the same content inserts nothing new.
        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/upload?db_name=up&format=ntriples",
            Some(&root),
            ntriples,
        ));
        assert_eq!(status, 200);
        assert!(
            body.contains("\"triples_inserted\":0"),
            "dup upload: {body}"
        );
    }

    #[test]
    fn upload_creates_database_when_absent() {
        let (_srv, addr) = start("upload_new");
        let root = basic("root", "root");

        // No prior /build: the default format (turtle) and an unseen db_name
        // must still work, creating the database on the fly.
        let ttl = "@prefix : <http://ex/> .\n:a :p :b .\n";
        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/upload?db_name=fresh",
            Some(&root),
            ttl,
        ));
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
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=ex2",
            Some(&root),
            data,
        ));
        assert_eq!(status, 200);

        // Default: an attachment (file download) with a filename.
        let resp = raw_request(&addr, "GET", "/export?db_name=ex2", Some(&root), "");
        assert!(resp.contains("200 OK"));
        assert!(
            resp.contains("Content-Type: application/n-triples"),
            "{resp}"
        );
        assert!(
            resp.contains("Content-Disposition: attachment; filename=\"ex2.nt\""),
            "{resp}"
        );

        // `download=0` switches to an inline disposition.
        let inline = raw_request(
            &addr,
            "GET",
            "/export?db_name=ex2&download=0",
            Some(&root),
            "",
        );
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
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=tk",
            Some(&root),
            "",
        ));
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
        let (status, body) =
            split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
        assert_eq!(status, 200);
        assert!(body.contains("\"tasks\":[]"), "{body}");

        // Cancelling an unknown task id is 404.
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/cancel?id=999999",
            Some(&root),
            "",
        ));
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
        let (status, _) =
            split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&reader), ""));
        assert_eq!(status, 403, "query-only user must not see /tasklist");

        // /cancel needs Admin or Update; a Query-only user is forbidden.
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/cancel?id=1",
            Some(&reader),
            "",
        ));
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
        let (status, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/cancel?id=999999",
            Some(&writer),
            "",
        ));
        assert_eq!(
            status, 404,
            "update-privileged user must reach the handler, not be forbidden"
        );
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
            st.users
                .add("alice", "pw", BTreeSet::from([Privilege::Update]))
                .unwrap();
            st.users
                .add("bob", "pw", BTreeSet::from([Privilege::Update]))
                .unwrap();
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

        assert_eq!(
            cancel_as("bob", "pw"),
            403,
            "non-owner non-admin must be forbidden"
        );
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
    /// (`/cancel`, `/tasklist`, …) queues behind it. Checked on every path
    /// that takes credentials — Basic auth, `/login`, `/shutdown` and the
    /// `/api` protocol: the hash is parked (a test hook) while the test
    /// looks at the lock. review http#20: this used to sample the lock
    /// while clients hammered it ("held in under a quarter of the samples").
    #[test]
    fn bad_credentials_never_hash_under_the_state_lock() {
        use crate::http_users::test_hooks::park_verifications_of;
        let (server, addr) = start("pbkdf2_lock");
        type Send = fn(&SocketAddr, &str) -> String;
        let paths: [(&str, Send); 4] = [
            ("Basic auth", |addr, name| {
                raw_request(addr, "GET", "/tasklist", Some(&basic(name, "x")), "")
            }),
            ("/login", |addr, name| {
                raw_request(
                    addr,
                    "POST",
                    "/login",
                    None,
                    &format!("username={name}&password=x"),
                )
            }),
            ("/shutdown", |addr, name| {
                raw_request(
                    addr,
                    "POST",
                    "/shutdown",
                    None,
                    &format!("username={name}&password=x"),
                )
            }),
            ("/api", |addr, name| {
                raw_request(
                    addr,
                    "POST",
                    "/api",
                    None,
                    &format!(r#"{{"operation":"testConnect","username":"{name}","password":"x"}}"#),
                )
            }),
        ];
        for (i, (path, send)) in paths.into_iter().enumerate() {
            // An unknown user each (one failure apiece: no lockout) — but
            // `/shutdown` verifies only root's credentials (one wrong
            // password does not lock root out either).
            let name = if path == "/shutdown" {
                "root".to_string()
            } else {
                format!("hash_lock_probe_{i}")
            };
            let park = park_verifications_of(&name);
            let request = {
                let name = name.clone();
                std::thread::spawn(move || split_status_body(&send(&addr, &name)))
            };
            assert!(
                park.wait_parked(Duration::from_secs(60)),
                "{path}: the bad credentials were never verified"
            );
            assert!(
                server.inner.state.try_lock().is_ok(),
                "{path}: the state lock is held while bad credentials are hashed"
            );
            park.release();
            let (s, b) = request.join().unwrap();
            assert!(
                [200, 401, 403].contains(&s) && !b.contains("\"StatusCode\":0"),
                "{path}: refused: {s} {b}"
            );
        }
        // Authenticated requests are still served.
        let root = basic("root", "root");
        let (s, b) = split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
        assert_eq!(s, 200, "{b}");
        assert!(
            server.inner.state.try_lock().is_ok(),
            "the lock is free once the requests are done"
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
        begin_body[i..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect()
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
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=w",
            Some(&root),
            data,
        ));
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
            let guard = handle.write().unwrap();
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
        assert_eq!(
            probe("POST", format!("/upload{w}&format=ntriples"), nt).0,
            200
        );
        assert_eq!(probe("POST", format!("/checkpoint{w}"), "").0, 200);
        assert_eq!(
            probe("POST", format!("/backup{w}&path={backup}"), "").0,
            200
        );
        assert_eq!(probe("GET", format!("/export{w}"), "").0, 200);
        let (s, b) = probe("POST", format!("/begin{w}"), "");
        assert_eq!(s, 200, "{b}");
        let tid = tid_of(&b);
        let sel = "SELECT * WHERE { ?s ?p ?o }";
        assert_eq!(probe("POST", format!("/tquery?tid={tid}"), sel).0, 200);
        assert_eq!(probe("POST", format!("/commit?tid={tid}"), "").0, 200);
        let (_, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            &format!("/begin{w}"),
            Some(&root),
            "",
        ));
        let tid = tid_of(&b);
        assert_eq!(probe("POST", format!("/rollback?tid={tid}"), "").0, 200);
        assert_eq!(probe("POST", format!("/build{w}"), data).0, 200);
        assert_eq!(
            probe("POST", format!("/restore{w}&path={backup}"), "").0,
            200
        );
        assert_eq!(probe("POST", format!("/unload{w}"), "").0, 200);
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            &format!("/load{w}"),
            Some(&root),
            "",
        ));
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
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=s",
            Some(&root),
            data,
        ));
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
        assert!(
            status != 200 || durable,
            "acknowledged ({body}) but not saved"
        );
        assert_ne!(
            status, 200,
            "a write after the final save is refused: {body}"
        );
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
            let (s, b) = split_status_body(&raw_request(
                &addr,
                "POST",
                &format!("/build?db_name={db}"),
                Some(&root),
                data,
            ));
            assert_eq!(s, 200, "build: {b}");
        }
        // LOAD is refused by the server's restricted policy: the update fails
        // after its INSERT DATA was applied.
        let failing = |s: &str| {
            format!("INSERT DATA {{ <http://ex/{s}> <http://ex/p> <http://ex/d> }} ; LOAD <file:///nonexistent/x.ttl>")
        };
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/update?db_name=x",
            Some(&root),
            &failing("c"),
        ));
        assert_eq!(s, 400, "{b}");
        assert!(b.contains("were applied"), "the client is told: {b}");
        let on_disk = |db: &str, s: &str| {
            let mut d = Database::open(srv.root_dir().join(format!("{db}.db"))).unwrap();
            d.query(&format!(
                "ASK {{ <http://ex/{s}> <http://ex/p> <http://ex/d> }}"
            ))
            .unwrap()
                == QueryResult::Ask(true)
        };
        assert!(
            on_disk("x", "c"),
            "the applied part was saved (check_point)"
        );

        let (_, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/begin?db_name=y",
            Some(&root),
            "",
        ));
        let tid = tid_of(&b);
        let (s, _) = split_status_body(&raw_request(
            &addr,
            "POST",
            &format!("/tquery?tid={tid}"),
            Some(&root),
            &failing("t"),
        ));
        assert_eq!(s, 400);
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            &format!("/commit?tid={tid}"),
            Some(&root),
            "",
        ));
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

    /// `/begin` copies no triple of an in-memory database: the snapshot shares
    /// the live store until the transaction's first write (a read, and a write
    /// that changes nothing, keep it shared); that write copies it once, and
    /// the snapshot keeps answering the begin-time state.
    #[test]
    fn begin_shares_the_store_until_the_first_tquery_write() {
        let (server, addr) = start("tq_cow");
        build_two(&addr, "c");
        let root = basic("root", "root");
        let post = |target: &str, body: &str| -> (u16, String) {
            split_status_body(&raw_request(&addr, "POST", target, Some(&root), body))
        };
        let (s, body) = post("/begin?db_name=c", "");
        assert_eq!(s, 200, "{body}");
        let tid = tid_of(&body);
        let dbh = served(&server, "c");
        let shared = || {
            let slot = dbh.read().unwrap();
            let open = slot.txn.as_ref().expect("a transaction is open");
            match open.snapshot.store() {
                SnapStore::Mem(snap) => snap.shares_storage_with(slot.db.store()),
                SnapStore::Disk(_) => panic!("an in-memory database"),
            }
        };
        assert!(shared(), "/begin shares the store");
        let tq = |sparql: &str| post(&format!("/tquery?db_name=c&tid={tid}"), sparql);
        assert_eq!(tq("SELECT ?s WHERE { ?s ?p ?o }").0, 200);
        // Already present: changes nothing, copies nothing.
        assert_eq!(
            tq("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> }").0,
            200
        );
        assert!(shared(), "reads and no-op writes keep it shared");
        assert_eq!(
            tq("INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }").0,
            200
        );
        assert!(!shared(), "the first write copies the store");
        // Everyone else still reads the begin-time state.
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "GET",
            "/sparql?db_name=c",
            Some(&root),
            "SELECT ?s WHERE { ?s ?p ?o }",
        ));
        assert_eq!(s, 200);
        assert!(!b.contains("http://ex/x"), "uncommitted data leaked: {b}");
        assert_eq!(post(&format!("/commit?db_name=c&tid={tid}"), "").0, 200);
        let (_, b) = split_status_body(&raw_request(
            &addr,
            "GET",
            "/sparql?db_name=c",
            Some(&root),
            "SELECT ?s WHERE { ?s ?p ?o }",
        ));
        assert!(b.contains("http://ex/x"), "committed data: {b}");
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
            let (_, body) =
                split_status_body(&raw_request(&addr, "GET", "/tasklist", Some(&root), ""));
            if let Some(id) = first_task_id(&body) {
                assert!(
                    body.contains("\"db\":\"t\"") && body.contains("\"user\":\"root\""),
                    "{body}"
                );
                task = Some(id);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let task = task.expect("the running /tquery must be listed by /tasklist");
        let (s, body) = post(&format!("/cancel?id={task}"), "");
        assert_eq!(s, 200, "{body}");
        let (s, body) = query.join().unwrap();
        // review design#3: a cancelled statement is a refused request (503),
        // no longer a 400 query error.
        assert_eq!(s, 503, "a cancelled /tquery fails: {body}");
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
            split_status_body(&raw_request(
                &addr,
                "POST",
                "/build?db_name=b",
                Some(&root),
                ""
            ))
            .0,
            200
        );
        let mut nt = String::new();
        for i in 0..150_000 {
            nt.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p> \"value number {i}\" .\n"
            ));
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
                    let (s, b) =
                        split_status_body(&raw_request(&addr, "POST", target, Some(&root), &body));
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
        assert!(
            samples > 20,
            "the requests took too little time to sample: {samples}"
        );
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
        let (status, body) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=slow",
            Some(&root),
            &ttl,
        ));
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
        assert_eq!(
            cstatus, 200,
            "the concurrent cancel must be accepted: {cbody}"
        );

        // The query, whose cancel flag was flipped while it ran, returns the
        // cancellation error (HTTP 503 + "query cancelled"; review design#3:
        // it was a 400 query error).
        let (qstatus, qbody) = split_status_body(&query_thread.join().unwrap());
        assert_eq!(qstatus, 503, "a cancelled query returns 503: {qbody}");
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
        assert_eq!(
            req(
                "POST",
                "/build?db_name=p",
                "<http://ex/a> <http://ex/p> <http://ex/b> ."
            )
            .0,
            200
        );

        // Panics while holding BOTH the state lock and db `p`'s lock.
        let (status, body) = req("GET", "/__test/panic?db_name=p", "");
        assert_eq!(status, 500, "{body}");
        assert!(
            body.contains("\"StatusCode\":500") && body.contains("panicked"),
            "{body}"
        );
        assert!(
            server.inner.state.is_poisoned(),
            "the test route poisoned the state lock"
        );

        // Both poisoned locks are recovered: the server keeps answering.
        let (status, body) = req(
            "POST",
            "/sparql?db_name=p",
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("http://ex/b"), "{body}");
        assert_eq!(
            req(
                "POST",
                "/update?db_name=p",
                "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }"
            )
            .0,
            200
        );
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
        assert!(
            tasks.contains("\"tasks\":[]"),
            "the panicked task is unregistered: {tasks}"
        );
        let (status, _) = req("POST", "/sparql?db_name=q", "SELECT * WHERE { ?s ?p ?o }");
        assert_eq!(status, 200, "the poisoned db lock is recovered");
    }

    #[test]
    fn isolevel_parsing_maps_upstream_levels() {
        assert_eq!(TxnIsolation::parse("1"), Some(TxnIsolation::Serializable));
        assert_eq!(TxnIsolation::parse(" 2 "), Some(TxnIsolation::Snapshot));
        assert_eq!(TxnIsolation::parse("3"), Some(TxnIsolation::ReadCommitted));
        assert_eq!(
            TxnIsolation::parse("read_committed"),
            Some(TxnIsolation::ReadCommitted)
        );
        assert_eq!(
            TxnIsolation::parse("Read-Committed"),
            Some(TxnIsolation::ReadCommitted)
        );
        assert_eq!(
            TxnIsolation::parse("SNAPSHOT"),
            Some(TxnIsolation::Snapshot)
        );
        assert_eq!(
            TxnIsolation::parse("serializable"),
            Some(TxnIsolation::Serializable)
        );
        for bad in ["0", "4", "-1", "", "abc", "12"] {
            assert_eq!(TxnIsolation::parse(bad), None, "{bad:?}");
        }
        for level in [
            TxnIsolation::Serializable,
            TxnIsolation::Snapshot,
            TxnIsolation::ReadCommitted,
        ] {
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
        let (f, ts) = parse_batch_payload(
            None,
            "<http://a> <http://b> <http://c> .\n<http://a> <http://b> \"x\" .\n",
        )
        .unwrap();
        assert_eq!((f, ts.len()), ("ntriples", 2));
        // Several statements on one line fall back to Turtle (a superset).
        let (f, ts) = parse_batch_payload(
            None,
            "<http://a> <http://b> <http://c> . <http://d> <http://b> <http://c> .",
        )
        .unwrap();
        assert_eq!((f, ts.len()), ("turtle", 2));
        // @prefix / PREFIX select Turtle.
        let (f, ts) =
            parse_batch_payload(None, "@prefix : <http://ex/> .\n:a :p :b , :c .").unwrap();
        assert_eq!((f, ts.len()), ("turtle", 2));
        let (f, _) = parse_batch_payload(None, "PREFIX : <http://ex/>\n:a :p :b .").unwrap();
        assert_eq!(f, "turtle");
        // An explicit format wins; bare `json` (a response format) is ignored.
        let (f, _) =
            parse_batch_payload(Some("TTL"), "<http://a> <http://b> <http://c> .").unwrap();
        assert_eq!(f, "turtle");
        let (f, _) =
            parse_batch_payload(Some("json"), "<http://a> <http://b> <http://c> .").unwrap();
        assert_eq!(f, "ntriples");
        assert!(parse_batch_payload(Some("nt"), "@prefix : <http://ex/> .\n:a :p :b .").is_err());
        assert!(parse_batch_payload(Some("csv"), "a,b,c")
            .unwrap_err()
            .contains("unknown format"));
        // A SPARQL update is not RDF.
        assert!(
            parse_batch_payload(None, "INSERT DATA { <http://a> <http://b> <http://c> }").is_err()
        );
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
            let slot = h.read().unwrap();
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

        let sp =
            r#"SELECT (shortestPathLen(<http://ex/a>, <http://ex/c>, true, {}) AS ?l) WHERE {}"#;
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
        let (_, b) = api(&op(&format!(
            r#""operation":"query","sparql":{}"#,
            json_str(insert)
        )));
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
            pfn: crate::pfn::PfnLimits {
                fuel_per_call: 4242,
                ..crate::pfn::PfnLimits::default()
            },
        };
        let root = tmp_root("qsettings");
        let server = Arc::new(
            ApiServer::bind(&root, "root", "127.0.0.1:0")
                .expect("bind")
                .with_query_settings(custom),
        );
        assert_eq!(server.query_settings(), custom);
        // The PFN registry checks the same [pfn] limits.
        assert_eq!(server.pfn_registry().limits(), custom.pfn);
        let addr = server.local_addr().expect("addr");
        let s2 = Arc::clone(&server);
        std::thread::spawn(move || s2.serve_forever());
        let auth = basic("root", "root");
        let rest = |method: &str, target: &str, body: &str| {
            split_status_body(&raw_request(&addr, method, target, Some(&auth), body))
        };
        let api =
            |body: String| split_status_body(&raw_request(&addr, "POST", "/api", None, &body));
        let op = |op: &str, db: &str| {
            format!(
                r#"{{"username":"root","password":"root","operation":"{op}","db_name":"{db}"}}"#
            )
        };
        let settings_of = |name: &str| {
            let h = Arc::clone(locked(&server.inner.state).dbs.get(name).expect("loaded"));
            let slot = h.read().unwrap();
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
        let path = b
            .split("\"path\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
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
        let tid = b
            .split("\"TID\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
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

    /// `show` and `refreshconf` never wait for a busy database (a long
    /// query, CSR build or batch holds its lock): `show` reads the CSR state
    /// without that lock, and `refreshconf` replaces the configuration at
    /// once — the request handlers' copy, the `/api` limits and the
    /// `[query]` settings of every idle database — while a busy database
    /// takes the new `[query]` settings the next time it is used. Of two
    /// settings installed while it was busy, the later one wins.
    #[test]
    fn show_and_refreshconf_do_not_wait_for_a_busy_database() {
        use std::sync::mpsc;
        let root = tmp_root("busy_slot");
        fs::create_dir_all(&root).unwrap();
        let conf = root.join("conf.ini");
        let write_conf = |cache: usize, max_out: usize| {
            fs::write(
                &conf,
                format!(
                    "[server]\nmax_output_size={max_out}\nip_deny_path=off\n\
                     [query]\nquery_cache_size={cache}\n"
                ),
            )
            .unwrap();
        };
        write_conf(1000, 111);
        let server = Arc::new(
            ApiServer::bind(&root, "root", "127.0.0.1:0")
                .expect("bind")
                .with_config(GStoreConfig::load(&conf).unwrap()),
        );
        let addr = server.local_addr().expect("addr");
        let s2 = Arc::clone(&server);
        std::thread::spawn(move || s2.serve_forever());
        let auth = basic("root", "root");
        let rest = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        // An /api request answered on another thread, or `None` if it does
        // not answer within 10 s.
        let api_within = |op: &str| {
            let (tx, rx) = mpsc::channel();
            let body = format!(r#"{{"username":"root","password":"root","operation":"{op}"}}"#);
            std::thread::spawn(move || {
                let _ =
                    tx.send(split_status_body(&raw_request(&addr, "POST", "/api", None, &body)).1);
            });
            rx.recv_timeout(Duration::from_secs(10)).ok()
        };
        const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        assert_eq!(rest("/build?db_name=busy", DATA).0, 200);
        assert_eq!(rest("/build?db_name=idle", DATA).0, 200);
        // A graph-function query caches busy's CSR.
        let (s, b) = rest(
            "/sparql?db_name=busy",
            "SELECT (shortestPathLen(<http://ex/a>, <http://ex/b>, true, {}) AS ?l) WHERE {}",
        );
        assert_eq!(s, 200, "{b}");
        let handle =
            |name: &str| Arc::clone(locked(&server.inner.state).dbs.get(name).expect("loaded"));
        let cache_of = |h: &DbHandle| h.read().unwrap().db.query_settings().query_cache_bytes;
        let old = handle("idle").read().unwrap().db.query_settings();
        assert_eq!(old.query_cache_bytes, 1000);

        // busy's lock is held, as by a running statement.
        let busy = handle("busy");
        let guard = busy.write().unwrap();
        let shown = api_within("show").expect("show waited for the busy database");
        assert!(shown.contains("\"StatusCode\":0"), "{shown}");
        let listed = crate::json::parse(&shown).unwrap();
        let csr_of = |name: &str| {
            listed
                .get("ResponseBody")
                .and_then(crate::json::Json::as_array)
                .unwrap()
                .iter()
                .find(|d| d.get("database").and_then(crate::json::Json::as_str) == Some(name))
                .and_then(|d| d.get("csr"))
                .and_then(crate::json::Json::as_bool)
                .expect(name)
        };
        assert!(csr_of("busy"), "busy's CSR is built: {shown}");
        assert!(!csr_of("idle"), "{shown}");

        // refreshconf answers, with the new file in effect everywhere but on
        // the busy database's [query] settings.
        write_conf(2000, 222);
        let j = api_within("refreshconf").expect("refreshconf waited for the busy database");
        assert!(j.contains("refreshing configuration success"), "{j}");
        assert_eq!(server.inner.config().max_output_size, 222);
        assert_eq!(locked(&server.inner.state).gapi.max_output_size, 222);
        let installed = locked(&server.inner.state).query_settings;
        assert_eq!(installed.query_cache_bytes, 2000);
        assert_eq!(cache_of(&handle("idle")), 2000);
        assert_eq!(guard.db.query_settings(), old);
        // A second refresh while still busy: the later file wins.
        write_conf(3000, 333);
        let j = api_within("refreshconf").expect("refreshconf waited for the busy database");
        assert!(j.contains("refreshing configuration success"), "{j}");
        drop(guard);
        let (s, b) = rest("/sparql?db_name=busy", "ASK { ?s ?p ?o }");
        assert_eq!(s, 200, "{b}");
        assert_eq!(cache_of(&busy), 3000);
        assert_eq!(cache_of(&handle("idle")), 3000);
        server.shutdown();
    }

    /// A database an `/upload` creates gets the `[query]` settings in effect
    /// when it is registered, also when new ones were installed while the
    /// upload was parsed (after the request was dispatched).
    #[test]
    fn an_uploaded_database_gets_the_settings_installed_meanwhile() {
        let root = tmp_root("upload_settings");
        let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
            .expect("bind")
            .with_query_settings(QuerySettings::default());
        let body = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
        let raw = format!(
            "POST /upload?db_name=fresh&format=nt HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let req = Request::parse(&mut raw.as_bytes()).unwrap().unwrap();
        let dispatched = {
            let mut st = locked(&server.inner.state);
            let root = Some("root".to_string());
            server.inner.dispatch(&mut st, &req, root)
        };
        let Dispatch::Deferred(work) = dispatched else {
            panic!("an upload runs deferred")
        };
        let new = QuerySettings {
            query_cache_bytes: 0,
            gfun_max_output_bytes: 777,
            ..QuerySettings::default()
        };
        server.inner.apply_query_settings(new);
        let resp = work(&server.inner);
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let h = Arc::clone(
            locked(&server.inner.state)
                .dbs
                .get("fresh")
                .expect("created"),
        );
        assert_eq!(h.read().unwrap().db.query_settings(), new);
    }

    // --- R3: shared reads, re-entrancy, lock-free monitoring -------------

    /// A two-party latch with a timeout: [`arrive`](Self::arrive) returns
    /// `true` once two callers arrived, `false` if `timeout` passed first —
    /// so a query calling it proves another query ran *at the same time*.
    #[derive(Default)]
    struct Latch {
        n: Mutex<usize>,
        cv: std::sync::Condvar,
    }

    impl Latch {
        fn arrive(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut n = self.n.lock().unwrap();
            *n += 1;
            self.cv.notify_all();
            while *n < 2 {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                n = self.cv.wait_timeout(n, left).unwrap().0;
            }
            true
        }
    }

    /// A gate readers wait at (holding their database guard) until it opens.
    #[derive(Default)]
    struct Gate {
        state: Mutex<(usize, bool)>,
        cv: std::sync::Condvar,
    }

    impl Gate {
        /// Count in, wait until open (or 20 s), return 1.
        fn pass(&self) -> i64 {
            let mut st = self.state.lock().unwrap();
            st.0 += 1;
            self.cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(20);
            while !st.1 {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return 0;
                }
                st = self.cv.wait_timeout(st, left).unwrap().0;
            }
            1
        }

        fn wait_entered(&self, n: usize) {
            let mut st = self.state.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while st.0 < n {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "only {} of {n} readers arrived", st.0);
                st = self.cv.wait_timeout(st, left).unwrap().0;
            }
        }

        fn open(&self) {
            self.state.lock().unwrap().1 = true;
            self.cv.notify_all();
        }
    }

    /// The served handle of database `name`.
    fn served(server: &ApiServer, name: &str) -> DbHandle {
        Arc::clone(
            locked(&server.inner.state)
                .dbs
                .get(name)
                .expect("the database is loaded"),
        )
    }

    /// Build database `name` (two triples) through REST `/build`.
    fn build_two(addr: &SocketAddr, name: &str) {
        let auth = basic("root", "root");
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .";
        let (s, b) = split_status_body(&raw_request(
            addr,
            "POST",
            &format!("/build?db_name={name}"),
            Some(&auth),
            data,
        ));
        assert_eq!(s, 200, "build: {b}");
    }

    /// An `/api` request body for root.
    fn api_body(fields: &str) -> String {
        format!(r#"{{"username":"root","password":"root",{fields}}}"#)
    }

    /// Two reads of the same database through REST `/sparql`, and two through
    /// the `/api` `query` operation, each calling a function that waits for a
    /// second caller: both answer `1` only if the two ran at the same time
    /// (under one exclusive lock the first would time out with `0`).
    #[test]
    fn two_reads_of_one_database_run_at_once() {
        let (server, addr) = start("latch");
        build_two(&addr, "lat");
        let h = served(&server, "lat");
        let rest_latch = Arc::new(Latch::default());
        let api_latch = Arc::new(Latch::default());
        {
            let mut slot = h.write().unwrap();
            for (iri, latch) in [
                ("http://ex/latch-rest", Arc::clone(&rest_latch)),
                ("http://ex/latch-api", Arc::clone(&api_latch)),
            ] {
                slot.db.register_function(iri, move |_| {
                    Some(crate::query::Value::Int(i64::from(
                        latch.arrive(Duration::from_secs(15)),
                    )))
                });
            }
        }
        let auth = basic("root", "root");
        let rest: Vec<_> = (0..2)
            .map(|_| {
                let auth = auth.clone();
                std::thread::spawn(move || {
                    split_status_body(&raw_request(
                        &addr,
                        "POST",
                        "/sparql?db_name=lat",
                        Some(&auth),
                        "SELECT (<http://ex/latch-rest>(1) AS ?x) WHERE {}",
                    ))
                })
            })
            .collect();
        for t in rest {
            let (s, body) = t.join().unwrap();
            assert_eq!(s, 200, "{body}");
            assert!(body.contains("\"value\":\"1\""), "REST /sparql: {body}");
        }
        let api: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(move || {
                    let body = api_body(
                        r#""operation":"query","db_name":"lat","sparql":"SELECT (<http://ex/latch-api>(1) AS ?x) WHERE {}""#,
                    );
                    split_status_body(&raw_request(&addr, "POST", "/api", None, &body)).1
                })
            })
            .collect();
        for t in api {
            let body = t.join().unwrap();
            assert!(body.contains("\"StatusCode\":0"), "{body}");
            assert!(body.contains("\"value\":\"1\""), "/api query: {body}");
        }
        server.shutdown();
    }

    /// A database guard taken twice by one thread is an error (in every
    /// build), not a deadlock; other threads are not affected.
    #[test]
    fn a_reentrant_database_guard_is_refused() {
        let cell = DbSlot::handle(
            Database::build_from_str("re", "<http://ex/a> <http://ex/p> <http://ex/b> .").unwrap(),
            tmp_root("reentrant"),
            false,
            &LoadPolicy::restricted(),
            &QuerySettings::default(),
        );
        let shared = cell.read().unwrap();
        let e = cell
            .read()
            .err()
            .expect("a second shared guard on one thread");
        assert_eq!(e.kind, OpErrorKind::Failed);
        assert!(e.msg.contains("re"), "{}", e.msg);
        assert!(e.msg.contains("deadlock"), "{}", e.msg);
        assert!(
            cell.write().is_err(),
            "no upgrade while holding the shared guard"
        );
        assert!(cell.try_write().is_none());
        // Another thread shares it.
        let c2 = Arc::clone(&cell);
        let other = std::thread::spawn(move || c2.read().map(|s| s.db.triple_num()).unwrap());
        assert_eq!(other.join().unwrap(), 1);
        drop(shared);
        let excl = cell.write().unwrap();
        assert!(cell.read().is_err());
        assert!(cell.write().is_err());
        drop(excl);
        // Released: usable again, and the registry forgot both.
        assert_eq!(cell.read().unwrap().db.triple_num(), 1);
        assert!(cell.try_write().is_some());
    }

    /// `/status`, `/monitor`, `/show` and the `/api` `show`, `monitor` and
    /// `stat` answer within a second while a statement holds the database —
    /// shared, exclusive, and shared with a writer queued behind it (which
    /// makes new shared guards wait too).
    #[test]
    fn monitoring_answers_while_the_database_is_held() {
        let (server, addr) = start("monitor_held");
        build_two(&addr, "mon");
        let auth = basic("root", "root");
        // Computes and caches the class statistics for the current data.
        let (_, warm) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/api",
            None,
            &api_body(r#""operation":"monitor","db_name":"mon""#),
        ));
        assert!(warm.contains("\"tripleNum\":\"2\""), "{warm}");
        let h = served(&server, "mon");
        // review http#20: the test holds the database for the whole check,
        // so a request that waited for it would never be answered — the
        // 30 s read timeout is a hang guard, not the assertion (this used
        // to be "within a second").
        let check = |what: &str| {
            let guard = Duration::from_secs(30);
            for (method, target, body, expect) in [
                ("GET", "/status", String::new(), "\"total_triples\":2"),
                ("GET", "/monitor", String::new(), "\"triples\":2"),
                ("GET", "/show", String::new(), "\"triples\":2"),
                (
                    "POST",
                    "/api",
                    api_body(r#""operation":"monitor","db_name":"mon""#),
                    "\"tripleNum\":\"2\"",
                ),
                (
                    "POST",
                    "/api",
                    api_body(r#""operation":"show""#),
                    "\"database\":\"mon\"",
                ),
                (
                    "POST",
                    "/api",
                    api_body(r#""operation":"stat""#),
                    "\"StatusCode\":0",
                ),
            ] {
                let got = request_within(&addr, method, target, Some(&auth), &body, guard);
                let (s, b) =
                    got.unwrap_or_else(|| panic!("{method} {target} {body} waited ({what})"));
                assert_eq!(s, 200, "{target}: {b}");
                assert!(b.contains(expect), "{target} ({what}): {b}");
            }
        };
        {
            let _shared = h.read().unwrap();
            check("a reader holds it");
        }
        {
            let _excl = h.write().unwrap();
            check("a writer holds it");
        }
        {
            let shared = h.read().unwrap();
            let h2 = Arc::clone(&h);
            let writer = std::thread::spawn(move || {
                let _w = h2.write().unwrap();
            });
            std::thread::sleep(Duration::from_millis(100));
            check("a reader holds it and a writer waits");
            drop(shared);
            writer.join().unwrap();
        }
        server.shutdown();
    }

    /// `/api monitor` and `schema` right after a write — their statistics
    /// cached for the data before it — do not wait for the database either
    /// while a writer holds it, or waits for it behind a reader: they answer
    /// with the published figures and the earlier statistics, and with
    /// fresh ones once the database is free.
    #[test]
    fn monitoring_after_a_write_answers_while_the_database_is_held() {
        let (server, addr) = start("monitor_after_write");
        build_two(&addr, "monw");
        let auth = basic("root", "root");
        let monitor = api_body(r#""operation":"monitor","db_name":"monw""#);
        let (_, warm) = split_status_body(&raw_request(&addr, "POST", "/api", None, &monitor));
        assert!(warm.contains("\"subjectNum\":2"), "{warm}");
        let h = served(&server, "monw");
        h.write()
            .unwrap()
            .db
            .query("INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }")
            .unwrap();
        // (As above, review http#20: the database is held throughout, the
        // timeout is a hang guard.)
        let check = |what: &str| {
            let guard = Duration::from_secs(30);
            for (body, expect) in [
                (monitor.clone(), "\"tripleNum\":\"3\""),
                (
                    api_body(r#""operation":"schema","db_name":"monw""#),
                    "\"StatusCode\":0",
                ),
            ] {
                let got = request_within(&addr, "POST", "/api", Some(&auth), &body, guard);
                let (s, b) = got.unwrap_or_else(|| panic!("{body} waited ({what})"));
                assert_eq!(s, 200, "{b}");
                assert!(b.contains(expect), "{what}: {b}");
            }
        };
        {
            let _excl = h.write().unwrap();
            check("a writer holds it");
        }
        {
            let shared = h.read().unwrap();
            let h2 = Arc::clone(&h);
            let writer = std::thread::spawn(move || {
                let _w = h2.write().unwrap();
            });
            std::thread::sleep(Duration::from_millis(100));
            check("a reader holds it and a writer waits");
            drop(shared);
            writer.join().unwrap();
        }
        // Free: the statistics of the current data, once the background
        // refresh the requests above started has run.
        settled(&addr, &monitor, "\"subjectNum\":3");
        server.shutdown();
    }

    /// Ask `/api` with `body` until the answer contains `expect` (the
    /// statistics refreshed in the background settled); panics after 10 s.
    fn settled(addr: &SocketAddr, body: &str, expect: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, b) = split_status_body(&raw_request(addr, "POST", "/api", None, body));
            if b.contains(expect) {
                return b;
            }
            assert!(Instant::now() < deadline, "never settled on {expect}: {b}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `/api monitor` right after a data change, while a long query holds
    /// the database and a writer waits behind it, answers at once (within
    /// the 40 ms it waits for a background refresh) — with the published
    /// counts and the statistics of the earlier data, never computing them
    /// on the request path — and settles on the current data's statistics
    /// once the database is free (the refresh). Its `diskUsed` is the size
    /// measured at the end of the write (after its save), which is the
    /// directory's size.
    #[test]
    fn monitor_after_a_change_answers_at_once_and_settles() {
        let (server, addr) = start("monitor_settles");
        build_two(&addr, "mset");
        let auth = basic("root", "root");
        let monitor = api_body(r#""operation":"monitor","db_name":"mset""#);
        let (_, warm) = split_status_body(&raw_request(&addr, "POST", "/api", None, &monitor));
        assert!(warm.contains("\"subjectNum\":2"), "{warm}");
        assert!(warm.contains("\"layout\":\"snapshot\""), "{warm}");
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/update?db_name=mset",
            Some(&auth),
            "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        ));
        assert_eq!(s, 200, "{b}");
        let h = served(&server, "mset");
        let dir = locked(&server.inner.state).db_dir("mset");
        assert_eq!(h.disk_bytes(), dir_size(&dir), "measured after the save");
        // A long query holds the database; a writer queues behind it.
        let reader = h.read().unwrap();
        let h2 = Arc::clone(&h);
        let writer = std::thread::spawn(move || drop(h2.write().unwrap()));
        std::thread::sleep(Duration::from_millis(100));
        for _ in 0..5 {
            let t0 = Instant::now();
            let got = request_within(
                &addr,
                "POST",
                "/api",
                Some(&auth),
                &monitor,
                Duration::from_secs(1),
            );
            let took = t0.elapsed();
            let (s, b) = got.expect("monitor waited");
            assert_eq!(s, 200, "{b}");
            assert!(b.contains("\"tripleNum\":\"3\""), "published counts: {b}");
            // It waits 40 ms for the refresh at most (generous for CI).
            assert!(took < Duration::from_millis(250), "took {took:?}");
        }
        drop(reader);
        writer.join().unwrap();
        let b = settled(&addr, &monitor, "\"subjectNum\":3");
        assert!(b.contains("\"tripleNum\":\"3\""), "{b}");
        server.shutdown();
    }

    /// `/api monitor` computes its class statistics in a read scope: a read
    /// error fails it (status 1005) instead of answering — and caching, to
    /// be served until the next write — the statistics of part of the data.
    #[cfg(feature = "rocksdb")]
    #[test]
    fn monitor_statistics_hit_by_a_read_error_fail_and_are_not_cached() {
        let (server, addr) = start("monitor_read_error");
        let dir = locked(&server.inner.state).db_dir("rkm");
        let data = "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n\
                    <http://ex/a> rdf:type <http://ex/Person> .\n\
                    <http://ex/b> rdf:type <http://ex/Person> .\n\
                    <http://ex/u> rdf:type <http://ex/Univ> .\n\
                    <http://ex/a> <http://ex/worksAt> <http://ex/u> .\n";
        drop(Database::build_rocksdb_from_str("rkm", &dir, data).unwrap());
        let (_, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/api",
            None,
            &api_body(r#""operation":"load","db_name":"rkm""#),
        ));
        assert!(b.contains("\"StatusCode\":0"), "load: {b}");
        let h = served(&server, "rkm");
        let monitor = api_body(r#""operation":"monitor","db_name":"rkm""#);
        let fail = |on: bool| {
            h.read()
                .unwrap()
                .db
                .backend()
                .as_rocks()
                .unwrap()
                .debug_fail_reads(on);
        };
        fail(true);
        let (_, m) = split_status_body(&raw_request(&addr, "POST", "/api", None, &monitor));
        assert!(
            m.contains("\"StatusCode\":1005"),
            "statistics despite a read error: {m}"
        );
        fail(false);
        let (_, m) = split_status_body(&raw_request(&addr, "POST", "/api", None, &monitor));
        assert!(m.contains("\"StatusCode\":0"), "{m}");
        assert!(
            m.contains("http://ex/Person"),
            "partial statistics were cached: {m}"
        );
        assert!(m.contains("\"value\":2"), "{m}");
        server.shutdown();
    }

    /// A write section that unwinds (a handler panic, which the server
    /// survives) after changing the data does not leave monitoring on the
    /// figures from before the change: they are published once the database
    /// is free.
    #[test]
    fn a_write_section_that_unwinds_is_published_later() {
        let (server, addr) = start("unwind_publish");
        build_two(&addr, "unw");
        let monitor = api_body(r#""operation":"monitor","db_name":"unw""#);
        let (_, warm) = split_status_body(&raw_request(&addr, "POST", "/api", None, &monitor));
        assert!(warm.contains("\"tripleNum\":\"2\""), "{warm}");
        let h = served(&server, "unw");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut w = h.write().unwrap();
            w.db.query("INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }")
                .unwrap();
            panic!("an engine bug after the first operation");
        }));
        assert!(unwound.is_err());
        assert!(h.unpublished());
        // Busy: the last figures, still marked.
        {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let h2 = Arc::clone(&h);
            let reader = std::thread::spawn(move || {
                let _r = h2.read().unwrap();
                held_tx.send(()).unwrap();
                rx.recv().ok();
            });
            held_rx.recv().unwrap();
            assert_eq!(h.figures().triples, 2);
            assert!(h.unpublished());
            tx.send(()).unwrap();
            reader.join().unwrap();
        }
        // Free: published.
        assert_eq!(h.figures().triples, 3);
        assert!(!h.unpublished());
        for (target, expect) in [
            ("/status", "\"total_triples\":3"),
            ("/monitor", "\"triples\":3"),
        ] {
            let (s, b) = split_status_body(&raw_request(
                &addr,
                "GET",
                target,
                Some(&basic("root", "root")),
                "",
            ));
            assert_eq!(s, 200, "{b}");
            assert!(b.contains(expect), "{target}: {b}");
        }
        let (_, m) = split_status_body(&raw_request(&addr, "POST", "/api", None, &monitor));
        assert!(m.contains("\"tripleNum\":\"3\""), "{m}");
        // The statistics of the old data are answered at once, and refreshed
        // in the background: they settle on the current data's.
        settled(&addr, &monitor, "\"subjectNum\":3");
        server.shutdown();
    }

    /// A read that finds new `[query]` settings while other reads hold the
    /// database does not wait for them to sync the settings (which, the lock
    /// preferring writers, would also stall every later reader): it runs
    /// with the settings the database has, and a read that finds the
    /// database free syncs them.
    #[test]
    fn a_read_with_stale_settings_does_not_wait_for_other_reads() {
        let (server, addr) = start("settings_no_wait");
        build_two(&addr, "nw");
        let h = served(&server, "nw");
        let old = h.read().unwrap().db.query_settings();
        let new = QuerySettings {
            integer_division: IntegerDivision::Truncate,
            ..old
        };
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let h2 = Arc::clone(&h);
        let long_read = std::thread::spawn(move || {
            let _r = h2.read().unwrap();
            held_tx.send(()).unwrap();
            // Held until the read below is done. Bounded: a read that waits
            // for this one would find the settings synced once it gets the
            // database, and fail the test below instead of hanging it
            // (review http#20: that, not a one-second bound, is the
            // assertion).
            rx.recv_timeout(Duration::from_secs(60)).ok();
        });
        held_rx.recv().unwrap();
        server.inner.apply_query_settings(new);
        {
            let slot = server.inner.read_synced(&h).unwrap();
            assert_eq!(
                slot.db.query_settings(),
                old,
                "busy: not synced yet (a read that waited for the other read would have synced)"
            );
        }
        tx.send(()).unwrap();
        long_read.join().unwrap();
        assert_eq!(
            server.inner.read_synced(&h).unwrap().db.query_settings(),
            new
        );
        server.shutdown();
    }

    /// `/api backup` requests of one database in the same second (the same
    /// backup name) run one after the other: each answers success with a
    /// complete backup. They used to delete each other's files mid-write:
    /// a request arriving while another wrote removed the directory under it.
    #[test]
    fn same_second_api_backups_of_one_database_all_succeed() {
        let (server, addr) = start("backup_same_second");
        let auth = basic("root", "root");
        let mut data = String::new();
        for i in 0..30000 {
            data.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{i}> .\n",
                i % 7
            ));
        }
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=bkp",
            Some(&auth),
            &data,
        ));
        assert_eq!(s, 200, "build: {b}");
        let backups = server.inner.backup_dir();
        for round in 0..3 {
            // Early in a second, staggered: the later requests arrive while
            // the first one writes, all under one backup name.
            let into_second = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_millis();
            std::thread::sleep(Duration::from_millis(u64::from(
                1010 - into_second.min(1000),
            )));
            let requests: Vec<_> = (0..3u64)
                .map(|i| {
                    std::thread::sleep(Duration::from_millis(if i == 0 { 0 } else { 15 }));
                    std::thread::spawn(move || {
                        split_status_body(&raw_request(
                            &addr,
                            "POST",
                            "/api",
                            None,
                            &api_body(r#""operation":"backup","db_name":"bkp""#),
                        ))
                    })
                })
                .collect();
            for r in requests {
                let (s, b) = r.join().unwrap();
                assert_eq!(s, 200, "{b}");
                assert!(b.contains("\"StatusCode\":0"), "round {round}: {b}");
            }
        }
        let mut seen = 0;
        for e in std::fs::read_dir(&backups).unwrap() {
            let p = e.unwrap().path();
            if p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("bkp.db_")
            {
                assert_eq!(
                    Database::read_copy(&p).unwrap().triple_num(),
                    30000,
                    "{p:?}"
                );
                seen += 1;
            }
        }
        assert!(seen >= 1);
        server.shutdown();
    }

    /// Concurrent REST `/backup`s of one database into its default
    /// destination (`<db>.backup`) all succeed: they run one after the other
    /// (the per-destination claim the `/api` backup takes). Under the shared
    /// guard they checked and wrote the destination's owner record at the
    /// same time — a request saw another's half-written record (`409 …
    /// already holds data that is not a backup`) or lost the race to rename
    /// it (`500 … No such file or directory`).
    #[test]
    fn concurrent_rest_backups_of_one_database_all_succeed() {
        let (server, addr) = start("rest_backup_race");
        let auth = basic("root", "root");
        let mut data = String::new();
        for i in 0..20000 {
            data.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{i}> .\n",
                i % 7
            ));
        }
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=rbk",
            Some(&auth),
            &data,
        ));
        assert_eq!(s, 200, "build: {b}");
        let mut failures = Vec::new();
        for round in 0..6 {
            let requests: Vec<_> = (0..4)
                .map(|_| {
                    let auth = auth.clone();
                    std::thread::spawn(move || {
                        split_status_body(&raw_request(
                            &addr,
                            "POST",
                            "/backup?db_name=rbk",
                            Some(&auth),
                            "",
                        ))
                    })
                })
                .collect();
            for r in requests {
                let (s, b) = r.join().unwrap();
                if s != 200 {
                    failures.push(format!("round {round}: {s} {b}"));
                }
            }
            let copy = server.inner.backup_dir().join("rbk.backup");
            assert_eq!(Database::read_copy(&copy).unwrap().triple_num(), 20000);
        }
        assert!(failures.is_empty(), "{failures:#?}");
        server.shutdown();
    }

    /// Round-3 review: a REST `/backup` stopped part-way without its
    /// cleanup — a crash, or a panic, which the request answers with a
    /// `500` — left its directory holding data but no owner record, and
    /// every later default-path backup of the database was refused (`409
    /// "already holds data that is not a backup of …"`) until someone
    /// removed it by hand; one stopped while replacing an earlier backup
    /// left that one's owner record beside new rules, and `/restore` took
    /// it. The directory is now marked as a backup in progress first: a
    /// later backup of the database writes there again, and a restore
    /// refuses it until one completes.
    #[test]
    fn a_rest_backup_stopped_part_way_can_be_retried_and_is_never_restored() {
        let (server, addr) = start("rest_backup_stopped");
        let auth = basic("root", "root");
        let post =
            |target: &str| split_status_body(&raw_request(&addr, "POST", target, Some(&auth), ""));
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=pbk",
            Some(&auth),
            data,
        ));
        assert_eq!(s, 200, "build: {b}");
        let dir = server.inner.backup_dir().join("pbk.backup");
        // A backup into a new directory, stopped after it started writing
        // (a stray file stands in for what it copied).
        let stray = dir.join("LOCK");
        backup_test_hook::set(&dir, {
            let stray = stray.clone();
            Arc::new(move || {
                fs::write(&stray, b"").unwrap();
                panic!("stopped part-way (test)");
            })
        });
        let (s, _) = post("/backup?db_name=pbk");
        assert_eq!(s, 500, "the stopped backup is answered as failed");
        backup_test_hook::clear(&dir);
        assert!(stray.exists(), "the stopped backup left its files behind");
        // Not restorable, also for root.
        let (s, b) = post("/restore?db_name=pbk&path=pbk.backup");
        assert_eq!(s, 409, "an incomplete backup was restored: {b}");
        // The next backup of the database writes there again.
        let (s, b) = post("/backup?db_name=pbk");
        assert_eq!(
            s, 200,
            "the stopped backup's directory blocks the next one: {b}"
        );
        assert_eq!(backup_owner(&dir).as_deref(), Some("pbk"));
        assert!(!dir.join("BACKUP_OF.partial").exists());
        // Stopped again while replacing that complete backup: refused by
        // `/restore` until a backup into it completes.
        backup_test_hook::set(&dir, Arc::new(|| panic!("stopped part-way (test)")));
        let (s, _) = post("/backup?db_name=pbk");
        assert_eq!(s, 500);
        backup_test_hook::clear(&dir);
        let (s, b) = post("/restore?db_name=pbk&path=pbk.backup");
        assert_eq!(s, 409, "a partly replaced backup was restored: {b}");
        let (s, b) = post("/backup?db_name=pbk");
        assert_eq!(s, 200, "{b}");
        let (s, b) = post("/restore?db_name=pbk&path=pbk.backup");
        assert_eq!(s, 200, "{b}");
        // Another database's backup into the same directory stays refused.
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/build?db_name=other",
            Some(&auth),
            data,
        ));
        assert_eq!(s, 200, "build: {b}");
        let (s, _) = post("/backup?db_name=other&path=pbk.backup");
        assert_eq!(s, 409);
        server.shutdown();
    }

    /// A REST `/batchRemove` whose terms cannot be looked up (a damaged
    /// dictionary page of a Stream-mode database) fails: it used to read the
    /// error as "no such term" and answer `200 batch removed` with the
    /// triple counted as absent — while it stayed stored (C2).
    #[test]
    fn rest_batch_remove_fails_on_a_dictionary_read_error() {
        use crate::db::{DiskQueryMode, OpenOptions};
        let (server, addr) = start("batch_remove_read_error");
        let dir = locked(&server.inner.state).db_dir("brm");
        let nt = dir.with_extension("nt");
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(
            &nt,
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
        )
        .unwrap();
        Database::build_disk(&dir, &[&nt]).unwrap();
        // Overwrite the root page of the entity dictionary.
        let kv = dir.join("kvstore.kv");
        let page = crate::kvstore::Pager::open(&kv, 8)
            .unwrap()
            .root(crate::kvstore::format::ENTITY2ID);
        {
            use std::io::{Seek, SeekFrom};
            let mut f = fs::OpenOptions::new().write(true).open(&kv).unwrap();
            f.seek(SeekFrom::Start(page * crate::kvstore::PAGE_SIZE as u64))
                .unwrap();
            f.write_all(&[0xAB; crate::kvstore::PAGE_SIZE]).unwrap();
            f.sync_all().unwrap();
        }
        let stream = OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream);
        let db = Database::open_with(&dir, &stream).unwrap();
        assert_eq!(db.disk_query_mode(), Some(DiskQueryMode::Stream));
        {
            let mut st = locked(&server.inner.state);
            let slot = DbSlot::handle(db, dir.clone(), false, &st.load_policy, &st.query_settings);
            st.dbs.insert("brm".into(), slot);
        }
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/batchRemove?db_name=brm",
            Some(&basic("root", "root")),
            "<http://ex/a> <http://ex/p> <http://ex/b> .",
        ));
        assert_eq!(s, 500, "answered {s} {b} despite a read error");
        assert!(b.contains("batchRemove stopped"), "{b}");
        assert_eq!(served(&server, "brm").read().unwrap().db.triple_num(), 2);
        server.shutdown();
    }

    /// REST `/restore` and `/api restore` of a database the server has not
    /// loaded lock its directory before the backup is staged beside it and
    /// hold it through the swap: another process opening it meanwhile — whose
    /// open repairs the staged copy away — is refused. Both used to stage
    /// first and take the lock only for the swap.
    #[test]
    fn restores_of_an_unloaded_database_hold_it_while_staging() {
        use crate::db::persist::restore_probe;
        let (server, addr) = start("restore_lock_staging");
        build_two(&addr, "rsl");
        let auth = basic("root", "root");
        let rest =
            |target: &str| split_status_body(&raw_request(&addr, "POST", target, Some(&auth), ""));
        let (s, b) = rest("/backup?db_name=rsl");
        assert_eq!(s, 200, "backup: {b}");
        let dir = locked(&server.inner.state).db_dir("rsl");
        let (s, b) = rest("/unload?db_name=rsl");
        assert_eq!(s, 200, "unload: {b}");
        restore_probe::watch(&dir);
        let (s, b) = rest("/restore?db_name=rsl&path=rsl.backup");
        assert_eq!(s, 200, "REST restore: {b}");
        assert_eq!(
            restore_probe::lock_was_free(&dir),
            [false],
            "REST /restore: the directory was free while the backup was staged"
        );
        let (s, b) = rest("/unload?db_name=rsl");
        assert_eq!(s, 200, "unload: {b}");
        restore_probe::watch(&dir);
        let backup = server.inner.backup_dir().join("rsl.backup");
        let (_, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/api",
            None,
            &api_body(&format!(
                r#""operation":"restore","db_name":"rsl","backup_path":{}"#,
                json_str(&backup.to_string_lossy())
            )),
        ));
        assert!(b.contains("\"StatusCode\":0"), "/api restore: {b}");
        assert_eq!(
            restore_probe::lock_was_free(&dir),
            [false],
            "/api restore: the directory was free while the backup was staged"
        );
        server.shutdown();
    }

    /// New `[query]` settings installed while two reads hold a database are
    /// not waited for: the database takes them with its next request.
    #[test]
    fn query_settings_installed_while_reads_run_apply_next() {
        let (server, addr) = start("settings_readers");
        build_two(&addr, "set");
        let h = served(&server, "set");
        let gate = Arc::new(Gate::default());
        {
            let g = Arc::clone(&gate);
            h.write()
                .unwrap()
                .db
                .register_function("http://ex/gate", move |_| {
                    Some(crate::query::Value::Int(g.pass()))
                });
        }
        let auth = basic("root", "root");
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let auth = auth.clone();
                std::thread::spawn(move || {
                    split_status_body(&raw_request(
                        &addr,
                        "POST",
                        "/sparql?db_name=set",
                        Some(&auth),
                        "SELECT (<http://ex/gate>(1) AS ?x) WHERE {}",
                    ))
                })
            })
            .collect();
        gate.wait_entered(2);
        let old = h.read().unwrap().db.query_settings();
        let new = QuerySettings {
            integer_division: IntegerDivision::Truncate,
            ..old
        };
        // The readers are parked in the gate until it opens below: installing
        // settings that waited for them would only return once the gate's
        // hang guard let them go, and they would then be applied — so "not
        // yet" is the assertion (review http#20: it used to be a one-second
        // bound).
        server.inner.apply_query_settings(new);
        assert_eq!(h.read().unwrap().db.query_settings(), old, "not yet: busy");
        gate.open();
        for r in readers {
            let (s, b) = r.join().unwrap();
            assert_eq!(s, 200, "{b}");
            assert!(b.contains("\"value\":\"1\""), "{b}");
        }
        // The next read syncs them (dropping its shared guard to do so).
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/sparql?db_name=set",
            Some(&auth),
            "SELECT (7/2 AS ?x) WHERE {}",
        ));
        assert_eq!(s, 200, "{b}");
        assert!(b.contains("\"value\":\"3\""), "truncating division: {b}");
        assert_eq!(h.read().unwrap().db.query_settings(), new);
        server.shutdown();
    }

    /// A read error recorded while a transaction's snapshot evaluates fails
    /// the read (contract C2).
    #[test]
    fn a_read_error_fails_a_snapshot_read() {
        let mut db =
            Database::build_from_str("snap_err", "<http://ex/a> <http://ex/p> <http://ex/b> .")
                .unwrap();
        db.register_function("http://ex/fail", |_| {
            crate::store::read_scope::note_read_error("injected: page 7 checksum");
            Some(crate::query::Value::Int(1))
        });
        let snap = ReadSnapshot::capture(&db).unwrap();
        let e = snap
            .query("SELECT (<http://ex/fail>(1) AS ?x) WHERE {}", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("page 7 checksum"), "{e}");
        assert!(snap.query("ASK { ?s ?p ?o }", None).is_ok());
    }

    // ---- Round 4 review ---------------------------------------------------

    /// Start an `ApiServer` with the check_point policy `check_point`. Stop
    /// it with [`stop_cp`]: with check_point off, the shutdown saves the
    /// dirty databases on the serving thread, after [`ApiServer::shutdown`]
    /// returned — a test that ended then would remove its temp root first,
    /// and the save would create the database's directory again (a leak).
    fn start_cp(
        tag: &str,
        check_point: bool,
    ) -> (
        Arc<ApiServer>,
        SocketAddr,
        std::thread::JoinHandle<ShutdownReport>,
    ) {
        let root = tmp_root(tag);
        let server = Arc::new(
            ApiServer::bind(&root, "root", "127.0.0.1:0")
                .expect("bind")
                .with_check_point(check_point),
        );
        let addr = server.local_addr().expect("addr");
        let s2 = Arc::clone(&server);
        let serving = std::thread::spawn(move || s2.serve_forever());
        (server, addr, serving)
    }

    /// Shut down a server [`start_cp`] started and wait until it has saved
    /// its databases and stopped.
    fn stop_cp(server: &ApiServer, serving: std::thread::JoinHandle<ShutdownReport>) {
        server.shutdown();
        serving.join().expect("the serving thread");
    }

    /// A REST `/build` in the disk or RocksDB layout that fails (a body that
    /// does not parse) keeps every write the loaded database acknowledged
    /// but had not saved yet (check_point off): it is saved before it is
    /// retired, so the copy served again has them. They used to be lost.
    /// A failed build of a new name leaves no directory behind.
    #[test]
    fn a_failed_rest_bulk_build_keeps_the_acknowledged_writes() {
        let (server, addr, serving) = start_cp("bulk_build_keeps", false);
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let ask = "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }";
        assert_eq!(
            post(
                "/build?db_name=x",
                "<http://ex/a> <http://ex/p> <http://ex/b> ."
            )
            .0,
            200
        );
        let (s, b) = post(
            "/update?db_name=x",
            "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        );
        assert_eq!(s, 200, "{b}");
        assert!(post("/sparql?db_name=x", ask).1.contains("true"));
        let (s, b) = post("/build?db_name=x&layout=disk", "this is not rdf");
        assert_eq!(s, 400, "{b}");
        let (s, b) = post("/sparql?db_name=x", ask);
        assert_eq!(s, 200, "{b}");
        assert!(b.contains("true"), "the acknowledged insert survived: {b}");

        let (s, b) = post("/build?db_name=fresh&layout=disk", "this is not rdf");
        assert_eq!(s, 400, "{b}");
        assert!(
            !server.root_dir().join("fresh.db").exists(),
            "a failed build of a new name leaves no directory"
        );
        stop_cp(&server, serving);
    }

    /// A REST `/build` from a request body stores its blank-node labels as
    /// written in every layout (the disk layout used to rewrite `_:alice`
    /// to `_:<scope>_alice`, the snapshot layout keeping it).
    #[test]
    fn rest_builds_from_a_body_keep_blank_node_labels_in_every_layout() {
        let (server, addr) = start("bulk_build_bnodes");
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        for layout in ["snapshot", "disk"] {
            let (s, b) = post(
                &format!("/build?db_name=b_{layout}&layout={layout}"),
                "_:alice <http://ex/p> \"x\" .",
            );
            assert_eq!(s, 200, "{layout}: {b}");
            let (s, b) = post(
                &format!("/sparql?db_name=b_{layout}"),
                "SELECT ?s WHERE { ?s ?p ?o }",
            );
            assert_eq!(s, 200, "{layout}: {b}");
            assert!(b.contains("\"value\":\"alice\""), "{layout}: {b}");
        }
        server.shutdown();
    }

    /// A REST bulk rebuild whose install committed but could not finish its
    /// cleanup (here: the old rules cannot be removed) is reported as failed
    /// and not served — it used to answer `200 built` and serve whatever
    /// the leftovers made of it.
    #[test]
    fn a_rest_bulk_build_with_an_unfinished_install_is_not_served() {
        let (server, addr) = start("bulk_build_warnings");
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        assert_eq!(post("/build?db_name=w&layout=disk", data).0, 200);
        // A file where the rule directory belongs cannot be removed as one.
        fs::write(
            server.root_dir().join("w.db").join(RULE_DIR),
            "not a directory",
        )
        .unwrap();
        let (s, b) = post("/build?db_name=w&layout=disk", data);
        assert_eq!(s, 500, "{b}");
        assert!(b.contains("install did not finish"), "{b}");
        assert!(
            !locked(&server.inner.state).dbs.contains_key("w"),
            "not served"
        );
        server.shutdown();
    }

    /// A snapshot-layout REST rebuild that fails after the loaded database
    /// was retired (here: the old rules cannot be removed) keeps every write
    /// the loaded copy acknowledged but had not saved (check_point off): it
    /// is saved before it is retired and served again from its directory.
    /// The writes used to be lost, and the database left unloaded.
    #[test]
    fn a_failed_snapshot_rebuild_keeps_the_acknowledged_writes() {
        let (server, addr, serving) = start_cp("snap_build_keeps", false);
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        let ask = "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }";
        assert_eq!(post("/build?db_name=x&layout=snapshot", data).0, 200);
        let (s, b) = post(
            "/update?db_name=x",
            "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        );
        assert_eq!(s, 200, "{b}");
        // review http#10: the rebuild's save fails (a directory stands where
        // its staging file goes) — the fault used to be a rule directory
        // that cannot be removed, which no longer precedes the save: the
        // replaced rules go only once the new database is saved.
        let blocker = server.root_dir().join("x.db").join("dict.bin.tmp");
        fs::create_dir_all(blocker.join("blocker")).unwrap();
        let (s, b) = post("/build?db_name=x&layout=snapshot", data);
        assert_eq!(s, 500, "{b}");
        let (s, b) = post("/sparql?db_name=x", ask);
        assert_eq!(s, 200, "served again: {b}");
        assert!(b.contains("true"), "the acknowledged insert survived: {b}");
        fs::remove_dir_all(&blocker).unwrap();
        // A rebuild saved over a rule directory it cannot remove (a file
        // there) is in place, but not served — as a bulk build whose
        // install did not finish — and the old rules are reported.
        fs::write(
            server.root_dir().join("x.db").join(RULE_DIR),
            "not a directory",
        )
        .unwrap();
        let (s, b) = post("/build?db_name=x&layout=snapshot", data);
        assert_eq!(s, 500, "{b}");
        assert!(b.contains(RULE_DIR), "{b}");
        let (s, b) = post("/sparql?db_name=x", ask);
        assert_eq!(s, 404, "not served: {b}");
        stop_cp(&server, serving);
    }

    /// A REST bulk build does not hold the rule directory's lock while it
    /// loads: a `reasonManage` check of the database answers at once (it
    /// used to wait for the whole build — every such request a worker
    /// thread held for the build's duration).
    #[test]
    fn a_rest_bulk_build_holds_no_rule_lock_while_it_loads() {
        let (server, addr) = start("bulk_build_rule_lock");
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        assert_eq!(post("/build?db_name=big&layout=disk", data).0, 200);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        build_hooks::set(&server.root_dir().join("big.db"), move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        });
        let builder = {
            let auth = auth.clone();
            std::thread::spawn(move || {
                split_status_body(&raw_request(
                    &addr,
                    "POST",
                    "/build?db_name=big&layout=disk",
                    Some(&auth),
                    data,
                ))
            })
        };
        entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the build started");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let body = api_body(
                r#""operation":"reasonManage","db_name":"big","type":"8","rulename":"r1""#,
            );
            let _ = tx.send(split_status_body(&raw_request(
                &addr, "POST", "/api", None, &body,
            )));
        });
        let answered = rx.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).unwrap();
        let (s, b) = builder.join().unwrap();
        assert_eq!(s, 200, "{b}");
        let (_, b) = answered.expect("reasonManage answered while the build loads");
        assert!(b.contains("not exist"), "{b}");
        server.shutdown();
    }

    /// A failed REST bulk build of a new name removes the directory it made
    /// only under the directory's lock: one another process created and
    /// locked meanwhile (a `gbuild` of the same name) is left alone — it
    /// used to be deleted under that process.
    #[test]
    fn a_failed_rest_bulk_build_leaves_a_directory_another_process_locked() {
        let (server, addr) = start("bulk_build_foreign_lock");
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let dir = server.root_dir().join("race.db");
        let held = Arc::new(Mutex::new(None::<fs::File>));
        {
            let (dir, held) = (dir.clone(), Arc::clone(&held));
            build_hooks::set(&dir.clone(), move || {
                // The other process: its own descriptor of `LOCK`, locked.
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("other-process-work"), "x").unwrap();
                let f = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(dir.join(crate::db::dirlock::LOCK_FILE))
                    .unwrap();
                f.try_lock().unwrap();
                *held.lock().unwrap() = Some(f);
            });
        }
        let (s, b) = post(
            "/build?db_name=race&layout=disk",
            "<http://ex/a> <http://ex/p> <http://ex/b> .",
        );
        // review design#3: a lock another process holds is a conflict (409)
        // on every endpoint, no longer a 400.
        assert_eq!(s, 409, "{b}");
        assert!(b.contains("locked by another process"), "{b}");
        assert!(
            dir.join("other-process-work").is_file(),
            "the other process's directory is left alone"
        );
        drop(held.lock().unwrap().take());
        server.shutdown();
    }

    /// A REST bulk build whose install did not finish (not served) still
    /// does the registry's bookkeeping: a name dropped outside the server
    /// loses its old grants — the new data must not inherit them — and a
    /// new name is registered. Neither happened: the old grants applied to
    /// the new data once it was loaded.
    #[test]
    fn an_unfinished_rest_bulk_build_still_updates_the_registry() {
        let (server, addr) = start("bulk_build_registry");
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let data = "<http://ex/a> <http://ex/p> <http://ex/b> .";
        assert_eq!(post("/build?db_name=x&layout=disk", data).0, 200);
        {
            let mut st = locked(&server.inner.state);
            st.users.add("bob", "bob-pw", BTreeSet::new()).unwrap();
            st.users
                .grant_on("bob", "x", &BTreeSet::from([Privilege::Query]))
                .unwrap();
        }
        assert_eq!(post("/unload?db_name=x", "").0, 200);
        // Dropped outside the server (`gdrop`): still registered.
        fs::remove_dir_all(server.root_dir().join("x.db")).unwrap();
        for name in ["x", "fresh"] {
            let dir = server.root_dir().join(format!("{name}.db"));
            // The install cannot remove the rule directory: a file there.
            build_hooks::set(&dir.clone(), move || {
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join(RULE_DIR), "not a directory").unwrap();
            });
            let (s, b) = post(&format!("/build?db_name={name}&layout=disk"), data);
            assert_eq!(s, 500, "{name}: {b}");
            assert!(b.contains("install did not finish"), "{name}: {b}");
        }
        let st = locked(&server.inner.state);
        assert!(
            !st.users.get("bob").unwrap().has_on(Privilege::Query, "x"),
            "the dropped database's grant is gone"
        );
        assert!(st.sysdb.contains("fresh"), "the new name is registered");
        assert!(!st.dbs.contains_key("x") && !st.dbs.contains_key("fresh"));
        drop(st);
        server.shutdown();
    }

    /// `/api monitor` while a transaction is open reports the committed
    /// state: none of the transaction's classes, subjects or terms (they
    /// used to show — to any client allowed to monitor — and a rollback
    /// then took back classes that had never existed). (Its own database
    /// name: the statistics cache is keyed by name, process-wide.)
    #[test]
    fn monitoring_shows_no_uncommitted_writes() {
        const TYPE: &str = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";
        let (server, addr) = start("monitor_txn");
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let monitor = || {
            post(
                "/api",
                &api_body(r#""operation":"monitor","db_name":"montxn""#),
            )
            .1
        };
        assert_eq!(
            post(
                "/build?db_name=montxn",
                &format!("<http://ex/a> {TYPE} <http://ex/C> .")
            )
            .0,
            200
        );
        let before = monitor();
        assert!(before.contains("\"entityNum\":2"), "{before}");
        let (_, b) = post("/begin?db_name=montxn", "");
        let tid = tid_of(&b);
        let (s, b) = post(
            &format!("/tquery?tid={tid}"),
            &format!("INSERT DATA {{ <http://ex/b> {TYPE} <http://ex/D> . <http://ex/c> {TYPE} <http://ex/D> }}"),
        );
        assert_eq!(s, 200, "{b}");
        for _ in 0..3 {
            let during = monitor();
            assert!(during.contains("\"entityNum\":2"), "{during}");
            assert!(during.contains("\"subjectNum\":1"), "{during}");
            assert!(!during.contains("http://ex/D"), "{during}");
            std::thread::sleep(Duration::from_millis(50));
        }
        let (_, schema) = post(
            "/api",
            &api_body(r#""operation":"schema","db_name":"montxn""#),
        );
        assert!(!schema.contains("http://ex/D"), "{schema}");
        assert_eq!(post(&format!("/commit?tid={tid}"), "").0, 200);
        let after = monitor();
        assert!(after.contains("\"entityNum\":5"), "{after}");
        assert!(after.contains("http://ex/D"), "{after}");
        server.shutdown();
    }

    /// The commit of a transaction that frees a term of an in-memory
    /// database lets the begin-time snapshot go first: the dictionary's
    /// interners are no longer shared, so the free copies none of them (it
    /// used to copy the whole entity interner, then drop the snapshot).
    #[test]
    fn a_commit_that_frees_terms_copies_no_interner() {
        let (server, addr, serving) = start_cp("commit_no_copy", false);
        let auth = basic("root", "root");
        let post = |target: &str, body: &str| {
            split_status_body(&raw_request(&addr, "POST", target, Some(&auth), body))
        };
        let data = "<http://ex/a> <http://ex/p> <http://ex/x> .\n<http://ex/b> <http://ex/p> <http://ex/c> .";
        assert_eq!(post("/build?db_name=m", data).0, 200);
        let h = served(&server, "m");
        let (_, b) = post("/begin?db_name=m", "");
        let tid = tid_of(&b);
        let (s, b) = post(
            &format!("/tquery?tid={tid}"),
            "DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/x> }",
        );
        assert_eq!(s, 200, "{b}");
        let before = h.read().unwrap().db.dict().interner_addrs();
        let entities = h.read().unwrap().db.stats().entity_num;
        assert_eq!(post(&format!("/commit?tid={tid}"), "").0, 200);
        let slot = h.read().unwrap();
        assert_eq!(slot.db.stats().entity_num, entities - 2, "a and x freed");
        assert_eq!(
            slot.db.dict().interner_addrs()[0],
            before[0],
            "the entity interner was not copied"
        );
        drop(slot);
        stop_cp(&server, serving);
    }

    /// An exclusive section that changed nothing does not walk the
    /// database's directory (it used to, at the end of every one, with the
    /// guard held); one that changes the data measures it again.
    #[test]
    fn only_a_section_that_changes_the_data_measures_the_directory() {
        let (server, addr) = start("disk_measure");
        build_two(&addr, "dm");
        let h = served(&server, "dm");
        let dir = locked(&server.inner.state).db_dir("dm");
        let measured = h.disk_bytes();
        // Grow the directory behind the server's back.
        fs::write(dir.join("extra.bin"), vec![0u8; 4096]).unwrap();
        drop(h.write().unwrap());
        assert_eq!(h.disk_bytes(), measured, "nothing changed: not measured");
        let (s, b) = split_status_body(&raw_request(
            &addr,
            "POST",
            "/update?db_name=dm",
            Some(&basic("root", "root")),
            "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }",
        ));
        assert_eq!(s, 200, "{b}");
        assert_eq!(h.disk_bytes(), dir_size(&dir), "measured after the change");
        assert!(h.disk_bytes() >= measured + 4096);
        server.shutdown();
    }

    /// A graph-function read on an on-disk snapshot whose CSR build hits a
    /// read error fails — and caches nothing, so the next one fails too (it
    /// used to cache the partial graph and answer the next read from it).
    #[test]
    fn a_disk_snapshot_caches_no_csr_built_from_a_failed_read() {
        use crate::db::{DiskQueryMode, OpenOptions};
        let root = tmp_root("snap_csr_error");
        fs::create_dir_all(&root).unwrap();
        let nt = root.join("d.nt");
        fs::write(
            &nt,
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
        )
        .unwrap();
        let dir = root.join("d.db");
        Database::build_disk(&dir, &[&nt]).unwrap();
        let kv = dir.join("kvstore.kv");
        let pager = crate::kvstore::Pager::open(&kv, 8).unwrap();
        let pages = [
            pager.root(crate::kvstore::format::SPO),
            pager.root(crate::kvstore::format::POS),
        ];
        drop(pager);
        {
            use std::io::{Seek, SeekFrom};
            let mut f = fs::OpenOptions::new().write(true).open(&kv).unwrap();
            for page in pages {
                f.seek(SeekFrom::Start(page * crate::kvstore::PAGE_SIZE as u64))
                    .unwrap();
                f.write_all(&[0xAB; crate::kvstore::PAGE_SIZE]).unwrap();
            }
            f.sync_all().unwrap();
        }
        let stream = OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream);
        let db = Database::open_with(&dir, &stream).unwrap();
        let snap = ReadSnapshot::capture(&db).unwrap();
        assert!(matches!(snap.store, SnapStore::Disk(_)));
        let q = "SELECT (shortestPathLen(<http://ex/a>, <http://ex/c>, true, {}) AS ?l) WHERE {}";
        for attempt in 0..2 {
            let r = snap.query(q, None);
            assert!(r.is_err(), "attempt {attempt}: {r:?}");
            assert!(snap.csr.cached_generation().is_none(), "nothing cached");
        }
    }

    #[test]
    fn invalid_ground_triples_are_not_applied() {
        let t = |s: Term, p: Term| Triple::new(s, p, Term::iri("http://o"));
        assert!(valid_ground_triple(&t(
            Term::iri("http://s"),
            Term::iri("http://p")
        )));
        assert!(valid_ground_triple(&t(
            Term::Blank("b".into()),
            Term::iri("http://p")
        )));
        assert!(!valid_ground_triple(&t(
            Term::plain_literal("lit"),
            Term::iri("http://p")
        )));
        assert!(!valid_ground_triple(&t(
            Term::iri("http://s"),
            Term::Blank("b".into())
        )));
    }
}
