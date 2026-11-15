//! Upstream's file routes — `POST /file/upload` and `/file/download`
//! (`GET /file/download/<path>`, `POST /file/download` with `filepath=`), also
//! under the documented `/grpc/` prefix (upstream `src/Main/grpc.cpp`
//! `upload_file` / `download_file` / `download_file_post`) — and the
//! server-side path rules every operation that names a server path follows
//! (`build`'s `db_path`, `batchInsert`'s `file` / `dir`, `backup` /
//! `restore`'s `backup_path`, `export`'s `db_path`; see `ops_db`).
//!
//! # Server-side paths
//!
//! A client never makes the server read or write outside a few configured
//! directories (the *roots*):
//!
//! | root | what lives there | read by | written by |
//! |------|------------------|---------|------------|
//! | upload dir (`upload_path`, else `<db home>/upload`) | uploaded RDF files and zip packages; packages unpacked for one operation | `build`, `batchInsert`, `batchRemove`, `/file/download` (each upload by its uploader) | `/file/upload`; `build` / `batch*` unpacking a zip |
//! | export dir (`<db home>/export` unless configured) | exports, `format=file` query results | the same (each file by the users of its database) | `export`, `query` |
//! | backup dir (`backup_path`, else `<db home>/backups`) | `<db>.db_<YYYYmmddHHMMSS>` backups (directories, or `.zip` archives) | `restore` | `backup` |
//! | the load policy's directories (`gserver --load-dir`) | server-local data sets | `build`, `batch*`, `restore` | — |
//!
//! Every path parameter is resolved like upstream — a relative path against
//! the server's working directory (upstream: "the gStore root directory"),
//! and for convenience also against the root it is meant for — then
//! **canonicalized** (symlinks resolved) and must lie inside a root. A `..`
//! component is refused outright, so is a path that resolves (or, when it
//! does not exist, would resolve) outside every root — with the same answer
//! whether or not the file exists, so the check reveals nothing about the
//! rest of the file system.
//!
//! # Owner records, names and retention
//!
//! Every file the server writes for a client — an upload, an export, a
//! `format=file` query result — gets an **owner record** (its path, the kind
//! of file, the database it came from, the user whose request wrote it),
//! kept in `<db home>/file_owners/`, which no route serves. A user other
//! than `root` (or an `admin`) may read such a file — download it, or name
//! it as a `build` / `batch*` source — only when the record allows: an
//! upload its uploader, an export the users holding the `export` privilege
//! on its database, a query result those holding `query` on it (review
//! sec#4, gapi#2, gapi#14). A file in these two directories without a
//! record (an operator's, or one written before records existed) is
//! `root`'s / `admin`'s alone.
//!
//! Export and query-result names carry 128 random bits
//! (`<db>_<YYYYmmddHHMMSS>_<32 hex digits>.nt`, `<db>_<thread>_<UTC
//! time>_<32 hex digits>.json`): clients use the name the answer reports, so
//! nothing depends on guessing one. The recorded files are removed once they
//! are older than `file_retention_hours` (`conf.ini`), and the spilled
//! results of asynchronous operations an earlier run of the server left
//! under `<root>/tasks` are removed too: a lazy sweep, run on uploads,
//! exports, downloads, `format=file` queries and asynchronous submissions, at
//! most once a minute per server (review sec#14, gapi#13). An upload beyond
//! the uploader's `upload_user_quota` is refused (`1005`).
//!
//! # `/file/upload`
//!
//! `multipart/form-data` with the file in the part named `file` and the
//! usual `username` / `password` (`encryption=1`) fields: the IP checks,
//! authentication, then the file is checked like upstream — no file → `1003
//! "Upload file can not be empty!"`; larger than `upload_max_body_size` →
//! `1005 "Upload file more than max_body_size!"`; an extension outside
//! `upload_allow_extensions` / `upload_allow_compress_packages` → `1005 "The
//! type of upload file is not supported!"` — and stored in the upload dir as
//! `<name>_<YYYYmmddHHMMSS>.<ext>` (the client's base name, reduced to
//! `[A-Za-z0-9._-]`; a numeric suffix keeps two uploads of one second apart).
//! The answer `{"StatusCode":0,"StatusMsg":"success","filepath":…}` names the
//! stored file as a path `build` / `batchInsert` accept as `db_path` / `file`
//! — from the uploader (or `root`) only. (Upstream checks this route's
//! privilege under the name `uploadfile`, which its privilege table does not
//! list, so only `root` may upload there; its documentation and its table's
//! `upload` entry open uploads to every user, which is what this server
//! does.)
//!
//! # `/file/download`
//!
//! The file named by the path after `/file/download/` (URL-decoded) or by the
//! `filepath` parameter, sent as `application/octet-stream` with a
//! `Content-Disposition: attachment` file name. Like upstream, a relative path
//! is taken inside the export directory (`export/…` and bare names alike).
//!
//! A download needs valid credentials — `username` / `password` parameters
//! (`encryption=1` as everywhere), or an HTTP `Authorization: Basic` header;
//! without them the answer is HTTP `401` (`WWW-Authenticate: Basic`, a
//! `1001` envelope as the body) before anything is resolved. A user then
//! reads the files the owner records allow (see above), and `root` (or an
//! `admin` user) every file of the export and upload directories, of the
//! backup directory and of database directories; nothing else under the
//! database home — the user table, the registry, the logs, the owner
//! records — is ever served. A file outside those roots, that the caller
//! may not read, or that does not exist, is answered with upstream's
//! `FileNotFound` envelope (`StatusCode` 11) either way.
//!
//! Upstream asks for no credentials and serves the export directory to
//! anyone: `conf.ini`'s upstream-compat switch `[upload] anonymous_download=on`
//! restores that (every file of the export directory, to anyone; `gserver`
//! warns at startup while it is on). The body is streamed from the file in
//! bounded chunks, never compressed, and at most `download_max_concurrent`
//! downloads run at once (more are answered `503` with `Retry-After`).
//!
//! A single `Range: bytes=a-b` (or `a-`, `-n`) request header is honoured
//! with `206 Partial Content` and a `Content-Range` (`416` when it cannot be
//! satisfied); every response carries `Accept-Ranges: bytes`. (Upstream always
//! sends the whole file, with a `Content-Range` header that does not follow
//! RFC 7233.)

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use super::auth::{self, IpStage};
use super::envelope::{FileBody, GResp, Status};
use super::params::{self, Params};
use super::{Ctx, PathRoute, Registry};
use crate::http_api::ApiInner;
use crate::json::Json;
use crate::server::locked;
use crate::sysdb::clock;

