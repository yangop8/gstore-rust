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
//!   `accesslog_mode` is on. An operation run asynchronously (`async=true`)
//!   is recorded under its `opt_id`: once when submitted (`state` 0), and
//!   again when it ends — its final `code` / `msg` and `state` (`1`
//!   succeeded, `-1` failed or cancelled), with the submission's
//!   `createtime`. Upstream rewrites the record in place; here the final
//!   record is appended and the reader lists one record per `opt_id` (the
//!   final one, where the submission was), as for the transaction log.
//! * **transaction log** — `<root>/logs/transaction.json` (upstream's
//!   `logs/transaction.json`): `db_name`, `TID` (`<begin ms>_<tid>`, as
//!   upstream), `user`, `state` (`RUNNING`, then `COMMITED` / `ROLLBACK`),
//!   `begin_time` and `end_time` (ms, `INF` while running). Upstream rewrites
//!   a transaction's record in place when it ends; here the final record is
//!   appended (crash-safe, O(1)) and the reader keeps the last record of each
//!   `TID`. Unlike upstream's, the file is rotated at [`TXN_LOG_MAX_BYTES`]
//!   (to `transaction.json.1`, one older generation kept; the transactions
//!   still running are carried over), so `txnlog` lists the transactions of
//!   the current generation.
//!
//! Every record is appended as one line in one write. A line a crash, a full
//! disk or a power loss left without its newline is ended before the next
//! record is appended (review durability#13), so it never swallows that
//! record; readers skip it.
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
//! `pageNo` below 1 is 1 and `pageSize` below 1 is 10. A `txnlog` read
//! parses only the lines appended since the previous one (the server keeps
//! where each transaction's last record is, not the records), then the
//! records of its page (review gapi#18); a daily log is read whole, as it is
//! bounded by its day. `querylog` and
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
use std::hash::BuildHasher;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::tasks::TaskInfo;
use super::{required, Ctx};
use crate::config::GStoreConfig;
use crate::json::{self, Json};
use crate::sysdb::clock;

/// The transaction log, relative to the server root.
pub const TXN_LOG_FILE: &str = "logs/transaction.json";

/// The size at which the transaction log is rotated (review gapi#18): an
/// append that would make [`TXN_LOG_FILE`] larger moves it to
/// `transaction.json.1` (replacing an older one) and starts a new file
/// holding the records of the transactions still open. So the log takes at
/// most about twice this on disk, and `txnlog` lists the transactions of
/// the current file (about 120 000 at 256 bytes per begin + end).
pub const TXN_LOG_MAX_BYTES: u64 = 32 << 20;

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
    /// The `state` of an asynchronous operation's final record (upstream's
    /// `1` succeeded, `-1` failed); `None` derives it (see
    /// [`to_json`](Self::to_json)).
    pub state: Option<i64>,
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
    /// running) 0, unless [`state`](Self::state) says otherwise. Every record
    /// with an `opt_id` carries a `state`.
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj()
            .with("ip", self.ip.as_str())
            .with("operation", self.operation.as_str())
            .with("code", self.code)
            .with("msg", self.msg.as_str())
            .with("createtime", self.createtime.as_str());
        if Self::has_state(&self.operation) || !self.opt_id.is_empty() {
            let derived = if self.opt_id.is_empty() { 1 } else { 0 };
            j.set("state", self.state.unwrap_or(derived));
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
    txn_begun: Mutex<HashMap<(String, u64), u128>>,
    /// The transaction log as far as it was read (review gapi#18). Lock
    /// order: `io` → `txn_index`.
    txn_index: Mutex<TxnIndex>,
    /// Bytes of the transaction log parsed into `txn_index` so far.
    txn_parsed: AtomicU64,
}

/// Where in the transaction log one transaction's last record is.
#[derive(Debug, Clone, Copy)]
struct TxnEntry {
    offset: u64,
    len: u32,
    /// That record's `state` is `RUNNING`.
    running: bool,
}

