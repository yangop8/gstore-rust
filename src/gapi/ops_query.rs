//! Query and transaction operations: `query`, `begin`, `tquery`, `commit`,
//! `rollback`, `checkpoint` (upstream `src/Server/ApiQueryOperation.cpp`,
//! `ApiTransactionOperation.cpp`, `ApiUpdateOperation.cpp`, and the response
//! classes in `src/Server/MessageProtocol/`).
//!
//! They run on the same machinery as the REST endpoints of
//! [`crate::http_api`]: queries go through the task registry (so `/tasklist`
//! and `/cancel` see them), reads during an open transaction are answered from
//! its begin-time snapshot, writes are refused while one is open, every
//! committed write is saved under the check_point policy, and a transaction
//! belongs to the user who began it.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::{auth, required, tasks, Ctx, OpSpec, Registry};
use crate::db::Statement;
use crate::http_api::{
    log_query, ApiInner, DbHandle, JobOutput, OpError, OpErrorKind, QueryJob, TxnIsolation,
};
use crate::json::Json;
use crate::parser::sparql::ast::Query;
use crate::query::{QueryResult, ResultSet};
use crate::sparql_results::{
    self, upstream_ask_result, upstream_binding, upstream_ntriple_json, upstream_select_json,
    ResultFormat,
};

pub(super) fn register(reg: &mut Registry) {
    reg.add(OpSpec::new("query", query).needs_db().async_capable());
    reg.add(OpSpec::new("begin", begin).needs_db());
    reg.add(OpSpec::new("tquery", tquery).needs_db());
    reg.add(OpSpec::new("commit", commit).needs_db());
    reg.add(OpSpec::new("rollback", rollback).needs_db());
    reg.add(OpSpec::new("checkpoint", checkpoint).needs_db());
}

/// A `query` output format (upstream `query_format_response_data`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutFormat {
    /// The result in the response, in gStore's JSON shape.
    Json,
    /// The (uncapped) JSON result written to a file in the export directory;
    /// the response names it (`FileName`).
    File,
    /// The result in the response as rows of N-Triples terms.
    NTriple,
    /// The (uncapped) result written as tab-separated N-Triples terms to a
    /// `.txt` file in the export directory; the response names it.
    NTripleFile,
}

impl OutFormat {
    /// The format named by `format=` (default `json`; an empty value is
    /// `json` too). Unknown names (e.g. the `html` older upstream docs list)
    /// are `None`, and handled as upstream does: an update ignores the format,
    /// a read is run and then answered `1005 "Unknown result format."` — or,
    /// run asynchronously (where upstream does not check the format), as
    /// `json`.
    fn parse(v: Option<&str>) -> Option<OutFormat> {
        match v.map(str::trim).unwrap_or("") {
            "" | "json" => Some(OutFormat::Json),
            "file" => Some(OutFormat::File),
            "n-triple" => Some(OutFormat::NTriple),
            "n-triple-file" => Some(OutFormat::NTripleFile),
            _ => None,
        }
    }
}