pub(super) fn register(reg: &mut Registry) {
    for path in ["/file/upload", "/grpc/file/upload"] {
        reg.add_path(PathRoute {
            name: "upload",
            path,
            prefix: false,
            methods: &["POST"],
            handler: upload,
        });
    }
    for path in ["/file/download", "/grpc/file/download"] {
        reg.add_path(PathRoute {
            name: "download",
            path,
            prefix: false,
            methods: &["GET", "POST"],
            handler: download,
        });
    }
    for path in ["/file/download/", "/grpc/file/download/"] {
        reg.add_path(PathRoute {
            name: "download",
            path,
            prefix: true,
            methods: &["GET"],
            handler: download,
        });
    }
}

// ---------------------------------------------------------------------------
// The roots
// ---------------------------------------------------------------------------

/// The upload directory: `conf.ini`'s `upload_path` when set, else
/// `<db home>/upload`.
pub(super) fn upload_dir(ctx: &Ctx<'_>) -> PathBuf {
    let cfg = ctx.inner.config();
    if cfg.is_set("upload_path") {
        cfg.upload_path.clone()
    } else {
        ctx.root_dir().join("upload")
    }
}

/// The backup directory (see [`crate::http_api`]'s `backup_dir`).
pub(super) fn backup_dir(ctx: &Ctx<'_>) -> PathBuf {
    ctx.inner.backup_dir()
}

/// Where a server-local *source* (an RDF file or directory, a backup to
/// restore) may be read from, besides the root an operation names itself:
/// the directories of the server's load policy — `None` when that policy
/// allows any local path (a library default; servers start restricted).
pub(super) fn load_policy_dirs(ctx: &Ctx<'_>) -> Option<Vec<PathBuf>> {
    let st = ctx.state();
    let policy = &st.load_policy;
    if !policy.allow_local_files {
        return Some(Vec::new());
    }
    policy.allowed_dirs.clone()
}

/// The roots an RDF source file (`build`'s `db_path`, `batch*`'s `file` /
/// `dir`) may be read from, `None` meaning any path (see
/// [`load_policy_dirs`]).
pub(super) fn source_roots(ctx: &Ctx<'_>) -> Option<Vec<PathBuf>> {
    let mut roots = load_policy_dirs(ctx)?;
    roots.push(upload_dir(ctx));
    roots.push(ctx.export_dir());
    Some(roots)
}

/// Why a path parameter was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathRefusal {
    /// A `..` component (or a NUL byte): refused before anything is resolved.
    Traversal,
    /// Outside every root (whether or not it exists).
    Outside,
    /// Inside a root, but nothing is there.
    Missing,
}

impl PathRefusal {
    /// The `1003` answer for `requested` (named `what`), listing the roots.
    pub(super) fn answer(self, what: &str, requested: &str, roots: &[PathBuf]) -> GResp {
        let list = roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let msg = match self {
            PathRefusal::Traversal => {
                format!("the {what} '{requested}' is refused: '..' is not allowed in a path")
            }
            PathRefusal::Outside | PathRefusal::Missing => format!(
                "the {what} '{requested}' is refused: it is not inside the directories the \
                 server may use for it ({list})"
            ),
        };
        GResp::error(Status::ParamIsIllegal, msg)
    }
}

/// Whether `requested` has a `..` component or a NUL byte.
fn has_traversal(requested: &str) -> bool {
    requested.contains('\0')
        || Path::new(requested)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
}

/// The candidates `requested` may name: itself when absolute; else relative
/// to the working directory, then to each of `bases`.
fn candidates(requested: &str, bases: &[PathBuf]) -> Vec<PathBuf> {
    let p = Path::new(requested);
    if p.is_absolute() {
        return vec![p.to_path_buf()];
    }
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join(p));
    }
    out.extend(bases.iter().map(|b| b.join(p)));
    out
}

/// The canonical form of `path`'s deepest existing ancestor with the rest of
/// `path` re-appended (plain names: no `..` can remain, see
/// [`has_traversal`]) — where `path` would be once created.
fn canonical_prefix(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut existing = path;
    let mut base = loop {
        match fs::canonicalize(existing) {
            Ok(c) => break c,
            Err(_) => {
                rest.push(existing.file_name()?.to_os_string());
                existing = existing.parent()?;
            }
        }
    };
    for part in rest.iter().rev() {
        base.push(part);
    }
    Some(base)
}

/// The canonical roots (those that exist).
fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .collect()
}