/// What a read of the transaction log learnt (review gapi#18): for each
/// transaction, in the order they began, where its last record is — so a
/// `txnlog` read parses only the lines appended since the previous read,
/// and then only the records of the page it answers. About 40 bytes per
/// transaction: never the records themselves.
#[derive(Debug, Default)]
struct TxnIndex {
    /// The file indexed (device and inode); `None` before the first read.
    file: Option<(u64, u64)>,
    /// How much of it was read: whole lines only.
    offset: u64,
    /// The transactions in the order they began.
    entries: Vec<TxnEntry>,
    /// Each transaction's place in `entries`, by a 128-bit fingerprint of
    /// its database and `TID` (ids are numbered per database).
    by_key: HashMap<u128, u32>,
    /// The two keys of the fingerprint (random per process: the index
    /// lives in memory only).
    hashers: (std::hash::RandomState, std::hash::RandomState),
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
        self.txn_begin_at(root, db, tid, user, clock::unix_millis());
    }

    /// [`txn_begin`](Self::txn_begin) at `begun` (Unix milliseconds): the
    /// begin time a server keeps with the transaction, so that the
    /// `<begun>_<tid>` form of the record's `TID` names it exactly.
    pub fn txn_begin_at(&self, root: &Path, db: &str, tid: u64, user: &str, begun: u128) {
        self.txn_begun
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((db.to_string(), tid), begun);
        let rec = txn_record(db, tid, user, TxnState::Running, begun, None);
        let _ = self.append_txn(root, &[record_line(rec)]);
    }

    /// Record that transaction `tid` (of `user`, on `db`) ended in `state`.
    /// Best effort.
    pub fn txn_end(&self, root: &Path, db: &str, tid: u64, user: &str, state: TxnState) {
        let ended = clock::unix_millis();
        let begun = self
            .txn_begun
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(db.to_string(), tid))
            .unwrap_or(ended);
        let rec = txn_record(db, tid, user, state, begun, Some(ended));
        let _ = self.append_txn(root, &[record_line(rec)]);
    }

    /// Mark every transaction the log still shows `RUNNING` as `ABORTED`
    /// (review gap-txn-lifecycle#2): run once by a server as it starts, when
    /// no transaction of its own is open, so these are the transactions a
    /// previous process left behind (killed, crashed) — never committed,
    /// and undone (RocksDB, Stream mode) or lost (in memory) with it. Their
    /// records keep their `TID` and begin time; the end time is now. Best
    /// effort. Returns how many were marked.
    pub fn abort_stale_running(&self, root: &Path) -> usize {
        let path = txnlog_path(root);
        if !path.exists() {
            return 0;
        }
        let ended = clock::unix_millis();
        let lines: Vec<String> = self
            .running_records(&path)
            .into_iter()
            .map(|mut rec| {
                rec.set("state", TxnState::Aborted.as_str());
                rec.set("end_time", ended.to_string());
                record_line(rec)
            })
            .collect();
        self.append_txn(root, &lines).unwrap_or(0)
    }

    /// The last records of the transactions the log shows `RUNNING`, in
    /// the order they began (read through the index: the log is not held
    /// in memory).
    fn running_records(&self, path: &Path) -> Vec<Json> {
        let Ok(mut file) = fs::File::open(path) else {
            return Vec::new();
        };
        let mut index = self.txn_index();
        index.refresh(&mut file, &self.txn_parsed);
        let running: Vec<TxnEntry> = index
            .entries
            .iter()
            .copied()
            .filter(|e| e.running)
            .collect();
        running
            .iter()
            .filter_map(|e| read_entry(&mut file, e))
            .collect()
    }

    /// Append `lines` (records) to the transaction log, rotating it first
    /// when they would take it past [`TXN_LOG_MAX_BYTES`] (review
    /// gapi#18). Returns how many were appended.
    fn append_txn(&self, root: &Path, lines: &[String]) -> io::Result<usize> {
        let path = txnlog_path(root);
        let _guard = self.io();
        let len = fs::metadata(&path).map_or(0, |m| m.len());
        let adding: u64 = lines.iter().map(|l| l.len() as u64 + 2).sum();
        if len > 0 && len.saturating_add(adding) > TXN_LOG_MAX_BYTES {
            if let Err(e) = self.rotate_txn_log(&path) {
                crate::gs_warn!(
                    "could not rotate the transaction log {}: {e}",
                    path.display()
                );
            }
        }
        let mut appended = 0;
        for line in lines {
            append_line(&path, line)?;
            appended += 1;
        }
        Ok(appended)
    }

    /// Move the transaction log to `transaction.json.1` (replacing an older
    /// generation) and start a new one holding the records of the
    /// transactions still running, so their ends still find them. Run under
    /// the `io` lock. A crash part-way leaves the old log in place or the
    /// new one complete (it is written beside and renamed over).
    fn rotate_txn_log(&self, path: &Path) -> io::Result<()> {
        let running = self.running_records(path);
        let fresh = path.with_file_name("transaction.json.tmp");
        let rotated = path.with_file_name("transaction.json.1");
        {
            let mut f = fs::File::create(&fresh)?;
            let mut text = String::new();
            for rec in running {
                text.push_str(&record_line(rec));
                text.push('\n');
            }
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        let mut index = self.txn_index();
        *index = TxnIndex::default();
        // The old generation is linked under its new name first, so the
        // log is never missing; a file system without hard links moves it.
        let _ = fs::remove_file(&rotated);
        if fs::hard_link(path, &rotated).is_err() {
            if let Err(e) = fs::rename(path, &rotated) {
                let _ = fs::remove_file(&fresh);
                return Err(e);
            }
        }
        fs::rename(&fresh, path)
    }

    fn txn_index(&self) -> MutexGuard<'_, TxnIndex> {
        self.txn_index.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One page of the transaction log, newest first: one record per
    /// transaction (its last), as [`txn_records`] lists them. Parses only
    /// the lines appended since the previous read, then the page's records
    /// (review gapi#18).
    pub fn txn_page(&self, root: &Path, page_no: i64, page_size: i64) -> Result<Page, String> {
        let Ok(mut file) = fs::File::open(txnlog_path(root)) else {
            return paginate(Vec::new(), page_no, page_size);
        };
        let mut index = self.txn_index();
        index.refresh(&mut file, &self.txn_parsed);
        let b = page_bounds(index.entries.len(), page_no, page_size)?;
        let items = index.entries[b.start..b.end]
            .iter()
            .rev()
            .filter_map(|e| read_entry(&mut file, e))
            .collect();
        Ok(Page {
            total_size: b.total_size,
            total_page: b.total_page,
            page_no: b.page_no,
            page_size: b.page_size,
            items,
        })
    }

    /// How many bytes of the transaction log were parsed into the index
    /// `txnlog` reads from (for tests: a read parses what was appended).
    #[doc(hidden)]
    pub fn txn_log_bytes_parsed(&self) -> u64 {
        self.txn_parsed.load(Ordering::Relaxed)
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

impl TxnIndex {
    /// Bring the index up to date with `file` (the transaction log, opened
    /// now): parse the whole lines appended since the last read — all of
    /// it when it is another file than the one indexed (rotated, replaced)
    /// or shorter than what was read. A line without its newline yet (being
    /// written, or torn: the next append ends it) is left for later.
    fn refresh(&mut self, file: &mut fs::File, parsed: &AtomicU64) {
        let Ok(meta) = file.metadata() else {
            return;
        };
        let id = file_id(&meta);
        if self.file != Some(id) || meta.len() < self.offset {
            *self = TxnIndex {
                file: Some(id),
                hashers: std::mem::take(&mut self.hashers),
                ..TxnIndex::default()
            };
        }
        if meta.len() == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut reader = io::BufReader::new(file.take(meta.len() - self.offset));
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(n) if n > 0 && line.ends_with(b"\n") => {}
                _ => break,
            }
            let at = self.offset;
            self.offset += line.len() as u64;
            parsed.fetch_add(line.len() as u64, Ordering::Relaxed);
            let Some(rec) = std::str::from_utf8(&line).ok().and_then(parse_record) else {
                continue;
            };
            let field = |k: &str| rec.get(k).and_then(Json::as_str).unwrap_or_default();
            let key = self.fingerprint(field("db_name"), field("TID"));
            let entry = TxnEntry {
                offset: at,
                len: u32::try_from(line.len()).unwrap_or(u32::MAX),
                running: field("state") == TxnState::Running.as_str(),
            };
            match self.by_key.get(&key) {
                Some(&i) => self.entries[i as usize] = entry,
                None => {
                    let i = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
                    self.by_key.insert(key, i);
                    self.entries.push(entry);
                }
            }
        }
    }

    fn fingerprint(&self, db: &str, tid: &str) -> u128 {
        let key = (db, tid);
        (u128::from(self.hashers.0.hash_one(key)) << 64) | u128::from(self.hashers.1.hash_one(key))
    }
}

