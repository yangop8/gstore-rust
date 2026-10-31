//! The API logs: the query log, the access log and the transaction log, and
//! the operations reading them (`querylogdate`, `querylog`, `accesslogdate`,
//! `accesslog`, `txnlog`) — upstream `APIUtil::write_query_log`,
//! `write_access_log`, `add_transactionlog` … (`src/Api/APIUtil.cpp`) and
//! `src/Server/ApiLogOperation.cpp`.
//!
//! Every log is a file of JSON records, one per line, with upstream's field
//! names (`src/Api/APILogQueryUtil.h`):
//!
//! * **query log** — `<querylog_path>/<YYYYMMDD>.log` (default
//!   `<root>/logs/endpoint/`), one record per `query` operation:
//!   `queryDateTime` (when it started, `YYYY-MM-DD HH:MM:SS.mmm`),
//!   `remoteIP`, `sparql`, `ansNum`, `format`, `fileName`, `statusCode`,
//!   `statusMsg`, `queryTime` (ms) and `dbname`. Written when `querylog_mode`
//!   is on.
//! * **access log** — `<accesslog_path>/<YYYYMMDD>.log` (default
//!   `<root>/logs/ipaccess/`), one record per answered `/api` operation:
//!   `ip`, `operation`, `code`, `msg`, `createtime`, and as upstream `dbname`
//!   (when a `db_name` was given), `state` (for the operations upstream can
//!   run asynchronously), `num` / `fail_num` (the `success_num` /
//!   `failed_num` of `build`, `batchInsert`, `batchRemove`). Written when
//!   `accesslog_mode` is on.
//! * **transaction log** — `<root>/logs/transaction.json` (upstream's
//!   `logs/transaction.json`): `db_name`, `TID` (`<begin ms>_<tid>`, as
//!   upstream), `user`, `state` (`RUNNING`, then `COMMITED` / `ROLLBACK`),
//!   `begin_time` and `end_time` (ms, `INF` while running). Upstream rewrites
//!   a transaction's record in place when it ends; here the final record is
//!   appended (crash-safe, O(1)) and the reader keeps the last record of each
//!   `TID`.
//!
//! Relative log directories (`querylog_path`, `accesslog_path` of
//! `conf.ini`) are resolved against the server's root directory. When a day's
//! file is created, the oldest daily files beyond `querylog_days` /
//! `accesslog_days` are deleted (upstream counts files, not days, the same
//! way).
//!
//! # Reading
//!
//! `querylog`, `accesslog` (`date`, `pageNo`, `pageSize`) and `txnlog`
//! (`pageNo`, `pageSize`) answer upstream's page envelope: `totalSize`,
//! `totalPage`, `pageNo`, `pageSize` and the `list` of records, **newest
//! first** (page 1 holds the latest records). A `pageNo` beyond the last page
//! is `1005 "… fail: more then max page number N"` (upstream's text);
//! `pageNo` below 1 is 1 and `pageSize` below 1 is 10. `querylog` and
//! `accesslog` may also filter by `db_name`, `accesslog` by `db_operation` —
//! here before paging, so pages are full (upstream filters while paging).
//!
//! The records of the query log are listed with the code's field names and,
//! for clients written after upstream's API docs, also under the docs' names
//! (`QueryDateTime`, `RemoteIP`, `Sparql`, `AnsNum`, `Format`, `FileName`,
//! `QueryTime`): upstream's server sends the former, its docs and its C++
//! client parser use the latter.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::{required, Ctx};
use crate::config::GStoreConfig;
use crate::json::{self, Json};
use crate::sysdb::clock;

/// The transaction log, relative to the server root.
pub const TXN_LOG_FILE: &str = "logs/transaction.json";

/// Upstream's default page size.
const DEFAULT_PAGE_SIZE: i64 = 10;

/// A transaction's state in the transaction log (upstream's spelling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Running,
    Committed,
    RolledBack,
    Aborted,
}

impl TxnState {
    pub fn as_str(self) -> &'static str {
        match self {
            TxnState::Running => "RUNNING",
            TxnState::Committed => "COMMITED",
            TxnState::RolledBack => "ROLLBACK",
            TxnState::Aborted => "ABORTED",
        }
    }
}

/// One query-log record (upstream `DBQueryLogInfo`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryLogRecord {
    pub query_date_time: String,
    pub remote_ip: String,
    pub sparql: String,
    pub ans_num: i64,
    pub format: String,
    pub file_name: String,
    pub status_code: i64,
    pub status_msg: String,
    /// Milliseconds.
    pub query_time: u64,
    pub db_name: String,
}

