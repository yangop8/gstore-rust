//! Database lifecycle operations: `show`, `load`, `loadcsr`, `unload`,
//! `monitor`, `build`, `drop`, `rename`, `backup`, `backuppath`, `restore`,
//! `export`, `batchInsert`, `batchRemove`, `schema` (upstream
//! `src/Main/grpc.cpp` `*_task`, `src/Server/ApiBuildOperation.cpp`,
//! `ApiUpdateOperation.cpp`, `ApiBackUpRestoreOperation.cpp`,
//! `ApiBatchInsertOperation.cpp`, `ApiBatchRemoveOperation.cpp`,
//! `ApiProvider.cpp`, `src/Api/APIUtil.cpp`, and the response classes of
//! `src/Server/MessageProtocol/`). Parameters, response fields, status codes
//! and messages follow upstream's **code** where its documentation differs
//! (noted at each operation).
//!
//! They work on the same state as the REST endpoints of [`crate::http_api`]
//! and follow its rules: the state lock is held only briefly; a lifecycle
//! operation reserves the database name ([`ApiState::busy`], answered with
//! `1007 LossOfLock` while another one runs, as upstream answers a write lock
//! it cannot get); nothing runs against a database while one of its HTTP
//! transactions is open (`1004`); writes go through the direct triple API
//! (never SPARQL text) and are saved under the check_point policy.
//!
//! Server-side paths (`db_path`, `file`, `dir`, `backup_path`, `export`'s
//! `db_path`) are confined to configured directories — see `ops_file`.
//!
//! Inter-process safety: a database directory is locked while a process has
//! it open ([`crate::db::dirlock`]); `drop`, `rename`, `restore` and the
//! backup of an unloaded database take that lock first, so they never pull a
//! directory from under another process.
//!
//! `build`, `load`, `loadcsr`, `unload`, `backup`, `restore`, `batchInsert`
//! and `batchRemove` honour `async=true` (and `callback`) through the task
//! manager ([`super::tasks`]).
//!
//! [`ApiState::busy`]: crate::http_api::ApiState

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::envelope::{GResp, Status};
use super::ops_file::{self, PathRefusal};
use super::params::Params;
use super::{check_db_name, required, tasks, Ctx, OpSpec, Registry};
use crate::db::dirlock::DirLock;
use crate::db::SchemaGraph;
use crate::http_api::{ApiInner, BusyGuard, DbHandle, DbSlot, OpError, OpErrorKind};
use crate::http_users::ROOT_USER;
use crate::json::Json;
use crate::model::{Term, Triple};
use crate::server::locked;
use crate::sysdb::{clock, DbRecord, DbStatus};
use crate::{Database, DB_SUFFIX};

pub(super) fn register(reg: &mut Registry) {
    reg.add(OpSpec::new("show", show));
    reg.add(OpSpec::new("load", load).needs_db().async_capable());
    reg.add(OpSpec::new("loadcsr", loadcsr).needs_db().async_capable());
    reg.add(OpSpec::new("unload", unload).needs_db().async_capable());
    reg.add(OpSpec::new("monitor", monitor).needs_db());
    reg.add(OpSpec::new("build", build).needs_db().async_capable());
    reg.add(OpSpec::new("drop", drop_db).needs_db());
    reg.add(OpSpec::new("rename", rename).needs_db());
    reg.add(OpSpec::new("backup", backup).needs_db().async_capable());
    reg.add(OpSpec::new("backuppath", backuppath).needs_db());
    reg.add(OpSpec::new("restore", restore).needs_db().async_capable());
    reg.add(OpSpec::new("export", export).needs_db());
    reg.add(
        OpSpec::new("batchInsert", batch_insert)
            .needs_db()
            .async_capable(),
    );
    reg.add(
        OpSpec::new("batchRemove", batch_remove)
            .needs_db()
            .async_capable(),
    );
    reg.add(OpSpec::new("schema", schema).needs_db());
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The file upstream keeps a database's build time in (`DatabaseInfo::success`:
/// the milliseconds as one native-endian `int64`), reported as `costTime`.
const COST_FILE: &str = "success";

/// The validated `db_name` of the operation.
fn db_of(ctx: &Ctx<'_>) -> String {
    ctx.db_name.clone().unwrap_or_default()
}

/// The `1007` answer for a database another lifecycle operation holds
/// (upstream: the write lock could not be taken).
fn busy(db: &str) -> GResp {
    GResp::error(
        Status::LossOfLock,
        format!(
            "database[{db}] try write lock fail: another build/load/unload/drop/rename/\
             backup/restore of it is in progress"
        ),
    )
}

/// The `1004` answer while HTTP transaction `tid` is open on `db`.
fn in_txn(db: &str, tid: u64) -> GResp {
    GResp::error(
        Status::OperationConditionsAreNotSatisfied,
        format!("database[{db}] is busy in transaction {tid}; commit or roll back first"),
    )
}

/// Upstream `validate_databaseinfo` with `check_loaded`: `1005` "does not
/// exist" or "is not loaded yet".
fn not_loaded(db: &str, on_disk: bool) -> GResp {
    if on_disk {
        GResp::error(
            Status::OperationFailed,
            format!("database[{db}] is not loaded yet"),
        )
    } else {
        GResp::error(
            Status::OperationFailed,
            format!("database[{db}] does not exist"),
        )
    }
}

/// The handle of loaded database `db`, or the [`not_loaded`] answer.
fn loaded_handle(ctx: &Ctx<'_>, db: &str) -> Result<DbHandle, GResp> {
    let st = ctx.state();
    match st.dbs.get(db) {
        Some(h) => Ok(Arc::clone(h)),
        None => Err(not_loaded(db, st.db_dir(db).is_dir())),
    }
}

/// Reserve `db` for a lifecycle operation (released when the guard drops).
fn claim<'a>(ctx: &Ctx<'a>, db: &str) -> Result<BusyGuard<'a>, GResp> {
    if ctx.state().busy.insert(db.to_string()) {
        Ok(BusyGuard {
            inner: ctx.inner,
            name: db.to_string(),
        })
    } else {
        Err(busy(db))
    }
}

/// A shared database / transaction failure as an upstream answer.
fn op_error(db: &str, prefix: &str, e: OpError) -> GResp {
    match e.kind {
        OpErrorKind::NotLoaded => not_loaded(db, true),
        OpErrorKind::Conflict | OpErrorKind::NotOwner => {
            GResp::error(Status::OperationConditionsAreNotSatisfied, e.msg)
        }
        _ => GResp::error(Status::OperationFailed, format!("{prefix}{}", e.msg)),
    }
}

/// Report progress of an asynchronous operation (`checkOperationState` of a
/// running task shows it).
fn progress(msg: &str, done: u64) {
    if let Some(task) = tasks::current() {
        task.set_progress(msg, done);
    }
}