/// `query` — run a SPARQL query or update against `db_name`.
///
/// A read answers upstream's `MessageQueryResponse`: the result (per `format`)
/// merged with `StatusCode`, `StatusMsg` (`"success"`), `AnsNum` (the number
/// of answers), `OutputLimit` (the query's `LIMIT`, or `-1`; lowered to
/// `max_output_size` when there are more answers than that — only that many
/// are then returned), `ThreadId` and `QueryTime` (milliseconds, a string).
/// ASK is the one-row `_askResult` table; CONSTRUCT / DESCRIBE (a Rust
/// extension) an `s`/`p`/`o` table. An update answers `StatusMsg` `"update
/// query returns true."` with `AnsNum` = the triples it changed.
fn query(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = ctx.db_name.clone().unwrap_or_default();
    let sparql = match required(p, "sparql") {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fmt_param = p.str("format");
    let format = OutFormat::parse(fmt_param.as_deref())
        .or_else(|| tasks::current().is_some().then_some(OutFormat::Json));
    // `async=true` (an opt_id answered at once, the result fetched later with
    // checkOperationState) is the dispatcher's: this then runs on a task
    // runner, and the statement's task is that operation's (`tasks`).
    // Parsed once, with no lock held: the parse classifies the statement
    // (privilege; a shared guard for a read, the exclusive one for an
    // update) and is what the job runs.
    let stmt = Statement::parse(&sparql).ok();
    let is_update = stmt.as_ref().is_some_and(Statement::is_update);
    // A projected `PFN.name(…)` answers upstream's `{"results": …}` shape.
    let pfn = stmt
        .as_ref()
        .and_then(|s| super::ops_pfn::PfnShape::of(s.query(), &sparql));
    let limit = match stmt.as_ref().map(Statement::query) {
        Some(Query::Select(q)) => q.limit,
        Some(Query::Construct(q)) => q.limit,
        _ => None,
    };
    // Reading needs the query privilege (checked by the dispatcher); an
    // update additionally needs update (upstream passes the update privilege
    // into `Database::query`).
    if is_update && !auth::check_privilege(ctx, "update", Some(&db)) {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "You have no update privilege",
        );
    }
    if !ctx.state().dbs.contains_key(&db) {
        let on_disk = ctx.state().db_dir(&db).is_dir();
        return not_loaded(&db, on_disk);
    }
    // Logged with no lock held (the statement may be large).
    let query_log = super::apilog::QueryLog::start(ctx, &db, &sparql, fmt_param.as_deref());
    let job = {
        let mut st = ctx.state();
        let Some(dbh) = st.dbs.get(&db).map(Arc::clone) else {
            let on_disk = st.db_dir(&db).is_dir();
            drop(st);
            return not_loaded(&db, on_disk);
        };
        let (task_id, cancel) = ctx.inner.register_task(&mut st, &db, &sparql, ctx.user());
        QueryJob {
            db: dbh,
            name: db.clone(),
            cancel,
            task_id,
            sparql,
            stmt,
            fmt: ResultFormat::Json,
            user: ctx.user().to_string(),
        }
    };
    let started = Instant::now();
    let outcome = ctx.inner.run_query_job(job);
    let query_time = started.elapsed().as_millis().to_string();
    let thread_id = thread_id();
    // Every answer from here on — a failure included — is recorded in the
    // query log with its status (review gapi#8).
    let resp = match outcome {
        Err(e) => query_error(e),
        Ok(JobOutput::Updated { changed, warning }) => {
            update_response(changed, warning, &query_time, &thread_id)
        }
        Ok(JobOutput::Read(QueryResult::Update { changed })) => {
            update_response(changed, None, &query_time, &thread_id)
        }
        Ok(JobOutput::Read(result)) => match format {
            None => GResp::error(Status::OperationFailed, "Unknown result format."),
            Some(format) => {
                let rs = result_table(result);
                let pfn = pfn.as_ref();
                read_response(ctx, &db, &rs, format, limit, &query_time, &thread_id, pfn)
                    .unwrap_or_else(|r| r)
            }
        },
    };
    query_log.finish(ctx, &resp);
    resp.with_header("Cache-Control", "no-cache")
        .with_header("Pragma", "no-cache")
        .with_header("Expires", "0")
}

/// The answer of an applied update (upstream: `isUpdate` responses carry no
/// result and no `OutputLimit`).
///
/// An update whose check_point save failed (`warning`) is not acknowledged:
/// it answers `1005` with the warning (`"applied in memory but not durable:
/// <cause>"`) as `StatusMsg` — the code `batchInsert` answers for a failed
/// save, as upstream fails an operation whose save fails — keeping `AnsNum`
/// and the warning as a `"warning"` field (a Rust extension). The HTTP status
/// stays 200, as for every `/api` answer (review durability#2).
fn update_response(
    changed: usize,
    warning: Option<String>,
    query_time: &str,
    thread_id: &str,
) -> GResp {
    let (code, msg) = match &warning {
        None => (0, "update query returns true."),
        Some(w) => (Status::OperationFailed.code(), w.as_str()),
    };
    let mut r = GResp::json(
        Json::obj()
            .with("StatusCode", code)
            .with("StatusMsg", msg)
            .with("AnsNum", changed)
            .with("QueryTime", query_time)
            .with("ThreadId", thread_id),
    );
    if let Some(w) = warning {
        r.set("warning", w);
    }
    r
}

/// A read result as a table (ASK and graph results converted).
fn result_table(result: QueryResult) -> ResultSet {
    match result {
        QueryResult::Select(rs) => rs,
        QueryResult::Ask(b) => upstream_ask_result(b),
        QueryResult::Construct(ts) => sparql_results::triples_as_result_set(&ts),
        QueryResult::Update { .. } => ResultSet::default(),
    }
}

