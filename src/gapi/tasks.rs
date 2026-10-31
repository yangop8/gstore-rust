//! The background task manager: asynchronous operations (`async=true`,
//! `opt_id`, `checkOperationState`, `callback`) and the registry of running
//! SPARQL statements, shared by the upstream `/api` layer and the REST
//! endpoints of [`crate::http_api`] (`/tasklist`, `/cancel`).
//!
//! # Tasks
//!
//! Every task has an **`opt_id`**: upstream's snowflake-style id
//! (`src/Util/IdUtil.cpp`) — milliseconds since 2024-03-12 00:00 UTC
//! ([`OPT_ID_EPOCH_MS`]) shifted left 22 bits, worker id `1` in bits 12-21
//! and a per-millisecond sequence in bits 0-11 — sent to clients as a
//! decimal string. Ids are unique and increasing per server (also across a
//! clock step backwards). They are *not* secrets: every operation on a task
//! checks that the caller owns it (or is an administrator).
//!
//! There are two kinds:
//!
//! * **synchronous** tasks — a SPARQL statement running on an HTTP worker
//!   ([`TaskManager::register`] / [`TaskManager::finish`], wrapped by the
//!   server's `register_task` and `TaskGuard`). They are listed while they
//!   run, can be cancelled through their cancellation flag, and are kept in a
//!   short history afterwards ([`TaskConfig::max_history`]);
//! * **asynchronous** tasks — an operation submitted with `async=true`
//!   ([`TaskManager::submit`]). The request is answered at once with the
//!   `opt_id`; the operation runs on the manager's own runner threads (at
//!   most [`TaskConfig::workers`], never the HTTP worker pool), its final
//!   JSON envelope is kept ([`TaskConfig::max_finished`]) for
//!   `checkOperationState`, and — given a `callback` URL — POSTed there.
//!
//! A running asynchronous operation that runs SPARQL (an `async` `query`)
//! does not get a second task: [`TaskManager::register`] called on a runner
//! thread attaches to the current task ([`current`]), so the statement is
//! listed and cancelled under the operation's `opt_id`.
//!
//! # States
//!
//! | [`TaskState`] | `checkOperationState` `state` | `operationTaskList` `status` |
//! |---------------|-------------------------------|------------------------------|
//! | `Running`     | `0` (executing)               | `0` (handling)               |
//! | `Succeeded`   | `1` (success)                 | `1` (finished)               |
//! | `Failed`      | `-1` (failed)                 | `1` (finished)               |
//! | `Cancelled`   | `-1` (failed)                 | `-1` (cancelled)             |
//!
//! (upstream `ApiLogOperation.cpp` `checkOperationState` and
//! `src/Database/TaskManager.cpp`). A task is `Cancelled` when it ended
//! unsuccessfully after a cancellation was requested — cooperative
//! cancellation, as upstream: the flag ([`TaskHandle::is_cancelled`],
//! threaded into [`crate::Database::query_cancellable`]) is checked by the
//! running work, which then stops with an error.
//!
//! # Retention
//!
//! Finished tasks are kept for [`TaskConfig::retention`], at most
//! [`TaskConfig::max_finished`] asynchronous and [`TaskConfig::max_history`]
//! synchronous ones (oldest first out). An asynchronous `query` result — or
//! any result over [`TaskConfig::spill_threshold`] bytes — is written to
//! `<export dir>/<opt_id>.json` (upstream's location) instead of being held
//! in memory; the file is removed when the task is dropped from retention.
//!
//! # Shutdown
//!
//! [`TaskManager::close`] refuses new asynchronous operations,
//! [`TaskManager::cancel_all`] raises every running task's cancellation flag
//! and [`TaskManager::wait_async_idle`] waits (bounded) for the runners — the
//! graceful shutdown of [`crate::http_api::ApiServer`] does all three after
//! its in-flight requests had their grace period.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::{Ctx, OpSpec};
use crate::http_client::{self, Limits, PostOptions};
use crate::json::Json;
use crate::server::{locked, Request};

/// Upstream's `opt_id` epoch (`IdUtil::kEpoch`): 2024-03-12 00:00:00.000 UTC,
/// in milliseconds since the Unix epoch.
pub const OPT_ID_EPOCH_MS: u64 = 1_710_172_800_000;
const SEQUENCE_BITS: u32 = 12;
const WORKER_ID_BITS: u32 = 10;
const SEQUENCE_MASK: u64 = (1 << SEQUENCE_BITS) - 1;
/// Upstream's fixed worker id (`IdUtil::worker_id_`).
const WORKER_ID: u64 = 1;

/// The `StatusMsg` an `async=true` request is answered with (upstream:
/// `"Operation Success."`), next to the `opt_id`.
pub const ACCEPTED_MSG: &str = "Operation Success.";

/// The `StatusMsg` of an asynchronous operation that is still running and
/// has reported no progress (upstream logs it as `"Operation success"`).
pub const RUNNING_MSG: &str = "Operation success";

/// Longest SPARQL text kept with a task (for the task lists).
const MAX_SPARQL_CHARS: usize = 4096;

/// The milliseconds-since-epoch time encoded in an `opt_id` (upstream
/// `IdUtil::getConvertTimeById`, before its date formatting).
pub fn opt_id_time_ms(opt_id: u64) -> u64 {
    (opt_id >> (SEQUENCE_BITS + WORKER_ID_BITS)) + OPT_ID_EPOCH_MS
}