/// Resolve `requested`, which must exist, inside one of `roots` (`None` =
/// anywhere): see the module docs. A relative path is also tried against each
/// of `bases`. Returns the canonical path.
pub(super) fn resolve_existing(
    requested: &str,
    roots: Option<&[PathBuf]>,
    bases: &[PathBuf],
) -> Result<PathBuf, PathRefusal> {
    if requested.trim().is_empty() {
        return Err(PathRefusal::Missing);
    }
    if has_traversal(requested) {
        return Err(PathRefusal::Traversal);
    }
    let roots = roots.map(canonical_roots);
    let inside = |p: &Path| {
        roots
            .as_ref()
            .is_none_or(|rs| rs.iter().any(|r| p.starts_with(r)))
    };
    let mut missing = false;
    for cand in candidates(requested, bases) {
        match fs::canonicalize(&cand) {
            Ok(c) if inside(&c) => return Ok(c),
            Ok(_) => {}
            Err(_) => missing |= canonical_prefix(&cand).is_some_and(|c| inside(&c)),
        }
    }
    Err(if missing {
        PathRefusal::Missing
    } else {
        PathRefusal::Outside
    })
}

/// Resolve `requested` — a directory the server will create or write into,
/// which need not exist — inside `root` (itself allowed), trying it relative
/// to the working directory, then to `root`. Returns where it is (canonical
/// up to the parts still to be created).
pub(super) fn resolve_target(requested: &str, root: &Path) -> Result<PathBuf, PathRefusal> {
    if has_traversal(requested) {
        return Err(PathRefusal::Traversal);
    }
    let Ok(canon_root) = fs::canonicalize(root) else {
        return Err(PathRefusal::Outside);
    };
    for cand in candidates(requested, &[root.to_path_buf()]) {
        if let Some(c) = canonical_prefix(&cand) {
            if c.starts_with(&canon_root) {
                return Ok(c);
            }
        }
    }
    Err(PathRefusal::Outside)
}

/// `dir` and `name` joined for an answer, keeping `dir` as configured
/// (`./backups/` + `x` → `./backups/x`, as upstream reports paths).
pub(super) fn shown(dir: &Path, name: &str) -> String {
    let d = dir.to_string_lossy();
    if d.is_empty() {
        name.to_string()
    } else if d.ends_with('/') || d.ends_with(std::path::MAIN_SEPARATOR) {
        format!("{d}{name}")
    } else {
        format!("{d}/{name}")
    }
}

/// The local time as `YYYYmmddHHMMSS` (upstream `TimeUtil::now()`,
/// `PURE_DATETIME_PATTERN`).
pub(super) fn timestamp_now() -> String {
    let t = clock::now();
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// The extension of a file name (after its last `.`), if any.
pub(super) fn suffix_of(name: &str) -> Option<&str> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty())
}

/// Whether `ext` is one of the configured compressed package types (`zip`).
pub(super) fn is_package(ctx: &Ctx<'_>, ext: &str) -> bool {
    ctx.inner
        .config()
        .upload_allow_compress_packages
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// Whether `ext` is one of the configured RDF extensions.
pub(super) fn is_rdf_ext(ctx: &Ctx<'_>, ext: &str) -> bool {
    ctx.inner
        .config()
        .upload_allow_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// Create `dir/<stem><n>.<ext>` exclusively (`n` empty, then `_1`, `_2`, …),
/// returning the open file and its name.
pub(super) fn create_unique(dir: &Path, stem: &str, ext: &str) -> std::io::Result<(File, String)> {
    for n in 0u32..10_000 {
        let name = if n == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}_{n}.{ext}")
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
        {
            Ok(f) => return Ok((f, name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free file name",
    ))
}

/// Create directory `dir/<stem><n>` exclusively (`n` empty, then `_1`, `_2`,
/// …), returning its path — where a zip package is unpacked (upstream:
/// `<upload_path><zip name>_<YYYYmmddHHMMSS>`).
pub(super) fn create_unique_dir(dir: &Path, stem: &str) -> std::io::Result<PathBuf> {
    for n in 0u32..10_000 {
        let path = if n == 0 {
            dir.join(stem)
        } else {
            dir.join(format!("{stem}_{n}"))
        };
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free directory name",
    ))
}

// ---------------------------------------------------------------------------
// Owner records, unguessable names (see the module docs)
// ---------------------------------------------------------------------------

/// What wrote a file the server keeps for clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileKind {
    /// An `export` of a database: read with the `export` privilege on it.
    Export,
    /// A `format=file` / `n-triple-file` query result: the `query` privilege.
    QueryResult,
    /// A `/file/upload`: its uploader.
    Upload,
}

impl FileKind {
    fn name(self) -> &'static str {
        match self {
            FileKind::Export => "export",
            FileKind::QueryResult => "query",
            FileKind::Upload => "upload",
        }
    }

    fn parse(s: &str) -> Option<FileKind> {
        [FileKind::Export, FileKind::QueryResult, FileKind::Upload]
            .into_iter()
            .find(|k| k.name() == s)
    }
}

/// The owner record of a server-written file (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Owner {
    pub(super) kind: FileKind,
    /// The database an export or a query result came from.
    pub(super) db: Option<String>,
    /// The user whose request wrote the file.
    pub(super) user: String,
}

impl Owner {
    /// The record of a file written for `ctx`'s caller.
    pub(super) fn of(ctx: &Ctx<'_>, kind: FileKind, db: Option<&str>) -> Owner {
        Owner {
            kind,
            db: db.map(str::to_string),
            user: ctx.user().to_string(),
        }
    }
}

/// The directory, under the server root, that holds the owner records
/// (outside every directory a client reads from).
const OWNER_DIR: &str = "file_owners";

/// The record store of `ctx`'s server.
fn owner_dir(ctx: &Ctx<'_>) -> PathBuf {
    ctx.root_dir().join(OWNER_DIR)
}