/// The JSON scaffolding counted per cell of an answer on top of its
/// variable name and value: `{"x":{"type":"literal","value":""}}`, its
/// separators and the escapes a value may need.
const CELL_OVERHEAD: u64 = 48;

/// Whether the first `shown` rows of `rs` (all with `None`) would serialize
/// to more than `budget` bytes, by the size of their cells (each row's
/// variable names and values plus [`CELL_OVERHEAD`]) — stopping at the
/// first row past the budget.
fn result_bytes_exceed(rs: &ResultSet, shown: Option<usize>, budget: u64) -> bool {
    let mut total: u64 = 0;
    for row in rs.rows.iter().take(shown.unwrap_or(usize::MAX)) {
        for (var, cell) in rs.vars.iter().zip(row) {
            if let Some(v) = cell {
                total += (var.len() + v.len()) as u64 + CELL_OVERHEAD;
            }
        }
        total += 4;
        if total > budget {
            return true;
        }
    }
    false
}

/// The `OutputLimit` of a result with `ans_num` answers for a query whose
/// `LIMIT` is `limit` (upstream `ApiHandler::query`): the `LIMIT` (or `-1`),
/// lowered to `max` when there are more than `max` answers.
fn output_limit(ans_num: usize, limit: Option<usize>, max: usize) -> i64 {
    let limit = limit.map_or(-1, |l| i64::try_from(l).unwrap_or(i64::MAX));
    if ans_num > max && (limit == -1 || limit > max as i64) {
        max as i64
    } else {
        limit
    }
}

#[allow(clippy::too_many_arguments)]
fn read_response(
    ctx: &Ctx<'_>,
    db: &str,
    rs: &ResultSet,
    format: OutFormat,
    limit: Option<usize>,
    query_time: &str,
    thread_id: &str,
    pfn: Option<&super::ops_pfn::PfnShape>,
) -> Result<GResp, GResp> {
    let ans_num = rs.rows.len();
    let out_limit = output_limit(ans_num, limit, ctx.config.max_output_size);
    let shown = usize::try_from(out_limit).ok();
    // The result byte budget (`max_result_bytes`, review sec#10) of an
    // answer carried in the response: its cells are summed up before the
    // JSON document is built, which is never built past the budget.
    if matches!(format, OutFormat::Json | OutFormat::NTriple) {
        let budget = ctx.inner.config().max_result_bytes;
        if budget != 0 && result_bytes_exceed(rs, shown, budget) {
            return Err(query_error(OpError::new(
                OpErrorKind::ResultTooLarge,
                crate::http_api::result_too_large_msg(budget),
            )));
        }
    }
    // A PFN result replaces the JSON document (upstream `ResultSet::to_JSON`).
    let json = |shown| match pfn.and_then(|p| p.results_json(rs, shown)) {
        Some(body) => body,
        None => upstream_select_json(rs, shown),
    };
    let (mut body, file_name) = match format {
        OutFormat::Json => (json(shown), None),
        OutFormat::NTriple => (upstream_ntriple_json(rs, shown), None),
        // File outputs are not capped (upstream resets the output limit).
        // They are written row by row: no JSON tree, no text of the whole
        // result in memory (review gapi#11).
        OutFormat::File => {
            let write = |w: &mut dyn Write| match pfn.and_then(|p| p.results_json(rs, None)) {
                Some(body) => w.write_all(body.into_sorted().to_string().as_bytes()),
                None => write_upstream_select_json(rs, w),
            };
            (
                Json::obj(),
                Some(write_result_file(ctx, db, thread_id, "json", write)?),
            )
        }
        OutFormat::NTripleFile => (
            Json::obj(),
            Some(write_result_file(ctx, db, thread_id, "txt", |w| {
                write_upstream_tsv(rs, w)
            })?),
        ),
    };
    body.set("StatusCode", 0);
    body.set("StatusMsg", "success");
    body.set("QueryTime", query_time);
    // Upstream drops these for a query that starts with `select (pfn.`
    // (`MessageQueryResponse::is_pfn`).
    if !pfn.is_some_and(|p| p.bare) {
        body.set("AnsNum", ans_num);
        body.set("OutputLimit", out_limit);
        body.set("ThreadId", thread_id);
    }
    if let Some(name) = file_name {
        body.set("FileName", name);
    }
    Ok(GResp::json(body))
}