/// The identity of an open file (device and inode; on other systems none:
/// a rotation by this server resets the index itself).
fn file_id(meta: &fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
}

/// The record `entry` locates in `file`.
fn read_entry(file: &mut fs::File, entry: &TxnEntry) -> Option<Json> {
    file.seek(SeekFrom::Start(entry.offset)).ok()?;
    let mut buf = vec![0; entry.len as usize];
    file.read_exact(&mut buf).ok()?;
    parse_record(std::str::from_utf8(&buf).ok()?)
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

/// Append `line` and a newline in one write (`O_APPEND`). A file whose last
/// byte is not a newline — a record torn by a crash or a full disk, a tail
/// a power loss filled with zeros — first gets one (review durability#13):
/// the torn line stays one line of its own, which readers skip, instead of
/// swallowing this record.
fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    let mut buf = Vec::with_capacity(line.len() + 2);
    if ends_unterminated(&mut f)? {
        buf.push(b'\n');
    }
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    f.write_all(&buf)
}

/// Whether `f` is non-empty and its last byte is not a newline.
fn ends_unterminated(f: &mut fs::File) -> io::Result<bool> {
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
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
    text.lines().filter_map(parse_record).collect()
}

/// One log line as a record (upstream also tolerates a trailing comma).
fn parse_record(line: &str) -> Option<Json> {
    let line = line.trim();
    let line = line.strip_suffix(',').unwrap_or(line);
    match json::parse(line) {
        Ok(j @ Json::Obj(_)) => Some(j),
        _ => None,
    }
}

