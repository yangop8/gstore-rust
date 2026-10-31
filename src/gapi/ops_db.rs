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
//! directory from under another process (the REST `/build`, `/drop` and
//! `/restore` do the same, through the same `DirLock::acquire_existing`).
//! `build` and the REST `/build` share the `max_database_num` count
//! (`ApiState::database_limit_reached`).
//!
//! `build`, `load`, `loadcsr`, `unload`, `backup`, `restore`, `batchInsert`
//! and `batchRemove` honour `async=true` (and `callback`) through the task
//! manager ([`super::tasks`]).
//!
//! [`ApiState::busy`]: crate::http_api::ApiState

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

use super::envelope::{GResp, Status};
use super::ops_file::{self, PathRefusal};
use super::params::Params;
use super::{check_db_name, required, tasks, Ctx, OpSpec, Registry};
use crate::db::dirlock::DirLock;
use crate::db::SchemaGraph;
use crate::http_api::{
    may_restore_from, ApiInner, BusyGuard, DbHandle, DbSlot, OpError, OpErrorKind,
};
use crate::http_users::ROOT_USER;
use crate::json::Json;
use crate::parser::import;
use crate::server::locked;
use crate::sysdb::{clock, DbRecord, DbStatus};
use crate::zipio::{self, ExportFormat, ScratchDir, ZipError, ZipLimits};
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
    reg.add(OpSpec::new("init", init));
}