/// Write a `format=file` / `n-triple-file` result (`write` streams it) into
/// the export directory as `<db>_<threadId>_<YYYYmmddHHMMSS>_<32 hex
/// digits>.<ext>` (upstream's name, the time in UTC, plus 128 random bits so
/// nobody can guess it — review sec#4), with an owner record: the users
/// holding the `query` privilege on `db` may download it. Returns the file
/// name (relative to the export directory, which is where `/file/download`
/// looks).
fn write_result_file(
    ctx: &Ctx<'_>,
    db: &str,
    thread_id: &str,
    ext: &str,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<String, GResp> {
    let fail = |e: io::Error| {
        GResp::error(
            Status::OperationFailed,
            format!("Query fail: could not write the result file: {e}"),
        )
    };
    let dir = ctx.export_dir();
    fs::create_dir_all(&dir).map_err(fail)?;
    super::ops_file::sweep_files(ctx);
    let stem = format!(
        "{db}_{thread_id}_{}_{}",
        compact_utc_now(),
        super::ops_file::random_tag()
    );
    let owner = super::ops_file::Owner::of(ctx, super::ops_file::FileKind::QueryResult, Some(db));
    let (file, name) =
        super::ops_file::create_owned(ctx, &dir, &stem, ext, &owner).map_err(fail)?;
    let mut out = BufWriter::new(file);
    if let Err(e) = write(&mut out).and_then(|()| out.flush()) {
        drop(out);
        super::ops_file::remove_with_owner(ctx, &dir.join(&name));
        return Err(fail(e));
    }
    Ok(name)
}

/// [`upstream_select_json`] of every row, key-sorted
/// (`upstream_select_json(rs, None).into_sorted().to_string()`, byte for
/// byte), written one row at a time.
fn write_upstream_select_json(rs: &ResultSet, w: &mut dyn Write) -> io::Result<()> {
    let vars: Vec<Json> = rs.vars.iter().map(|v| Json::from(v.as_str())).collect();
    let head = Json::obj()
        .with("link", Json::arr())
        .with("vars", Json::Arr(vars))
        .into_sorted();
    write!(w, "{{\"head\":{head},\"results\":{{\"bindings\":[")?;
    let mut first = true;
    for row in &rs.rows {
        let mut b = Json::obj();
        for (ci, cell) in row.iter().enumerate() {
            if let (Some(val), Some(var)) = (cell, rs.vars.get(ci)) {
                b.set(var.as_str(), upstream_binding(val));
            }
        }
        if b.as_object().is_none_or(|e| e.is_empty()) {
            continue;
        }
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        write!(w, "{}", b.into_sorted())?;
    }
    w.write_all(b"]}}")
}

/// [`sparql_results::upstream_tsv`], byte for byte, written one row at a
/// time.
fn write_upstream_tsv(rs: &ResultSet, w: &mut dyn Write) -> io::Result<()> {
    if rs.rows.is_empty() {
        return w.write_all(b"[empty result]\n");
    }
    let head: Vec<String> = rs.vars.iter().map(|v| format!("?{v}")).collect();
    writeln!(w, "{}", head.join("\t"))?;
    for row in &rs.rows {
        let cells: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
        writeln!(w, "{}", cells.join("\t"))?;
    }
    Ok(())
}

#[cfg(test)]
mod result_file_tests {
    use super::*;
    use crate::sparql_results::upstream_tsv;

    fn table(vars: &[&str], rows: &[&[Option<&str>]]) -> ResultSet {
        let mut rs = ResultSet::new(vars.iter().map(|v| v.to_string()).collect());
        for row in rows {
            rs.rows
                .push(row.iter().map(|c| c.map(str::to_string)).collect());
        }
        rs
    }

    /// The streamed file outputs are byte-identical to the documents they
    /// replace (review gapi#11).
    #[test]
    fn streamed_result_files_match_the_buffered_documents() {
        let tables = [
            table(&["x"], &[]),
            table(&[], &[&[]]),
            table(
                &["s", "o", "z"],
                &[
                    &[
                        Some("<http://ex/a>"),
                        Some("\"tab\\there \\\"q\\\" é\"@en"),
                        None,
                    ],
                    &[None, None, None],
                    &[
                        Some("_:b0"),
                        Some("\"5\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
                        Some("\"plain\""),
                    ],
                    &[Some("<http://ex/\u{1F600}>"), Some("not a term"), Some("")],
                ],
            ),
            // A repeated variable name (the later cell wins, as in the tree).
            table(
                &["v", "v"],
                &[&[Some("<http://ex/1>"), Some("<http://ex/2>")]],
            ),
        ];
        for rs in &tables {
            let mut json = Vec::new();
            write_upstream_select_json(rs, &mut json).unwrap();
            assert_eq!(
                String::from_utf8(json).unwrap(),
                upstream_select_json(rs, None).into_sorted().to_string(),
                "{rs:?}"
            );
            let mut tsv = Vec::new();
            write_upstream_tsv(rs, &mut tsv).unwrap();
            assert_eq!(String::from_utf8(tsv).unwrap(), upstream_tsv(rs), "{rs:?}");
        }
    }
}

/// `begin` — open a transaction on `db_name` (`isolevel` 1, 2 or 3, default
/// 1; see [`TxnIsolation`]); answers its `TID` (a string, like upstream).
/// One transaction may be open per database at a time; while it is, other
/// writes to the database are refused and other reads see its begin-time
/// snapshot.
fn begin(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = ctx.db_name.clone().unwrap_or_default();
    let isolation = match p.non_empty("isolevel") {
        None => TxnIsolation::Serializable,
        Some(v) => match TxnIsolation::parse(&v) {
            Some(level) => level,
            None => {
                return GResp::error(
                    Status::ParamIsIllegal,
                    "The isolation level's value only can been 1/2/3",
                )
            }
        },
    };
    let target = {
        let st = ctx.state();
        ApiInner::begin_target(&st, &db).map_err(|e| (e, st.db_dir(&db).is_dir()))
    };
    let dbh = match target {
        Ok(h) => h,
        Err((e, on_disk)) => return begin_error(&db, e, on_disk),
    };
    match ctx.inner.begin_txn(&dbh, &db, isolation, ctx.user()) {
        Ok(tid) => GResp::success("Transaction begin success").with("TID", tid.to_string()),
        Err(e) => begin_error(&db, e, true),
    }
}

fn begin_error(db: &str, e: OpError, on_disk: bool) -> GResp {
    match e.kind {
        OpErrorKind::NotLoaded if on_disk => GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "Database is not loaded yet.",
        ),
        OpErrorKind::NotLoaded => GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            format!("Database[{db}] does not exist."),
        ),
        _ => GResp::error(
            Status::TransactionManageFailed,
            format!("Transaction begin failed: {}", e.msg),
        ),
    }
}