fn write_cost(dir: &Path, millis: i64) {
    let _ = fs::write(dir.join(COST_FILE), millis.to_ne_bytes());
}

fn read_cost(dir: &Path) -> i64 {
    fs::read(dir.join(COST_FILE))
        .ok()
        .and_then(|b| b.get(..8).and_then(|b| b.try_into().ok()))
        .map_or(0, i64::from_ne_bytes)
}

/// The databases on disk under `root`: `<name>.db` directories with a valid,
/// non-system name.
fn databases_on_disk(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if let Some(name) = fname.strip_suffix(DB_SUFFIX) {
            if check_db_name(name).is_ok() && entry.path().is_dir() {
                out.insert(name.to_string(), entry.path());
            }
        }
    }
    out
}

/// The total size of the files under `dir`, in bytes.
fn dir_size(dir: &Path) -> u64 {
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

/// `<iri>` → `iri`, then without the namespace (upstream
/// `NodeUtil::clear_angle_brackets_and_prefix`): from the first `http` up to
/// the last `#`, else the last `/`.
fn clean_label(s: &str) -> String {
    let s = s.strip_prefix('<').unwrap_or(s);
    let s = s.strip_suffix('>').unwrap_or(s);
    let Some(start) = s.find("http") else {
        return s.to_string();
    };
    let end = s.rfind('#').or_else(|| s.rfind('/'));
    match end {
        Some(end) if end >= start => format!("{}{}", &s[..start], &s[end + 1..]),
        _ => s.to_string(),
    }
}

/// `<iri>` → `iri` (upstream `Util::clear_angle_brackets`).
fn clear_brackets(s: &str) -> &str {
    let s = s.strip_prefix('<').unwrap_or(s);
    s.strip_suffix('>').unwrap_or(s)
}

/// A schema graph as upstream's `getSchemaInfo(…, all=true)` renders it:
/// `{"nodes":[{"id","label","type"}],"edges":[{"source","target"[,"label"],"id"}]}`
/// with ids as strings and labels without namespace.
fn schema_json(g: &SchemaGraph) -> Json {
    let mut nodes = Json::arr();
    for n in &g.nodes {
        nodes.push(
            Json::obj()
                .with("id", n.id.to_string())
                .with("label", clean_label(&n.label))
                .with("type", i64::from(n.property)),
        );
    }
    let mut edges = Json::arr();
    for (i, e) in g.edges.iter().enumerate() {
        let mut item = Json::obj()
            .with("source", e.source.to_string())
            .with("target", e.target.to_string());
        if let Some(l) = &e.label {
            item.set("label", clean_label(l));
        }
        item.set("id", (i + 1).to_string());
        edges.push(item);
    }
    Json::obj().with("nodes", nodes).with("edges", edges)
}

// ---------------------------------------------------------------------------
// Reading RDF sources
// ---------------------------------------------------------------------------

/// RDF read from server-side files: the valid triples and how many
/// statements could not be used.
#[derive(Default)]
struct Parsed {
    triples: Vec<Triple>,
    /// Statements that did not parse (or a whole file that did not), and
    /// parsed statements that are not valid RDF data (a literal subject, a
    /// non-IRI predicate) — upstream's `failed_num` (its parse error log).
    failed: u64,
}

/// Whether a parsed triple is RDF data: an IRI or blank subject and an IRI
/// predicate.
fn valid_ground(t: &Triple) -> bool {
    matches!(t.subject, Term::Iri(_) | Term::Blank(_)) && matches!(t.predicate, Term::Iri(_))
}

/// Parse one RDF file into `out` with upstream's per-statement accounting.
/// The format follows the extension ([`crate::parser::parse_file_by_ext`]).
/// When an N-Triples-like file (`nt`, `txt`, `n3`) does not parse as a
/// whole, it is read again line by line, keeping the good statements and
/// counting the bad ones (upstream logs them to `parse_error.log` and goes
/// on); any other file that does not parse counts as one failure.
fn parse_into(path: &Path, out: &mut Parsed) {
    match crate::parser::parse_file_by_ext(path) {
        Ok(ts) => {
            for t in ts {
                if valid_ground(&t) {
                    out.triples.push(t);
                } else {
                    out.failed += 1;
                }
            }
        }
        Err(_) => {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if !matches!(ext.as_str(), "nt" | "ntriples" | "txt" | "n3") {
                out.failed += 1;
                return;
            }
            let Ok(bytes) = fs::read(path) else {
                out.failed += 1;
                return;
            };
            for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                match crate::parser::ntriples::parse_line(line, i + 1) {
                    Ok(Some(t)) if valid_ground(&t) => out.triples.push(t),
                    Ok(None) => {}
                    Ok(Some(_)) | Err(_) => out.failed += 1,
                }
            }
        }
    }
}

/// Which "not exist" message an operation answers (upstream's differ).
#[derive(Clone, Copy)]
enum SourceKind {
    /// `build`: `1005 "RDF file '<p>' not exist."`
    Build,
    /// `batchInsert` / `batchRemove`: `1005 "The file '<p>' is not exist."`
    Batch,
}

