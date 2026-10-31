//! Task operations: `checkOperationState`, `operationTaskCancel`,
//! `operationTaskList` (upstream `grpc.cpp` `checkOperationState_task`,
//! `operation_task_cancel`, `operation_task_list`, and `ApiLogOperation.cpp`
//! `checkOperationState`), on the background task manager ([`super::tasks`]).
//!
//! **Who may use them.** Upstream lets only `root` call them (they are not in
//! its `check_privilege` table). Here any authenticated user may — for **their
//! own** tasks (so whoever submitted an `async` operation can poll and cancel
//! it); `root` and `admin`-privileged users see and cancel every task. Another
//! user's task reads as not found. (The REST `/cancel` keeps its own rule:
//! Update privilege for your own tasks, Admin for any.)
//!
//! Also here: the hidden test-only operation `__asyncTest` (see
//! [`tasks::enable_test_ops`]).

use std::time::{Duration, Instant};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::tasks::{self, CancelOutcome, TaskInfo, TaskState};
use super::{Ctx, OpSpec, Registry};
use crate::http_users::Privilege;
use crate::json::Json;

/// The privilege the dispatcher checks for the task operations: one every
/// authenticated user has (upstream's "anyone" class); the handlers then
/// scope what the caller sees to their own tasks.
const ANY_USER: &str = "testConnect";

/// The test-only operation (answers `1100` unless enabled).
const TEST_OP: &str = "__asyncTest";

/// The operations whose `checkOperationState` answer carries `success_num` /
/// `failed_num` (upstream `MessageCheckOperationStateResponse::toJsonString`).
const COUNTING_OPS: &[&str] = &["build", "batchInsert", "batchRemove"];

pub(super) fn register(reg: &mut Registry) {
    reg.add(OpSpec::new("checkOperationState", check_operation_state).privilege(ANY_USER));
    reg.add(OpSpec::new("operationTaskCancel", task_cancel).privilege(ANY_USER));
    reg.add(OpSpec::new("operationTaskList", task_list).privilege(ANY_USER));
    reg.add(OpSpec::new(TEST_OP, test_op).async_capable());
}

/// Whether the caller sees every task (root, or the Admin privilege).
fn sees_all(ctx: &Ctx<'_>) -> bool {
    ctx.is_root() || ctx.state().users.has_priv(ctx.user(), Privilege::Admin)
}

/// Whether the caller may see (and cancel) `task`.
fn visible(ctx: &Ctx<'_>, all: bool, task: &TaskInfo) -> bool {
    all || task.user == ctx.user()
}

/// `checkOperationState` — the state of an asynchronous operation (`opt_id`).
///
/// * missing `opt_id` → `1005 "the value of opt_id can not be empty!"`;
///   unknown (or another user's, or a synchronous statement's — upstream
///   records only asynchronous operations under their `opt_id`) →
///   `1005 "opt_id not found."`;
/// * a finished `query`: its result (upstream re-reads it from
///   `export/<opt_id>.json`) with `"state":1`; the file gone →
///   `9 "Read result file failed."`;
/// * otherwise `{"StatusCode":0,"StatusMsg":<the operation's latest
///   message>,"state":0|1|-1}`, plus `backupfilepath` for `backup` and
///   `success_num` / `failed_num` for `build` / `batchInsert` /
///   `batchRemove` (and any operation whose result carried them).
///
/// As upstream, the answer's `StatusCode` is `0` whatever the operation's
/// outcome: `state` and `StatusMsg` report it.
fn check_operation_state(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let Some(raw) = p.non_empty("opt_id") else {
        return GResp::error(
            Status::OperationFailed,
            "the value of opt_id can not be empty!",
        );
    };
    let not_found = || GResp::error(Status::OperationFailed, "opt_id not found.");
    let Some(id) = tasks::parse_opt_id(&raw) else {
        return not_found();
    };
    let all = sees_all(ctx);
    let Some(info) = ctx
        .inner
        .tasks
        .get(id)
        .filter(|t| t.is_async && visible(ctx, all, t))
    else {
        return not_found();
    };
    if info.operation == "query" && info.state == TaskState::Succeeded {
        return match ctx.inner.tasks.result(id).map(|r| r.load()) {
            Some(Ok(result @ Json::Obj(_))) => GResp::json(result.with("state", 1)),
            _ => GResp::error(Status::FileReadError, "Read result file failed."),
        };
    }
    let mut body = Json::obj()
        .with("StatusCode", 0)
        .with("StatusMsg", info.status_msg.as_str());
    if info.operation == "backup" {
        body.set("backupfilepath", info.filepath.clone().unwrap_or_default());
    }
    let counted = info.success_num != 0 || info.failed_num != 0;
    if COUNTING_OPS.contains(&info.operation.as_str()) || counted {
        body.set("success_num", info.success_num);
        body.set("failed_num", info.failed_num);
    }
    body.set("state", info.state.state_code());
    GResp::json(body)
}