/// Where the owner record of the file at canonical `path` is kept: one
/// file per recorded file, named by the SHA-256 of its path.
fn record_path(ctx: &Ctx<'_>, path: &Path) -> PathBuf {
    let key = crate::query::hash::sha256_hex(&path.to_string_lossy());
    owner_dir(ctx).join(format!("{key}.json"))
}

/// Record `owner` as the owner of `file` (written right after `file` was
/// created, before its name is reported to anyone). The record is written
/// atomically and durably (temp file, `fsync`, rename, directory `fsync`):
/// a crash never leaves a torn record, which would make the file unreadable
/// to its owner and exempt from retention and the upload quota.
pub(super) fn record_owner(ctx: &Ctx<'_>, file: &Path, owner: &Owner) -> io::Result<()> {
    let path = fs::canonicalize(file)?;
    let mut j = Json::obj()
        .with("path", path.to_string_lossy().as_ref())
        .with("kind", owner.kind.name())
        .with("user", owner.user.as_str());
    if let Some(db) = &owner.db {
        j.set("db", db.as_str());
    }
    let dir = owner_dir(ctx);
    if !dir.is_dir() {
        fs::create_dir_all(&dir)?;
        crate::db::fsutil::sync_dir(ctx.root_dir()).map_err(to_io)?;
    }
    crate::db::fsutil::atomic_write(&record_path(ctx, &path), j.to_string().as_bytes())
        .map_err(to_io)
}