/// One page of the daily log file `path`, newest first, of the records whose
/// string field `key` equals the wanted value for every `(key, value)` of
/// `filters`, an asynchronous operation's records folded into one
/// ([`fold_opt_ids`]). Without filters and without such records only the
/// page's own lines are parsed — every non-blank line counts, and one that
/// does not parse (torn by a crash) is left out of its page, as upstream
/// does.
fn page_of_file(
    path: &Path,
    filters: &[(&str, String)],
    page_no: i64,
    page_size: i64,
) -> Result<Page, String> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if filters.is_empty() && !text.contains("\"opt_id\"") {
        let page = paginate(lines.collect::<Vec<&str>>(), page_no, page_size)?;
        return Ok(Page {
            total_size: page.total_size,
            total_page: page.total_page,
            page_no: page.page_no,
            page_size: page.page_size,
            items: page.items.into_iter().filter_map(parse_record).collect(),
        });
    }
    let records = fold_opt_ids(lines.filter_map(parse_record).collect())
        .into_iter()
        .filter(|r| {
            filters
                .iter()
                .all(|(k, want)| r.get(k).and_then(Json::as_str) == Some(want.as_str()))
        })
        .collect();
    paginate(records, page_no, page_size)
}

/// One record per `opt_id` (an asynchronous operation's submission and
/// final records, see the module docs): its final record — a non-zero
/// `state`, else the latest — where its first was. Records without an
/// `opt_id` are kept as they are, in order.
fn fold_opt_ids(records: Vec<Json>) -> Vec<Json> {
    let is_final = |r: &Json| {
        r.get("state")
            .and_then(Json::as_i64)
            .is_some_and(|s| s != 0)
    };
    let mut slot: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Json> = Vec::with_capacity(records.len());
    for rec in records {
        let id = rec
            .get("opt_id")
            .and_then(Json::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let Some(id) = id else {
            out.push(rec);
            continue;
        };
        match slot.get(&id) {
            Some(&i) => {
                if is_final(&rec) || !is_final(&out[i]) {
                    out[i] = rec;
                }
            }
            None => {
                slot.insert(id, out.len());
                out.push(rec);
            }
        }
    }
    out
}

/// The transaction-log records, one per transaction (the last record of each
/// `TID`), in the order the transactions began.
pub fn txn_records(root: &Path) -> Vec<Json> {
    let mut order: Vec<String> = Vec::new();
    let mut last: HashMap<String, Json> = HashMap::new();
    for rec in read_records(&txnlog_path(root)) {
        // Ids are numbered per database: two databases' transactions begun in
        // the same millisecond share a `TID`.
        let field = |k: &str| {
            rec.get(k)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let tid = format!("{}\u{0}{}", field("db_name"), field("TID"));
        if last.insert(tid.clone(), rec).is_none() {
            order.push(tid);
        }
    }
    order.into_iter().filter_map(|t| last.remove(&t)).collect()
}

/// One page of records, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T = Json> {
    pub total_size: usize,
    pub total_page: usize,
    pub page_no: usize,
    pub page_size: usize,
    pub items: Vec<T>,
}

/// Page `page_no` (from 1; below 1 is 1) of `page_size` records (below 1 is
/// 10) of `records` (oldest first), newest first — upstream
/// `get_file_lines`. A page beyond the last is an error naming the last.
pub fn paginate<T>(records: Vec<T>, page_no: i64, page_size: i64) -> Result<Page<T>, String> {
    let b = page_bounds(records.len(), page_no, page_size)?;
    let mut items = records;
    items.truncate(b.end);
    let mut items: Vec<T> = items.drain(b.start..).collect();
    items.reverse();
    Ok(Page {
        total_size: b.total_size,
        total_page: b.total_page,
        page_no: b.page_no,
        page_size: b.page_size,
        items,
    })
}

/// A page of `total_size` records (oldest first): which of them it holds
/// (`start..end`, listed in reverse), with the normalized paging figures.
struct PageBounds {
    total_size: usize,
    total_page: usize,
    page_no: usize,
    page_size: usize,
    start: usize,
    end: usize,
}

/// The bounds of page `page_no` (see [`paginate`]).
fn page_bounds(total_size: usize, page_no: i64, page_size: i64) -> Result<PageBounds, String> {
    let page_no = usize::try_from(page_no.max(1)).unwrap_or(1);
    let page_size = if page_size < 1 {
        DEFAULT_PAGE_SIZE as usize
    } else {
        usize::try_from(page_size).unwrap_or(usize::MAX)
    };
    let total_page = total_size.div_ceil(page_size);
    let mut b = PageBounds {
        total_size,
        total_page,
        page_no,
        page_size,
        start: 0,
        end: 0,
    };
    if total_size == 0 {
        return Ok(b);
    }
    if page_no > total_page {
        return Err(format!("more then max page number {total_page}"));
    }
    // Newest first: page 1 is the last `page_size` records, reversed.
    b.end = total_size - page_size.saturating_mul(page_no - 1);
    b.start = b.end.saturating_sub(page_size);
    Ok(b)
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// The access-log hook of the dispatcher: record one answered operation
/// (upstream registers the same callback in `checkRequest`). Unknown
/// operations are not recorded. An accepted asynchronous submission
/// (`async=true` on an operation that supports it, answered with an
/// `opt_id`) is recorded under that `opt_id` with `state` 0; its runner
/// records the outcome later ([`log_async_finish`]).
pub(super) fn log_access(ctx: &Ctx<'_>, params: &Params, resp: &GResp) {
    if ctx.op.is_empty() || resp.status_code() == Status::OperationUndefined.code() {
        return;
    }
    let config = ctx.inner.config();
    if !config.accesslog_mode {
        return;
    }
    let submitted = super::registry()
        .get(&ctx.op)
        .is_some_and(|spec| spec.async_capable && params.bool("async", false));
    let opt_id = if submitted {
        resp.body_json()
            .and_then(|b| b.get("opt_id"))
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    };
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
        opt_id,
        state: None,
    };
    ctx.inner
        .apilogs
        .write_access(&config, &ctx.inner.root, &rec);
}

/// The final access-log record of asynchronous operation `info` (submitted
/// from `ip`): its outcome under its `opt_id`, dated like its submission
/// record (upstream `update_access_log`). Run by the task manager once the
/// operation has ended.
pub(crate) fn log_async_finish(inner: &crate::http_api::ApiInner, ip: &str, info: &TaskInfo) {
    let config = inner.config();
    if !config.accesslog_mode {
        return;
    }
    let started = SystemTime::UNIX_EPOCH + Duration::from_millis(info.start_ms);
    let filepath = match (&info.result_file, &info.filepath) {
        (Some(file), _) if info.operation == "query" => file.display().to_string(),
        (_, Some(path)) => path.clone(),
        _ => String::new(),
    };
    let rec = AccessLogRecord {
        ip: ip.to_string(),
        operation: info.operation.clone(),
        code: info.status_code.unwrap_or(Status::OperationFailed.code()),
        msg: clean_msg(&info.status_msg),
        createtime: clock::at(started).datetime(),
        db_name: info.db_name.clone(),
        num: info.success_num,
        fail_num: info.failed_num,
        filepath,
        opt_id: info.opt_id.to_string(),
        state: Some(info.state.state_code()),
    };
    inner.apilogs.write_access(&config, &inner.root, &rec);
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
        crate::http_api::log_query(&ctx.inner.config(), ctx.root_dir(), db, ctx.user(), sparql);
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
        ctx.inner
            .apilogs
            .write_query(&config, &ctx.inner.root, &rec);
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

/// The record filters of a log request: for each `(parameter, record
/// key)`, the parameter's value when given.
fn filters<'k>(p: &Params, pairs: &[(&str, &'k str)]) -> Vec<(&'k str, String)> {
    pairs
        .iter()
        .filter_map(|(param, key)| Some((*key, p.non_empty(param)?.trim().to_string())))
        .collect()
}