impl QueryLogRecord {
    /// The stored form (upstream `DBQueryLogInfo::toJSON`).
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("queryDateTime", self.query_date_time.as_str())
            .with("remoteIP", self.remote_ip.as_str())
            .with("sparql", self.sparql.as_str())
            .with("ansNum", self.ans_num)
            .with("format", self.format.as_str())
            .with("fileName", self.file_name.as_str())
            .with("statusCode", self.status_code)
            .with("statusMsg", self.status_msg.as_str())
            .with("queryTime", self.query_time)
            .with("dbname", self.db_name.as_str())
    }
}

/// One access-log record (upstream `DBAccessLogInfo`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccessLogRecord {
    pub ip: String,
    pub operation: String,
    pub code: i64,
    pub msg: String,
    pub createtime: String,
    pub db_name: String,
    pub num: u64,
    pub fail_num: u64,
    pub filepath: String,
    pub opt_id: String,
}

impl AccessLogRecord {
    /// Whether upstream records a `state` for `operation`
    /// (`DBAccessLogInfo::checkOperation`: the ones it can run
    /// asynchronously).
    fn has_state(operation: &str) -> bool {
        matches!(
            operation,
            "load"
                | "unload"
                | "build"
                | "batchInsert"
                | "batchRemove"
                | "backup"
                | "restore"
                | "query"
                | "loadcsr"
        )
    }

    /// The stored form (upstream `DBAccessLogInfo::toJSON`). A finished
    /// operation has `state` 1; one registered under an `opt_id` (still
    /// running) 0.
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj()
            .with("ip", self.ip.as_str())
            .with("operation", self.operation.as_str())
            .with("code", self.code)
            .with("msg", self.msg.as_str())
            .with("createtime", self.createtime.as_str());
        if Self::has_state(&self.operation) {
            j.set("state", if self.opt_id.is_empty() { 1 } else { 0 });
        }
        if !self.opt_id.is_empty() {
            j.set("opt_id", self.opt_id.as_str());
        }
        if !self.db_name.is_empty() {
            j.set("dbname", self.db_name.as_str());
        }
        if matches!(
            self.operation.as_str(),
            "build" | "batchInsert" | "batchRemove"
        ) {
            j.set("num", self.num);
            j.set("fail_num", self.fail_num);
        }
        if matches!(self.operation.as_str(), "backup" | "query") && !self.filepath.is_empty() {
            j.set("filepath", self.filepath.as_str());
        }
        j
    }
}

/// The log writers' shared state (one per server). Thread-safe.
#[derive(Debug, Default)]
pub struct ApiLogs {
    /// Serializes appends, the creation of a day's file and the retention
    /// sweep it triggers.
    io: Mutex<()>,
    /// When each open transaction began (ms), for its final record.
    txn_begun: Mutex<HashMap<u64, u128>>,
}

impl ApiLogs {
    pub fn new() -> ApiLogs {
        ApiLogs::default()
    }

    fn io(&self) -> MutexGuard<'_, ()> {
        self.io.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append a query-log record (when `querylog_mode` is on). Best effort:
    /// a write error is ignored, as a log must never fail a request.
    pub fn write_query(&self, config: &GStoreConfig, root: &Path, rec: &QueryLogRecord) {
        if config.querylog_mode {
            let dir = querylog_dir(config, root);
            let _ = self.append_daily(&dir, config.querylog_days, &record_line(rec.to_json()));
        }
    }

    /// Append an access-log record (when `accesslog_mode` is on). Best effort.
    pub fn write_access(&self, config: &GStoreConfig, root: &Path, rec: &AccessLogRecord) {
        if config.accesslog_mode {
            let dir = accesslog_dir(config, root);
            let _ = self.append_daily(&dir, config.accesslog_days, &record_line(rec.to_json()));
        }
    }