/// Resolve the RDF source files `requested` (named `param`) inside the
/// server's source roots, checking each like upstream: it must exist (`1005`)
/// and carry an allowed RDF extension (`1003 "The file suffix '<ext>' is not
/// allowed."`); a compressed package (`zip`) is refused with `1003` until zip
/// input is supported. Duplicates are read once.
fn source_files(
    ctx: &Ctx<'_>,
    requested: &[String],
    param: &str,
    kind: SourceKind,
) -> Result<Vec<PathBuf>, GResp> {
    let roots = ops_file::source_roots(ctx);
    let bases = [ops_file::upload_dir(ctx)];
    let missing = |r: &str| {
        let msg = match kind {
            SourceKind::Build => format!("RDF file '{r}' not exist."),
            SourceKind::Batch => format!("The file '{r}' is not exist."),
        };
        GResp::error(Status::OperationFailed, msg)
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for r in requested {
        let path = match ops_file::resolve_existing(r, roots.as_deref(), &bases) {
            Ok(p) if p.is_file() => p,
            Ok(_) | Err(PathRefusal::Missing) => return Err(missing(r)),
            Err(refusal) => {
                return Err(refusal.answer(param, r, roots.as_deref().unwrap_or_default()))
            }
        };
        let ext = ops_file::suffix_of(r).unwrap_or_default();
        if ops_file::is_package(ctx, ext) {
            return Err(GResp::error(
                Status::ParamIsIllegal,
                format!("zip input not supported yet ('{r}'): unpack it and pass the RDF files"),
            ));
        }
        if !ops_file::is_rdf_ext(ctx, ext) {
            return Err(GResp::error(
                Status::ParamIsIllegal,
                format!("The file suffix '{ext}' is not allowed."),
            ));
        }
        if !out.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

/// The RDF files directly inside directory `requested` (a `batchInsert`
/// `dir`), sorted by name: those with an allowed RDF extension.
fn dir_files(ctx: &Ctx<'_>, requested: &str) -> Result<Vec<PathBuf>, GResp> {
    let roots = ops_file::source_roots(ctx);
    let bases = [ops_file::upload_dir(ctx)];
    let dir = match ops_file::resolve_existing(requested, roots.as_deref(), &bases) {
        Ok(p) if p.is_dir() => p,
        Ok(_) | Err(PathRefusal::Missing) => {
            return Err(GResp::error(
                Status::OperationFailed,
                format!("The dir '{requested}' is not exist."),
            ))
        }
        Err(refusal) => {
            return Err(refusal.answer("dir", requested, roots.as_deref().unwrap_or_default()))
        }
    };
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| {
            GResp::error(
                Status::OperationFailed,
                format!("cannot read '{requested}': {e}"),
            )
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(ops_file::suffix_of)
                    .is_some_and(|e| ops_file::is_rdf_ext(ctx, e))
        })
        .collect();
    files.sort();
    Ok(files)
}

// ---------------------------------------------------------------------------
// show / load / loadcsr / unload / monitor / schema
// ---------------------------------------------------------------------------

/// `show` — the databases the caller may query (`root`: all), each as
/// `{"database","creator","built_time","status","lockCount","costTime"}`
/// plus `"csr"` / `"txn"` when loaded (upstream `DatabaseInfo::toJSON`), in
/// `ResponseBody`. With `db_name`, only that database (upstream does not
/// filter that one by privilege; here it is filtered too).
fn show(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let only = p.non_empty("db_name");
    let user = ctx.user().to_string();
    let (records, loaded, busy_names, txn_dbs, root) = {
        let st = ctx.state();
        (
            st.sysdb.records().cloned().collect::<Vec<DbRecord>>(),
            st.dbs.keys().cloned().collect::<HashSet<String>>(),
            st.busy.clone(),
            st.db_txn.keys().cloned().collect::<HashSet<String>>(),
            st.root.clone(),
        )
    };
    // The registry, reconciled with the directories on disk (a database
    // built or removed outside the server) and the loaded databases.
    let on_disk = databases_on_disk(&root);
    let mut all: BTreeMap<String, DbRecord> = BTreeMap::new();
    for r in records {
        if on_disk.contains_key(&r.database) || loaded.contains(&r.database) {
            all.insert(r.database.clone(), r);
        }
    }
    for (name, dir) in &on_disk {
        all.entry(name.clone()).or_insert_with(|| DbRecord {
            database: name.clone(),
            creator: ROOT_USER.to_string(),
            built_time: crate::sysdb::modified_datetime(dir)
                .unwrap_or_else(|| clock::now().datetime()),
        });
    }
    for name in &loaded {
        all.entry(name.clone()).or_insert_with(|| DbRecord {
            database: name.clone(),
            creator: ROOT_USER.to_string(),
            built_time: clock::now().datetime(),
        });
    }
    if let Some(only) = &only {
        all.retain(|name, _| name == only);
    }
    let visible: Vec<DbRecord> = {
        let st = ctx.state();
        all.into_values()
            .filter(|r| crate::sysdb::allowed(&st.users, &user, "query", Some(&r.database)))
            .collect()
    };
    let mut body = Json::arr();
    for r in &visible {
        let is_loaded = loaded.contains(&r.database);
        let status = if is_loaded {
            DbStatus::Loaded
        } else if busy_names.contains(&r.database) && !on_disk.contains_key(&r.database) {
            DbStatus::Building
        } else {
            DbStatus::AlreadyBuilt
        };
        let lock_count =
            i64::from(busy_names.contains(&r.database) || txn_dbs.contains(&r.database));
        let mut item = r.to_json(status).with("lockCount", lock_count).with(
            "costTime",
            read_cost(&root.join(crate::db_dir_for(&r.database))),
        );
        if is_loaded {
            // No CSR cache yet (a later phase adds one); transactions are
            // always available.
            item.set("csr", false);
            item.set("txn", true);
        }
        body.push(item);
    }
    GResp::success("Get the database list successfully!").with("ResponseBody", body)
}

/// The CSR cache hook of `load csr=1` / `loadcsr`.
///
/// TODO(CSR cache): a later phase builds the per-database CSR the advanced
/// graph functions use here. Until then those functions build what they need
/// per query, so there is nothing to load and this succeeds without work.
fn load_csr(_ctx: &Ctx<'_>, _db: &str) {}

/// `load` — open `db_name` (any layout: snapshot, on-disk, RocksDB) and serve
/// it. `csr=1` (or `true`) also loads the CSR cache ([`load_csr`]); the
/// answer's `csr` echoes it (`"1"` / `"0"`). Already loaded: `0 "The
/// database already load yet."` (upstream's text). Unknown: `1005
/// "database[<db>] does not exist."`.
fn load(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let csr = p.bool("csr", false);
    let csr_field = if csr { "1" } else { "0" };
    let dir = {
        let mut st = ctx.state();
        if st.dbs.contains_key(&db) {
            drop(st);
            if csr {
                load_csr(ctx, &db);
            }
            return GResp::success("The database already load yet.").with("csr", csr_field);
        }
        let dir = st.db_dir(&db);
        if !st.busy.contains(&db) && !dir.is_dir() {
            return GResp::error(
                Status::OperationFailed,
                format!("database[{db}] does not exist."),
            );
        }
        if !st.busy.insert(db.clone()) {
            return GResp::error(
                Status::OperationFailed,
                format!("database[{db}] is currently being processed by another operation"),
            );
        }
        dir
    };
    let _busy = BusyGuard {
        inner: ctx.inner,
        name: db.clone(),
    };
    progress("loading", 0);
    match Database::open(&dir) {
        Ok(opened) => {
            let mut st = ctx.state();
            let slot = DbSlot::handle(opened, dir, false, &st.load_policy);
            st.dbs.insert(db.clone(), slot);
        }
        Err(e) => {
            return GResp::error(Status::OperationFailed, format!("load failed: {e}"));
        }
    }
    if csr {
        load_csr(ctx, &db);
    }
    GResp::success("Database loaded successfully.").with("csr", csr_field)
}

/// `loadcsr` — load the CSR cache of a loaded database ([`load_csr`]):
/// `0 "CSR loaded successfully."`.
fn loadcsr(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let db = db_of(ctx);
    let st = ctx.state();
    if st.dbs.contains_key(&db) {
        drop(st);
        load_csr(ctx, &db);
        return GResp::success("CSR loaded successfully.");
    }
    if st.db_dir(&db).is_dir() {
        GResp::error(
            Status::OperationFailed,
            format!("database[{db}] is not loaded yet"),
        )
    } else {
        GResp::error(
            Status::OperationFailed,
            format!("database[{db}] does not exist."),
        )
    }
}

/// `unload` — save unsaved changes, then stop serving the database (`0
/// "Database unloaded."`). If the save fails it stays loaded (`1005`).
fn unload(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let db = db_of(ctx);
    let dbh = {
        let mut st = ctx.state();
        if let Some(&tid) = st.db_txn.get(&db) {
            return in_txn(&db, tid);
        }
        let Some(h) = st.dbs.get(&db).map(Arc::clone) else {
            let on_disk = st.db_dir(&db).is_dir();
            return not_loaded(&db, on_disk);
        };
        if !st.busy.insert(db.clone()) {
            return busy(&db);
        }
        h
    };
    let _busy = BusyGuard {
        inner: ctx.inner,
        name: db.clone(),
    };
    let mut slot = locked(&dbh);
    if slot.retired {
        return not_loaded(&db, true);
    }
    if let Some(open) = &slot.txn {
        return in_txn(&db, open.tid);
    }
    let saved = if slot.dirty {
        slot.save()
    } else {
        slot.db.sync()
    };
    if let Err(e) = saved {
        return GResp::error(
            Status::OperationFailed,
            format!("Unload fail: saving database[{db}] failed ({e}); it stays loaded"),
        );
    }
    slot.retire();
    {
        let mut st = ctx.state();
        if st.dbs.get(&db).is_some_and(|h| Arc::ptr_eq(h, &dbh)) {
            st.dbs.remove(&db);
        }
    }
    GResp::success("Database unloaded.")
}

/// `monitor` — statistics of a loaded database (upstream
/// `MessageMonitorResponse`): `database`, `creator`, `builtTime`,
/// `tripleNum` (a string), `entityNum`, `literalNum`, `subjectNum`,
/// `predicateNum`, `connectionNum` (requests served by the `/api` layer and
/// the REST endpoints), `lockNum`, `diskUsed` (MB; `0` with `disk=0`),
/// `costTime` (the build time, ms), `status`, `subjectList` (`{"name","value"}`
/// per `rdf:type` class: its number of instances) and `schema` (as
/// [`schema`]).
fn monitor(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let dbh = match loaded_handle(ctx, &db) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let (record, connections) = {
        let st = ctx.state();
        (st.sysdb.get(&db).cloned(), st.requests)
    };
    let slot = locked(&dbh);
    if slot.retired {
        return not_loaded(&db, true);
    }
    let triples = slot.committed_triple_num();
    let stats = slot.db.stats();
    let subjects = slot.db.subject_num();
    let classes = slot.db.type_statistics();
    let graph = slot.db.schema_graph();
    let lock_num = i64::from(slot.txn.is_some());
    let dir = slot.dir.clone();
    drop(slot);
    let disk_used = if p.str("disk").as_deref() == Some("0") {
        0
    } else {
        dir_size(&dir) >> 20
    };
    let mut subject_list = Json::arr();
    for (class, n) in classes {
        subject_list.push(
            Json::obj()
                .with("name", clear_brackets(&class))
                .with("value", n),
        );
    }
    let (creator, built_time) = record.map_or_else(
        || (ROOT_USER.to_string(), String::new()),
        |r| (r.creator, r.built_time),
    );
    GResp::success("success")
        .with("database", db.as_str())
        .with("creator", creator)
        .with("builtTime", built_time)
        .with("tripleNum", triples.to_string())
        .with("entityNum", stats.entity_num)
        .with("literalNum", stats.literal_num)
        .with("subjectNum", subjects)
        .with("predicateNum", stats.predicate_num)
        .with("connectionNum", connections)
        .with("lockNum", lock_num)
        .with("diskUsed", disk_used)
        .with("costTime", read_cost(&dir))
        .with("status", DbStatus::Loaded.as_str())
        .with("subjectList", subject_list)
        .with("schema", schema_json(&graph))
}

/// `schema` — the class / property graph of a loaded database (upstream
/// `schema_task`: `ResponseBody` = `{"nodes":[…],"edges":[…]}`, see
/// [`crate::db::SchemaGraph`]). Computed from the current data on every call,
/// so upstream's `refresh=true` is implied.
fn schema(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let db = db_of(ctx);
    let dbh = match loaded_handle(ctx, &db) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let slot = locked(&dbh);
    if slot.retired {
        return not_loaded(&db, true);
    }
    let graph = slot.db.schema_graph();
    drop(slot);
    GResp::success("success").with("ResponseBody", schema_json(&graph))
}

// ---------------------------------------------------------------------------
// build / drop / rename
// ---------------------------------------------------------------------------

/// `build` — create database `db_name` from the server-side RDF files
/// `db_path` (a string or an array; none = an empty database). Upstream's
/// checks, in its order: each file exists (`1005 "RDF file '<p>' not
/// exist."`, after the path confinement of `ops_file`) and has an allowed
/// extension (`1003`; a `.zip` → `1003 "zip input not supported yet"`), the
/// database does not exist yet (`1004 "database already built."`), and the
/// `max_database_num` limit. Statements that do not parse are skipped and
/// counted. Answers `0 "Import RDF file to database done."` with
/// `success_num` (the triples in the new database) and `failed_num`. The
/// creator gets every privilege on the new database; it is not loaded.
fn build(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let started = Instant::now();
    let requested = p.strings("db_path");
    let files = match source_files(ctx, &requested, "db_path", SourceKind::Build) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let max_dbs = ctx.inner.config().max_database_num;
    let dir = {
        let mut st = ctx.state();
        let dir = st.db_dir(&db);
        if st.dbs.contains_key(&db) || dir.exists() {
            return GResp::error(
                Status::OperationConditionsAreNotSatisfied,
                "database already built.",
            );
        }
        let mut names: HashSet<String> = st.sysdb.names().into_iter().collect();
        names.extend(st.dbs.keys().cloned());
        if names.len() >= max_dbs {
            return GResp::error(
                Status::OperationConditionsAreNotSatisfied,
                "The total number of databases more than max_databse_num.",
            );
        }
        if !st.busy.insert(db.clone()) {
            return busy(&db);
        }
        dir
    };
    let _busy = BusyGuard {
        inner: ctx.inner,
        name: db.clone(),
    };
    let mut parsed = Parsed::default();
    for (i, f) in files.iter().enumerate() {
        progress(
            &format!("parsing RDF file {} of {}", i + 1, files.len()),
            parsed.triples.len() as u64,
        );
        parse_into(f, &mut parsed);
    }
    progress("building the database", parsed.triples.len() as u64);
    let fail = |msg: String| {
        GResp::error(
            Status::OperationFailed,
            format!("Import RDF file to database failed:{msg}"),
        )
    };
    let built = match Database::build_from_triples(db.clone(), parsed.triples) {
        Ok(b) => b,
        Err(e) => return fail(e.to_string()),
    };
    let success_num = built.triple_num();
    if let Err(e) = built.save(&dir) {
        drop(built);
        let _ = fs::remove_dir_all(&dir);
        return fail(e.to_string());
    }
    drop(built);
    write_cost(
        &dir,
        i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    );
    {
        let mut st = ctx.state();
        let creator = ctx.user().to_string();
        // Best effort, like the REST endpoints: the registry is reconciled
        // with the directories at every start.
        let _ = st.sysdb.register(&db, &creator);
        let _ = st.users.grant_all_on(&creator, &db);
    }
    GResp::success("Import RDF file to database done.")
        .with("failed_num", parsed.failed)
        .with("success_num", success_num)
}

/// Where a dropped database's directory goes with `is_backup=true`:
/// `<db>.db.bak` (upstream), or `<db>.db.bak.<n>` when that is taken.
fn bak_path(dir: &Path) -> PathBuf {
    let base = format!("{}.bak", dir.display());
    let mut cand = PathBuf::from(&base);
    let mut n = 1;
    while cand.exists() {
        cand = PathBuf::from(format!("{base}.{n}"));
        n += 1;
    }
    cand
}

/// `drop` — remove database `db_name` (`root` / `admin` only). With
/// `is_backup` (default **true**) its directory is kept as `<db>.db.bak`
/// (upstream's logical deletion; its unsaved changes are saved first),
/// with `is_backup=false` deleted. Its registry entry and every grant on it
/// go. Answers `0 "Database <db> dropped."`.
fn drop_db(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let is_backup = p.bool("is_backup", true);
    let (dir, loaded) = {
        let mut st = ctx.state();
        if let Some(&tid) = st.db_txn.get(&db) {
            return in_txn(&db, tid);
        }
        let dir = st.db_dir(&db);
        let loaded = st.dbs.get(&db).map(Arc::clone);
        if loaded.is_none() && !dir.is_dir() {
            return GResp::error(
                Status::OperationFailed,
                format!("database[{db}] does not exist"),
            );
        }
        if !st.busy.insert(db.clone()) {
            return busy(&db);
        }
        (dir, loaded)
    };
    let _busy = BusyGuard {
        inner: ctx.inner,
        name: db.clone(),
    };
    // No other process may have it open.
    let _lock = if dir.is_dir() {
        match DirLock::acquire(&dir) {
            Ok(l) => Some(l),
            Err(e) => return GResp::error(Status::OperationFailed, format!("Drop fail: {e}")),
        }
    } else {
        None
    };
    if let Some(h) = &loaded {
        let mut slot = locked(h);
        if !slot.retired {
            if let Some(open) = &slot.txn {
                return in_txn(&db, open.tid);
            }
            if is_backup && slot.dirty {
                if let Err(e) = slot.save() {
                    return GResp::error(
                        Status::OperationFailed,
                        format!(
                            "Drop fail: saving database[{db}] failed ({e}); nothing was dropped"
                        ),
                    );
                }
            }
            slot.retire();
        }
        drop(slot);
        let mut st = ctx.state();
        if st.dbs.get(&db).is_some_and(|cur| Arc::ptr_eq(cur, h)) {
            st.dbs.remove(&db);
        }
    }
    if dir.is_dir() {
        let moved = if is_backup {
            fs::rename(&dir, bak_path(&dir))
        } else {
            fs::remove_dir_all(&dir)
        };
        if let Err(e) = moved {
            return GResp::error(Status::OperationFailed, format!("Drop fail: {e}"));
        }
    }
    {
        let mut st = ctx.state();
        let _ = st.sysdb.unregister(&db);
        let _ = st.users.forget_database(&db);
    }
    GResp::success(&format!("Database {db} dropped."))
}

/// `rename` — rename database `db_name` to `new_name` (`root` / `admin`
/// only), keeping its creator, built time and grants. Refused, like upstream
/// (`1005`), while it is loaded ("Database is loaded, need unload first."),
/// when it does not exist, or when `new_name` is taken. Answers `0 "Database
/// rename successfully."` (upstream's code; its docs say `"success"`).
fn rename(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let new_name = match required(p, "new_name") {
        Ok(n) => n,
        Err(r) => return r,
    };
    if let Err(msg) = check_db_name(&new_name) {
        return GResp::error(Status::ParamIsIllegal, msg.replace("db_name", "new_name"));
    }
    let failed = |msg: String| GResp::error(Status::OperationFailed, msg);
    let (from, to) = {
        let mut st = ctx.state();
        let from = st.db_dir(&db);
        let to = st.db_dir(&new_name);
        if !from.is_dir() && !st.dbs.contains_key(&db) {
            return failed(format!("Database[{db}] does not exist"));
        }
        if st.dbs.contains_key(&db) {
            return failed("Database is loaded, need unload first.".to_string());
        }
        if st.dbs.contains_key(&new_name) || st.sysdb.contains(&new_name) || new_name == db {
            return failed(format!("Database name {new_name} already exists."));
        }
        if to.exists() {
            return failed(format!("Database path {} already exists.", to.display()));
        }
        if st.busy.contains(&db) || st.busy.contains(&new_name) {
            return busy(if st.busy.contains(&db) {
                &db
            } else {
                &new_name
            });
        }
        st.busy.insert(db.clone());
        st.busy.insert(new_name.clone());
        (from, to)
    };
    let _busy = (
        BusyGuard {
            inner: ctx.inner,
            name: db.clone(),
        },
        BusyGuard {
            inner: ctx.inner,
            name: new_name.clone(),
        },
    );
    let lock = match DirLock::acquire(&from) {
        Ok(l) => l,
        Err(e) => return failed(format!("Rename fail: {e}")),
    };
    if let Err(e) = fs::rename(&from, &to) {
        return failed(format!("Rename fail: {e}"));
    }
    drop(lock);
    let mut st = ctx.state();
    match st.sysdb.rename(&db, &new_name) {
        Ok(true) => {}
        _ => {
            let _ = st.sysdb.register(&new_name, ROOT_USER);
        }
    }
    let _ = st.users.copy_database_privileges(&db, &new_name);
    let _ = st.users.forget_database(&db);
    GResp::success("Database rename successfully.")
}

// ---------------------------------------------------------------------------
// backup / backuppath / restore / export
// ---------------------------------------------------------------------------

/// The `1003` answer to `compress=true` until zip output is supported.
fn no_compress(op: &str) -> GResp {
    GResp::error(
        Status::ParamIsIllegal,
        format!("compress is not supported yet: {op} writes uncompressed output (omit compress)"),
    )
}

/// The backups of `db` in `dir`: upstream's `<db>.db_<YYYYmmddHHMMSS>`
/// entries (directories, or `.zip` files), sorted oldest first.
fn backups_of(dir: &Path, db: &str) -> Vec<String> {
    let prefix = format!("{db}{DB_SUFFIX}_");
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| {
            n.strip_prefix(&prefix)
                .map(|ts| ts.strip_suffix(".zip").unwrap_or(ts))
                .is_some_and(|ts| ts.len() == 14 && ts.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    out.sort();
    out
}

/// The built time a backup's name records (`…_YYYYmmddHHMMSS` →
/// `YYYY-MM-DD HH:MM:SS`, upstream `Util::get_backup_time`).
fn backup_time(path: &Path) -> Option<String> {
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

/// Remove the oldest backups of `db` in `dir` until at most `max` (at least
/// one: the newest) remain.
fn rotate_backups(dir: &Path, db: &str, max: usize) {
    let mut list = backups_of(dir, db);
    while list.len() > max.max(1) {
        let oldest = dir.join(list.remove(0));
        if oldest.is_dir() {
            let _ = fs::remove_dir_all(&oldest);
        } else {
            let _ = fs::remove_file(&oldest);
        }
    }
}

/// The directory a `backup` (`backup_path`) or `export` (`db_path`) writes
/// into — `requested` inside `root`, else `root` itself — and how to show it
/// in the answer (as the client named it when it is relative to the working
/// directory; else under `root` as configured).
fn output_dir(
    requested: Option<&str>,
    root: &Path,
    param: &str,
) -> Result<(PathBuf, PathBuf), GResp> {
    let io = |e: std::io::Error| {
        GResp::error(
            Status::OperationFailed,
            format!("cannot create '{}': {e}", root.display()),
        )
    };
    fs::create_dir_all(root).map_err(io)?;
    let Some(r) = requested else {
        let canon = fs::canonicalize(root).map_err(io)?;
        return Ok((canon, root.to_path_buf()));
    };
    let target = ops_file::resolve_target(r, root)
        .map_err(|refusal| refusal.answer(param, r, &[root.to_path_buf()]))?;
    let as_given = Path::new(r);
    let via_cwd = as_given.is_absolute()
        || std::env::current_dir()
            .ok()
            .and_then(|cwd| fs::canonicalize(cwd).ok())
            .is_some_and(|cwd| target.starts_with(&cwd) && target == cwd.join(as_given));
    let shown = if via_cwd {
        as_given.to_path_buf()
    } else {
        root.join(as_given)
    };
    Ok((target, shown))
}

/// `backup` — copy database `db_name` (loaded or not) to
/// `<backup_path>/<db>.db_<YYYYmmddHHMMSS>` (upstream `Database::backup`'s
/// name), `backup_path` defaulting to the server's backup directory and
/// confined to it; then keep at most `max_backups` backups of the database
/// there (the oldest go). Answers `0 "Database backup successfully."` with
/// `backupfilepath`, which `restore` accepts. (Upstream's code ignores a
/// `backup_path` parameter its documentation describes; it is honoured here,
/// inside the backup directory.) `compress=true` (a zip) is refused until zip
/// output is supported.
fn backup(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    if p.bool("compress", false) {
        return no_compress("backup");
    }
    let root = ops_file::backup_dir(ctx);
    let requested = p.non_empty("backup_path");
    let (target, shown) = match output_dir(requested.as_deref(), &root, "backup_path") {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Never write into a backup (or a database) inside the backup directory.
    if let Ok(canon_root) = fs::canonicalize(&root) {
        let mut cur = Some(target.as_path());
        while let Some(d) = cur.filter(|d| d.starts_with(&canon_root) && *d != canon_root) {
            if Database::storage_kind(d).is_some() {
                return GResp::error(
                    Status::ParamIsIllegal,
                    format!(
                        "the backup_path '{}' lies inside a database backup; choose another path",
                        requested.as_deref().unwrap_or_default()
                    ),
                );
            }
            cur = d.parent();
        }
    }
    let (dbh, dir) = {
        let st = ctx.state();
        let dir = st.db_dir(&db);
        let dbh = st.dbs.get(&db).map(Arc::clone);
        if dbh.is_none() && !dir.is_dir() {
            return GResp::error(
                Status::OperationConditionsAreNotSatisfied,
                format!("Database[{db}] does not exist."),
            );
        }
        (dbh, dir)
    };
    let fail = |msg: String| GResp::error(Status::OperationFailed, format!("Backup fail: {msg}"));
    if let Err(e) = fs::create_dir_all(&target) {
        return fail(e.to_string());
    }
    let name = format!("{db}{DB_SUFFIX}_{}", ops_file::timestamp_now());
    let dest = target.join(&name);
    progress("backing up", 0);
    let written = match dbh {
        Some(h) => {
            let slot = match ApiInner::lock_for_op(&h, &db, "backing up") {
                Ok(s) => s,
                Err(e) => return op_error(&db, "Backup fail: ", e),
            };
            // Upstream replaces a backup of the same second.
            let _ = fs::remove_dir_all(&dest);
            slot.db.backup(&dest).map_err(|e| e.to_string())
        }
        None => {
            let _busy = match claim(ctx, &db) {
                Ok(g) => g,
                Err(r) => return r,
            };
            match DirLock::acquire(&dir) {
                Ok(_lock) => {
                    let _ = fs::remove_dir_all(&dest);
                    Database::backup_dir(&dir, &dest).map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            }
        }
    };
    if let Err(e) = written {
        let _ = fs::remove_dir_all(&dest);
        return fail(e);
    }
    rotate_backups(&target, &db, ctx.inner.config().max_backups);
    GResp::success("Database backup successfully.")
        .with("backupfilepath", ops_file::shown(&shown, &name))
}

/// `backuppath` — the backups of `db_name` in the server's backup directory
/// (`paths`, oldest first; `[]` when there are none).
fn backuppath(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let db = db_of(ctx);
    let root = ops_file::backup_dir(ctx);
    let mut paths = Json::arr();
    for name in backups_of(&root, &db) {
        paths.push(ops_file::shown(&root, &name));
    }
    GResp::success("success").with("paths", paths)
}

/// `restore` — replace database `db_name` with a backup: `backup_path` (inside
/// the backup directory or a directory of the server's load policy), or —
/// when omitted or empty, a Rust extension — the newest backup of the
/// database in the backup directory. The backup is copied beside the
/// database and validated before anything is replaced; a loaded database is
/// saved, swapped and loaded again. A database that does not exist yet is
/// created (registered with the caller as creator and the backup's time as
/// built time). Answers `0 "Database restore successfully."` (upstream's
/// code; its docs show `"Database <db> restore successfully."`). A `.zip`
/// backup is refused until zip input is supported.
fn restore(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let root = ops_file::backup_dir(ctx);
    let source = match p.non_empty("backup_path") {
        None => match backups_of(&root, &db)
            .into_iter()
            .rev()
            .map(|n| root.join(n))
            .find(|p| p.is_dir())
        {
            Some(newest) => newest,
            None => {
                return GResp::error(
                    Status::ParamIsIllegal,
                    format!(
                        "no backup of database[{db}] found in '{}'; pass backup_path",
                        root.display()
                    ),
                )
            }
        },
        Some(r) => {
            let roots = ops_file::load_policy_dirs(ctx).map(|mut dirs| {
                dirs.push(root.clone());
                dirs
            });
            match ops_file::resolve_existing(&r, roots.as_deref(), std::slice::from_ref(&root)) {
                Ok(path) if path.is_dir() => path,
                Ok(path) => {
                    let ext = ops_file::suffix_of(&path.to_string_lossy())
                        .unwrap_or_default()
                        .to_string();
                    let msg = if ops_file::is_package(ctx, &ext) {
                        format!(
                            "zip input not supported yet ('{r}'): restore from a backup directory"
                        )
                    } else {
                        "Backup file is not zip file.".to_string()
                    };
                    return GResp::error(Status::ParamIsIllegal, msg);
                }
                Err(PathRefusal::Missing) => {
                    return GResp::error(Status::ParamIsIllegal, "Backup path not exist.")
                }
                Err(refusal) => {
                    return refusal.answer("backup_path", &r, roots.as_deref().unwrap_or_default())
                }
            }
        }
    };
    let fail = |msg: String| GResp::error(Status::OperationFailed, format!("Restore fail: {msg}"));
    if Database::storage_kind(&source).is_none() {
        return fail(format!(
            "'{}' does not contain a database backup",
            source.display()
        ));
    }
    let dir = {
        let mut st = ctx.state();
        if let Some(&tid) = st.db_txn.get(&db) {
            return in_txn(&db, tid);
        }
        if !st.busy.insert(db.clone()) {
            return busy(&db);
        }
        st.db_dir(&db)
    };
    let _busy = BusyGuard {
        inner: ctx.inner,
        name: db.clone(),
    };
    progress("restoring", 0);
    // Copy and validate first; nothing is touched if the backup is bad.
    let staged = match Database::stage_restore(&source, &dir) {
        Ok(s) => s,
        Err(e) => return fail(e.to_string()),
    };
    // No other process may have the database open while it is swapped.
    let probe = if dir.is_dir() {
        match DirLock::acquire(&dir) {
            Ok(l) => Some(l),
            Err(e) => {
                Database::discard_staged_restore(&staged);
                return fail(e.to_string());
            }
        }
    } else {
        None
    };
    let loaded = ctx.state().dbs.get(&db).map(Arc::clone);
    if let Some(h) = &loaded {
        let mut slot = locked(h);
        if !slot.retired {
            if let Some(tid) = slot.txn.as_ref().map(|t| t.tid) {
                drop(slot);
                Database::discard_staged_restore(&staged);
                return in_txn(&db, tid);
            }
            if slot.dirty {
                if let Err(e) = slot.save() {
                    drop(slot);
                    Database::discard_staged_restore(&staged);
                    return fail(format!(
                        "saving the unsaved changes of database[{db}] failed ({e}); it stays loaded"
                    ));
                }
            }
            slot.retire();
        }
        drop(slot);
        let mut st = ctx.state();
        if st.dbs.get(&db).is_some_and(|cur| Arc::ptr_eq(cur, h)) {
            st.dbs.remove(&db);
        }
    }
    let installed = Database::install_restore(&staged, &dir);
    // Release the probe before reopening: the directory at this path is a
    // new one now.
    drop(probe);
    if let Err(e) = installed {
        Database::discard_staged_restore(&staged);
        if loaded.is_some() {
            if let Ok(old) = Database::open(&dir) {
                let mut st = ctx.state();
                let slot = DbSlot::handle(old, dir.clone(), false, &st.load_policy);
                st.dbs.insert(db.clone(), slot);
            }
        }
        return fail(e.to_string());
    }
    if loaded.is_some() {
        match Database::open(&dir) {
            Ok(restored) => {
                let mut st = ctx.state();
                let slot = DbSlot::handle(restored, dir.clone(), false, &st.load_policy);
                st.dbs.insert(db.clone(), slot);
            }
            Err(e) => return fail(format!("the restored copy could not be opened: {e}")),
        }
    }
    {
        let mut st = ctx.state();
        if !st.sysdb.contains(&db) {
            let user = ctx.user().to_string();
            let built = backup_time(&source).unwrap_or_else(|| clock::now().datetime());
            let _ = st.sysdb.register_at(&db, &user, &built);
            let _ = st.users.grant_all_on(&user, &db);
        }
    }
    GResp::success("Database restore successfully.")
}

/// `export` — write the loaded database `db_name` as N-Triples to
/// `<db_path>/<db>_<YYYYmmddHHMMSS>.nt` (upstream's name; a numeric suffix
/// keeps two exports of one second apart), `db_path` defaulting to the
/// export directory and confined to it. Answers `0 "Export the database
/// successfully."` with `filepath`, which `/file/download` serves.
/// `compress=true` (a zip) is refused until zip output is supported.
fn export(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    if p.bool("compress", false) {
        return no_compress("export");
    }
    let dbh = match loaded_handle(ctx, &db) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let root = ctx.export_dir();
    let requested = p.non_empty("db_path");
    let (target, shown) = match output_dir(requested.as_deref(), &root, "db_path") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let fail = |msg: String| GResp::error(Status::OperationFailed, format!("Export fail: {msg}"));
    if let Err(e) = fs::create_dir_all(&target) {
        return fail(e.to_string());
    }
    let slot = match ApiInner::lock_for_op(&dbh, &db, "exporting") {
        Ok(s) => s,
        Err(e) => return op_error(&db, "Export fail: ", e),
    };
    let stem = format!("{db}_{}", ops_file::timestamp_now());
    let (file, name) = match ops_file::create_unique(&target, &stem, "nt") {
        Ok(x) => x,
        Err(e) => return fail(e.to_string()),
    };
    let mut out = BufWriter::new(file);
    let written = slot.db.export_ntriples(&mut out);
    drop(slot);
    drop(out);
    if let Err(e) = written {
        let _ = fs::remove_file(target.join(&name));
        return fail(e.to_string());
    }
    GResp::success("Export the database successfully.")
        .with("filepath", ops_file::shown(&shown, &name))
}

// ---------------------------------------------------------------------------
// batchInsert / batchRemove
// ---------------------------------------------------------------------------

/// `batchInsert` — insert the triples of server-side RDF files: `file` (a
/// string or an array) or, when it is absent, every RDF file directly inside
/// `dir` (upstream's documentation describes `dir`; its code reads only
/// `file`). Answers `0 "Batch insert data successfully."` with `success_num`
/// (triples inserted; duplicates are not counted) and `failed_num`
/// (statements that did not parse or are not RDF data).
fn batch_insert(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    batch(ctx, p, true)
}

/// `batchRemove` — remove the triples of server-side RDF files (as
/// [`batch_insert`]): `0 "Batch remove data successfully."`, `success_num`
/// = triples removed.
fn batch_remove(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    batch(ctx, p, false)
}

fn batch(ctx: &mut Ctx<'_>, p: &Params, insert: bool) -> GResp {
    let db = db_of(ctx);
    let (done, fail_prefix) = if insert {
        ("Batch insert data successfully.", "Batch insert fail: ")
    } else {
        ("Batch remove data successfully.", "Batch remove fail: ")
    };
    // Upstream's checks, in its order.
    {
        let st = ctx.state();
        if !st.dbs.contains_key(&db) {
            return if st.db_dir(&db).is_dir() {
                GResp::error(
                    Status::OperationConditionsAreNotSatisfied,
                    "Database is not loaded yet.",
                )
            } else {
                GResp::error(
                    Status::OperationConditionsAreNotSatisfied,
                    format!("Database[{db}] does not exist."),
                )
            };
        }
    }
    let requested = p.strings("file");
    let files = if !requested.is_empty() {
        source_files(ctx, &requested, "file", SourceKind::Batch)
    } else if let Some(d) = p.non_empty("dir") {
        dir_files(ctx, &d)
    } else {
        Err(GResp::error(
            Status::ParamIsIllegal,
            "the value of file can not be empty!",
        ))
    };
    let files = match files {
        Ok(f) => f,
        Err(r) => return r,
    };
    let mut parsed = Parsed::default();
    for f in &files {
        parse_into(f, &mut parsed);
    }
    let dbh = match loaded_handle(ctx, &db) {
        Ok(h) => h,
        Err(_) => {
            return GResp::error(
                Status::OperationConditionsAreNotSatisfied,
                "Database is not loaded yet.",
            )
        }
    };
    let mut slot = locked(&dbh);
    if slot.retired {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "Database is not loaded yet.",
        );
    }
    if let Some(open) = &slot.txn {
        return in_txn(&db, open.tid);
    }
    let mut applied = 0u64;
    let mut failure = None;
    for (i, t) in parsed.triples.iter().enumerate() {
        if i % 10_000 == 0 {
            progress(if insert { "inserting" } else { "removing" }, applied);
        }
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
        slot.wrote(ctx.inner.save_writes())
    } else {
        None
    };
    drop(slot);
    if let Some(e) = failure {
        return GResp::error(
            Status::OperationFailed,
            format!("{fail_prefix}stopped after applying {applied} triple(s): {e}"),
        );
    }
    if let Some(w) = warning {
        // Upstream fails the operation when its check_point save fails (the
        // change stays applied in memory and is saved later).
        return GResp::error(Status::OperationFailed, format!("{fail_prefix}{w}"));
    }
    GResp::success(done)
        .with("success_num", applied)
        .with("failed_num", parsed.failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_lose_brackets_and_namespaces_like_upstream() {
        assert_eq!(clean_label("<http://ex.org/onto#Person>"), "Person");
        assert_eq!(clean_label("<http://ex.org/a/b/name>"), "name");
        assert_eq!(clean_label("<urn:x>"), "urn:x");
        assert_eq!(clean_label("plain"), "plain");
        assert_eq!(clear_brackets("<http://x/C>"), "http://x/C");
    }

    #[test]
    fn backup_names_sort_and_parse() {
        let d = std::env::temp_dir().join(format!("gstore_opsdb_bk_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        for n in [
            "lubm.db_20240102000000",
            "lubm.db_20240101000000",
            "lubm.db_20240103000000.zip",
            "xlubm.db_20240101000000",
            "lubm.db_2024",
            "lubm.db_20240101000000x",
        ] {
            fs::create_dir_all(d.join(n)).unwrap();
        }
        assert_eq!(
            backups_of(&d, "lubm"),
            vec![
                "lubm.db_20240101000000",
                "lubm.db_20240102000000",
                "lubm.db_20240103000000.zip"
            ]
        );
        rotate_backups(&d, "lubm", 2);
        assert_eq!(
            backups_of(&d, "lubm"),
            vec!["lubm.db_20240102000000", "lubm.db_20240103000000.zip"]
        );
        rotate_backups(&d, "lubm", 0);
        assert_eq!(backups_of(&d, "lubm"), vec!["lubm.db_20240103000000.zip"]);
        assert!(d.join("xlubm.db_20240101000000").exists());
        assert_eq!(
            backup_time(Path::new("/b/lubm.db_20240102030405")).as_deref(),
            Some("2024-01-02 03:04:05")
        );
        assert_eq!(backup_time(Path::new("/b/lubm.db")), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rdf_files_are_parsed_with_per_statement_accounting() {
        let d = std::env::temp_dir().join(format!("gstore_opsdb_parse_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let good = d.join("good.nt");
        fs::write(
            &good,
            "<http://a> <http://p> <http://b> .\n\"lit\" <http://p> <http://b> .\n",
        )
        .unwrap();
        let mixed = d.join("mixed.nt");
        fs::write(
            &mixed,
            "<http://a> <http://p> <http://c> .\nthis is not rdf\n# comment\n<http://a> <http://p> \"x\" .\n",
        )
        .unwrap();
        let bad_ttl = d.join("bad.ttl");
        fs::write(&bad_ttl, "@prefix : <http://x/> .\n:a :b \n").unwrap();
        let mut parsed = Parsed::default();
        parse_into(&good, &mut parsed);
        assert_eq!(
            (parsed.triples.len(), parsed.failed),
            (1, 1),
            "literal subject"
        );
        parse_into(&mixed, &mut parsed);
        assert_eq!((parsed.triples.len(), parsed.failed), (3, 2));
        parse_into(&bad_ttl, &mut parsed);
        assert_eq!(
            (parsed.triples.len(), parsed.failed),
            (3, 3),
            "a whole file"
        );
        let _ = fs::remove_dir_all(&d);
    }
}