/// `init` — register existing database directories (upstream `init_task`;
/// root only): `db_names`, comma-separated (or a JSON array). Each name is
/// answered in `data` as `{"db_name","status","msg"}`: `"0"` / `"success"`
/// when it was registered (with the caller as creator, now as built time),
/// `"1"` with `"exist"` when it already is, with `"<db>.db is not exist."`
/// when there is no such directory (one holding no database counts as
/// none), or with the reason a name is not valid. Answers `0 "init database
/// successfully!"` whatever the names' outcomes; no `db_names` is `1003`.
fn init(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let names: Vec<String> = p
        .strings("db_names")
        .iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return GResp::error(Status::ParamIsIllegal, "db_names can't be empty");
    }
    let user = ctx.user().to_string();
    let built = clock::now().datetime();
    let mut data = Json::arr();
    for name in names {
        let (status, msg) = {
            let mut st = ctx.state();
            if let Err(e) = check_db_name(&name) {
                ("1", e)
            } else if st.sysdb.contains(&name) {
                ("1", "exist".to_string())
            } else if Database::storage_kind(st.db_dir(&name)).is_none() {
                ("1", format!("{name}{DB_SUFFIX} is not exist."))
            } else {
                match st.sysdb.register_at(&name, &user, &built) {
                    Ok(()) => ("0", "success".to_string()),
                    Err(_) => ("1", "fail".to_string()),
                }
            }
        };
        data.push(
            Json::obj()
                .with("db_name", name.as_str())
                .with("status", status)
                .with("msg", msg),
        );
    }
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", "init database successfully!")
            .with("data", data),
    )
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
            // A directory holding no database (a build cut short) is not one.
            if check_db_name(name).is_ok()
                && entry.path().is_dir()
                && Database::storage_kind(entry.path()).is_some()
            {
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
//
// `build`, `batchInsert` and `batchRemove` read their files through
// [`crate::parser::import`], tolerantly like upstream: the files stream in
// (N-Triples / N-Quads / Turtle / TriG with bounded memory; the format follows
// the extension), a statement that does not parse — or is not RDF data (a
// literal subject, a non-IRI predicate), or fails upstream's numeric literal
// checks — is skipped and counted in `failed_num`, and logged to
// `<db>.db/parse_error.log` in upstream's format: an `Info … <build|batch
// insert|batch remove> parser info, file path <p>` line per file and a `Line
// <n> (<statement>): <message>` line per skipped statement. (Upstream's
// `batch_remove` forgets to pass its log path, so its failures go to stdout and
// its `failed_num` is always 0; here removals are counted and logged like
// inserts.) Every file of a `build` / `batchInsert` gets its own blank-node
// scope, so `_:b` of two files — or of two calls — are two nodes; a
// `batchRemove` reads blank-node labels verbatim (they must name stored
// nodes, as an export writes them). N-Quads / TriG statements keep their
// named graph.
//
// `remote=true` (upstream's parameter) makes each `db_path` / `file` value an
// `http(s)://` URL: refused unless the server's load policy allows remote
// sources (`gserver --allow-service`); the URL's file name must carry an
// allowed extension; the body (at most 256 MiB) is downloaded into the upload
// directory, read like a local file, and removed when the operation ends —
// a download that fails answers upstream's `1005 "Download file '<url>'
// failed."`.
//
// A source with a package extension (`upload_allow_compress_packages`,
// `zip`) is a zip of RDF files (feature `zip`; without it `1003` naming the
// feature), local or downloaded, handled like upstream's
// `ApiHandler::uncompress_zip`: the package is checked — every file entry
// must carry an allowed RDF extension, else upstream's `StatusCode` `3` "zip
// format or data is error." (the codes are upstream's `ZipCode`s, see
// [`crate::zipio`], which also lists the zip-slip and zip-bomb guards and
// their `conf.ini` limits) — then unpacked into
// `<upload dir>/<package name>_<YYYYmmddHHMMSS>`, its files read in path
// order like any other source, and that directory removed when the
// operation ends, whatever its outcome.

/// The import options of `build` / `batchInsert` (upstream: tolerant).
fn load_options() -> import::ImportOptions {
    import::ImportOptions::tolerant()
}

/// The resolved sources of an operation, and the downloaded copies and
/// unpacked packages to remove when it ends (upstream's `temp_paths`).
#[derive(Default)]
struct Inputs {
    sources: Vec<import::Source>,
    temp: Vec<PathBuf>,
    /// Unpacked zip packages (removed when dropped).
    unpacked: Vec<ScratchDir>,
}

impl Drop for Inputs {
    fn drop(&mut self) {
        for p in &self.temp {
            let _ = fs::remove_file(p);
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

/// Check a source name's extension like upstream: `Ok(true)` for a
/// compressed package (`upload_allow_compress_packages`: a zip, see the
/// section comment; without the `zip` feature `1003` naming it), `Ok(false)`
/// for an allowed RDF extension, else `1003 "The file suffix '<ext>' is not
/// allowed."`.
fn check_suffix(ctx: &Ctx<'_>, r: &str, name: &str) -> Result<bool, GResp> {
    let ext = ops_file::suffix_of(name).unwrap_or_default();
    if ops_file::is_package(ctx, ext) {
        if !zipio::AVAILABLE {
            return Err(GResp::error(
                Status::ParamIsIllegal,
                format!(
                    "zip input ('{r}'): {}; unpack the package and pass its RDF files",
                    zipio::FEATURE_MISSING
                ),
            ));
        }
        return Ok(true);
    }
    if !ops_file::is_rdf_ext(ctx, ext) {
        return Err(GResp::error(
            Status::ParamIsIllegal,
            format!("The file suffix '{ext}' is not allowed."),
        ));
    }
    Ok(false)
}

/// The answer to a failed zip operation: upstream's `ZipCode` as the
/// `StatusCode` with its message (and what went wrong); `1003` without the
/// `zip` feature; `1005` for an I/O failure.
fn zip_answer(e: &ZipError) -> GResp {
    match e {
        ZipError::FeatureMissing => GResp::error(Status::ParamIsIllegal, zipio::FEATURE_MISSING),
        ZipError::Unzip { code, .. } => GResp::error(
            Status::from_code(code.code()).unwrap_or(Status::OperationFailed),
            e.to_string(),
        ),
        ZipError::Io(_) => GResp::error(Status::OperationFailed, e.to_string()),
    }
}

/// Unpack the zip package `zip` (see the section comment) into a fresh
/// directory of the upload directory, adding its RDF files to `out`.
fn unpack_package(ctx: &Ctx<'_>, zip: &Path, out: &mut Inputs) -> Result<(), GResp> {
    let cfg = ctx.inner.config();
    let limits = ZipLimits::from_config(&cfg);
    let name = zip
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem: String = name
        .rsplit_once('.')
        .map_or(name.as_str(), |(s, _)| s)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = ops_file::upload_dir(ctx);
    let made = fs::create_dir_all(&dir).and_then(|()| {
        ops_file::create_unique_dir(&dir, &format!("{stem}_{}", ops_file::timestamp_now()))
    });
    let scratch = match made {
        Ok(p) => ScratchDir::adopt(p),
        Err(e) => {
            return Err(GResp::error(
                Status::OperationFailed,
                format!("cannot unpack '{name}': {e}"),
            ))
        }
    };
    let accept = |entry: &str| {
        ops_file::suffix_of(entry).is_some_and(|e| {
            cfg.upload_allow_extensions
                .iter()
                .any(|a| a.eq_ignore_ascii_case(e))
        })
    };
    let files = zipio::unpack(zip, scratch.path(), &limits, &accept).map_err(|e| zip_answer(&e))?;
    out.sources
        .extend(files.into_iter().map(|f| import::Source::File(f.path)));
    out.unpacked.push(scratch);
    Ok(())
}

/// Resolve the RDF sources `requested` (named `param`): server-side files
/// inside the server's source roots, checked like upstream — each must exist
/// (`1005`) and carry an allowed RDF extension (see [`check_suffix`]) — or,
/// with `remote`, URLs (see the section comment). Duplicates are read once.
fn source_files(
    ctx: &Ctx<'_>,
    requested: &[String],
    param: &str,
    kind: SourceKind,
    remote: bool,
) -> Result<Inputs, GResp> {
    if remote {
        return remote_sources(ctx, requested, param);
    }
    let roots = ops_file::source_roots(ctx);
    let bases = [ops_file::upload_dir(ctx)];
    let missing = |r: &str| {
        let msg = match kind {
            SourceKind::Build => format!("RDF file '{r}' not exist."),
            SourceKind::Batch => format!("The file '{r}' is not exist."),
        };
        GResp::error(Status::OperationFailed, msg)
    };
    let mut out = Inputs::default();
    for r in requested {
        let path = match ops_file::resolve_existing(r, roots.as_deref(), &bases) {
            Ok(p) if p.is_file() => p,
            Ok(_) | Err(PathRefusal::Missing) => return Err(missing(r)),
            Err(refusal) => {
                return Err(refusal.answer(param, r, roots.as_deref().unwrap_or_default()))
            }
        };
        if check_suffix(ctx, r, r)? {
            unpack_package(ctx, &path, &mut out)?;
            continue;
        }
        let source = import::Source::File(path);
        if !out.sources.contains(&source) {
            out.sources.push(source);
        }
    }
    Ok(out)
}

/// `remote=true`: download each URL into the upload directory.
fn remote_sources(ctx: &Ctx<'_>, requested: &[String], param: &str) -> Result<Inputs, GResp> {
    let allowed = ctx.state().load_policy.allow_remote;
    let mut out = Inputs::default();
    for r in requested {
        let failed = || GResp::error(Status::OperationFailed, format!("Download file '{r}' failed."));
        if !import::is_url(r) {
            return Err(failed());
        }
        if !allowed {
            return Err(GResp::error(
                Status::ParamIsIllegal,
                format!(
                    "the {param} '{r}' is refused: the server's load policy does not allow \
                     remote sources"
                ),
            ));
        }
        let path = import::url_path(r);
        let name = path.rsplit('/').next().unwrap_or("");
        let package = check_suffix(ctx, r, name)?;
        let ext = ops_file::suffix_of(name).unwrap_or_default().to_string();
        let stem: String = name
            .strip_suffix(&format!(".{ext}"))
            .unwrap_or(name)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        let body = import::fetch_url(r, import::DEFAULT_MAX_DOCUMENT_BYTES).map_err(|_| failed())?;
        let dir = ops_file::upload_dir(ctx);
        let saved = fs::create_dir_all(&dir).and_then(|()| {
            let (mut f, file) = ops_file::create_unique(
                &dir,
                &format!("{stem}_{}", clock::now().date_compact()),
                &ext,
            )?;
            std::io::Write::write_all(&mut f, &body)?;
            Ok(dir.join(file))
        });
        let local = saved.map_err(|_| failed())?;
        out.temp.push(local.clone());
        if package {
            unpack_package(ctx, &local, &mut out)?;
        } else {
            out.sources.push(import::Source::File(local));
        }
    }
    Ok(out)
}

/// The RDF files directly inside directory `requested` (a `batchInsert`
/// `dir`), sorted by name: those with an allowed RDF extension.
fn dir_files(ctx: &Ctx<'_>, requested: &str) -> Result<Inputs, GResp> {
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
    Ok(Inputs {
        sources: files.into_iter().map(import::Source::File).collect(),
        temp: Vec::new(),
        unpacked: Vec::new(),
    })
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

/// The statistics `monitor` and `schema` report of one database instance.
struct Inspection {
    triples: u64,
    stats: crate::db::DbStats,
    subjects: usize,
    classes: Arc<Vec<(String, u64)>>,
    graph: Arc<SchemaGraph>,
    in_txn: bool,
    status: DbStatus,
}

/// A cached class statistics + schema graph of one loaded database instance
/// (they take a pass over the data; Workbench polls `monitor`).
struct CachedStats {
    /// The slot it was computed for: a `Weak` keeps the slot's allocation
    /// alive, so a later slot can never be mistaken for this one.
    slot: Weak<Mutex<DbSlot>>,
    /// [`Database::mutation_count`] at computation: any write invalidates.
    mutations: u64,
    classes: Arc<Vec<(String, u64)>>,
    graph: Arc<SchemaGraph>,
}

fn stats_cache() -> &'static Mutex<HashMap<String, CachedStats>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedStats>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The class statistics and schema graph of loaded database `db` (`slot`,
/// locked, of handle `h`), cached until the database changes.
fn class_stats(
    db: &str,
    h: &DbHandle,
    slot: &DbSlot,
) -> (Arc<Vec<(String, u64)>>, Arc<SchemaGraph>) {
    let mutations = slot.db.mutation_count();
    let this = Arc::downgrade(h);
    if let Some(c) = locked(stats_cache()).get(db) {
        if c.mutations == mutations && Weak::ptr_eq(&c.slot, &this) {
            return (Arc::clone(&c.classes), Arc::clone(&c.graph));
        }
    }
    let classes = Arc::new(slot.db.type_statistics());
    let graph = Arc::new(slot.db.schema_graph());
    locked(stats_cache()).insert(
        db.to_string(),
        CachedStats {
            slot: this,
            mutations,
            classes: Arc::clone(&classes),
            graph: Arc::clone(&graph),
        },
    );
    (classes, graph)
}

/// Inspect database `db`: the loaded instance, else — upstream reads an
/// unloaded database's statistics from its files — a temporary copy (a
/// snapshot read without the directory lock, another layout opened under
/// it), the database name reserved meanwhile. `1005` when it does not exist
/// or cannot be read.
fn inspect(ctx: &Ctx<'_>, db: &str) -> Result<Inspection, GResp> {
    let (dbh, dir) = {
        let st = ctx.state();
        (st.dbs.get(db).map(Arc::clone), st.db_dir(db))
    };
    if let Some(h) = dbh {
        let slot = locked(&h);
        if !slot.retired {
            let (classes, graph) = class_stats(db, &h, &slot);
            return Ok(Inspection {
                triples: slot.committed_triple_num(),
                stats: slot.db.stats(),
                subjects: slot.db.subject_num(),
                classes,
                graph,
                in_txn: slot.txn.is_some(),
                status: DbStatus::Loaded,
            });
        }
    }
    if !dir.is_dir() {
        return Err(not_loaded(db, false));
    }
    let _busy = claim(ctx, db)?;
    let copy = if Database::storage_kind(&dir) == Some(crate::StorageKind::Snapshot) {
        Database::read_copy(&dir)
    } else {
        Database::open(&dir)
    };
    let copy = copy.map_err(|e| {
        GResp::error(
            Status::OperationFailed,
            format!("database[{db}] could not be read: {e}"),
        )
    })?;
    Ok(Inspection {
        triples: copy.triple_num(),
        stats: copy.stats(),
        subjects: copy.subject_num(),
        classes: Arc::new(copy.type_statistics()),
        graph: Arc::new(copy.schema_graph()),
        in_txn: false,
        status: DbStatus::AlreadyBuilt,
    })
}

/// `monitor` — statistics of a database (upstream `MessageMonitorResponse`):
/// `database`, `creator`, `builtTime`, `tripleNum` (a string), `entityNum`,
/// `literalNum`, `subjectNum`, `predicateNum`, `connectionNum` (requests
/// served by the `/api` layer and the REST endpoints), `lockNum`, `diskUsed`
/// (MB; `0` with `disk=0`), `costTime` (the build time, ms), `status`,
/// `subjectList` (`{"name","value"}` per `rdf:type` class: its number of
/// instances) and `schema` (as [`schema`]). Like upstream, an unloaded
/// database is reported too (read from disk; its docs say it must be
/// loaded, its code and test script do not).
fn monitor(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let ins = match inspect(ctx, &db) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let (record, connections, dir) = {
        let st = ctx.state();
        (st.sysdb.get(&db).cloned(), st.requests, st.db_dir(&db))
    };
    let disk_used = if p.str("disk").as_deref() == Some("0") {
        0
    } else {
        dir_size(&dir) >> 20
    };
    let mut subject_list = Json::arr();
    for (class, n) in ins.classes.iter() {
        subject_list.push(
            Json::obj()
                .with("name", clear_brackets(class))
                .with("value", *n),
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
        .with("tripleNum", ins.triples.to_string())
        .with("entityNum", ins.stats.entity_num)
        .with("literalNum", ins.stats.literal_num)
        .with("subjectNum", ins.subjects)
        .with("predicateNum", ins.stats.predicate_num)
        .with("connectionNum", connections)
        .with("lockNum", i64::from(ins.in_txn))
        .with("diskUsed", disk_used)
        .with("costTime", read_cost(&dir))
        .with("status", ins.status.as_str())
        .with("subjectList", subject_list)
        .with("schema", schema_json(&ins.graph))
}

/// `schema` — the class / property graph of a database (upstream
/// `schema_task`: `ResponseBody` = `{"nodes":[…],"edges":[…]}`, see
/// [`crate::db::SchemaGraph`]), loaded or not (as [`monitor`]). Always
/// current — derived from the data, cached until it changes — so upstream's
/// `refresh=true` is implied.
fn schema(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let db = db_of(ctx);
    match inspect(ctx, &db) {
        Ok(ins) => GResp::success("success").with("ResponseBody", schema_json(&ins.graph)),
        Err(r) => r,
    }
}

// ---------------------------------------------------------------------------
// build / drop / rename
// ---------------------------------------------------------------------------

/// `build` — create database `db_name` from the server-side RDF files
/// `db_path` (a string or an array; none = an empty database; URLs with
/// `remote=true`; zip packages of RDF files, see "Reading RDF sources").
/// Upstream's checks, in its order: each file exists (`1005 "RDF file '<p>'
/// not exist."`, after the path confinement of `ops_file`) and has an
/// allowed extension (`1003`), each package unpacks (its `ZipCode`), the
/// database does not exist yet (`1004 "database already built."`), and the
/// `max_database_num` limit. The files stream in;
/// statements that fail are skipped, counted and logged to
/// `<db>.db/parse_error.log` (see "Reading RDF sources" above). Answers `0
/// "Import RDF file to database done."` with `success_num` (the distinct
/// statements in the new database: triples plus named-graph quads) and
/// `failed_num`. The creator gets every privilege on the new database; it
/// is not loaded.
fn build(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let started = Instant::now();
    let requested = p.strings("db_path");
    let remote = p.bool("remote", false);
    let inputs = match source_files(ctx, &requested, "db_path", SourceKind::Build, remote) {
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
        if st.database_limit_reached(max_dbs) {
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
    let fail = |msg: String| {
        GResp::error(
            Status::OperationFailed,
            format!("Import RDF file to database failed:{msg}"),
        )
    };
    // Written into a staging directory and renamed into place once complete:
    // a build cut short (a crash, a shutdown that does not wait for it)
    // leaves no `<db>.db` that would read as a built database. The parse
    // error log is written there too, so it lands in `<db>.db/`.
    let staging = PathBuf::from(format!("{}.build-tmp", dir.display()));
    let _ = fs::remove_dir_all(&staging);
    let mut log = match import::ParseErrorLog::in_dir(&staging) {
        Ok(l) => l,
        Err(e) => return fail(e.to_string()),
    };
    progress(
        &format!("parsing {} RDF file(s)", inputs.sources.len()),
        0,
    );
    let (built, report) =
        match Database::build_from_sources(db.clone(), &inputs.sources, &load_options(), &mut log)
        {
            Ok(b) => b,
            Err(e) => {
                drop(log);
                let _ = fs::remove_dir_all(&staging);
                return fail(e.to_string());
            }
        };
    drop(log);
    drop(inputs);
    progress("saving the database", report.statements);
    let success_num = statement_count(&built);
    if report.failed > 0 {
        crate::gs_warn!(
            "build '{db}': RDF parse error num {}; see {}",
            report.failed,
            dir.join(import::PARSE_ERROR_LOG).display()
        );
    }
    let saved = built
        .save(&staging)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            write_cost(
                &staging,
                i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
            );
            fs::rename(&staging, &dir).map_err(|e| e.to_string())
        });
    drop(built);
    if let Err(e) = saved {
        let _ = fs::remove_dir_all(&staging);
        return fail(e);
    }
    {
        let mut st = ctx.state();
        let creator = ctx.user().to_string();
        // Still registered although it did not exist (`build` refuses an
        // existing database): an earlier database of the name, dropped
        // outside the server (`gdrop`). The new one starts without its grants.
        if st.sysdb.contains(&db) {
            let _ = st.users.forget_database(&db);
        }
        // Best effort, like the REST endpoints: the registry is reconciled
        // with the directories at every start.
        let _ = st.sysdb.register(&db, &creator);
        let _ = st.users.grant_all_on(&creator, &db);
    }
    GResp::success("Import RDF file to database done.")
        .with("failed_num", report.failed)
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
    let _lock = match DirLock::acquire_existing(&dir) {
        Ok(l) => l,
        Err(e) => return GResp::error(Status::OperationFailed, format!("Drop fail: {e}")),
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

/// The `1003` answer to `compress=true` without the `zip` feature.
fn no_compress(op: &str) -> GResp {
    GResp::error(
        Status::ParamIsIllegal,
        format!(
            "compress=true ({op}): {}; omit compress for uncompressed output",
            zipio::FEATURE_MISSING
        ),
    )
}

/// Whether `path` is a zipped backup this build can restore: a file named
/// `….zip` (with the `zip` feature).
fn is_zip_backup(path: &Path) -> bool {
    zipio::AVAILABLE && path.is_file() && zipio::is_zip_name(path)
}

/// The database a zipped backup records itself as a backup of (its
/// [`crate::http_api::BACKUP_OWNER_FILE`] entry, as
/// [`crate::http_api::backup_owner`] reads a backup directory's).
fn zip_backup_owner(zip: &Path) -> Option<String> {
    zipio::read_entry(zip, crate::http_api::BACKUP_OWNER_FILE, 4096)
        .ok()
        .flatten()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

/// The backups of `db` in `dir`: upstream's `<db>.db_<YYYYmmddHHMMSS>`
/// entries (directories, or `.zip` files), sorted oldest first. A directory
/// counts only when it is a backup of `db`: one this server recorded as such
/// ([`crate::http_api::backup_owner`]), or one without that record that
/// holds a database (made by upstream or by hand) — never one recorded as
/// another database's, nor a directory holding no database, whatever its
/// name says.
fn backups_of(dir: &Path, db: &str) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| crate::http_api::backup_name_db(n) == Some(db))
        .filter(|n| {
            let p = dir.join(n);
            let owner = if p.is_file() {
                zip_backup_owner(&p)
            } else {
                crate::http_api::backup_owner(&p)
            };
            match owner {
                Some(owner) => owner == db,
                None => p.is_file() || Database::storage_kind(&p).is_some(),
            }
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

/// A path as written, without leading `./` and trailing `/` (`.` → empty).
fn lexical(path: &str) -> &str {
    let mut p = path.trim();
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    let p = p.trim_end_matches('/');
    if p == "." {
        ""
    } else {
        p
    }
}

/// The directory a `backup` (`backup_path`) or `export` (`db_path`) writes
/// into — `requested` inside `root`, else `root` itself — and how to show it
/// in the answer (as the client named it when it is relative to the working
/// directory; else under `root` as configured). A `requested` path written
/// as one of `aliases` names `root` itself: upstream's own defaults, which
/// SDKs send (`./backups`; `export` / the gStore root `./` for exports —
/// upstream's code writes every backup and export to its configured
/// directory, whatever these parameters say).
fn output_dir(
    requested: Option<&str>,
    root: &Path,
    param: &str,
    aliases: &[&str],
) -> Result<(PathBuf, PathBuf), GResp> {
    let io = |e: std::io::Error| {
        GResp::error(
            Status::OperationFailed,
            format!("cannot create '{}': {e}", root.display()),
        )
    };
    fs::create_dir_all(root).map_err(io)?;
    let requested = requested.filter(|r| {
        let r = lexical(r);
        !aliases.iter().any(|a| lexical(a) == r) && lexical(&root.to_string_lossy()) != r
    });
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
/// inside the backup directory.) `compress=true` packs the backup into
/// `<db>.db_<YYYYmmddHHMMSS>.zip` instead (upstream's archive: the backup's
/// files at the archive root; the directory is removed) and answers that as
/// `backupfilepath` — feature `zip`, else `1003` naming it.
fn backup(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let compress = p.bool("compress", false);
    if compress && !zipio::AVAILABLE {
        return no_compress("backup");
    }
    let root = ops_file::backup_dir(ctx);
    let requested = p.non_empty("backup_path");
    let configured = ctx
        .inner
        .config()
        .backup_path
        .to_string_lossy()
        .into_owned();
    let aliases = [configured.as_str(), "backups"];
    let (target, shown) = match output_dir(requested.as_deref(), &root, "backup_path", &aliases) {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Never write into a backup (or a database) inside the backup directory,
    // nor into a directory named like a backup (`<db>.db_<timestamp>`, where
    // `backuppath` / `restore` look for a database's backups).
    if let Ok(canon_root) = fs::canonicalize(&root) {
        let mut cur = Some(target.as_path());
        while let Some(d) = cur.filter(|d| d.starts_with(&canon_root) && *d != canon_root) {
            let backup_named = d
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::http_api::backup_name_db(n).is_some());
            if Database::storage_kind(d).is_some() || backup_named {
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
    let zip_name = format!("{name}.zip");
    let zip_dest = target.join(&zip_name);
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
    // Recorded as a backup of `db` once complete (an interrupted backup holds
    // no record, so `restore` never picks it for anyone but root).
    let written =
        written.and_then(|()| crate::http_api::mark_backup(&dest, &db).map_err(|e| e.to_string()));
    if let Err(e) = written {
        let _ = fs::remove_dir_all(&dest);
        return fail(e);
    }
    let shown_name = if compress {
        // The record travels inside the archive; a zip of the same second
        // is replaced, like a directory.
        progress("compressing the backup", 0);
        let _ = fs::remove_file(&zip_dest);
        if let Err(e) = Database::pack_backup(&dest, &zip_dest) {
            let _ = fs::remove_dir_all(&dest);
            let _ = fs::remove_file(&zip_dest);
            return fail(format!("backup compress fail: {e}"));
        }
        zip_name
    } else {
        name
    };
    rotate_backups(&target, &db, ctx.inner.config().max_backups);
    GResp::success("Database backup successfully.")
        .with("backupfilepath", ops_file::shown(&shown, &shown_name))
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
/// code; its docs show `"Database <db> restore successfully."`). A zipped
/// backup (`….zip`, as `backup compress=true` writes; feature `zip`, else
/// `1003` naming it) is unpacked beside the database under the `conf.ini`
/// zip limits first (upstream `restore_databaseinfo`; a package that does not
/// unpack: `1005 "backup uncompress fail: …"`); any other file is `1003
/// "Backup file is not zip file."`.
fn restore(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let root = ops_file::backup_dir(ctx);
    let user = ctx.user().to_string();
    // A zipped backup carries its record inside the archive.
    let may_restore = |ctx: &Ctx<'_>, path: &Path| {
        (is_zip_backup(path) && zip_backup_owner(path).as_deref() == Some(db.as_str()))
            || may_restore_from(&ctx.state().users, &user, &db, path)
    };
    let source = match p.non_empty("backup_path") {
        None => match backups_of(&root, &db)
            .into_iter()
            .rev()
            .map(|n| root.join(n))
            .find(|p| (p.is_dir() || is_zip_backup(p)) && may_restore(ctx, p))
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
                Ok(path) if path.is_dir() || is_zip_backup(&path) => path,
                Ok(path) => {
                    let msg = if zipio::is_zip_name(&path) {
                        format!(
                            "zip backup ('{r}'): {}; restore from a backup directory",
                            zipio::FEATURE_MISSING
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
    // The restore privilege on `db` restores `db`'s own backups: another
    // database's backup would hand the caller that database's data.
    if !may_restore(ctx, &source) {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            format!(
                "You have no restore privilege on the backup '{}': it is not a backup of \
                 database[{db}] (only root and admin users may restore another database's backup)",
                p.str("backup_path").unwrap_or_default()
            ),
        );
    }
    let fail = |msg: String| GResp::error(Status::OperationFailed, format!("Restore fail: {msg}"));
    let zipped = source.is_file();
    let no_backup = |at: &Path| {
        fail(format!(
            "'{}' does not contain a database backup",
            at.display()
        ))
    };
    if !zipped && Database::storage_kind(&source).is_none() {
        return no_backup(&source);
    }
    let max_dbs = ctx.inner.config().max_database_num;
    let (dir, fresh) = {
        let mut st = ctx.state();
        if let Some(&tid) = st.db_txn.get(&db) {
            return in_txn(&db, tid);
        }
        let dir = st.db_dir(&db);
        // A restore that creates the database counts against
        // `max_database_num`, as `build` does.
        let fresh = !st.dbs.contains_key(&db) && !dir.exists();
        if fresh && !st.sysdb.contains(&db) && st.database_limit_reached(max_dbs) {
            return GResp::error(
                Status::OperationConditionsAreNotSatisfied,
                "The total number of databases more than max_databse_num.",
            );
        }
        if !st.busy.insert(db.clone()) {
            return busy(&db);
        }
        (dir, fresh)
    };
    let _busy = BusyGuard {
        inner: ctx.inner,
        name: db.clone(),
    };
    // A zipped backup is unpacked beside the database first (removed once
    // staged); the name is reserved meanwhile, so no other restore of it
    // shares the scratch directory.
    let mut scratch = None;
    let from = if zipped {
        progress("uncompressing the backup", 0);
        let mut at = dir.as_os_str().to_owned();
        at.push(".unzip-tmp");
        let s = match ScratchDir::fresh(PathBuf::from(at)) {
            Ok(s) => s,
            Err(e) => return fail(format!("cannot unpack the backup: {e}")),
        };
        let limits = ZipLimits::from_config(&ctx.inner.config());
        let found = match Database::unpack_backup(&source, s.path(), &limits) {
            Ok(found) => found,
            Err(e) => {
                return GResp::error(
                    Status::OperationFailed,
                    format!("backup uncompress fail: {e}"),
                )
            }
        };
        if Database::storage_kind(&found).is_none() {
            return no_backup(&source);
        }
        scratch = Some(s);
        found
    } else {
        source.clone()
    };
    progress("restoring", 0);
    // Copy and validate first; nothing is touched if the backup is bad.
    let staged = Database::stage_restore(&from, &dir);
    drop(scratch);
    let staged = match staged {
        Ok(s) => s,
        Err(e) => return fail(e.to_string()),
    };
    // No other process may have the database open while it is swapped.
    let probe = match DirLock::acquire_existing(&dir) {
        Ok(l) => l,
        Err(e) => {
            Database::discard_staged_restore(&staged);
            return fail(e.to_string());
        }
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
        // A database the restore created joins the registry with the caller
        // as its creator. No grants: none left from an earlier database of
        // the name (dropped outside the server) hold, and the caller — `root`
        // or an `admin` user, or one holding the restore privilege on every
        // database — is not given more than it had (upstream grants the
        // creator everything; a user with only the restore privilege would
        // gain all the others on the copy).
        let mut st = ctx.state();
        if fresh && st.sysdb.contains(&db) {
            let _ = st.users.forget_database(&db);
        }
        if fresh || !st.sysdb.contains(&db) {
            let built = backup_time(&source).unwrap_or_else(|| clock::now().datetime());
            let _ = st.sysdb.register_at(&db, &user, &built);
        }
    }
    GResp::success("Database restore successfully.")
}

/// `export` — write the loaded database `db_name` to
/// `<db_path>/<db>_<YYYYmmddHHMMSS>.nt` (upstream's name; a numeric suffix
/// keeps two exports of one second apart), `db_path` defaulting to the
/// export directory and confined to it. Answers `0 "Export the database
/// successfully."` with `filepath`, which `/file/download` serves.
///
/// The format (a Rust extension; upstream writes N-Triples): `format=nt` (or
/// `ntriples` / `n-triples`) writes N-Triples — the default graph, as
/// upstream; `format=nq` (`nquads` / `n-quads`) writes N-Quads to `….nq` —
/// the default graph, then every named graph. Without `format` a database
/// with named graphs is written as N-Quads (nothing is lost), any other as
/// N-Triples (upstream's file). Another `format` is `1003`.
///
/// `compress=true` packs the file into `<db>_<YYYYmmddHHMMSS>.zip` (holding
/// it by its file name; the file itself is removed) and answers that as
/// `filepath`, like upstream — feature `zip`, else `1003` naming it; a
/// failure to pack is upstream's `StatusCode` `2` `"export compress fail."`.
fn export(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = db_of(ctx);
    let compress = p.bool("compress", false);
    if compress && !zipio::AVAILABLE {
        return no_compress("export");
    }
    let format = match p.non_empty("format") {
        None => None,
        Some(f) => match ExportFormat::parse(&f) {
            Some(fmt) => Some(fmt),
            None => {
                return GResp::error(
                    Status::ParamIsIllegal,
                    format!(
                        "the export format '{f}' is not supported (use nt for N-Triples or nq \
                         for N-Quads)"
                    ),
                )
            }
        },
    };
    let dbh = match loaded_handle(ctx, &db) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let root = ctx.export_dir();
    let requested = p.non_empty("db_path");
    let (target, shown) = match output_dir(requested.as_deref(), &root, "db_path", &["", "export"])
    {
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
    let format = format.unwrap_or_else(|| ExportFormat::for_database(&slot.db));
    let stem = format!("{db}_{}", ops_file::timestamp_now());
    let (file, name) = match ops_file::create_unique(&target, &stem, format.extension()) {
        Ok(x) => x,
        Err(e) => return fail(e.to_string()),
    };
    let mut out = BufWriter::new(file);
    let written = zipio::write_export(&slot.db, format, &mut out);
    drop(slot);
    drop(out);
    if let Err(e) = written {
        let _ = fs::remove_file(target.join(&name));
        return fail(e.to_string());
    }
    let name = if compress {
        // `<stem>.zip` beside the file (a free name: another export of the
        // same second may hold the plain one).
        let file_stem = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(s, _)| s)
            .to_string();
        let packed = match ops_file::create_unique(&target, &file_stem, "zip") {
            Ok((_, zip)) => {
                match zipio::pack_file(&target.join(&name), &target.join(&zip), &name) {
                    Ok(()) => Ok(zip),
                    Err(e) => {
                        let _ = fs::remove_file(target.join(&zip));
                        Err(e.to_string())
                    }
                }
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = fs::remove_file(target.join(&name));
        match packed {
            Ok(zip) => zip,
            Err(e) => {
                return GResp::error(
                    Status::CompressError,
                    format!("export compress fail. ({e})"),
                )
            }
        }
    } else {
        name
    };
    GResp::success("Export the database successfully.")
        .with("filepath", ops_file::shown(&shown, &name))
}

// ---------------------------------------------------------------------------
// batchInsert / batchRemove
// ---------------------------------------------------------------------------

/// `batchInsert` — insert the statements of server-side RDF files: `file` (a
/// string or an array; URLs with `remote=true`) or, when it is absent, every
/// RDF file directly inside `dir` (upstream's documentation describes `dir`;
/// its code reads only `file`). The files stream in, each with its own
/// blank-node scope; N-Quads / TriG statements go to their named graphs.
/// Answers `0 "Batch insert data successfully."` with `success_num`
/// (statements inserted; duplicates are not counted) and `failed_num`
/// (statements skipped: see "Reading RDF sources", which also describes the
/// `<db>.db/parse_error.log` entries).
fn batch_insert(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    batch(ctx, p, true)
}

/// `batchRemove` — remove the statements of server-side RDF files (as
/// [`batch_insert`]; blank-node labels are read verbatim): `0 "Batch remove
/// data successfully."`, `success_num` = statements removed, `failed_num`.
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
    let dir = {
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
        st.db_dir(&db)
    };
    let requested = p.strings("file");
    let remote = p.bool("remote", false);
    let inputs = if !requested.is_empty() {
        source_files(ctx, &requested, "file", SourceKind::Batch, remote)
    } else if let Some(d) = p.non_empty("dir") {
        dir_files(ctx, &d)
    } else {
        Err(GResp::error(
            Status::ParamIsIllegal,
            "the value of file can not be empty!",
        ))
    };
    let inputs = match inputs {
        Ok(f) => f,
        Err(r) => return r,
    };
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
    // Upstream appends to the database's parse error log; a log that cannot
    // be opened does not fail the operation (it is reported on stderr).
    let mut file_log;
    let mut no_log = ();
    let log: &mut dyn import::ErrorLog = match import::ParseErrorLog::in_dir(&dir) {
        Ok(l) => {
            file_log = l;
            &mut file_log
        }
        Err(e) => {
            crate::gs_warn!("cannot open the parse error log of '{db}': {e}");
            &mut no_log
        }
    };
    progress(if insert { "inserting" } else { "removing" }, 0);
    let mut applied = 0u64;
    let result = if insert {
        slot.db
            .insert_sources(&inputs.sources, &load_options(), log, &mut applied)
    } else {
        slot.db.remove_sources(
            &inputs.sources,
            &load_options().for_removal(),
            log,
            &mut applied,
        )
    };
    let warning = if applied > 0 {
        slot.wrote(ctx.inner.save_writes())
    } else {
        None
    };
    drop(slot);
    drop(inputs);
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            return GResp::error(
                Status::OperationFailed,
                format!("{fail_prefix}stopped after applying {applied} triple(s): {e}"),
            )
        }
    };
    if let Some(w) = warning {
        // Upstream fails the operation when its check_point save fails (the
        // change stays applied in memory and is saved later).
        return GResp::error(Status::OperationFailed, format!("{fail_prefix}{w}"));
    }
    GResp::success(done)
        .with("success_num", applied)
        .with("failed_num", report.failed)
}

/// The distinct statements a database holds: default-graph triples plus
/// named-graph quads (`build`'s `success_num`).
fn statement_count(db: &Database) -> u64 {
    db.triple_num()
        + db
            .named_graphs()
            .values()
            .map(|g| g.triple_count())
            .sum::<u64>()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_as_written() {
        assert_eq!(lexical("./backups/"), "backups");
        assert_eq!(lexical("././x//"), "x");
        assert_eq!(lexical("./"), "");
        assert_eq!(lexical("."), "");
        assert_eq!(lexical("/abs/dir/"), "/abs/dir");
    }

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
        // Backup directories hold a database (here: its snapshot's meta file).
        for n in [
            "lubm.db_20240102000000",
            "lubm.db_20240101000000",
            "xlubm.db_20240101000000",
            "lubm.db_2024",
            "lubm.db_20240101000000x",
        ] {
            fs::create_dir_all(d.join(n)).unwrap();
            fs::write(d.join(n).join("meta.bin"), b"").unwrap();
        }
        fs::write(d.join("lubm.db_20240103000000.zip"), b"zip").unwrap();
        // Not backups of `lubm`, whatever their names say: one recorded as
        // another database's, one holding no database.
        fs::create_dir_all(d.join("lubm.db_20990101000000")).unwrap();
        fs::write(d.join("lubm.db_20990101000000").join("meta.bin"), b"").unwrap();
        crate::http_api::mark_backup(&d.join("lubm.db_20990101000000"), "other").unwrap();
        fs::create_dir_all(
            d.join("lubm.db_20980101000000")
                .join("mine.db_20240101000000"),
        )
        .unwrap();
        crate::http_api::mark_backup(&d.join("lubm.db_20240101000000"), "lubm").unwrap();
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
        let (mut stored, mut failed) = (0u64, 0u64);
        let mut read = |f: &Path| {
            let mut errors: Vec<(String, crate::parser::stream::SyntaxError)> = Vec::new();
            let r = import::import_source(
                &import::Source::File(f.to_path_buf()),
                "build",
                &load_options(),
                &mut errors,
                &mut |b: &mut Vec<crate::parser::nquads::Quad>| {
                    b.clear();
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(errors.len() as u64, r.failed);
            stored += r.statements;
            failed += r.failed;
            (stored, failed)
        };
        assert_eq!(read(&good), (1, 1), "literal subject");
        assert_eq!(read(&mixed), (3, 2));
        assert_eq!(read(&bad_ttl), (3, 3), "an unterminated statement");
        let _ = fs::remove_dir_all(&d);
    }
}