/// Parse a `tid` like upstream's `check_txn_id`: a positive integer, or the
/// Workbench's `beginTime_tid` form.
fn parse_tid(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let digits = raw.rsplit_once('_').map_or(raw, |(_, t)| t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().filter(|&t| t != 0)
}

/// The transaction id parameter, or the `1003` answer.
fn tid_param(p: &Params) -> Result<u64, GResp> {
    let raw = required(p, "tid")?;
    parse_tid(&raw).ok_or_else(|| {
        GResp::error(
            Status::ParamIsIllegal,
            format!("TID {raw} is not a pure number."),
        )
    })
}

/// Upstream's first transaction check (`ApiHandler::transaction_check`):
/// `tquery` / `commit` / `rollback` on a database that is not loaded answer
/// `1004 "Database txn is not loaded yet."` before the `tid` is looked at.
fn txn_db_loaded(ctx: &Ctx<'_>, db: &str) -> Result<(), GResp> {
    if ctx.state().dbs.contains_key(db) {
        Ok(())
    } else {
        Err(GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "Database txn is not loaded yet.",
        ))
    }
}

/// Resolve transaction `tid` of the caller on `db`: its database's handle.
/// Ids are numbered per database (upstream keeps one transaction manager per
/// loaded database), so `tid` is only looked up on `db`.
fn txn_target(ctx: &Ctx<'_>, db: &str, tid: u64) -> Result<Option<DbHandle>, OpError> {
    let st = ctx.state();
    let name = ctx.inner.txn_for(&st, Some(db), tid, ctx.user())?;
    Ok(st.dbs.get(&name).map(Arc::clone))
}