    /// Record that transaction `tid` of `user` began on `db`. Best effort.
    pub fn txn_begin(&self, root: &Path, db: &str, tid: u64, user: &str) {
        let begun = clock::unix_millis();
        self.txn_begun
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tid, begun);
        let rec = txn_record(db, tid, user, TxnState::Running, begun, None);
        let _guard = self.io();
        let _ = append_line(&txnlog_path(root), &record_line(rec));
    }

    /// Record that transaction `tid` (of `user`, on `db`) ended in `state`.
    /// Best effort.
    pub fn txn_end(&self, root: &Path, db: &str, tid: u64, user: &str, state: TxnState) {
        let ended = clock::unix_millis();
        let begun = self
            .txn_begun
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tid)
            .unwrap_or(ended);
        let rec = txn_record(db, tid, user, state, begun, Some(ended));
        let _guard = self.io();
        let _ = append_line(&txnlog_path(root), &record_line(rec));
    }

    /// Append `line` to today's file in `dir`; when that file is new, delete
    /// the oldest daily files beyond `keep`.
    fn append_daily(&self, dir: &Path, keep: u32, line: &str) -> io::Result<()> {
        let _guard = self.io();
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.log", clock::now().date_compact()));
        let fresh = !path.exists();
        append_line(&path, line)?;
        if fresh {
            prune_daily(dir, keep.max(1));
        }
        Ok(())
    }
}

fn txn_record(
    db: &str,
    tid: u64,
    user: &str,
    state: TxnState,
    begun: u128,
    ended: Option<u128>,
) -> Json {
    Json::obj()
        .with("db_name", db)
        .with("TID", format!("{begun}_{tid}"))
        .with("user", user)
        .with("state", state.as_str())
        .with("begin_time", begun.to_string())
        .with(
            "end_time",
            ended.map_or_else(|| "INF".to_string(), |e| e.to_string()),
        )
}

/// A record as one log line (keys sorted, like upstream's `nlohmann::json`).
fn record_line(rec: Json) -> String {
    rec.into_sorted().to_string()
}

/// Append `line` and a newline in one write (`O_APPEND`).
fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    f.write_all(&buf)
}

/// Delete the oldest `YYYYMMDD.log` files in `dir` beyond the newest `keep`.
fn prune_daily(dir: &Path, keep: u32) {
    let mut dates = log_dates(dir);
    dates.reverse(); // oldest first
    let keep = keep as usize;
    if dates.len() > keep {
        for d in &dates[..dates.len() - keep] {
            let _ = fs::remove_file(dir.join(format!("{d}.log")));
        }
    }
}

/// `p` resolved against `root` when relative.
pub fn resolve(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// The query-log directory (`querylog_path`, against `root`).
pub fn querylog_dir(config: &GStoreConfig, root: &Path) -> PathBuf {
    resolve(root, &config.querylog_path)
}

/// The access-log directory (`accesslog_path`, against `root`).
pub fn accesslog_dir(config: &GStoreConfig, root: &Path) -> PathBuf {
    resolve(root, &config.accesslog_path)
}

/// The transaction log.
pub fn txnlog_path(root: &Path) -> PathBuf {
    root.join(TXN_LOG_FILE)
}

/// The dates (`YYYYMMDD`) of the daily log files in `dir`, newest first.
pub fn log_dates(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let date = name.strip_suffix(".log")?;
            is_date(date).then(|| date.to_string())
        })
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

fn is_date(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit())
}

/// The JSON-object records of a log file, oldest first (a missing file has
/// none; lines that do not parse — e.g. one torn by a crash — are skipped).
pub fn read_records(path: &Path) -> Vec<Json> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_suffix(',').unwrap_or(line);
            match json::parse(line) {
                Ok(j @ Json::Obj(_)) => Some(j),
                _ => None,
            }
        })
        .collect()
}

/// The transaction-log records, one per transaction (the last record of each
/// `TID`), in the order the transactions began.
pub fn txn_records(root: &Path) -> Vec<Json> {
    let mut order: Vec<String> = Vec::new();
    let mut last: HashMap<String, Json> = HashMap::new();
    for rec in read_records(&txnlog_path(root)) {
        let tid = rec
            .get("TID")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        if last.insert(tid.clone(), rec).is_none() {
            order.push(tid);
        }
    }
    order.into_iter().filter_map(|t| last.remove(&t)).collect()
}

/// One page of records, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub total_size: usize,
    pub total_page: usize,
    pub page_no: usize,
    pub page_size: usize,
    pub items: Vec<Json>,
}

