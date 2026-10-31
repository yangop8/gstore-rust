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
//! | upload dir (`upload_path`, else `<db home>/upload`) | uploaded RDF files and zip packages; packages unpacked for one operation | `build`, `batchInsert`, `batchRemove` | `/file/upload`; `build` / `batch*` unpacking a zip |
//! | export dir (`<db home>/export` unless configured) | exports, `format=file` query results | the same, `/file/download` | `export`, `query` |
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
//! stored file as a path `build` / `batchInsert` accept as `db_path` / `file`.
//! (Upstream checks this route's privilege under the name `uploadfile`,
//! which its privilege table does not list, so only `root` may upload there;
//! its documentation and its table's `upload` entry open uploads to every
//! user, which is what this server does.)
//!
//! # `/file/download`
//!
//! The file named by the path after `/file/download/` (URL-decoded) or by the
//! `filepath` parameter, sent as `application/octet-stream` with a
//! `Content-Disposition: attachment` file name. Like upstream, a relative path
//! is taken inside the export directory (`export/…` and bare names alike) and
//! **no credentials are needed** for files there — what an `export` or a
//! `format=file` query produced. With valid `username` / `password`
//! parameters a caller may also download its uploads (any user), and `root`
//! (or an `admin` user) files of the backup directory and of database
//! directories; nothing else under the database home — the user table, the
//! registry, the logs — is ever served. A file outside those roots, or that
//! does not exist, is answered with upstream's `FileNotFound` envelope
//! (`StatusCode` 11) either way.
//!
//! A single `Range: bytes=a-b` (or `a-`, `-n`) request header is honoured
//! with `206 Partial Content` and a `Content-Range` (`416` when it cannot be
//! satisfied); every response carries `Accept-Ranges: bytes`. (Upstream always
//! sends the whole file, with a `Content-Range` header that does not follow
//! RFC 7233.)

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use super::auth::{self, IpStage};
use super::envelope::{GResp, Status};
use super::params::{self, Params};
use super::{Ctx, PathRoute, Registry};
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
    let (mut out, name) = match create_unique(&dir, &stem, &ext) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    if let Err(e) = out.write_all(&file.data).and_then(|()| out.sync_all()) {
        drop(out);
        let _ = fs::remove_file(dir.join(&name));
        return fail(e);
    }
    GResp::success("success").with("filepath", shown(&dir, &name))
}

// ---------------------------------------------------------------------------
// /file/download
// ---------------------------------------------------------------------------

/// What a download request may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// No (valid) credentials: the export directory only.
    Public,
    /// An authenticated user: also the upload directory.
    User,
    /// `root` or an `admin` user: also backups and database directories.
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
    // The export directory is public unless it holds the whole database home
    // (a configuration that would publish the user table).
    let export_public = match (fs::canonicalize(&export), &canon_home) {
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
    if export_public || reach == Reach::Admin {
        if let Some(p) = in_root(export) {
            return file(p);
        }
    }
    if reach == Reach::Public {
        return None;
    }
    if let Some(p) = in_root(upload_dir(ctx)) {
        return file(p);
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
    // Credentials are optional (upstream asks for none): they widen what may
    // be downloaded, and wrong ones are refused.
    let reach = if p.has("username") {
        if let Err(r) = auth::check_ip(ctx, IpStage::Operation) {
            return r;
        }
        if let Err(r) = auth::authenticate(ctx, p) {
            return r;
        }
        let admin = ctx.is_root() || ctx.state().users.is_superuser(ctx.user());
        if admin {
            Reach::Admin
        } else {
            Reach::User
        }
    } else {
        Reach::Public
    };
    let Some(path) = download_file(ctx, &requested, reach) else {
        return not_found();
    };
    let Ok(mut f) = File::open(&path) else {
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
    let mut bytes = Vec::new();
    let read = f
        .seek(SeekFrom::Start(start))
        .and_then(|_| f.take(len).read_to_end(&mut bytes));
    if read.is_err() {
        return GResp::error(Status::FileReadError, "");
    }
    let mut resp = GResp::raw(status, "application/octet-stream", bytes)
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