/// `tquery` — run a statement inside transaction `tid`. An update answers
/// `"Transaction query success"` with `AnsNum` = the triples changed; a read
/// the result in gStore's JSON shape with `"success"` (and `AnsNum`, which
/// upstream leaves at 0 — here the number of answers).
fn tquery(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = ctx.db_name.clone().unwrap_or_default();
    if let Err(r) = txn_db_loaded(ctx, &db) {
        return r;
    }
    let tid = match tid_param(p) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let sparql = match required(p, "sparql") {
        Ok(s) => s,
        Err(r) => return r,
    };
    let dbh = match txn_target(ctx, &db, tid) {
        Ok(Some(h)) => h,
        Ok(None) => return not_loaded(&db, true),
        Err(e) => return txn_error("Transaction query fail", e),
    };
    log_query(
        &ctx.inner.config(),
        ctx.root_dir(),
        &db,
        ctx.user(),
        &sparql,
    );
    // Parsed with no lock held; a read then shares the database.
    let stmt = Statement::parse(&sparql);
    match ctx
        .inner
        .run_in_txn_task(&dbh, &db, tid, &sparql, stmt, ctx.user())
    {
        Ok(QueryResult::Update { changed }) => {
            GResp::success("Transaction query success").with("AnsNum", changed)
        }
        Ok(result) => {
            let rs = result_table(result);
            let mut body = upstream_select_json(&rs, None);
            body.set("StatusCode", 0);
            body.set("StatusMsg", "success");
            body.set("AnsNum", rs.rows.len());
            GResp::json(body)
        }
        Err(e) if e.kind == OpErrorKind::Query => GResp::error(Status::OperationFailed, e.msg),
        Err(e) => txn_error("Transaction query fail", e),
    }
}

/// `commit` — commit transaction `tid` (saved under the check_point policy).
fn commit(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    end(ctx, p, true)
}

/// `rollback` — discard transaction `tid`.
fn rollback(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    end(ctx, p, false)
}

fn end(ctx: &mut Ctx<'_>, p: &Params, commit: bool) -> GResp {
    let (verb, done) = if commit {
        ("commit", "Transaction commit success.")
    } else {
        ("rollback", "Transaction rollback success.")
    };
    let fail_prefix = format!("Transaction {verb} fail");
    let db = ctx.db_name.clone().unwrap_or_default();
    if let Err(r) = txn_db_loaded(ctx, &db) {
        return r;
    }
    let tid = match tid_param(p) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let dbh = match txn_target(ctx, &db, tid) {
        Ok(h) => h,
        Err(e) => return txn_error(&fail_prefix, e),
    };
    match ctx
        .inner
        .end_txn(dbh.as_ref(), &db, tid, ctx.user(), commit)
    {
        Ok(None) => GResp::success(done),
        // Committed in memory but not saved: not acknowledged (review
        // durability#2) — the code `batchInsert` answers for a failed save.
        Ok(Some(w)) => {
            GResp::error(Status::OperationFailed, format!("{fail_prefix}: {w}")).with("warning", w)
        }
        Err(e) => txn_error(&fail_prefix, e),
    }
}

/// A transaction-operation failure as upstream reports it.
fn txn_error(prefix: &str, e: OpError) -> GResp {
    match e.kind {
        OpErrorKind::TxnUnknown => GResp::error(
            Status::OperationFailed,
            format!("{prefix}: Wrong transaction id. ({})", e.msg),
        ),
        OpErrorKind::TxnEnded => GResp::error(
            Status::OperationFailed,
            format!("{prefix}: Transaction not in running state! ({})", e.msg),
        ),
        OpErrorKind::NotOwner | OpErrorKind::Conflict => {
            GResp::error(Status::OperationConditionsAreNotSatisfied, e.msg)
        }
        _ => GResp::error(Status::OperationFailed, format!("{prefix}: {}", e.msg)),
    }
}

/// `checkpoint` — save `db_name` to disk now (whatever the check_point
/// policy). Refused while a transaction is open on it.
fn checkpoint(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let db = ctx.db_name.clone().unwrap_or_default();
    let dbh = {
        let st = ctx.state();
        match st.dbs.get(&db).map(Arc::clone) {
            Some(h) => h,
            // Upstream's `validate_databaseinfo` answers (both `1005`).
            None if st.db_dir(&db).is_dir() => {
                return GResp::error(
                    Status::OperationFailed,
                    format!("database[{db}] is not loaded yet"),
                )
            }
            None => {
                return GResp::error(
                    Status::OperationFailed,
                    format!("database[{db}] does not exist"),
                )
            }
        }
    };
    match ApiInner::checkpoint_db(&dbh, &db) {
        Ok(()) => GResp::success("Database saved successfully."),
        Err(e) => match e.kind {
            OpErrorKind::Conflict => {
                GResp::error(Status::OperationConditionsAreNotSatisfied, e.msg)
            }
            _ => GResp::error(
                Status::OperationFailed,
                format!("Checkpoint fail: {}", e.msg),
            ),
        },
    }
}