/// Parse an `opt_id` parameter (a decimal string or number).
pub fn parse_opt_id(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// Settings of the [`TaskManager`] (install with
/// [`ApiServer::with_task_config`](crate::http_api::ApiServer::with_task_config)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskConfig {
    /// Runner threads for asynchronous operations (at least 1). Further
    /// submissions wait in a queue (and are listed as running).
    pub workers: usize,
    /// The most asynchronous operations that may wait for a runner; beyond
    /// it a submission is refused (`1005`).
    pub max_pending: usize,
    /// Finished asynchronous operations kept for `checkOperationState`.
    pub max_finished: usize,
    /// Finished synchronous SPARQL tasks kept for `operationTaskList`
    /// (`status=1` / `-1`).
    pub max_history: usize,
    /// How long a finished task is kept at most.
    pub retention: Duration,
    /// The time bound of one callback delivery (connect, send, receive).
    pub callback_timeout: Duration,
    /// Further delivery attempts after a failed one (transport error or HTTP
    /// 5xx). `0` (the default) = one attempt, as upstream
    /// (`HttpUtil::Post`, no retry).
    pub callback_retries: u32,
    /// For `https://` callbacks (feature `tls`): the CA bundle to trust
    /// (`None` = the system bundle).
    pub callback_ca_file: Option<PathBuf>,
    /// How long `operationTaskCancel` waits for the task to stop before it
    /// answers (upstream waits until it stops).
    pub cancel_wait: Duration,
    /// How long a graceful shutdown waits for running asynchronous
    /// operations after cancelling them.
    pub shutdown_wait: Duration,
    /// A finished asynchronous result larger than this (serialized, bytes)
    /// is written to the export directory instead of kept in memory
    /// (`query` results always are, like upstream).
    pub spill_threshold: usize,
}

impl Default for TaskConfig {
    fn default() -> TaskConfig {
        TaskConfig {
            workers: 4,
            max_pending: 256,
            max_finished: 1000,
            max_history: 200,
            retention: Duration::from_secs(24 * 3600),
            callback_timeout: Duration::from_secs(10),
            callback_retries: 0,
            callback_ca_file: None,
            cancel_wait: Duration::from_secs(3),
            shutdown_wait: Duration::from_secs(5),
            spill_threshold: 256 * 1024,
        }
    }
}

/// The state of a task (see the module docs' table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    /// `checkOperationState`'s `state`: `0` running, `1` success, `-1`
    /// failed (a cancelled operation failed).
    pub fn state_code(self) -> i64 {
        match self {
            TaskState::Running => 0,
            TaskState::Succeeded => 1,
            TaskState::Failed | TaskState::Cancelled => -1,
        }
    }

    /// `operationTaskList`'s `status`: `0` handling, `1` finished, `-1`
    /// cancelled.
    pub fn list_status(self) -> i64 {
        match self {
            TaskState::Running => 0,
            TaskState::Succeeded | TaskState::Failed => 1,
            TaskState::Cancelled => -1,
        }
    }

    /// Whether the task has ended.
    pub fn is_finished(self) -> bool {
        self != TaskState::Running
    }

    /// A lowercase name (`running`, `succeeded`, `failed`, `cancelled`).
    pub fn name(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::Succeeded => "succeeded",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }
}

/// A snapshot of one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    pub opt_id: u64,
    /// The operation (`query`, `build`, …).
    pub operation: String,
    /// The target database (empty when none).
    pub db_name: String,
    /// The user who started it.
    pub user: String,
    /// The SPARQL text (truncated), if any.
    pub sparql: String,
    /// Submitted with `async=true`.
    pub is_async: bool,
    pub state: TaskState,
    /// Whether a cancellation was requested.
    pub cancel_requested: bool,
    /// Start time, milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// End time, milliseconds since the Unix epoch (`0` while running).
    pub end_ms: u64,
    /// Run time so far (or in total, once finished).
    pub elapsed: Duration,
    /// The latest status message: the progress of a running asynchronous
    /// operation, its final `StatusMsg` once finished.
    pub status_msg: String,
    /// The final `StatusCode` of a finished asynchronous operation.
    pub status_code: Option<i64>,
    /// Upstream's `success_num` (the result's `success_num`, else its
    /// `AnsNum`; or the reported progress).
    pub success_num: u64,
    /// Upstream's `failed_num`.
    pub failed_num: u64,
    /// A file the operation produced (the result's `backupfilepath` /
    /// `filepath` / `FileName`).
    pub filepath: Option<String>,
    /// Where the result was written (asynchronous results over the spill
    /// threshold, and every asynchronous `query` result).
    pub result_file: Option<PathBuf>,
    /// The callback URL, if one was given.
    pub callback: Option<String>,
    /// `pending`, `delivered (HTTP <code>)` or `failed: <reason>`.
    pub callback_status: Option<String>,
}

/// A finished asynchronous operation's result envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskResult {
    /// Kept in memory.
    Json(Json),
    /// Written to this file (`<export dir>/<opt_id>.json`).
    File(PathBuf),
}

impl TaskResult {
    /// The envelope (read back from its file when spilled).
    pub fn load(&self) -> std::io::Result<Json> {
        match self {
            TaskResult::Json(j) => Ok(j.clone()),
            TaskResult::File(path) => {
                let text = fs::read_to_string(path)?;
                crate::json::parse(&text)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            }
        }
    }
}

/// What an asynchronous submission describes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskSpec {
    pub operation: String,
    pub db_name: String,
    pub user: String,
    pub sparql: String,
    /// POST the final result here (`http://…`; `https://…` with `tls`).
    pub callback: Option<String>,
    /// Where large (and `query`) results are written; `None` keeps every
    /// result in memory.
    pub result_dir: Option<PathBuf>,
}

/// The outcome of [`TaskManager::request_cancel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// No such task (or it left retention).
    NotFound,
    /// The flag is raised now.
    Requested,
    /// A cancellation was already requested; the task is still stopping.
    AlreadyRequested,
    /// The task has already ended in this state.
    Finished(TaskState),
}

/// The work of an asynchronous operation: it gets the task's handle
/// (cancellation flag, progress) and returns the final JSON envelope
/// (`{"StatusCode":…,"StatusMsg":…, …}`).
pub type TaskJob = Box<dyn FnOnce(&TaskHandle) -> Json + Send + 'static>;