/// `querylogdate` — the days with a query log, newest first.
pub(super) fn querylogdate(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let dir = querylog_dir(&ctx.inner.config(), ctx.root_dir());
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
    let dir = querylog_dir(&ctx.inner.config(), ctx.root_dir());
    let path = dir.join(format!("{date}.log"));
    let filters = filters(p, &[("db_name", "dbname")]);
    match page_of_file(&path, &filters, page_no, page_size) {
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
    let dir = accesslog_dir(&ctx.inner.config(), ctx.root_dir());
    dates_response("Get access log date success", &dir)
}

/// `accesslog` — one page of a day's access log.
pub(super) fn accesslog(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let (date, (page_no, page_size)) = match (date_param(p), paging(p)) {
        (Ok(d), Ok(pg)) => (d, pg),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let dir = accesslog_dir(&ctx.inner.config(), ctx.root_dir());
    let path = dir.join(format!("{date}.log"));
    let filters = filters(p, &[("db_name", "dbname"), ("db_operation", "operation")]);
    match page_of_file(&path, &filters, page_no, page_size) {
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
    match ctx
        .inner
        .apilogs
        .txn_page(ctx.root_dir(), page_no, page_size)
    {
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
        let p = paginate(Vec::<Json>::new(), 3, 5).unwrap();
        assert_eq!((p.total_size, p.total_page, p.items.len()), (0, 0, 0));
    }

    #[test]
    fn file_pages_parse_only_their_lines_unless_filtered() {
        let path = std::env::temp_dir().join(format!(
            "gstore_apilog_page_{}_{}.log",
            std::process::id(),
            clock::unix_millis()
        ));
        let text =
            "{\"i\":1,\"k\":\"a\"}\n{\"i\":2,\"k\":\"b\"}\n{torn\n\n{\"i\":4,\"k\":\"a\"},\n";
        fs::write(&path, text).unwrap();
        let p = page_of_file(&path, &[], 1, 2).unwrap();
        assert_eq!((p.total_size, p.total_page), (4, 2), "the torn line counts");
        assert_eq!(ids(&p), vec![4], "but is not listed");
        let p = page_of_file(&path, &[("k", "a".to_string())], 1, 10).unwrap();
        assert_eq!((p.total_size, ids(&p)), (2, vec![4, 1]));
        let missing = path.with_extension("nope");
        assert_eq!(page_of_file(&missing, &[], 1, 10).unwrap().total_size, 0);
        let _ = fs::remove_file(&path);
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
    fn async_records_fold_into_their_final_record() {
        let submitted = AccessLogRecord {
            ip: "127.0.0.1".into(),
            operation: "query".into(),
            code: 0,
            msg: "Operation Success.".into(),
            createtime: "2026-01-02 03:04:05".into(),
            db_name: "lubm".into(),
            opt_id: "77".into(),
            ..Default::default()
        };
        let j = record_line(submitted.to_json());
        assert!(
            j.contains("\"opt_id\":\"77\"") && j.contains("\"state\":0"),
            "{j}"
        );
        let finished = AccessLogRecord {
            code: 14,
            msg: "failed".into(),
            state: Some(-1),
            ..submitted.clone()
        };
        let j = record_line(finished.to_json());
        assert!(
            j.contains("\"state\":-1") && j.contains("\"code\":14"),
            "{j}"
        );
        // An operation the access log gives no state still gets one under
        // an opt_id.
        let other = AccessLogRecord {
            operation: "__asyncTest".into(),
            opt_id: "78".into(),
            ..Default::default()
        };
        assert!(record_line(other.to_json()).contains("\"state\":0"));

        let plain = |i: i64| Json::obj().with("i", i);
        let rec = |id: &str, state: i64, i: i64| {
            Json::obj()
                .with("opt_id", id)
                .with("state", state)
                .with("i", i)
        };
        // The final record wins, at the submission's place — also when it
        // was written first (a fast operation).
        let folded = fold_opt_ids(vec![
            plain(1),
            rec("a", 0, 2),
            rec("b", 1, 3),
            plain(4),
            rec("a", 1, 5),
            rec("b", 0, 6),
            rec("c", 0, 7),
        ]);
        let got: Vec<i64> = folded
            .iter()
            .map(|j| j.get("i").and_then(Json::as_i64).unwrap())
            .collect();
        assert_eq!(got, vec![1, 5, 3, 4, 7]);

        let path = std::env::temp_dir().join(format!(
            "gstore_apilog_fold_{}_{}.log",
            std::process::id(),
            clock::unix_millis()
        ));
        let lines: Vec<String> = [plain(1), rec("a", 0, 2), plain(3), rec("a", -1, 4)]
            .into_iter()
            .map(record_line)
            .collect();
        fs::write(&path, lines.join("\n")).unwrap();
        let p = page_of_file(&path, &[], 1, 10).unwrap();
        assert_eq!(p.total_size, 3);
        assert_eq!(ids(&p), vec![3, 4, 1]);
        let _ = fs::remove_file(&path);
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

    /// Ids are numbered per database: transaction 1 of two databases, open
    /// at once, keeps its own begin time and outcome.
    #[test]
    fn txn_records_tell_databases_apart() {
        let root = std::env::temp_dir().join(format!(
            "gstore_apilog_txn2_{}_{}",
            std::process::id(),
            clock::unix_millis()
        ));
        let logs = ApiLogs::new();
        logs.txn_begin(&root, "dbaa", 1, "alice");
        std::thread::sleep(std::time::Duration::from_millis(5));
        logs.txn_begin(&root, "dbbb", 1, "bob");
        std::thread::sleep(std::time::Duration::from_millis(5));
        logs.txn_end(&root, "dbaa", 1, "alice", TxnState::Committed);
        logs.txn_end(&root, "dbbb", 1, "bob", TxnState::RolledBack);
        let all = txn_records(&root);
        assert_eq!(all.len(), 2, "{all:?}");
        let get = |j: &Json, k: &str| j.get(k).and_then(Json::as_str).unwrap().to_string();
        let a = all.iter().find(|j| get(j, "db_name") == "dbaa").unwrap();
        let b = all.iter().find(|j| get(j, "db_name") == "dbbb").unwrap();
        assert_eq!(get(a, "state"), "COMMITED");
        assert_eq!(get(b, "state"), "ROLLBACK");
        assert!(get(a, "begin_time") < get(b, "begin_time"), "{a} {b}");
        assert_ne!(get(b, "begin_time"), get(b, "end_time"), "{b}");
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