/// The answer for a database that is not loaded (upstream
/// `validate_databaseinfo` / `query_check`).
fn not_loaded(db: &str, on_disk: bool) -> GResp {
    if on_disk {
        GResp::error(
            Status::OperationFailed,
            format!("database[{db}] is not loaded yet"),
        )
    } else {
        GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "Database does not exist.",
        )
    }
}

/// A failed `query` as upstream reports it.
fn query_error(e: OpError) -> GResp {
    match e.kind {
        OpErrorKind::Conflict | OpErrorKind::NotOwner => {
            GResp::error(Status::OperationConditionsAreNotSatisfied, e.msg)
        }
        OpErrorKind::Query => {
            GResp::error(Status::OperationFailed, format!("Query fail: {}", e.msg))
        }
        // An update that failed part-way and whose applied part could not be
        // saved: `"applied in memory but not durable: …"` (review durability#2).
        OpErrorKind::Persist => GResp::error(Status::OperationFailed, e.msg),
        // `query_timeout` stopped it, or its answer passed
        // `max_result_bytes` (review sec#10).
        OpErrorKind::Timeout | OpErrorKind::ResultTooLarge => {
            GResp::error(Status::OperationFailed, format!("Query fail: {}", e.msg))
        }
        _ => GResp::error(Status::OperationFailed, e.msg),
    }
}

/// The current thread's numeric id (upstream reports its worker thread as
/// `ThreadId`, a string).
fn thread_id() -> String {
    let dbg = format!("{:?}", std::thread::current().id());
    let digits: String = dbg.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        "0".to_string()
    } else {
        digits
    }
}

/// The current UTC time as `YYYYmmddHHMMSS` (upstream's `PURE_DATETIME_PATTERN`,
/// which it renders in local time).
fn compact_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    compact_utc(secs)
}

fn compact_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_parse_strictly() {
        assert_eq!(OutFormat::parse(None), Some(OutFormat::Json));
        assert_eq!(OutFormat::parse(Some("")), Some(OutFormat::Json));
        assert_eq!(OutFormat::parse(Some("json")), Some(OutFormat::Json));
        assert_eq!(OutFormat::parse(Some("file")), Some(OutFormat::File));
        assert_eq!(OutFormat::parse(Some("n-triple")), Some(OutFormat::NTriple));
        assert_eq!(
            OutFormat::parse(Some("n-triple-file")),
            Some(OutFormat::NTripleFile)
        );
        for bad in ["xml", "csv", "JSON", "ntriples"] {
            assert_eq!(OutFormat::parse(Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn output_limit_follows_upstream() {
        // No LIMIT, few answers: -1.
        assert_eq!(output_limit(5, None, 100), -1);
        // A LIMIT is reported as is.
        assert_eq!(output_limit(5, Some(10), 100), 10);
        // More answers than max_output_size: capped.
        assert_eq!(output_limit(150, None, 100), 100);
        assert_eq!(output_limit(150, Some(500), 100), 100);
        // A LIMIT below the cap stays.
        assert_eq!(output_limit(150, Some(50), 100), 50);
    }

    #[test]
    fn tids_parse_like_upstream() {
        assert_eq!(parse_tid("1"), Some(1));
        assert_eq!(parse_tid(" 42 "), Some(42));
        assert_eq!(parse_tid("1700000000_7"), Some(7));
        for bad in ["", "0", "abc", "1.5", "-1", "1_x", "_"] {
            assert_eq!(parse_tid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(compact_utc(0), "19700101000000");
        assert_eq!(compact_utc(951_782_400), "20000229000000");
        assert_eq!(compact_utc(1_700_000_000), "20231114221320");
        assert_eq!(compact_utc(4_102_444_799), "20991231235959");
        assert!(thread_id().bytes().all(|b| b.is_ascii_digit()));
    }
}