/// The running task's side of the manager: its id, cancellation flag and
/// progress reporting.
#[derive(Clone)]
pub struct TaskHandle {
    id: u64,
    cancel: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl TaskHandle {
    /// The task's `opt_id`.
    pub fn opt_id(&self) -> u64 {
        self.id
    }

    /// Whether a cancellation was requested (stop soon, with an error).
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The live cancellation flag (for [`crate::Database::query_cancellable`]).
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Report progress: `checkOperationState` answers a running operation
    /// with this `StatusMsg` and `success_num` (upstream: a running `build`
    /// reports its progress text and triple count).
    pub fn set_progress(&self, msg: &str, success_num: u64) {
        let mut st = self.shared.lock();
        if let Some(r) = st.tasks.get_mut(&self.id) {
            if r.info.state == TaskState::Running {
                r.info.status_msg = msg.to_string();
                r.info.success_num = success_num;
            }
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<TaskHandle>> = const { RefCell::new(None) };
}

/// The asynchronous task running on this thread, if any — how an operation
/// running asynchronously reaches its cancellation flag and reports progress.
pub fn current() -> Option<TaskHandle> {
    CURRENT.with(|c| c.borrow().clone())
}

/// The background task manager (see the module docs).
pub struct TaskManager {
    shared: Arc<Shared>,
}

struct Shared {
    st: Mutex<Inner>,
    /// Signalled whenever a task finishes or a runner exits.
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        locked(&self.st)
    }
}

struct Inner {
    config: TaskConfig,
    tasks: BTreeMap<u64, Record>,
    /// Finished asynchronous / synchronous task ids, oldest first.
    finished_async: VecDeque<u64>,
    finished_sync: VecDeque<u64>,
    queue: VecDeque<Job>,
    /// Live runner threads.
    workers: usize,
    /// Asynchronous operations taken off the queue and not yet done
    /// (callback delivery included).
    active: usize,
    closed: bool,
    last_ms: u64,
    seq: u64,
}

struct Record {
    info: TaskInfo,
    cancel: Arc<AtomicBool>,
    started: Instant,
    finished_at: Option<Instant>,
    result: Option<TaskResult>,
}

struct Job {
    id: u64,
    operation: String,
    cancel: Arc<AtomicBool>,
    callback: Option<String>,
    result_dir: Option<PathBuf>,
    run: TaskJob,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

impl Inner {
    /// The next `opt_id` (see the module docs): strictly increasing, also
    /// when the clock steps back or a millisecond's 4096 sequence numbers
    /// run out (the timestamp part then runs ahead of the clock).
    fn next_id(&mut self) -> u64 {
        let now = now_ms().max(OPT_ID_EPOCH_MS);
        if now > self.last_ms {
            self.last_ms = now;
            self.seq = 0;
        } else {
            self.seq += 1;
            if self.seq > SEQUENCE_MASK {
                self.last_ms += 1;
                self.seq = 0;
            }
        }
        ((self.last_ms - OPT_ID_EPOCH_MS) << (SEQUENCE_BITS + WORKER_ID_BITS))
            | (WORKER_ID << SEQUENCE_BITS)
            | self.seq
    }

    fn insert(&mut self, spec: &TaskSpec, is_async: bool) -> (u64, Arc<AtomicBool>) {
        let id = self.next_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let callback = spec.callback.clone().filter(|c| !c.trim().is_empty());
        self.tasks.insert(
            id,
            Record {
                info: TaskInfo {
                    opt_id: id,
                    operation: spec.operation.clone(),
                    db_name: spec.db_name.clone(),
                    user: spec.user.clone(),
                    sparql: clip(&spec.sparql, MAX_SPARQL_CHARS),
                    is_async,
                    state: TaskState::Running,
                    cancel_requested: false,
                    start_ms: now_ms(),
                    end_ms: 0,
                    elapsed: Duration::ZERO,
                    status_msg: if is_async {
                        RUNNING_MSG.to_string()
                    } else {
                        String::new()
                    },
                    status_code: None,
                    success_num: 0,
                    failed_num: 0,
                    filepath: None,
                    result_file: None,
                    callback_status: callback.as_ref().map(|_| "pending".to_string()),
                    callback,
                },
                cancel: Arc::clone(&cancel),
                started: Instant::now(),
                finished_at: None,
                result: None,
            },
        );
        (id, cancel)
    }

    /// Mark a running task finished.
    fn end(&mut self, id: u64, state: TaskState) {
        let Some(r) = self.tasks.get_mut(&id) else {
            return;
        };
        if r.info.state.is_finished() {
            return;
        }
        r.info.state = state;
        r.info.end_ms = now_ms().max(r.info.start_ms);
        r.finished_at = Some(Instant::now());
        if r.info.is_async {
            self.finished_async.push_back(id);
        } else {
            self.finished_sync.push_back(id);
        }
    }

    /// Drop finished tasks beyond the retention limits; returns the result
    /// files to delete (outside the lock).
    fn prune(&mut self) -> Vec<PathBuf> {
        let now = Instant::now();
        let retention = self.config.retention;
        let mut files = Vec::new();
        for is_async in [true, false] {
            let max = if is_async {
                self.config.max_finished
            } else {
                self.config.max_history
            };
            loop {
                let queue = if is_async {
                    &mut self.finished_async
                } else {
                    &mut self.finished_sync
                };
                let Some(&id) = queue.front() else { break };
                let expired = self.tasks.get(&id).is_none_or(|r| {
                    r.finished_at
                        .is_some_and(|t| now.duration_since(t) > retention)
                });
                if queue.len() <= max && !expired {
                    break;
                }
                queue.pop_front();
                if let Some(Record {
                    result: Some(TaskResult::File(path)),
                    ..
                }) = self.tasks.remove(&id)
                {
                    files.push(path);
                }
            }
        }
        files
    }

    fn snapshot(r: &Record) -> TaskInfo {
        let mut info = r.info.clone();
        info.cancel_requested = r.cancel.load(Ordering::Relaxed);
        info.elapsed = match r.finished_at {
            Some(t) => t.duration_since(r.started),
            None => r.started.elapsed(),
        };
        info
    }
}

fn remove_files(files: Vec<PathBuf>) {
    for f in files {
        let _ = fs::remove_file(f);
    }
}

impl Default for TaskManager {
    fn default() -> TaskManager {
        TaskManager::new(TaskConfig::default())
    }
}

impl TaskManager {
    /// A manager with `config`.
    pub fn new(config: TaskConfig) -> TaskManager {
        TaskManager {
            shared: Arc::new(Shared {
                st: Mutex::new(Inner {
                    config,
                    tasks: BTreeMap::new(),
                    finished_async: VecDeque::new(),
                    finished_sync: VecDeque::new(),
                    queue: VecDeque::new(),
                    workers: 0,
                    active: 0,
                    closed: false,
                    last_ms: 0,
                    seq: 0,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// The settings in effect.
    pub fn config(&self) -> TaskConfig {
        self.shared.lock().config.clone()
    }

    /// Replace the settings (new limits apply from the next task on).
    pub fn set_config(&self, config: TaskConfig) {
        let files = {
            let mut st = self.shared.lock();
            st.config = config;
            st.prune()
        };
        remove_files(files);
    }

    /// Register a synchronous task (a SPARQL statement about to run on the
    /// calling thread) and return its id and cancellation flag; end it with
    /// [`finish`](Self::finish). On a runner thread of this manager the
    /// statement belongs to the asynchronous operation running there: no new
    /// task is created and that operation's id and flag are returned.
    pub fn register(
        &self,
        operation: &str,
        db_name: &str,
        sparql: &str,
        user: &str,
    ) -> (u64, Arc<AtomicBool>) {
        if let Some(h) = current().filter(|h| Arc::ptr_eq(&h.shared, &self.shared)) {
            let mut st = self.shared.lock();
            if let Some(r) = st.tasks.get_mut(&h.id) {
                if r.info.sparql.is_empty() {
                    r.info.sparql = clip(sparql, MAX_SPARQL_CHARS);
                }
                if r.info.db_name.is_empty() {
                    r.info.db_name = db_name.to_string();
                }
            }
            return (h.id, h.cancel_flag());
        }
        let spec = TaskSpec {
            operation: operation.to_string(),
            db_name: db_name.to_string(),
            user: user.to_string(),
            sparql: sparql.to_string(),
            ..TaskSpec::default()
        };
        let mut st = self.shared.lock();
        let files = st.prune();
        let out = st.insert(&spec, false);
        drop(st);
        remove_files(files);
        out
    }

    /// End synchronous task `id` (a no-op for an asynchronous one, which its
    /// runner ends): `Some(true)` it succeeded, `Some(false)` it failed —
    /// `Cancelled` when a cancellation had been requested — and `None` it
    /// did not finish normally (a panic): `Failed`.
    pub fn finish(&self, id: u64, ok: Option<bool>) {
        let files = {
            let mut st = self.shared.lock();
            let Some(r) = st.tasks.get(&id) else {
                return;
            };
            if r.info.is_async {
                return;
            }
            let state = match ok {
                Some(true) => TaskState::Succeeded,
                Some(false) if r.cancel.load(Ordering::Relaxed) => TaskState::Cancelled,
                _ => TaskState::Failed,
            };
            st.end(id, state);
            st.prune()
        };
        self.shared.changed.notify_all();
        remove_files(files);
    }

    /// Submit an asynchronous operation: it is queued for a runner thread
    /// and its `opt_id` returned at once. Refused (with the message for a
    /// `1005` answer) once [`close`](Self::close)d, or when
    /// [`TaskConfig::max_pending`] operations already wait.
    pub fn submit(&self, spec: TaskSpec, run: TaskJob) -> Result<u64, String> {
        let (id, spawn, files) = {
            let mut st = self.shared.lock();
            if st.closed {
                return Err(
                    "the server is shutting down: no new asynchronous operation is accepted"
                        .to_string(),
                );
            }
            if st.queue.len() >= st.config.max_pending {
                return Err(format!(
                    "too many asynchronous operations are waiting (at most {}); retry later",
                    st.config.max_pending
                ));
            }
            let files = st.prune();
            let (id, cancel) = st.insert(&spec, true);
            st.queue.push_back(Job {
                id,
                operation: spec.operation.clone(),
                cancel,
                callback: spec.callback.clone().filter(|c| !c.trim().is_empty()),
                result_dir: spec.result_dir.clone(),
                run,
            });
            let spawn = st.workers < st.config.workers.max(1);
            if spawn {
                st.workers += 1;
            }
            (id, spawn, files)
        };
        remove_files(files);
        if spawn {
            let shared = Arc::clone(&self.shared);
            let started = std::thread::Builder::new()
                .name("gstore-task".to_string())
                .spawn(move || runner(shared));
            if let Err(e) = started {
                let mut st = self.shared.lock();
                st.workers -= 1;
                if st.workers == 0 {
                    // Nobody would ever run it: take it back.
                    st.queue.retain(|j| j.id != id);
                    st.tasks.remove(&id);
                    return Err(format!("cannot start a task runner thread: {e}"));
                }
            }
        }
        Ok(id)
    }

    /// A snapshot of task `id`.
    pub fn get(&self, id: u64) -> Option<TaskInfo> {
        self.shared.lock().tasks.get(&id).map(Inner::snapshot)
    }

    /// The result envelope of finished asynchronous task `id`.
    pub fn result(&self, id: u64) -> Option<TaskResult> {
        self.shared.lock().tasks.get(&id)?.result.clone()
    }

    /// Every retained task (running and finished), by `opt_id`.
    pub fn list(&self) -> Vec<TaskInfo> {
        let files;
        let out = {
            let mut st = self.shared.lock();
            files = st.prune();
            st.tasks.values().map(Inner::snapshot).collect()
        };
        remove_files(files);
        out
    }

    /// The running tasks, by `opt_id`.
    pub fn running(&self) -> Vec<TaskInfo> {
        self.shared
            .lock()
            .tasks
            .values()
            .filter(|r| r.info.state == TaskState::Running)
            .map(Inner::snapshot)
            .collect()
    }

    /// The cancellation flag of running task `id`.
    pub fn cancel_flag(&self, id: u64) -> Option<Arc<AtomicBool>> {
        let st = self.shared.lock();
        let r = st.tasks.get(&id)?;
        (r.info.state == TaskState::Running).then(|| Arc::clone(&r.cancel))
    }

    /// Ask task `id` to stop (raise its cancellation flag). The caller checks
    /// ownership first.
    pub fn request_cancel(&self, id: u64) -> CancelOutcome {
        let st = self.shared.lock();
        let Some(r) = st.tasks.get(&id) else {
            return CancelOutcome::NotFound;
        };
        if r.info.state.is_finished() {
            return CancelOutcome::Finished(r.info.state);
        }
        if r.cancel.swap(true, Ordering::Relaxed) {
            CancelOutcome::AlreadyRequested
        } else {
            CancelOutcome::Requested
        }
    }

    /// Wait up to `timeout` for task `id` to end. Returns its state then
    /// (`Running` when it is still running), `None` if there is no such task.
    pub fn wait_finished(&self, id: u64, timeout: Duration) -> Option<TaskState> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            let state = st.tasks.get(&id)?.info.state;
            let now = Instant::now();
            if state.is_finished() || now >= deadline {
                return Some(state);
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Raise the cancellation flag of every running (and queued) task;
    /// returns how many.
    pub fn cancel_all(&self) -> usize {
        let st = self.shared.lock();
        let mut n = 0;
        for r in st.tasks.values() {
            if r.info.state == TaskState::Running {
                r.cancel.store(true, Ordering::Relaxed);
                n += 1;
            }
        }
        n
    }

    /// Refuse new asynchronous operations from now on (a shutdown).
    pub fn close(&self) {
        self.shared.lock().closed = true;
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }

    /// Whether no asynchronous operation is queued or running.
    pub fn async_idle(&self) -> bool {
        let st = self.shared.lock();
        st.queue.is_empty() && st.active == 0
    }

    /// Wait up to `timeout` until no asynchronous operation is queued or
    /// running (callback deliveries included); returns whether that is so.
    pub fn wait_async_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if st.queue.is_empty() && st.active == 0 {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A runner thread: run queued operations until the queue is empty.
fn runner(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut st = shared.lock();
            match st.queue.pop_front() {
                Some(job) => {
                    st.active += 1;
                    job
                }
                None => {
                    st.workers -= 1;
                    drop(st);
                    shared.changed.notify_all();
                    return;
                }
            }
        };
        run_job(&shared, job);
        shared.lock().active -= 1;
        shared.changed.notify_all();
    }
}

/// An error envelope (`msg` empty: the code's default message).
fn error_envelope(status: Status, msg: &str) -> Json {
    let msg = if msg.is_empty() {
        status.default_msg()
    } else {
        msg
    };
    Json::obj()
        .with("StatusCode", status.code())
        .with("StatusMsg", msg)
}

/// The envelope of an operation cancelled before it ran (upstream's
/// cancellation message).
fn cancelled_envelope(id: u64) -> Json {
    error_envelope(
        Status::OperationFailed,
        &format!("operation cancel opt_id:{id}"),
    )
}

/// A number field of an envelope (a JSON number or numeric string).
fn num_field(env: &Json, key: &str) -> Option<u64> {
    let v = env.get(key)?;
    v.as_u64()
        .or_else(|| v.as_i64().map(|n| n.max(0) as u64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Run one operation, record its outcome, deliver its callback.
fn run_job(shared: &Arc<Shared>, job: Job) {
    let handle = TaskHandle {
        id: job.id,
        cancel: Arc::clone(&job.cancel),
        shared: Arc::clone(shared),
    };
    let envelope = if handle.is_cancelled() {
        cancelled_envelope(job.id)
    } else {
        let prev = CURRENT.with(|c| c.replace(Some(handle.clone())));
        let out = catch_unwind(AssertUnwindSafe(|| (job.run)(&handle)));
        CURRENT.with(|c| *c.borrow_mut() = prev);
        match out {
            Ok(env @ Json::Obj(_)) => env,
            Ok(other) => Json::obj().with("result", other),
            Err(_) => error_envelope(
                Status::OperationFailed,
                "internal server error: the operation panicked",
            ),
        }
    };
    let mut envelope = envelope;
    if envelope.get("StatusCode").is_none() {
        envelope.set("StatusCode", 0);
    }
    if envelope.get("StatusMsg").is_none() {
        envelope.set("StatusMsg", "success");
    }
    envelope.set("opt_id", job.id.to_string());
    let envelope = envelope.into_sorted();
    let code = envelope
        .get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or(Status::OperationFailed.code());
    let msg = envelope
        .get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let success_num = num_field(&envelope, "success_num")
        .or_else(|| num_field(&envelope, "AnsNum"))
        .unwrap_or(0);
    let failed_num = num_field(&envelope, "failed_num").unwrap_or(0);
    let filepath = ["backupfilepath", "filepath", "FileName"]
        .iter()
        .find_map(|k| envelope.get(k).and_then(Json::as_str))
        .map(str::to_string);
    let text = envelope.to_string();
    let spill_threshold = shared.lock().config.spill_threshold;
    let result = match &job.result_dir {
        Some(dir) if job.operation == "query" || text.len() > spill_threshold => {
            let path = dir.join(format!("{}.json", job.id));
            match fs::create_dir_all(dir).and_then(|()| fs::write(&path, &text)) {
                Ok(()) => TaskResult::File(path),
                Err(_) => TaskResult::Json(envelope),
            }
        }
        _ => TaskResult::Json(envelope),
    };
    let config = {
        let mut st = shared.lock();
        let cancelled = job.cancel.load(Ordering::Relaxed);
        if let Some(r) = st.tasks.get_mut(&job.id) {
            r.info.status_code = Some(code);
            r.info.status_msg = msg;
            r.info.success_num = success_num;
            r.info.failed_num = failed_num;
            r.info.filepath = filepath;
            if let TaskResult::File(p) = &result {
                r.info.result_file = Some(p.clone());
            }
            r.result = Some(result);
        }
        let state = if code == 0 {
            TaskState::Succeeded
        } else if cancelled {
            TaskState::Cancelled
        } else {
            TaskState::Failed
        };
        st.end(job.id, state);
        let files = st.prune();
        let config = st.config.clone();
        drop(st);
        remove_files(files);
        config
    };
    shared.changed.notify_all();
    if let Some(url) = job.callback {
        let outcome = deliver_callback(&url, text.as_bytes(), &config);
        if let Some(r) = shared.lock().tasks.get_mut(&job.id) {
            r.info.callback_status = Some(outcome);
        }
    }
}

/// POST a finished operation's result to its callback URL (upstream
/// `HttpUtil::Post` with `Content-Type: application/json`); returns the
/// outcome for [`TaskInfo::callback_status`]. Bounded by
/// [`TaskConfig::callback_timeout`] per attempt; a transport error or an
/// HTTP 5xx is retried [`TaskConfig::callback_retries`] times.
fn deliver_callback(url: &str, body: &[u8], config: &TaskConfig) -> String {
    let opts = PostOptions {
        limits: Limits {
            total: config.callback_timeout,
            max_response: 1 << 20,
        },
        ca_file: config.callback_ca_file.clone(),
    };
    let mut last = String::new();
    for attempt in 0..=config.callback_retries {
        if attempt > 0 {
            let backoff = Duration::from_millis(500u64.saturating_mul(1 << (attempt - 1).min(4)));
            std::thread::sleep(backoff.min(Duration::from_secs(5)));
        }
        match http_client::post_with(url, "application/json", body, &opts) {
            Ok(reply) if reply.status < 500 => {
                return format!("delivered (HTTP {})", reply.status)
            }
            Ok(reply) => last = format!("HTTP {}", reply.status),
            Err(e) => last = e.to_string(),
        }
    }
    format!("failed: {last}")
}

/// Check a `callback` parameter: `http://…` (or `https://…` with the `tls`
/// feature). The error is the message for a `1003` answer.
pub fn check_callback_url(url: &str) -> Result<(), String> {
    http_client::parse_url(url)
        .map(|_| ())
        .map_err(|e| format!("the callback address is invalid: {e}"))
}

// ---- the dispatcher hook ---------------------------------------------------

/// Run operation `spec` asynchronously: the dispatcher's `async=true` path
/// for an [`OpSpec::async_capable`] operation, after authentication and the
/// privilege check. The handler runs on a runner thread with a copy of the
/// request context (its body already parsed into `params`; `async` reads
/// `false` there, so the handler simply does its work) and its envelope
/// becomes the task's result. Answers `{"StatusCode":0,"StatusMsg":"Operation
/// Success.","opt_id":"…"}` at once.
pub(crate) fn run_async(ctx: &mut Ctx<'_>, spec: &OpSpec, params: &Params) -> GResp {
    let callback = params.non_empty("callback");
    if let Some(url) = &callback {
        if let Err(msg) = check_callback_url(url) {
            return GResp::error(Status::ParamIsIllegal, msg);
        }
    }
    let Some(weak) = ctx.inner.weak_self() else {
        return GResp::error(
            Status::OperationFailed,
            "asynchronous operations are unavailable on this server instance",
        );
    };
    let req = Request {
        method: ctx.req.method.clone(),
        path: ctx.req.path.clone(),
        query: ctx.req.query.clone(),
        headers: ctx.req.headers.clone(),
        // Already parsed into `params`; not copied a second time.
        body: Vec::new(),
    };
    let mut params = params.clone();
    params.set("async", false);
    let handler = spec.handler;
    let (remote_ip, op, user, db_name, inner_request, config) = (
        ctx.remote_ip.clone(),
        ctx.op.clone(),
        ctx.user.clone(),
        ctx.db_name.clone(),
        ctx.inner_request,
        ctx.config.clone(),
    );
    let task_spec = TaskSpec {
        operation: spec.name.to_string(),
        db_name: ctx
            .db_name
            .clone()
            .or_else(|| params.str("db_name"))
            .unwrap_or_default(),
        user: ctx.user().to_string(),
        sparql: params.str("sparql").unwrap_or_default(),
        callback,
        result_dir: Some(ctx.export_dir()),
    };
    let job: TaskJob = Box::new(move |_task: &TaskHandle| {
        let Some(inner) = weak.upgrade() else {
            return error_envelope(Status::OperationFailed, "the server has stopped");
        };
        let mut ctx = Ctx {
            inner: &inner,
            req: &req,
            remote_ip,
            op,
            user,
            db_name,
            inner_request,
            config,
            started: Instant::now(),
        };
        let resp = handler(&mut ctx, &params);
        match resp.body_json() {
            Some(body) => body.clone(),
            // A raw (non-envelope) answer: report its outcome only.
            None if resp.is_ok() => Json::obj()
                .with("StatusCode", 0)
                .with("StatusMsg", "success"),
            None => error_envelope(Status::OperationFailed, ""),
        }
    });
    match ctx.inner.tasks.submit(task_spec, job) {
        Ok(id) => GResp::json(
            Json::obj()
                .with("StatusCode", 0)
                .with("StatusMsg", ACCEPTED_MSG)
                .with("opt_id", id.to_string()),
        ),
        Err(msg) => GResp::error(Status::OperationFailed, msg),
    }
}

// ---- the test-only operation ------------------------------------------------

static TEST_OPS: AtomicBool = AtomicBool::new(false);

/// Enable (or disable) the hidden test-only operation `__asyncTest`
/// (process-wide; off by default, when it answers `1100` like an unknown
/// operation). For the asynchronous-operation tests only.
#[doc(hidden)]
pub fn enable_test_ops(on: bool) {
    TEST_OPS.store(on, Ordering::SeqCst);
}

pub(super) fn test_ops_enabled() -> bool {
    TEST_OPS.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_state(m: &TaskManager, id: u64) -> TaskState {
        m.wait_finished(id, Duration::from_secs(10)).unwrap()
    }

    #[test]
    fn opt_ids_follow_the_upstream_layout_and_increase() {
        let m = TaskManager::default();
        let before = now_ms();
        let ids: Vec<u64> = (0..5000)
            .map(|_| m.register("query", "db", "ASK {}", "u").0)
            .collect();
        let after = now_ms();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
        for &id in &ids {
            assert_eq!((id >> SEQUENCE_BITS) & ((1 << WORKER_ID_BITS) - 1), WORKER_ID);
            let t = opt_id_time_ms(id);
            // (5000 ids may run past the clock by a millisecond.)
            assert!(t >= before && t <= after + 2, "{t} not in {before}..{after}");
        }
        assert_eq!(parse_opt_id(&format!(" {} ", ids[0])), Some(ids[0]));
        assert_eq!(parse_opt_id("x"), None);
    }

    #[test]
    fn synchronous_tasks_run_finish_and_are_retained_as_history() {
        let m = TaskManager::new(TaskConfig {
            max_history: 2,
            ..TaskConfig::default()
        });
        let (a, _) = m.register("query", "db", "SELECT 1", "alice");
        let (b, flag_b) = m.register("query", "db", "SELECT 2", "bob");
        let (c, _) = m.register("query", "db", "SELECT 3", "bob");
        assert_eq!(m.running().len(), 3);
        let info = m.get(a).unwrap();
        assert_eq!(
            (info.state, info.user.as_str(), info.is_async, info.end_ms),
            (TaskState::Running, "alice", false, 0)
        );
        m.finish(a, Some(true));
        assert_eq!(m.request_cancel(b), CancelOutcome::Requested);
        assert!(flag_b.load(Ordering::Relaxed));
        assert_eq!(m.request_cancel(b), CancelOutcome::AlreadyRequested);
        m.finish(b, Some(false));
        assert_eq!(m.get(a).unwrap().state, TaskState::Succeeded);
        assert_eq!(m.get(b).unwrap().state, TaskState::Cancelled);
        m.finish(c, None);
        assert_eq!(m.get(c).unwrap().state, TaskState::Failed);
        assert_eq!(
            m.request_cancel(b),
            CancelOutcome::Finished(TaskState::Cancelled)
        );
        // Only the two newest finished ones are kept.
        assert!(m.get(a).is_none(), "the oldest left the history");
        let (d, _) = m.register("query", "db", "SELECT 4", "u");
        assert!(m.get(b).is_some() && m.get(c).is_some() && m.get(d).is_some());
        assert_eq!(m.running().len(), 1);
        assert!(m.cancel_flag(d).is_some());
        assert!(m.cancel_flag(b).is_none(), "only running tasks have a live flag");
        assert_eq!(m.request_cancel(12345), CancelOutcome::NotFound);
        // Finishing twice, or an unknown id, is harmless.
        m.finish(a, Some(true));
        m.finish(b, Some(true));
        assert_eq!(m.get(b).unwrap().state, TaskState::Cancelled);
    }

    #[test]
    fn asynchronous_jobs_run_off_thread_and_keep_their_result() {
        let m = TaskManager::default();
        let caller = std::thread::current().id();
        let spec = TaskSpec {
            operation: "build".into(),
            db_name: "lubm".into(),
            user: "root".into(),
            ..TaskSpec::default()
        };
        let id = m
            .submit(
                spec,
                Box::new(move |h| {
                    assert_ne!(std::thread::current().id(), caller);
                    assert_eq!(current().unwrap().opt_id(), h.opt_id());
                    Json::obj()
                        .with("StatusCode", 0)
                        .with("StatusMsg", "Import RDF file to database done.")
                        .with("success_num", 7)
                        .with("failed_num", 1)
                }),
            )
            .unwrap();
        assert_eq!(wait_state(&m, id), TaskState::Succeeded);
        let info = m.get(id).unwrap();
        assert!(info.is_async);
        assert_eq!(info.status_msg, "Import RDF file to database done.");
        assert_eq!((info.success_num, info.failed_num), (7, 1));
        assert_eq!(info.status_code, Some(0));
        assert!(info.end_ms >= info.start_ms && info.end_ms > 0);
        let result = m.result(id).unwrap().load().unwrap();
        assert_eq!(
            result.get("opt_id").and_then(Json::as_str),
            Some(id.to_string().as_str())
        );
        assert!(m.wait_async_idle(Duration::from_secs(5)));
        assert!(current().is_none(), "the caller's thread has no current task");
    }

    #[test]
    fn failures_panics_and_cancellation_are_recorded() {
        let m = TaskManager::new(TaskConfig {
            workers: 1,
            ..TaskConfig::default()
        });
        let spec = || TaskSpec {
            operation: "x".into(),
            ..TaskSpec::default()
        };
        let failed = m
            .submit(
                spec(),
                Box::new(|_| error_envelope(Status::OperationFailed, "boom")),
            )
            .unwrap();
        let panicked = m.submit(spec(), Box::new(|_| panic!("job panic"))).unwrap();
        // A long job that polls its flag, and one queued behind it (one
        // runner) that is cancelled before it starts.
        let (tx, rx) = std::sync::mpsc::channel();
        let long = m
            .submit(
                spec(),
                Box::new(move |h| {
                    tx.send(()).unwrap();
                    while !h.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    error_envelope(Status::OperationFailed, "stopped")
                }),
            )
            .unwrap();
        let queued = m
            .submit(spec(), Box::new(|_| unreachable!("cancelled before it ran")))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(m.get(long).unwrap().state, TaskState::Running);
        assert_eq!(m.get(queued).unwrap().state, TaskState::Running);
        assert_eq!(m.request_cancel(queued), CancelOutcome::Requested);
        assert_eq!(m.request_cancel(long), CancelOutcome::Requested);
        assert_eq!(wait_state(&m, long), TaskState::Cancelled);
        assert_eq!(wait_state(&m, queued), TaskState::Cancelled);
        assert_eq!(
            m.get(queued).unwrap().status_msg,
            format!("operation cancel opt_id:{queued}")
        );
        assert_eq!(wait_state(&m, failed), TaskState::Failed);
        assert_eq!(m.get(failed).unwrap().status_code, Some(1005));
        assert_eq!(wait_state(&m, panicked), TaskState::Failed);
        assert!(m.get(panicked).unwrap().status_msg.contains("panicked"));
        assert!(m.wait_async_idle(Duration::from_secs(5)));
        // The runner survived the panic.
        let after = m
            .submit(spec(), Box::new(|_| Json::obj().with("StatusCode", 0)))
            .unwrap();
        assert_eq!(wait_state(&m, after), TaskState::Succeeded);
    }

    #[test]
    fn statements_on_a_runner_attach_to_its_task() {
        let m = Arc::new(TaskManager::default());
        let m2 = Arc::clone(&m);
        let id = m
            .submit(
                TaskSpec {
                    operation: "query".into(),
                    user: "u".into(),
                    ..TaskSpec::default()
                },
                Box::new(move |h| {
                    let (id, flag) = m2.register("query", "db1", "SELECT * {}", "u");
                    assert_eq!(id, h.opt_id());
                    assert!(Arc::ptr_eq(&flag, &h.cancel_flag()));
                    m2.finish(id, Some(true)); // a no-op for the async task
                    assert_eq!(m2.get(id).unwrap().state, TaskState::Running);
                    h.set_progress("halfway", 5);
                    assert_eq!(m2.get(id).unwrap().status_msg, "halfway");
                    Json::obj().with("StatusCode", 0).with("StatusMsg", "done")
                }),
            )
            .unwrap();
        assert_eq!(wait_state(&m, id), TaskState::Succeeded);
        let info = m.get(id).unwrap();
        assert_eq!((info.db_name.as_str(), info.sparql.as_str()), ("db1", "SELECT * {}"));
        assert_eq!(m.list().len(), 1, "no second task was created");
    }

    #[test]
    fn query_and_large_results_spill_to_files_removed_on_prune() {
        let dir = std::env::temp_dir().join(format!(
            "gstore_tasks_spill_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let m = TaskManager::new(TaskConfig {
            max_finished: 1,
            spill_threshold: 64,
            ..TaskConfig::default()
        });
        let spec = |op: &str| TaskSpec {
            operation: op.into(),
            result_dir: Some(dir.clone()),
            ..TaskSpec::default()
        };
        let q = m
            .submit(spec("query"), Box::new(|_| Json::obj().with("AnsNum", 3)))
            .unwrap();
        assert_eq!(wait_state(&m, q), TaskState::Succeeded);
        let file = dir.join(format!("{q}.json"));
        assert_eq!(m.result(q), Some(TaskResult::File(file.clone())));
        assert_eq!(m.get(q).unwrap().result_file, Some(file.clone()));
        assert_eq!(m.get(q).unwrap().success_num, 3, "AnsNum counts as success_num");
        let loaded = m.result(q).unwrap().load().unwrap();
        assert_eq!(loaded.get("AnsNum").and_then(Json::as_i64), Some(3));
        // A small non-query result stays in memory; a big one spills.
        let small = m
            .submit(spec("load"), Box::new(|_| Json::obj().with("StatusMsg", "ok")))
            .unwrap();
        assert_eq!(wait_state(&m, small), TaskState::Succeeded);
        assert!(matches!(m.result(small), Some(TaskResult::Json(_))));
        assert!(m.get(q).is_none(), "max_finished = 1 dropped the older result");
        assert!(!file.exists(), "and removed its file");
        let big = m
            .submit(spec("load"), Box::new(|_| Json::obj().with("x", "y".repeat(100))))
            .unwrap();
        assert_eq!(wait_state(&m, big), TaskState::Succeeded);
        assert!(matches!(m.result(big), Some(TaskResult::File(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_expires_finished_tasks() {
        let m = TaskManager::new(TaskConfig {
            retention: Duration::from_millis(50),
            ..TaskConfig::default()
        });
        let (a, _) = m.register("query", "", "", "u");
        m.finish(a, Some(true));
        let id = m
            .submit(TaskSpec::default(), Box::new(|_| Json::obj()))
            .unwrap();
        assert_eq!(wait_state(&m, id), TaskState::Succeeded);
        let (running, _) = m.register("query", "", "", "u");
        std::thread::sleep(Duration::from_millis(80));
        let listed: Vec<u64> = m.list().iter().map(|t| t.opt_id).collect();
        assert_eq!(listed, vec![running], "finished tasks expired, the running one stays");
    }

    #[test]
    fn close_refuses_submissions_and_pending_is_bounded() {
        let m = TaskManager::new(TaskConfig {
            workers: 1,
            max_pending: 1,
            ..TaskConfig::default()
        });
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let first = m
            .submit(
                TaskSpec::default(),
                Box::new(move |_| {
                    let _ = rx.recv_timeout(Duration::from_secs(10));
                    Json::obj()
                }),
            )
            .unwrap();
        // Wait until the runner took it, then fill the queue.
        while m.shared.lock().active == 0 {
            std::thread::sleep(Duration::from_millis(2));
        }
        let second = m.submit(TaskSpec::default(), Box::new(|_| Json::obj())).unwrap();
        let err = m
            .submit(TaskSpec::default(), Box::new(|_| Json::obj()))
            .unwrap_err();
        assert!(err.contains("too many"), "{err}");
        assert!(!m.async_idle());
        assert!(!m.wait_async_idle(Duration::from_millis(30)));
        m.close();
        assert!(m.is_closed());
        let err = m
            .submit(TaskSpec::default(), Box::new(|_| Json::obj()))
            .unwrap_err();
        assert!(err.contains("shutting down"), "{err}");
        assert_eq!(m.cancel_all(), 2);
        tx.send(()).unwrap();
        assert!(m.wait_async_idle(Duration::from_secs(10)));
        assert_eq!(m.get(first).unwrap().state, TaskState::Succeeded);
        assert_eq!(m.get(second).unwrap().state, TaskState::Cancelled);
    }

    #[test]
    fn callbacks_report_delivery_failures() {
        let m = TaskManager::new(TaskConfig {
            callback_timeout: Duration::from_secs(2),
            callback_retries: 1,
            ..TaskConfig::default()
        });
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let id = m
            .submit(
                TaskSpec {
                    callback: Some(format!("http://127.0.0.1:{port}/cb")),
                    ..TaskSpec::default()
                },
                Box::new(|_| Json::obj()),
            )
            .unwrap();
        assert_eq!(m.get(id).unwrap().callback_status.as_deref(), Some("pending"));
        assert!(m.wait_async_idle(Duration::from_secs(10)));
        let info = m.get(id).unwrap();
        assert_eq!(info.state, TaskState::Succeeded, "a failed callback does not fail the task");
        assert!(
            info.callback_status.as_deref().unwrap().starts_with("failed:"),
            "{:?}",
            info.callback_status
        );
        assert!(check_callback_url("http://127.0.0.1:1/x").is_ok());
        assert!(check_callback_url("ftp://x").is_err());
    }

    #[test]
    fn state_codes_match_upstream() {
        assert_eq!(TaskState::Running.state_code(), 0);
        assert_eq!(TaskState::Succeeded.state_code(), 1);
        assert_eq!(TaskState::Failed.state_code(), -1);
        assert_eq!(TaskState::Cancelled.state_code(), -1);
        assert_eq!(TaskState::Running.list_status(), 0);
        assert_eq!(TaskState::Succeeded.list_status(), 1);
        assert_eq!(TaskState::Failed.list_status(), 1);
        assert_eq!(TaskState::Cancelled.list_status(), -1);
    }
}