fn to_io(e: crate::error::GStoreError) -> io::Error {
    match e {
        crate::error::GStoreError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

/// A record's file and owner.
fn parse_record(text: &str) -> Option<(PathBuf, Owner)> {
    let j = crate::json::parse(text).ok()?;
    let path = PathBuf::from(j.get("path")?.as_str()?);
    let owner = Owner {
        kind: FileKind::parse(j.get("kind")?.as_str()?)?,
        db: j.get("db").and_then(Json::as_str).map(str::to_string),
        user: j.get("user")?.as_str()?.to_string(),
    };
    Some((path, owner))
}

/// The owner record of `file`, if it has one.
pub(super) fn read_owner(ctx: &Ctx<'_>, file: &Path) -> Option<Owner> {
    let path = fs::canonicalize(file).ok()?;
    let text = fs::read_to_string(record_path(ctx, &path)).ok()?;
    let (recorded, owner) = parse_record(&text)?;
    (recorded == path).then_some(owner)
}

/// Remove a server-written file and its owner record.
pub(super) fn remove_with_owner(ctx: &Ctx<'_>, file: &Path) {
    if let Ok(path) = fs::canonicalize(file) {
        let _ = fs::remove_file(record_path(ctx, &path));
    }
    let _ = fs::remove_file(file);
}

/// Create `dir/<stem><n>.<ext>` like [`create_unique`] and record `owner`
/// for it; on a failure to record, the new file is removed again.
pub(super) fn create_owned(
    ctx: &Ctx<'_>,
    dir: &Path,
    stem: &str,
    ext: &str,
    owner: &Owner,
) -> io::Result<(File, String)> {
    let (file, name) = create_unique(dir, stem, ext)?;
    if let Err(e) = record_owner(ctx, &dir.join(&name), owner) {
        drop(file);
        remove_with_owner(ctx, &dir.join(&name));
        return Err(e);
    }
    Ok((file, name))
}

/// 128 random bits as 32 lowercase hex digits: the part of an export's or a
/// query result's name nobody can guess (review sec#4, gapi#2).
pub(super) fn random_tag() -> String {
    let mut bytes = [0u8; 16];
    crate::http_users::random_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether the caller is `root` or an `admin` user.
fn is_admin(ctx: &Ctx<'_>) -> bool {
    ctx.is_root() || (ctx.user.is_some() && ctx.state().users.is_superuser(ctx.user()))
}

/// Whether the authenticated caller may read the server-written `file` (a
/// download, or a `build` / `batch*` source) as its owner record allows:
/// `root` / `admin` always; an upload its uploader; an export the holders
/// of the `export` privilege on its database, a query result those of
/// `query`. A file without a record is `root`'s / `admin`'s alone.
pub(super) fn may_read_recorded(ctx: &Ctx<'_>, file: &Path) -> bool {
    if ctx.user.is_none() {
        return false;
    }
    if is_admin(ctx) {
        return true;
    }
    match read_owner(ctx, file) {
        Some(Owner {
            kind: FileKind::Upload,
            user,
            ..
        }) => user == ctx.user(),
        Some(Owner {
            kind: FileKind::Export,
            db: Some(db),
            ..
        }) => auth::check_privilege(ctx, "export", Some(&db)),
        Some(Owner {
            kind: FileKind::QueryResult,
            db: Some(db),
            ..
        }) => auth::check_privilege(ctx, "query", Some(&db)),
        _ => false,
    }
}

/// Whether the caller may use `file` (canonical) as an RDF source: one in
/// the export or the upload directory must pass [`may_read_recorded`] — no
/// `batchInsert` from another database's export or another user's upload
/// (review sec#4, gapi#14). Sources in the load policy's directories are not
/// affected.
fn may_use_source(ctx: &Ctx<'_>, file: &Path) -> bool {
    let inside = |root: PathBuf| fs::canonicalize(root).is_ok_and(|r| file.starts_with(&r));
    let recorded_root = inside(ctx.export_dir()) || inside(upload_dir(ctx));
    !recorded_root || may_read_recorded(ctx, file)
}

/// What a refused source may be (the tail of the refusal messages).
const UNREADABLE: &str = "an export or query result of a database you lack the privilege on, \
                          another user's upload, or a file only root may use";

/// Refuse an RDF source the caller named (`build`'s `db_path`, `batch*`'s
/// `file`; `file` canonical) that it may not read (see [`may_use_source`]).
pub(super) fn check_source(
    ctx: &Ctx<'_>,
    file: &Path,
    requested: &str,
    param: &str,
) -> Result<(), GResp> {
    if may_use_source(ctx, file) {
        return Ok(());
    }
    Err(GResp::error(
        Status::ParamIsIllegal,
        format!("the {param} '{requested}' is refused: you may not read it ({UNREADABLE})"),
    ))
}

/// Refuse a `batch*` `dir` (as `requested`) holding an RDF file (`file`,
/// canonical) the caller may not read (see [`may_use_source`]). The answer
/// names the directory only: the names of the files in it are not the
/// caller's to learn — an export's random name is what keeps it from
/// anonymous readers under `anonymous_download=on`.
pub(super) fn check_dir_source(ctx: &Ctx<'_>, file: &Path, requested: &str) -> Result<(), GResp> {
    if may_use_source(ctx, file) {
        return Ok(());
    }
    Err(GResp::error(
        Status::ParamIsIllegal,
        format!(
            "the dir '{requested}' is refused: it holds a file you may not read ({UNREADABLE})"
        ),
    ))
}

// ---------------------------------------------------------------------------
// Per-server state
// ---------------------------------------------------------------------------

/// Per-server state of the file routes: the downloads in flight and when
/// the retention sweep last ran.
#[derive(Default)]
struct FileState {
    downloads: AtomicUsize,
    last_sweep: Mutex<Option<Instant>>,
}

/// The [`FileState`] of `ctx`'s server, kept by server (`None` before it
/// serves: no download limit, every sweep runs).
fn file_state(ctx: &Ctx<'_>) -> Option<Arc<FileState>> {
    // Each entry keeps its server's allocation (not the server) alive, so a
    // later server never shares an address — and an entry — with a dropped
    // one; entries of dropped servers go on the next lookup.
    static STATES: Mutex<Vec<(Weak<ApiInner>, Arc<FileState>)>> = Mutex::new(Vec::new());
    let me = ctx.inner.weak_self()?;
    let mut states = locked(&STATES);
    states.retain(|(server, _)| server.strong_count() > 0);
    if let Some((_, state)) = states.iter().find(|(server, _)| server.ptr_eq(&me)) {
        return Some(Arc::clone(state));
    }
    let state = Arc::new(FileState::default());
    states.push((me, Arc::clone(&state)));
    Some(state)
}

// ---------------------------------------------------------------------------
// Retention (see the module docs)
// ---------------------------------------------------------------------------

/// How often the retention sweep runs at most, per server.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The lazy retention sweep (see the module docs; review sec#14, gapi#13):
/// remove the recorded files older than `file_retention_hours` (with their
/// records, and records whose file is gone), and the spilled results under
/// `<root>/tasks` that no task of this server owns. At most once per
/// [`SWEEP_INTERVAL`] per server.
pub(super) fn sweep_files(ctx: &Ctx<'_>) {
    if let Some(state) = file_state(ctx) {
        let mut last = locked(&state.last_sweep);
        if last.is_some_and(|t| t.elapsed() < SWEEP_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    let hours = ctx.inner.config().file_retention_hours;
    let max_age = (hours > 0).then(|| Duration::from_secs(hours.saturating_mul(3600)));
    sweep_records(ctx, max_age);
    ctx.inner
        .tasks
        .remove_orphan_results(&ctx.task_result_dir());
}

/// How old an owner-record entry that does not parse must be before the
/// sweep takes it for garbage when no retention is configured (with one,
/// `file_retention_hours`): a record is written atomically, but its staging
/// file (`<key>.json.tmp`) exists while it is written.
const STALE_RECORD_AGE: Duration = Duration::from_secs(3600);

/// The record half of [`sweep_files`]: every record whose file is gone is
/// dropped; with `max_age`, every recorded file older than that is removed
/// with its record. Only files inside the upload and export directories are
/// ever removed.
///
/// An entry that does not parse — a record torn by a crash (records were
/// once written in place, without `fsync`), or the staging file of one a
/// crash interrupted — is dropped once it is older than `max_age` (else
/// [`STALE_RECORD_AGE`]), by its own modification time. Its file is found by
/// the record's name (the SHA-256 of the file's path, [`record_path`]) among
/// the files of the upload and export directories; with `max_age` that file
/// is removed too once it is older than that (it was written by the server
/// for a client, as its record says). Before, such an entry was skipped for
/// ever, and its file escaped retention.
fn sweep_records(ctx: &Ctx<'_>, max_age: Option<Duration>) {
    let Ok(records) = fs::read_dir(owner_dir(ctx)) else {
        return;
    };
    let roots: Vec<PathBuf> = [upload_dir(ctx), ctx.export_dir()]
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .collect();
    let now = SystemTime::now();
    let older_than = |meta: &fs::Metadata, max: Duration| {
        meta.modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max)
    };
    // The files of the roots by record name, built on the first torn record.
    let mut by_key: Option<std::collections::HashMap<String, PathBuf>> = None;
    for record in records.flatten() {
        let record = record.path();
        let Some((file, _)) = fs::read_to_string(&record)
            .ok()
            .and_then(|t| parse_record(&t))
        else {
            let stale = fs::symlink_metadata(&record)
                .is_ok_and(|m| m.is_file() && older_than(&m, max_age.unwrap_or(STALE_RECORD_AGE)));
            if !stale {
                continue;
            }
            let key = record
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split('.').next())
                .unwrap_or_default()
                .to_string();
            let files = by_key.get_or_insert_with(|| files_by_record_key(&roots));
            if let (Some(max), Some(file)) = (max_age, files.get(&key)) {
                match fs::symlink_metadata(file) {
                    Ok(meta) if !older_than(&meta, max) => continue, // not expired yet
                    Ok(meta) if meta.is_file() => {
                        let _ = fs::remove_file(file);
                    }
                    _ => {}
                }
            }
            let _ = fs::remove_file(&record);
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(&file) else {
            let _ = fs::remove_file(&record);
            continue;
        };
        let expired = max_age.is_some_and(|max| older_than(&meta, max));
        if expired && meta.is_file() && roots.iter().any(|r| file.starts_with(r)) {
            let _ = fs::remove_file(&file);
            let _ = fs::remove_file(&record);
        }
    }
}

/// The regular files under `roots` (canonical directories; symlinks are not
/// followed), by the name their owner record would have ([`record_path`]).
fn files_by_record_key(roots: &[PathBuf]) -> std::collections::HashMap<String, PathBuf> {
    let mut out = std::collections::HashMap::new();
    let mut pending: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let key = crate::query::hash::sha256_hex(&path.to_string_lossy());
                out.insert(key, path);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// /file/upload
// ---------------------------------------------------------------------------

/// A client file name reduced to a safe base name stem: the part after the
/// last `/` or `\`, without its extension, every character outside
/// `[A-Za-z0-9._-]` replaced by `_` (never empty, never starting with `.`).
fn safe_stem(filename: &str) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
    let mut out: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out.push_str("upload");
    }
    out.truncate(128);
    out
}

fn upload(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    if let Err(r) = auth::check_ip(ctx, IpStage::Operation) {
        return r;
    }
    if let Err(r) = auth::authenticate(ctx, p) {
        return r;
    }
    if !auth::check_privilege(ctx, "upload", None) {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "You have no upload privilege",
        );
    }
    sweep_files(ctx);
    let empty = || GResp::error(Status::ParamIsIllegal, "Upload file can not be empty!");
    let Some(file) = p.file("file") else {
        return empty();
    };
    let filename = if file.filename.trim().is_empty() {
        p.non_empty("filename").unwrap_or_default()
    } else {
        file.filename.clone()
    };
    if filename.trim().is_empty() {
        return empty();
    }
    let cfg = ctx.inner.config();
    if file.data.len() as u64 > cfg.upload_max_body_size {
        return GResp::error(
            Status::OperationFailed,
            "Upload file more than max_body_size!",
        );
    }
    let Some(ext) = suffix_of(&filename).map(str::to_string) else {
        return GResp::error(
            Status::OperationFailed,
            "The type of upload file is not supported!",
        );
    };
    if !is_rdf_ext(ctx, &ext) && !is_package(ctx, &ext) {
        return GResp::error(
            Status::OperationFailed,
            "The type of upload file is not supported!",
        );
    }
    let dir = upload_dir(ctx);
    if cfg.upload_user_quota > 0 {
        let used = uploads_of(ctx, ctx.user());
        if used.saturating_add(file.data.len() as u64) > cfg.upload_user_quota {
            return GResp::error(
                Status::OperationFailed,
                format!(
                    "Upload quota exceeded: your uploads take {used} bytes, this one {} more, \
                     the quota is {} bytes (upload_user_quota)",
                    file.data.len(),
                    cfg.upload_user_quota
                ),
            );
        }
    }
    let stem = format!("{}_{}", safe_stem(&filename), timestamp_now());
    let fail = |e: std::io::Error| {
        GResp::error(
            Status::FileWriteError,
            format!("could not store the upload: {e}"),
        )
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        return fail(e);
    }
    let owner = Owner::of(ctx, FileKind::Upload, None);
    let (mut out, name) = match create_owned(ctx, &dir, &stem, &ext, &owner) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    if let Err(e) = out.write_all(&file.data).and_then(|()| out.sync_all()) {
        drop(out);
        remove_with_owner(ctx, &dir.join(&name));
        return fail(e);
    }
    GResp::success("success").with("filepath", shown(&dir, &name))
}

/// The bytes of `user`'s recorded uploads (those still there).
fn uploads_of(ctx: &Ctx<'_>, user: &str) -> u64 {
    let Ok(records) = fs::read_dir(owner_dir(ctx)) else {
        return 0;
    };
    records
        .flatten()
        .filter_map(|r| parse_record(&fs::read_to_string(r.path()).ok()?))
        .filter(|(_, o)| o.kind == FileKind::Upload && o.user == user)
        .filter_map(|(file, _)| fs::metadata(file).ok())
        .map(|m| m.len())
        .sum()
}

// ---------------------------------------------------------------------------
// /file/download
// ---------------------------------------------------------------------------

/// What a download request may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// No credentials, under the upstream-compat switch `anonymous_download`:
    /// the export directory.
    Public,
    /// An authenticated user: the files of the export and upload directories
    /// the owner records allow (and the whole export directory under the
    /// switch).
    User,
    /// `root` or an `admin` user: every file of the export and upload
    /// directories, backups and database directories.
    Admin,
}

/// The path a download names: the URL path after `/file/download/`
/// (URL-decoded), else the `filepath` parameter.
fn download_target(ctx: &Ctx<'_>, p: &Params) -> String {
    for prefix in ["/file/download/", "/grpc/file/download/"] {
        if let Some(rest) = ctx.path().strip_prefix(prefix) {
            if !rest.is_empty() {
                return params::url_decode(rest);
            }
        }
    }
    p.str("filepath").unwrap_or_default()
}

/// Resolve a download inside the roots `reach` allows (see the module docs).
fn download_file(ctx: &Ctx<'_>, requested: &str, reach: Reach) -> Option<PathBuf> {
    let export = ctx.export_dir();
    let home = ctx.root_dir().to_path_buf();
    let canon_home = fs::canonicalize(&home).ok();
    // Under the upstream-compat switch the export directory is public,
    // unless it holds the whole database home (a configuration that would
    // publish the user table).
    let export_public = ctx.inner.config().anonymous_download
        && match (fs::canonicalize(&export), &canon_home) {
            (Ok(e), Some(h)) => !h.starts_with(&e),
            (Ok(_), None) => true,
            (Err(_), _) => false,
        };
    // A relative name is taken inside the root, or — `export/x.nt`, as
    // upstream's answers name files — inside the root's parent.
    let in_root = |root: PathBuf| {
        let mut bases = vec![root.clone()];
        bases.extend(root.parent().map(Path::to_path_buf));
        resolve_existing(requested, Some(std::slice::from_ref(&root)), &bases).ok()
    };
    let file = |p: PathBuf| p.is_file().then_some(p);
    let readable = |p: &Path| reach == Reach::Admin || may_read_recorded(ctx, p);
    if let Some(p) = in_root(export) {
        if export_public || readable(&p) {
            if let Some(f) = file(p) {
                return Some(f);
            }
        }
    }
    if reach == Reach::Public {
        return None;
    }
    if let Some(p) = in_root(upload_dir(ctx)) {
        if readable(&p) {
            if let Some(f) = file(p) {
                return Some(f);
            }
        }
    }
    if reach != Reach::Admin {
        return None;
    }
    if let Some(p) = in_root(backup_dir(ctx)) {
        return file(p);
    }
    // A database directory under the home: `<home>/<name>.db/…` only.
    let p = in_root(home)?;
    let rel = p.strip_prefix(canon_home.as_ref()?).ok()?;
    let mut parts = rel.components();
    let first = parts.next()?.as_os_str().to_string_lossy().into_owned();
    let is_db = first
        .strip_suffix(crate::DB_SUFFIX)
        .is_some_and(crate::http_api::valid_db_name);
    (is_db && parts.next().is_some())
        .then_some(p)
        .and_then(file)
}

/// A `Range: bytes=…` header against a file of `size` bytes: `Ok(None)` for
/// no (or an unsupported multi-range) header, `Ok(Some((start, end)))`
/// inclusive, `Err(())` when it cannot be satisfied.
#[allow(clippy::result_unit_err)]
fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (a, b) = (a.trim(), b.trim());
    let range = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 || size == 0 {
            return Err(());
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.parse().map_err(|_| ())?;
        let end = if b.is_empty() {
            size.saturating_sub(1)
        } else {
            b.parse::<u64>()
                .map_err(|_| ())?
                .min(size.saturating_sub(1))
        };
        if start >= size || end < start {
            return Err(());
        }
        (start, end)
    };
    Ok(Some(range))
}