/// `operationTaskCancel` — cancel a running task (`opt_id`): a SPARQL
/// statement (found with `operationTaskList`) or an asynchronous operation.
///
/// Upstream's answers: `1005 "opt_id not found"`, `1005 "task already
/// cancel"`, `1005 "task already finish, do not cancel"`, `1005 "task being
/// cancel, please do not repeat"` (a cancellation is already under way);
/// else the flag is raised and the request waits for the task to stop —
/// at most [`TaskConfig::cancel_wait`](tasks::TaskConfig::cancel_wait)
/// (upstream waits indefinitely) — and answers `0 "query cancel
/// successfully"`, or `0 "task already finish, do not cancel"` when it
/// completed anyway. A task still running after the wait is answered
/// `0 "query cancel successfully"` too, as the request is in effect
/// (cancellation is cooperative: it stops at its next check).
fn task_cancel(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let not_found = || GResp::error(Status::OperationFailed, "opt_id not found");
    // Upstream reads it as a number defaulting to 0 (never a task).
    let id = match p.str("opt_id") {
        None => return not_found(),
        Some(s) if s.trim().is_empty() => return not_found(),
        Some(s) => match tasks::parse_opt_id(&s) {
            Some(id) => id,
            None => {
                return GResp::error(
                    Status::OperationFailed,
                    format!("task cancel error: the opt_id '{s}' is not a number"),
                )
            }
        },
    };
    let all = sees_all(ctx);
    let tm = &ctx.inner.tasks;
    if !tm.get(id).is_some_and(|t| visible(ctx, all, &t)) {
        return not_found();
    }
    match tm.request_cancel(id) {
        CancelOutcome::NotFound => not_found(),
        CancelOutcome::Finished(TaskState::Cancelled) => {
            GResp::error(Status::OperationFailed, "task already cancel")
        }
        CancelOutcome::Finished(_) => GResp::error(
            Status::OperationFailed,
            "task already finish, do not cancel",
        ),
        CancelOutcome::AlreadyRequested => GResp::error(
            Status::OperationFailed,
            "task being cancel, please do not repeat",
        ),
        CancelOutcome::Requested => match tm.wait_finished(id, tm.config().cancel_wait) {
            Some(TaskState::Succeeded) | Some(TaskState::Failed) => {
                GResp::success("task already finish, do not cancel")
            }
            _ => GResp::success("query cancel successfully"),
        },
    }
}

/// `operationTaskList` — the tasks (running SPARQL statements, asynchronous
/// operations, and the retained finished ones), by `opt_id`, a page at a
/// time: `pageNo` (default 1), `pageSize` (default 10), `status` (`0`
/// running — the default —, `1` finished, `-1` cancelled, anything else
/// every task). Each item: `opt_id` (string), `operation`, `async` (0/1),
/// `database`, `sparql`, `status`, `resultFile`, `startTime`, `endTime`
/// (ms since the epoch, `0` while running), `queryTime` (ms), and — a Rust
/// extension — `user`. Then `pageNo`, `pageSize`, `totalPage`, `totalSize`.
fn task_list(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let int = |key: &str| {
        p.int(key)
            .map(Option::unwrap_or_default)
            .map_err(|e| GResp::error(Status::ParamIsIllegal, e))
    };
    let (page_no, page_size, status) = match (int("pageNo"), int("pageSize"), int("status")) {
        (Ok(n), Ok(s), Ok(st)) => (n, s, st),
        (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return r,
    };
    let page_no = if page_no <= 0 { 1 } else { page_no as u64 };
    let page_size = if page_size <= 0 { 10 } else { page_size as u64 };
    let every = !(-1..=1).contains(&status);
    let all = sees_all(ctx);
    let tasks: Vec<TaskInfo> = ctx
        .inner
        .tasks
        .list()
        .into_iter()
        .filter(|t| visible(ctx, all, t))
        .filter(|t| every || t.state.list_status() == status)
        .collect();
    let total = tasks.len() as u64;
    let start = (page_no - 1).saturating_mul(page_size);
    let mut list = Json::arr();
    for t in tasks
        .iter()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .take(usize::try_from(page_size).unwrap_or(usize::MAX))
    {
        list.push(task_json(t));
    }
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", "success")
            .with("list", list)
            .with("pageNo", page_no)
            .with("pageSize", page_size)
            .with("totalPage", total.div_ceil(page_size))
            .with("totalSize", total),
    )
}

/// One `operationTaskList` item (upstream field names and types).
fn task_json(t: &TaskInfo) -> Json {
    let result_file = t
        .result_file
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Json::obj()
        .with("opt_id", t.opt_id.to_string())
        .with("operation", t.operation.as_str())
        .with("async", i64::from(t.is_async))
        .with("database", t.db_name.as_str())
        .with("sparql", t.sparql.as_str())
        .with("status", t.state.list_status())
        .with("resultFile", result_file)
        .with("startTime", t.start_ms)
        .with("endTime", t.end_ms)
        .with("queryTime", t.elapsed.as_millis() as u64)
        .with("user", t.user.as_str())
}

/// `__asyncTest` — a test-only operation for the asynchronous machinery
/// (root only; `1100` unless [`tasks::enable_test_ops`] is on). It reports
/// progress, works for `sleep_ms` milliseconds — stopping early with
/// upstream's cancellation error when its task is cancelled, unless
/// `ignore_cancel` — then panics (`panic`), fails (`fail`), or answers
/// `"test operation done"` with the given `success_num` / `failed_num`.
fn test_op(_ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    if !tasks::test_ops_enabled() {
        return GResp::error(Status::OperationUndefined, "");
    }
    let num = |key: &str| p.int(key).ok().flatten().unwrap_or(0).max(0);
    let sleep_ms = num("sleep_ms") as u64;
    let ignore_cancel = p.bool("ignore_cancel", false);
    let task = tasks::current();
    if let Some(t) = &task {
        t.set_progress("test operation running", 1);
    }
    let deadline = Instant::now() + Duration::from_millis(sleep_ms);
    loop {
        if let Some(t) = task.as_ref().filter(|t| t.is_cancelled() && !ignore_cancel) {
            return GResp::error(
                Status::OperationFailed,
                format!("operation cancel opt_id:{}", t.opt_id()),
            );
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
    if p.bool("panic", false) {
        panic!("{TEST_OP} was asked to panic");
    }
    if p.bool("fail", false) {
        return GResp::error(Status::OperationFailed, "test operation failed");
    }
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", "test operation done")
            .with("success_num", num("success_num"))
            .with("failed_num", num("failed_num")),
    )
}