/// Page `page_no` (from 1; below 1 is 1) of `page_size` records (below 1 is
/// 10) of `records` (oldest first), newest first — upstream
/// `get_file_lines`. A page beyond the last is an error naming the last.
pub fn paginate(records: Vec<Json>, page_no: i64, page_size: i64) -> Result<Page, String> {
    let page_no = usize::try_from(page_no.max(1)).unwrap_or(1);
    let page_size = if page_size < 1 {
        DEFAULT_PAGE_SIZE as usize
    } else {
        usize::try_from(page_size).unwrap_or(usize::MAX)
    };
    let total_size = records.len();
    let total_page = total_size.div_ceil(page_size);
    if total_size == 0 {
        return Ok(Page {
            total_size,
            total_page,
            page_no,
            page_size,
            items: Vec::new(),
        });
    }
    if page_no > total_page {
        return Err(format!("more then max page number {total_page}"));
    }
    // Newest first: page 1 is the last `page_size` records, reversed.
    let end = total_size - page_size.saturating_mul(page_no - 1);
    let start = end.saturating_sub(page_size);
    let mut items = records;
    items.truncate(end);
    let mut items: Vec<Json> = items.drain(start..).collect();
    items.reverse();
    Ok(Page {
        total_size,
        total_page,
        page_no,
        page_size,
        items,
    })
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// The access-log hook of the dispatcher: record one answered operation
/// (upstream registers the same callback in `checkRequest`). Unknown
/// operations are not recorded, nor an asynchronous run of an operation that
/// supports `async=1` (which records itself under its `opt_id`, as
/// upstream).
pub(super) fn log_access(ctx: &Ctx<'_>, params: &Params, resp: &GResp) {
    if ctx.op.is_empty() || resp.status_code() == Status::OperationUndefined.code() {
        return;
    }
    let config = ctx.inner.config();
    if !config.accesslog_mode {
        return;
    }
    if let Some(spec) = super::registry().get(&ctx.op) {
        if spec.async_capable && params.bool("async", false) {
            return;
        }
    }
    let count = |key: &str| {
        resp.body_json()
            .and_then(|b| b.get(key))
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
            .unwrap_or(0)
    };
    let rec = AccessLogRecord {
        ip: ctx.remote_ip.clone(),
        operation: ctx.op.clone(),
        code: resp.status_code(),
        msg: clean_msg(&resp.status_msg()),
        createtime: clock::now().datetime(),
        db_name: params.str("db_name").unwrap_or_default(),
        num: count("success_num"),
        fail_num: count("failed_num"),
        filepath: String::new(),
        opt_id: String::new(),
    };
    ctx.inner
        .apilogs
        .write_access(config, &ctx.inner.root, &rec);
}

/// Upstream's clean-up of a logged message: no line breaks, no runs of four
/// spaces.
fn clean_msg(msg: &str) -> String {
    msg.replace(['\r', '\n'], "").replace("    ", "")
}

/// The query-log hook of the `query` operation: [`start`](Self::start) it
/// before the query runs, [`finish`](Self::finish) it with the response.
pub(crate) struct QueryLog {
    db: String,
    /// The statement (empty when the query log is off: not copied then).
    sparql: String,
    format: String,
    started: SystemTime,
}

impl QueryLog {
    /// Begin logging a query of `db`: also appends it to the REST layer's
    /// `query.log` (read by `GET /querylog`), as before.
    pub(crate) fn start(ctx: &Ctx<'_>, db: &str, sparql: &str, format: Option<&str>) -> QueryLog {
        crate::http_api::log_query(ctx.root_dir(), db, ctx.user(), sparql);
        let on = ctx.inner.config().querylog_mode;
        QueryLog {
            db: db.to_string(),
            sparql: if on {
                sparql.to_string()
            } else {
                String::new()
            },
            format: format
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .unwrap_or("json")
                .to_string(),
            started: SystemTime::now(),
        }
    }

    /// Write the query-log record of the answered query.
    pub(crate) fn finish(self, ctx: &Ctx<'_>, resp: &GResp) {
        let config = ctx.inner.config();
        if !config.querylog_mode {
            return;
        }
        let body = resp.body_json();
        let field = |k: &str| body.and_then(|b| b.get(k));
        let query_time = field("QueryTime")
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(self.started)
                    .unwrap_or(Duration::ZERO)
                    .as_millis() as u64
            });
        let rec = QueryLogRecord {
            query_date_time: clock::at(self.started).datetime_ms(),
            remote_ip: ctx.remote_ip.clone(),
            sparql: self.sparql,
            ans_num: field("AnsNum").and_then(Json::as_i64).unwrap_or(0),
            format: self.format,
            file_name: field("FileName")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string(),
            status_code: resp.status_code(),
            status_msg: resp.status_msg(),
            query_time,
            db_name: self.db,
        };
        ctx.inner.apilogs.write_query(config, &ctx.inner.root, &rec);
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// `pageNo` and `pageSize` (defaults 1 and 10).
fn paging(p: &Params) -> Result<(i64, i64), GResp> {
    let bad = |e: String| GResp::error(Status::ParamIsIllegal, e);
    let no = p.int("pageNo").map_err(bad)?.unwrap_or(1);
    let size = p.int("pageSize").map_err(bad)?.unwrap_or(DEFAULT_PAGE_SIZE);
    Ok((no, size))
}

/// The required `date` (`YYYYMMDD`; anything else is refused, so it can
/// never name a file outside the log directory).
fn date_param(p: &Params) -> Result<String, GResp> {
    let date = required(p, "date")?;
    let date = date.trim().to_string();
    if !is_date(&date) {
        return Err(GResp::error(
            Status::ParamIsIllegal,
            "the value of date must be a date in the yyyyMMdd format",
        ));
    }
    Ok(date)
}

fn page_response(msg: &str, page: Page) -> GResp {
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", msg)
            .with("totalSize", page.total_size)
            .with("totalPage", page.total_page)
            .with("pageNo", page.page_no)
            .with("pageSize", page.page_size)
            .with("list", Json::Arr(page.items)),
    )
}