fn not_found() -> GResp {
    GResp::error(Status::FileNotFound, "")
}

/// A non-envelope answer at HTTP `status` whose body is the envelope of
/// `code` / `msg` (a client that saves the body as the file sees the error).
fn http_error(status: u16, code: Status, msg: &str) -> GResp {
    let (_, ctype, body, _) = GResp::error(code, msg).into_parts();
    GResp::raw(status, ctype, body)
}

/// The answer to a download without credentials (see the module docs).
fn auth_required() -> GResp {
    http_error(
        401,
        Status::AuthenticationFailed,
        "Authentication required: send username and password (or HTTP Basic credentials) \
         to download a file",
    )
    .with_header("WWW-Authenticate", "Basic realm=\"gStore\"")
}

/// Authenticate a download's credentials, if it carries any: `username` /
/// `password` parameters (upstream's), else an `Authorization: Basic`
/// header. `Ok(false)` when it carries none.
fn authenticate_download(ctx: &mut Ctx<'_>, p: &Params) -> Result<bool, GResp> {
    let basic = if p.has("username") {
        None
    } else {
        match crate::server::parse_basic_auth(ctx.req) {
            Some(b) => Some(b),
            None => return Ok(false),
        }
    };
    auth::check_ip(ctx, IpStage::Operation)?;
    match basic {
        None => auth::authenticate(ctx, p)?,
        Some((username, password)) => {
            let creds = auth::Credentials {
                username,
                password,
                md5: false,
                inner: false,
            };
            auth::verify(ctx, &creds, false)?
        }
    };
    Ok(true)
}

/// Holds one of the server's `download_max_concurrent` download slots.
struct DownloadPermit(Arc<FileState>);

impl Drop for DownloadPermit {
    fn drop(&mut self) {
        self.0.downloads.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A download slot: `Ok(None)` without a limit, `Err(())` when all are
/// taken.
fn download_permit(ctx: &Ctx<'_>) -> Result<Option<DownloadPermit>, ()> {
    let max = ctx.inner.config().download_max_concurrent;
    let Some(state) = file_state(ctx).filter(|_| max > 0) else {
        return Ok(None);
    };
    if state.downloads.fetch_add(1, Ordering::SeqCst) >= max {
        state.downloads.fetch_sub(1, Ordering::SeqCst);
        return Err(());
    }
    Ok(Some(DownloadPermit(state)))
}

fn download(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let requested = download_target(ctx, p);
    if requested.trim().is_empty() {
        return GResp::error(
            Status::ParamIsIllegal,
            "Download filepath can not be empty!",
        );
    }
    if has_traversal(&requested) {
        return PathRefusal::Traversal.answer("filepath", &requested, &[]);
    }
    // Credentials are required (review sec#4, gapi#2), unless the
    // upstream-compat switch serves the export directory to anyone; wrong
    // ones are refused either way.
    let reach = match authenticate_download(ctx, p) {
        Err(r) => return r,
        Ok(false) if !ctx.inner.config().anonymous_download => return auth_required(),
        Ok(false) => Reach::Public,
        Ok(true) if is_admin(ctx) => Reach::Admin,
        Ok(true) => Reach::User,
    };
    sweep_files(ctx);
    let Some(path) = download_file(ctx, &requested, reach) else {
        return not_found();
    };
    let Ok(f) = File::open(&path) else {
        return not_found();
    };
    let size = match f.metadata() {
        Ok(m) => m.len(),
        Err(_) => return not_found(),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().replace(['"', '\\', '\r', '\n'], "_"))
        .unwrap_or_else(|| "download".to_string());
    let (status, start, len, content_range) = match parse_range(ctx.header("range"), size) {
        Ok(None) => (200, 0, size, None),
        Ok(Some((a, b))) => (206, a, b - a + 1, Some(format!("bytes {a}-{b}/{size}"))),
        Err(()) => {
            return GResp::raw(416, "text/plain", b"range not satisfiable".to_vec())
                .with_header("Content-Range", format!("bytes */{size}"))
                .with_header("Accept-Ranges", "bytes")
        }
    };
    // Streamed by the HTTP layer in bounded chunks, holding a download slot
    // until it has been sent (review gapi#4, robust#7, sec#11).
    let Ok(permit) = download_permit(ctx) else {
        return http_error(
            503,
            Status::OperationFailed,
            "too many downloads at once (download_max_concurrent): retry later",
        )
        .with_header("Retry-After", "1");
    };
    let mut body = FileBody::new(f, start, len);
    if let Some(permit) = permit {
        body = body.with_permit(permit);
    }
    let mut resp = GResp::file(status, "application/octet-stream", body)
        .with_header(
            "Content-Disposition",
            format!("attachment; filename=\"{name}\""),
        )
        .with_header("Accept-Ranges", "bytes");
    if let Some(cr) = content_range {
        resp = resp.with_header("Content-Range", cr);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gstore_opsfile_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn existing_paths_are_confined_to_their_roots() {
        let base = tmp("existing");
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("sub/a.nt"), "x").unwrap();
        fs::write(outside.join("secret.nt"), "x").unwrap();
        let roots = vec![root.clone()];
        let rs = Some(roots.as_slice());
        let abs = root.join("sub/a.nt");
        assert_eq!(
            resolve_existing(abs.to_str().unwrap(), rs, &[]),
            Ok(abs.clone())
        );
        // Relative to a base.
        assert_eq!(
            resolve_existing("sub/a.nt", rs, std::slice::from_ref(&root)),
            Ok(abs)
        );
        // Traversal, outside (existing or not), missing inside.
        let trav = format!("{}/sub/../../outside/secret.nt", root.display());
        assert_eq!(
            resolve_existing(&trav, rs, &[]),
            Err(PathRefusal::Traversal)
        );
        let out = outside.join("secret.nt");
        assert_eq!(
            resolve_existing(out.to_str().unwrap(), rs, &[]),
            Err(PathRefusal::Outside)
        );
        let gone = outside.join("nothing.nt");
        assert_eq!(
            resolve_existing(gone.to_str().unwrap(), rs, &[]),
            Err(PathRefusal::Outside)
        );
        let missing = root.join("sub/nothing.nt");
        assert_eq!(
            resolve_existing(missing.to_str().unwrap(), rs, &[]),
            Err(PathRefusal::Missing)
        );
        // A symlink inside the root pointing outside is outside.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
            let via = root.join("link/secret.nt");
            assert_eq!(
                resolve_existing(via.to_str().unwrap(), rs, &[]),
                Err(PathRefusal::Outside)
            );
        }
        // No roots at all = anywhere.
        assert!(resolve_existing(out.to_str().unwrap(), None, &[]).is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn targets_may_be_created_inside_their_root() {
        let base = tmp("target");
        let root = base.join("backups");
        fs::create_dir_all(&root).unwrap();
        assert_eq!(
            resolve_target(root.to_str().unwrap(), &root),
            Ok(root.clone())
        );
        assert_eq!(resolve_target("x/y", &root), Ok(root.join("x/y")));
        let outside = base.join("elsewhere");
        assert_eq!(
            resolve_target(outside.to_str().unwrap(), &root),
            Err(PathRefusal::Outside)
        );
        assert_eq!(resolve_target("../x", &root), Err(PathRefusal::Traversal));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn names_ranges_and_suffixes() {
        assert_eq!(safe_stem("../../etc/pass wd.nt"), "pass_wd");
        assert_eq!(safe_stem("C:\\x\\data.ttl"), "data");
        assert_eq!(safe_stem(".hidden.nt"), "hidden");
        assert_eq!(safe_stem(".nt"), "upload");
        assert_eq!(suffix_of("a/b.c/d.NT"), Some("NT"));
        assert_eq!(suffix_of("noext"), None);
        assert_eq!(parse_range(None, 10), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-3"), 10), Ok(Some((0, 3))));
        assert_eq!(parse_range(Some("bytes=5-"), 10), Ok(Some((5, 9))));
        assert_eq!(parse_range(Some("bytes=-4"), 10), Ok(Some((6, 9))));
        assert_eq!(parse_range(Some("bytes=8-100"), 10), Ok(Some((8, 9))));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Err(()));
        assert_eq!(parse_range(Some("bytes=0-1,3-4"), 10), Ok(None));
        assert_eq!(shown(Path::new("./backups/"), "x"), "./backups/x");
        assert_eq!(shown(Path::new("/b"), "x"), "/b/x");
        assert_eq!(timestamp_now().len(), 14);
    }
}