fn dates_response(msg: &str, dir: &Path) -> GResp {
    let list: Vec<Json> = log_dates(dir).into_iter().map(Json::from).collect();
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", msg)
            .with("list", list),
    )
}

/// Keep the records whose `key` equals the parameter `param`, when given.
fn filter_by(records: Vec<Json>, p: &Params, param: &str, key: &str) -> Vec<Json> {
    match p.non_empty(param) {
        None => records,
        Some(want) => records
            .into_iter()
            .filter(|r| r.get(key).and_then(Json::as_str) == Some(want.as_str()))
            .collect(),
    }
}

/// `querylogdate` — the days with a query log, newest first.
pub(super) fn querylogdate(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let dir = querylog_dir(ctx.inner.config(), ctx.root_dir());
    dates_response("Get query log date success", &dir)
}

/// The query-log names upstream's API docs use, added to each listed record.
const QUERY_LOG_DOC_NAMES: [(&str, &str); 7] = [
    ("queryDateTime", "QueryDateTime"),
    ("remoteIP", "RemoteIP"),
    ("sparql", "Sparql"),
    ("ansNum", "AnsNum"),
    ("format", "Format"),
    ("fileName", "FileName"),
    ("queryTime", "QueryTime"),
];

/// `querylog` — one page of a day's query log.
pub(super) fn querylog(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let (date, (page_no, page_size)) = match (date_param(p), paging(p)) {
        (Ok(d), Ok(pg)) => (d, pg),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let dir = querylog_dir(ctx.inner.config(), ctx.root_dir());
    let records = filter_by(
        read_records(&dir.join(format!("{date}.log"))),
        p,
        "db_name",
        "dbname",
    );
    match paginate(records, page_no, page_size) {
        Ok(mut page) => {
            for item in &mut page.items {
                for (code_name, doc_name) in QUERY_LOG_DOC_NAMES {
                    if let Some(v) = item.get(code_name).cloned() {
                        item.set(doc_name, v);
                    }
                }
            }
            page_response("Get query log success.", page)
        }
        Err(e) => GResp::error(Status::OperationFailed, format!("Get query log fail: {e}")),
    }
}

/// `accesslogdate` — the days with an access log, newest first.
pub(super) fn accesslogdate(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let dir = accesslog_dir(ctx.inner.config(), ctx.root_dir());
    dates_response("Get access log date success", &dir)
}

/// `accesslog` — one page of a day's access log.
pub(super) fn accesslog(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let (date, (page_no, page_size)) = match (date_param(p), paging(p)) {
        (Ok(d), Ok(pg)) => (d, pg),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let dir = accesslog_dir(ctx.inner.config(), ctx.root_dir());
    let records = read_records(&dir.join(format!("{date}.log")));
    let records = filter_by(records, p, "db_name", "dbname");
    let records = filter_by(records, p, "db_operation", "operation");
    match paginate(records, page_no, page_size) {
        Ok(page) => page_response("Get access log success.", page),
        Err(e) => GResp::error(Status::OperationFailed, format!("Get access log fail: {e}")),
    }
}

/// `txnlog` — one page of the transaction log.
pub(super) fn txnlog(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let (page_no, page_size) = match paging(p) {
        Ok(pg) => pg,
        Err(r) => return r,
    };
    match paginate(txn_records(ctx.root_dir()), page_no, page_size) {
        Ok(page) => page_response("Get transaction log success.", page),
        Err(e) => GResp::error(
            Status::OperationFailed,
            format!("Get transaction log fail: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recs(n: usize) -> Vec<Json> {
        (1..=n).map(|i| Json::obj().with("i", i)).collect()
    }

    fn ids(page: &Page) -> Vec<i64> {
        page.items
            .iter()
            .map(|j| j.get("i").and_then(Json::as_i64).unwrap())
            .collect()
    }

    #[test]
    fn pages_are_newest_first() {
        let p = paginate(recs(12), 1, 5).unwrap();
        assert_eq!((p.total_size, p.total_page), (12, 3));
        assert_eq!(ids(&p), vec![12, 11, 10, 9, 8]);
        assert_eq!(ids(&paginate(recs(12), 3, 5).unwrap()), vec![2, 1]);
        let p = paginate(recs(12), 0, 0).unwrap();
        assert_eq!((p.page_no, p.page_size), (1, 10));
        assert_eq!(ids(&p).len(), 10);
        assert_eq!(
            paginate(recs(12), 4, 5).unwrap_err(),
            "more then max page number 3"
        );
        let p = paginate(Vec::new(), 3, 5).unwrap();
        assert_eq!((p.total_size, p.total_page, p.items.len()), (0, 0, 0));
    }

    #[test]
    fn access_records_follow_upstream_fields() {
        let mut rec = AccessLogRecord {
            ip: "127.0.0.1".into(),
            operation: "batchInsert".into(),
            code: 0,
            msg: "ok".into(),
            createtime: "2026-01-02 03:04:05".into(),
            db_name: "lubm".into(),
            num: 3,
            fail_num: 1,
            ..Default::default()
        };
        let j = record_line(rec.to_json());
        assert_eq!(
            j,
            r#"{"code":0,"createtime":"2026-01-02 03:04:05","dbname":"lubm","fail_num":1,"ip":"127.0.0.1","msg":"ok","num":3,"operation":"batchInsert","state":1}"#
        );
        rec.operation = "login".into();
        rec.db_name.clear();
        let j = record_line(rec.to_json());
        assert!(
            !j.contains("state") && !j.contains("num") && !j.contains("dbname"),
            "{j}"
        );
        assert_eq!(clean_msg("a\r\n    b"), "ab");
    }

    #[test]
    fn txn_records_keep_the_last_state_per_tid() {
        let root = std::env::temp_dir().join(format!(
            "gstore_apilog_txn_{}_{}",
            std::process::id(),
            clock::unix_millis()
        ));
        let logs = ApiLogs::new();
        logs.txn_begin(&root, "lubm", 1, "alice");
        logs.txn_begin(&root, "lubm", 2, "bob");
        logs.txn_end(&root, "lubm", 1, "alice", TxnState::Committed);
        let all = txn_records(&root);
        assert_eq!(all.len(), 2);
        let state = |j: &Json| j.get("state").and_then(Json::as_str).unwrap().to_string();
        assert_eq!(state(&all[0]), "COMMITED");
        assert_eq!(state(&all[1]), "RUNNING");
        assert_eq!(all[1].get("end_time").and_then(Json::as_str), Some("INF"));
        let tid = all[0].get("TID").and_then(Json::as_str).unwrap();
        assert!(tid.ends_with("_1"), "{tid}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn daily_files_are_pruned_beyond_the_retention() {
        let dir = std::env::temp_dir().join(format!(
            "gstore_apilog_prune_{}_{}",
            std::process::id(),
            clock::unix_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        for d in ["20200101", "20200102", "20200103", "notadate"] {
            fs::write(dir.join(format!("{d}.log")), "{}\n").unwrap();
        }
        let logs = ApiLogs::new();
        logs.append_daily(&dir, 2, "{\"x\":1}").unwrap();
        let today = clock::now().date_compact();
        assert_eq!(log_dates(&dir), vec![today, "20200103".to_string()]);
        assert!(dir.join("notadate.log").exists(), "only dated files count");
        let _ = fs::remove_dir_all(&dir);
    }
}
