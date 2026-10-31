//! Zip packages and export files — upstream gStore's `CompressUtil`
//! (`src/Util/CompressFileUtil.cpp`, minizip) and `Database::export_db`, for
//! the `/api` operations ([`crate::gapi`]) and the CLI tools.
//!
//! # Zip packages (feature `zip`)
//!
//! * **RDF packages** — `build` / `batchInsert` / `batchRemove` accept a
//!   `.zip` of RDF files (upstream `ApiHandler::uncompress_zip`): the package
//!   is checked first ([`unpack`] with an `accept` filter: every file entry
//!   must carry an allowed RDF extension), then unpacked into a fresh
//!   directory and its files read in path order.
//! * **Backups** — `backup compress=true` packs the backup directory into
//!   `<db>.db_<YYYYmmddHHMMSS>.zip`, the directory's files and sub-directories
//!   at the archive root (upstream `CompressZip::compressDirExportZip(dir,
//!   zip, contain_base=false)`, see [`pack_dir`]); `restore` unpacks one.
//! * **Exports** — `export compress=true` packs the export file into
//!   `<db>_<YYYYmmddHHMMSS>.zip` holding the one entry `<db>_<…>.nt` (its file
//!   name only: upstream `FileHelper::compressExportZip(file, zip, false)`, see
//!   [`pack_file`]).
//!
//! Errors carry upstream's `CompressUtil::ZipCode` ([`ZipCode`]), which its
//! API answers as the `StatusCode` (`1` the package cannot be opened, `2` a
//! broken directory, `3` an entry that is not supported, `4` an entry whose
//! header cannot be read, `5` no file in the package, `6` unpacking failed),
//! with upstream's messages — `"zip format or data is error."` while the
//! package is checked, `"uncompress failed(code=6)"` while it is unpacked.
//!
//! **Safety** (upstream's minizip code has none of it): an entry whose name is
//! absolute (`/x`, `\x`, `C:x`), holds a `..` component or a NUL byte is
//! refused before anything is written (zip-slip); symbolic-link entries are
//! ignored; encrypted entries and compression methods other than stored /
//! deflate are refused; a package with more than [`ZipLimits::max_entries`]
//! entries, or whose files declare more than [`ZipLimits::max_size`] bytes in
//! all, is refused, and the bytes actually written are counted against the
//! same limit (a lying header does not help a zip bomb). The limits come from
//! `conf.ini` (`[upload] unzip_max_entries`, `unzip_max_size`). Files are
//! created exclusively inside the fresh destination directory; archives are
//! written to `<name>.part` and renamed into place once complete.
//!
//! Without the feature every zip function answers [`ZipError::FeatureMissing`]
//! (the `/api` layer: `1003`, a message naming the feature).
//!
//! # Export files
//!
//! [`ExportFormat`] and [`write_export`]: N-Triples (the default graph, what
//! upstream's `export_db` writes) or N-Quads (the default graph as triples,
//! then every named graph's statements with their graph IRI).

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::GStoreConfig;
use crate::db::Database;
use crate::error::{GStoreError, Result};
use crate::store::TripleSource;

/// Whether this build handles zip packages (feature `zip`).
pub const AVAILABLE: bool = cfg!(feature = "zip");

/// What an operation that needs zip support says without the feature.
pub const FEATURE_MISSING: &str =
    "zip packages need the `zip` feature: this gStore was built without it (rebuild with \
     `--features zip`)";

/// Default [`ZipLimits::max_entries`].
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// Default [`ZipLimits::max_size`]: 10 GiB.
pub const DEFAULT_MAX_SIZE: u64 = 10 << 30;

/// The limits a package must stay within to be unpacked (zip-bomb guard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipLimits {
    /// Entries (files, directories, links) a package may hold.
    pub max_entries: usize,
    /// Bytes the files of a package may unpack to, in all.
    pub max_size: u64,
}

impl Default for ZipLimits {
    fn default() -> Self {
        ZipLimits {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_size: DEFAULT_MAX_SIZE,
        }
    }
}

impl ZipLimits {
    /// The limits `conf.ini` sets (`[upload] unzip_max_entries`,
    /// `unzip_max_size`).
    pub fn from_config(cfg: &GStoreConfig) -> ZipLimits {
        ZipLimits {
            max_entries: cfg.unzip_max_entries,
            max_size: cfg.unzip_max_size,
        }
    }
}

/// Upstream's `CompressUtil::ZipCode` (the `StatusCode` its API answers a
/// package error with).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipCode {
    /// `1` — the package cannot be opened (missing, or not a zip).
    NotFound = 1,
    /// `2` — the package's directory is broken.
    Error = 2,
    /// `3` — an entry that is not supported (a disallowed extension; here
    /// also an unsafe name, encryption, a method other than stored / deflate,
    /// or a package over the limits).
    NotSupported = 3,
    /// `4` — an entry header cannot be read.
    GetInfoError = 4,
    /// `5` — the package holds no file.
    EmptyDir = 5,
    /// `6` — unpacking failed (a write error, corrupt data, or more data than
    /// the limits allow).
    WriteError = 6,
}

impl ZipCode {
    /// The numeric code.
    pub fn code(self) -> i64 {
        self as i64
    }
}

/// A failed zip operation.
#[derive(Debug)]
pub enum ZipError {
    /// Built without the `zip` feature ([`FEATURE_MISSING`]).
    FeatureMissing,
    /// Reading or unpacking a package failed: upstream's code, and what went
    /// wrong when upstream's message alone would not say.
    Unzip {
        code: ZipCode,
        detail: Option<String>,
    },
    /// Writing an archive failed.
    Io(io::Error),
}

impl ZipError {
    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    fn unzip(code: ZipCode, detail: impl Into<String>) -> ZipError {
        ZipError::Unzip {
            code,
            detail: Some(detail.into()),
        }
    }

    /// Upstream's own message for this error (no detail): `"zip format or
    /// data is error."` while checking, `"uncompress failed(code=6)"` while
    /// unpacking.
    pub fn upstream_msg(&self) -> String {
        match self {
            ZipError::FeatureMissing => FEATURE_MISSING.to_string(),
            ZipError::Unzip {
                code: ZipCode::WriteError,
                ..
            } => "uncompress failed(code=6)".to_string(),
            ZipError::Unzip { .. } => "zip format or data is error.".to_string(),
            ZipError::Io(e) => e.to_string(),
        }
    }

    /// The upstream code of a package error.
    pub fn code(&self) -> Option<ZipCode> {
        match self {
            ZipError::Unzip { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZipError::Unzip {
                detail: Some(d), ..
            } => write!(f, "{} ({d})", self.upstream_msg()),
            _ => f.write_str(&self.upstream_msg()),
        }
    }
}

impl std::error::Error for ZipError {}

impl From<io::Error> for ZipError {
    fn from(e: io::Error) -> Self {
        ZipError::Io(e)
    }
}

impl From<ZipError> for GStoreError {
    fn from(e: ZipError) -> Self {
        match e {
            ZipError::Io(e) => GStoreError::Io(e),
            other => GStoreError::Database(other.to_string()),
        }
    }
}

/// One file [`unpack`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// Where it is (inside the destination directory).
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
}

/// Whether `path` names a zip package by its extension (`.zip`, any case).
pub fn is_zip_name(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// The relative path a package entry name stands for — its components
/// (split at `/` and `\`, empty and `.` ones dropped) — or `None` when the
/// name is unsafe: empty, absolute (`/x`, `\x`, `C:…`), with a `..`
/// component or a NUL byte.
pub fn safe_entry_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') || name.starts_with(['/', '\\']) {
        return None;
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p => out.push(p),
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

// ---------------------------------------------------------------------------
// Reading packages
// ---------------------------------------------------------------------------

/// Check the package `zip` and unpack it into `dest` (created; it must not
/// exist yet, or be empty): every file entry must pass `accept` (given the
/// entry's name; upstream: an allowed RDF extension) or the package is
/// refused with [`ZipCode::NotSupported`] before anything is written;
/// directory entries are created, symbolic links ignored. Returns the files
/// written, sorted by path. See the [module docs](self) for the safety rules.
/// On error `dest` may hold part of the package: the caller removes it.
#[cfg(feature = "zip")]
pub fn unpack(
    zip: &Path,
    dest: &Path,
    limits: &ZipLimits,
    accept: &dyn Fn(&str) -> bool,
) -> std::result::Result<Vec<Unpacked>, ZipError> {
    use std::fs;
    let mut archive = open_archive(zip)?;
    let plan = check(&archive, limits, accept)?;
    fs::create_dir_all(dest).map_err(|e| ZipError::unzip(ZipCode::WriteError, e.to_string()))?;
    let write_err = |what: &Path, e: &dyn fmt::Display| {
        ZipError::unzip(
            ZipCode::WriteError,
            format!("'{}': {e}", what.display()),
        )
    };
    let mut budget = limits.max_size;
    let mut out = Vec::new();
    for item in plan {
        let target = dest.join(&item.rel);
        if item.dir {
            fs::create_dir_all(&target).map_err(|e| write_err(&item.rel, &e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| write_err(&item.rel, &e))?;
        }
        let mut entry = archive
            .by_index(item.index)
            .map_err(|e| write_err(&item.rel, &e))?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| write_err(&item.rel, &e))?;
        let mut w = io::BufWriter::new(file);
        // One byte more than the budget: reading it means the data is larger
        // than the header said and the limit allows.
        let written = io::copy(&mut io::Read::take(&mut entry, budget + 1), &mut w)
            .map_err(|e| write_err(&item.rel, &e))?;
        if written > budget {
            drop(w);
            let _ = fs::remove_file(&target);
            return Err(ZipError::unzip(
                ZipCode::WriteError,
                format!(
                    "the package unpacks to more than {} bytes (unzip_max_size)",
                    limits.max_size
                ),
            ));
        }
        budget -= written;
        w.flush().map_err(|e| write_err(&item.rel, &e))?;
        out.push(Unpacked {
            path: target,
            size: written,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// See the feature-enabled version: without the `zip` feature this is
/// always [`ZipError::FeatureMissing`].
#[cfg(not(feature = "zip"))]
pub fn unpack(
    _zip: &Path,
    _dest: &Path,
    _limits: &ZipLimits,
    _accept: &dyn Fn(&str) -> bool,
) -> std::result::Result<Vec<Unpacked>, ZipError> {
    Err(ZipError::FeatureMissing)
}

/// The content of entry `name` of package `zip` (at most `max` bytes; more is
/// an error), or `None` when it holds no such file.
#[cfg(feature = "zip")]
pub fn read_entry(
    zip: &Path,
    name: &str,
    max: u64,
) -> std::result::Result<Option<Vec<u8>>, ZipError> {
    use io::Read;
    let mut archive = open_archive(zip)?;
    let mut entry = match archive.by_name(name) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(ZipError::unzip(ZipCode::GetInfoError, e.to_string())),
    };
    if !entry.is_file() || entry.encrypted() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    entry
        .by_ref()
        .take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| ZipError::unzip(ZipCode::WriteError, e.to_string()))?;
    if buf.len() as u64 > max {
        return Err(ZipError::unzip(
            ZipCode::NotSupported,
            format!("entry '{name}' is larger than {max} bytes"),
        ));
    }
    Ok(Some(buf))
}

/// Without the `zip` feature: [`ZipError::FeatureMissing`].
#[cfg(not(feature = "zip"))]
pub fn read_entry(
    _zip: &Path,
    _name: &str,
    _max: u64,
) -> std::result::Result<Option<Vec<u8>>, ZipError> {
    Err(ZipError::FeatureMissing)
}

#[cfg(feature = "zip")]
type Archive = zip::ZipArchive<io::BufReader<std::fs::File>>;

/// Open `zip` (upstream `unzOpen64`: failure is code 1).
#[cfg(feature = "zip")]
fn open_archive(zip: &Path) -> std::result::Result<Archive, ZipError> {
    let file = std::fs::File::open(zip)
        .map_err(|e| ZipError::unzip(ZipCode::NotFound, format!("'{}': {e}", zip.display())))?;
    zip::ZipArchive::new(io::BufReader::new(file))
        .map_err(|e| ZipError::unzip(ZipCode::NotFound, format!("'{}': {e}", zip.display())))
}

/// One entry [`unpack`] writes.
#[cfg(feature = "zip")]
struct Planned {
    index: usize,
    rel: PathBuf,
    dir: bool,
}

/// Check every entry of `archive` (upstream `FileHelper::foreachZip`, plus
/// the safety rules), returning what to unpack.
#[cfg(feature = "zip")]
fn check(
    archive: &Archive,
    limits: &ZipLimits,
    accept: &dyn Fn(&str) -> bool,
) -> std::result::Result<Vec<Planned>, ZipError> {
    use zip::CompressionMethod;
    let n = archive.len();
    if n == 0 {
        return Err(ZipError::Unzip {
            code: ZipCode::EmptyDir,
            detail: None,
        });
    }
    if n > limits.max_entries {
        return Err(ZipError::unzip(
            ZipCode::NotSupported,
            format!(
                "the package holds {n} entries, more than {} (unzip_max_entries)",
                limits.max_entries
            ),
        ));
    }
    let mut plan = Vec::with_capacity(n);
    let mut declared: u64 = 0;
    let mut files = 0usize;
    for index in 0..n {
        let entry = archive
            .by_index_data(index)
            .map_err(|e| ZipError::unzip(ZipCode::GetInfoError, e.to_string()))?;
        let name = entry
            .name()
            .map_err(|e| ZipError::unzip(ZipCode::GetInfoError, e.to_string()))?
            .into_owned();
        // Zip-slip: refuse the whole package, not just the entry.
        let Some(rel) = safe_entry_path(&name).filter(|_| entry.enclosed_name().is_some()) else {
            return Err(ZipError::unzip(
                ZipCode::NotSupported,
                format!("entry '{name}' is not a relative path inside the package"),
            ));
        };
        if entry.is_symlink() {
            continue;
        }
        if entry.is_dir() {
            plan.push(Planned {
                index,
                rel,
                dir: true,
            });
            continue;
        }
        if entry.encrypted() {
            return Err(ZipError::unzip(
                ZipCode::NotSupported,
                format!("entry '{name}' is encrypted"),
            ));
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(ZipError::unzip(
                ZipCode::NotSupported,
                format!(
                    "entry '{name}' uses compression method {:?} (only stored and deflate are \
                     supported)",
                    entry.compression()
                ),
            ));
        }
        if !accept(&name) {
            return Err(ZipError::Unzip {
                code: ZipCode::NotSupported,
                detail: Some(format!("the file suffix of entry '{name}' is not allowed")),
            });
        }
        declared = declared.saturating_add(entry.size());
        if declared > limits.max_size {
            return Err(ZipError::unzip(
                ZipCode::NotSupported,
                format!(
                    "the package unpacks to more than {} bytes (unzip_max_size)",
                    limits.max_size
                ),
            ));
        }
        files += 1;
        plan.push(Planned {
            index,
            rel,
            dir: false,
        });
    }
    if files == 0 {
        return Err(ZipError::Unzip {
            code: ZipCode::EmptyDir,
            detail: None,
        });
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Writing archives
// ---------------------------------------------------------------------------

/// Pack directory `src` into a new archive `dest` (replaced if it exists):
/// every file and sub-directory below `src`, relative to it, sorted by path
/// — upstream `CompressZip::compressDirExportZip(src, dest)` (no base
/// directory in the archive; a sub-directory gets its own `name/` entry).
/// Symbolic links are skipped. Written to `<dest>.part`, synced and renamed
/// into place. Returns the number of files packed.
#[cfg(feature = "zip")]
pub fn pack_dir(src: &Path, dest: &Path) -> std::result::Result<u64, ZipError> {
    let mut items: Vec<(String, PathBuf, bool)> = Vec::new();
    collect(src, "", &mut items)?;
    items.sort_by(|a, b| a.0.cmp(&b.0));
    write_archive(dest, |zw| {
        let mut files = 0u64;
        for (name, path, dir) in &items {
            if *dir {
                zw.add_directory(format!("{name}/"), options_for(path)?)
                    .map_err(zip_io)?;
            } else {
                add_file(zw, name, path)?;
                files += 1;
            }
        }
        Ok(files)
    })
}

/// Without the `zip` feature: [`ZipError::FeatureMissing`].
#[cfg(not(feature = "zip"))]
pub fn pack_dir(_src: &Path, _dest: &Path) -> std::result::Result<u64, ZipError> {
    Err(ZipError::FeatureMissing)
}

/// Pack file `src` into a new archive `dest` (replaced if it exists) as the
/// one entry `entry` — upstream `FileHelper::compressExportZip(src, dest,
/// false)` names it by the file name. Written like [`pack_dir`].
#[cfg(feature = "zip")]
pub fn pack_file(src: &Path, dest: &Path, entry: &str) -> std::result::Result<(), ZipError> {
    write_archive(dest, |zw| add_file(zw, entry, src))
}

/// Without the `zip` feature: [`ZipError::FeatureMissing`].
#[cfg(not(feature = "zip"))]
pub fn pack_file(_src: &Path, _dest: &Path, _entry: &str) -> std::result::Result<(), ZipError> {
    Err(ZipError::FeatureMissing)
}

#[cfg(feature = "zip")]
type Writer = zip::ZipWriter<io::BufWriter<std::fs::File>>;

#[cfg(feature = "zip")]
fn zip_io(e: zip::result::ZipError) -> ZipError {
    match e {
        zip::result::ZipError::Io(e) => ZipError::Io(e),
        other => ZipError::Io(io::Error::other(other.to_string())),
    }
}

/// The relative names (`/`-separated) of everything below `dir`.
#[cfg(feature = "zip")]
fn collect(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf, bool)>,
) -> std::result::Result<(), ZipError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(fname) = entry.file_name().to_str().map(str::to_string) else {
            return Err(ZipError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("'{}' is not a UTF-8 file name", entry.path().display()),
            )));
        };
        let name = if prefix.is_empty() {
            fname
        } else {
            format!("{prefix}/{fname}")
        };
        let ty = entry.file_type()?;
        if ty.is_dir() {
            out.push((name.clone(), entry.path(), true));
            collect(&entry.path(), &name, out)?;
        } else if ty.is_file() {
            out.push((name, entry.path(), false));
        }
    }
    Ok(())
}

/// Entry options for `path`: deflate, its modification time (local, as
/// upstream's minizip writes it), zip64 for a file of 4 GiB or more.
#[cfg(feature = "zip")]
fn options_for(path: &Path) -> std::result::Result<zip::write::SimpleFileOptions, ZipError> {
    let meta = std::fs::metadata(path)?;
    let mut opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(meta.len() >= 0xFFFF_FFFF);
    if let Ok(modified) = meta.modified() {
        let t = crate::sysdb::clock::at(modified);
        let dt = zip::DateTime::from_date_and_time(
            u16::try_from(t.year).unwrap_or(1980),
            t.month as u8,
            t.day as u8,
            t.hour as u8,
            t.minute as u8,
            t.second as u8,
        );
        if let Ok(dt) = dt {
            opts = opts.last_modified_time(dt);
        }
    }
    Ok(opts)
}

#[cfg(feature = "zip")]
fn add_file(zw: &mut Writer, name: &str, path: &Path) -> std::result::Result<(), ZipError> {
    let opts = options_for(path)?;
    zw.start_file(name, opts).map_err(zip_io)?;
    let mut f = std::fs::File::open(path)?;
    io::copy(&mut f, zw)?;
    Ok(())
}

/// Write an archive to `<dest>.part` with `fill`, then sync and rename it to
/// `dest`; the partial file is removed on error.
#[cfg(feature = "zip")]
fn write_archive<T>(
    dest: &Path,
    fill: impl FnOnce(&mut Writer) -> std::result::Result<T, ZipError>,
) -> std::result::Result<T, ZipError> {
    use std::fs;
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let result = (|| {
        let file = fs::File::create(&part)?;
        let mut zw = zip::ZipWriter::new(io::BufWriter::new(file));
        let value = fill(&mut zw)?;
        let file = zw
            .finish()
            .map_err(zip_io)?
            .into_inner()
            .map_err(|e| ZipError::Io(e.into_error()))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&part, dest)?;
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = crate::db::fsutil::sync_dir(parent);
        }
        Ok(value)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

/// A directory removed (with its content) when dropped: where a package is
/// unpacked for the length of one operation.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Take over `path` as a fresh, empty directory (whatever was there — a
    /// leftover of an interrupted run — is removed first).
    pub fn fresh(path: PathBuf) -> io::Result<ScratchDir> {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir_all(&path)?;
        Ok(ScratchDir { path })
    }

    /// Adopt an already created directory.
    pub fn adopt(path: PathBuf) -> ScratchDir {
        ScratchDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Export files
// ---------------------------------------------------------------------------

/// The serialization of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// N-Triples: the default graph (upstream `Database::export_db`).
    NTriples,
    /// N-Quads: the default graph, then every named graph.
    NQuads,
}

impl ExportFormat {
    /// The format a name or extension stands for (case-insensitive): `nt`,
    /// `ntriples`, `n-triples`; `nq`, `nquads`, `n-quads`.
    pub fn parse(name: &str) -> Option<ExportFormat> {
        match name.trim().to_ascii_lowercase().as_str() {
            "nt" | "ntriples" | "n-triples" => Some(ExportFormat::NTriples),
            "nq" | "nquads" | "n-quads" => Some(ExportFormat::NQuads),
            _ => None,
        }
    }

    /// The format a file name's extension names, if any.
    pub fn from_path(path: &Path) -> Option<ExportFormat> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(ExportFormat::parse)
    }

    /// The format that loses nothing of `db`: N-Quads when it has a
    /// non-empty named graph, else N-Triples (all an upstream database holds).
    pub fn for_database(db: &Database) -> ExportFormat {
        if db.named_graphs().values().any(|g| g.triple_count() > 0) {
            ExportFormat::NQuads
        } else {
            ExportFormat::NTriples
        }
    }

    /// The file extension (`nt` / `nq`).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::NTriples => "nt",
            ExportFormat::NQuads => "nq",
        }
    }
}

/// Write `db` to `out` as `format`: its default-graph triples (`<s> <p> <o>
/// .`), and for N-Quads then each named graph's (`<s> <p> <o> <g> .`, graphs
/// in id order). Returns the number of statements written. A statement whose
/// terms the dictionary cannot resolve is skipped (as `export_ntriples`).
pub fn write_export(db: &Database, format: ExportFormat, out: &mut dyn Write) -> Result<u64> {
    let dict = db.dict();
    let mut count = 0u64;
    for t in db.backend().iter_all() {
        let (Some(s), Some(p), Some(o)) = (
            dict.id_to_string(t.sub),
            dict.predicate_to_string(t.pred),
            dict.id_to_string(t.obj),
        ) else {
            continue;
        };
        writeln!(out, "{s} {p} {o} .")?;
        count += 1;
    }
    if format == ExportFormat::NQuads {
        for (&gid, graph) in db.named_graphs() {
            let Some(g) = dict.id_to_string(gid) else {
                continue;
            };
            for t in graph.iter_all() {
                let (Some(s), Some(p), Some(o)) = (
                    dict.id_to_string(t.sub),
                    dict.predicate_to_string(t.pred),
                    dict.id_to_string(t.obj),
                ) else {
                    continue;
                };
                writeln!(out, "{s} {p} {o} {g} .")?;
                count += 1;
            }
        }
    }
    out.flush()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_are_confined() {
        assert_eq!(safe_entry_path("a/b.nt"), Some(PathBuf::from("a/b.nt")));
        assert_eq!(safe_entry_path("./a//b.nt"), Some(PathBuf::from("a/b.nt")));
        assert_eq!(safe_entry_path("a\\b.nt"), Some(PathBuf::from("a/b.nt")));
        for bad in [
            "",
            "/etc/passwd",
            "\\x.nt",
            "C:x.nt",
            "c:/x.nt",
            "../x.nt",
            "a/../../x.nt",
            "a/..",
            "a\\..\\..\\x",
            "x\0.nt",
            ".",
            "./",
        ] {
            assert_eq!(safe_entry_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn formats_and_names() {
        assert_eq!(ExportFormat::parse("NQ"), Some(ExportFormat::NQuads));
        assert_eq!(ExportFormat::parse("n-triples"), Some(ExportFormat::NTriples));
        assert_eq!(ExportFormat::parse("ttl"), None);
        assert_eq!(
            ExportFormat::from_path(Path::new("x/out.nquads")),
            Some(ExportFormat::NQuads)
        );
        assert_eq!(ExportFormat::from_path(Path::new("out")), None);
        assert!(is_zip_name(Path::new("a.ZIP")));
        assert!(!is_zip_name(Path::new("a.zip.part")));
        assert_eq!(ZipCode::WriteError.code(), 6);
        assert_eq!(
            ZipError::Unzip {
                code: ZipCode::EmptyDir,
                detail: None
            }
            .to_string(),
            "zip format or data is error."
        );
    }

    #[cfg(not(feature = "zip"))]
    #[test]
    fn without_the_feature_everything_names_it() {
        let e = unpack(Path::new("x.zip"), Path::new("d"), &ZipLimits::default(), &|_| true)
            .unwrap_err();
        assert!(matches!(e, ZipError::FeatureMissing));
        assert!(e.to_string().contains("`zip` feature"), "{e}");
        assert!(pack_dir(Path::new("."), Path::new("x.zip")).is_err());
    }

    #[cfg(feature = "zip")]
    mod with_zip {
        use super::super::*;
        use std::fs;

        fn tmp(tag: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("gstore_zipio_{tag}_{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            d
        }

        /// A package of `(name, content)` entries (a `None` content: a
        /// directory entry).
        fn package(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
            let mut zw = zip::ZipWriter::new(fs::File::create(path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            for (name, content) in entries {
                match content {
                    Some(c) => {
                        zw.start_file(*name, opts).unwrap();
                        zw.write_all(c).unwrap();
                    }
                    None => zw.add_directory(*name, opts).unwrap(),
                }
            }
            zw.finish().unwrap();
        }

        fn nt(name: &str) -> bool {
            name.ends_with(".nt")
        }

        #[test]
        fn packages_round_trip_and_are_checked() {
            let d = tmp("rt");
            let src = d.join("src");
            fs::create_dir_all(src.join("sub/deeper")).unwrap();
            fs::write(src.join("a.nt"), "<a> <b> <c> .\n").unwrap();
            fs::write(src.join("sub/b.nt"), "x".repeat(10_000)).unwrap();
            fs::create_dir_all(src.join("empty")).unwrap();
            let z = d.join("p.zip");
            assert_eq!(pack_dir(&src, &z).unwrap(), 2);
            assert!(!d.join("p.zip.part").exists());
            let out = d.join("out");
            let files = unpack(&z, &out, &ZipLimits::default(), &nt).unwrap();
            let rels: Vec<_> = files
                .iter()
                .map(|f| f.path.strip_prefix(&out).unwrap().to_path_buf())
                .collect();
            assert_eq!(rels, [PathBuf::from("a.nt"), PathBuf::from("sub/b.nt")]);
            assert_eq!(files[1].size, 10_000);
            assert!(out.join("empty").is_dir() && out.join("sub/deeper").is_dir());
            assert_eq!(
                read_entry(&z, "a.nt", 100).unwrap().as_deref(),
                Some(&b"<a> <b> <c> .\n"[..])
            );
            assert_eq!(read_entry(&z, "nope", 100).unwrap(), None);
            assert!(read_entry(&z, "sub/b.nt", 10).is_err());

            // The accept filter refuses the whole package, writing nothing.
            let out2 = d.join("out2");
            let e = unpack(&z, &out2, &ZipLimits::default(), &|n| n == "a.nt").unwrap_err();
            assert_eq!(e.code(), Some(ZipCode::NotSupported));
            assert!(!out2.exists());

            // Limits: entries, declared size, actual size.
            let few = ZipLimits {
                max_entries: 3,
                ..ZipLimits::default()
            };
            let e = unpack(&z, &d.join("o3"), &few, &nt).unwrap_err();
            assert_eq!(e.code(), Some(ZipCode::NotSupported), "{e}");
            let small = ZipLimits {
                max_size: 5_000,
                ..ZipLimits::default()
            };
            let e = unpack(&z, &d.join("o4"), &small, &nt).unwrap_err();
            assert!(e.to_string().contains("unzip_max_size"), "{e}");

            // Not a zip / empty.
            fs::write(d.join("fake.zip"), b"PK\x03\x04 nope").unwrap();
            let e = unpack(&d.join("fake.zip"), &d.join("o5"), &few, &nt).unwrap_err();
            assert_eq!(e.code(), Some(ZipCode::NotFound));
            package(&d.join("dirs.zip"), &[("only/", None)]);
            let e = unpack(&d.join("dirs.zip"), &d.join("o6"), &few, &nt).unwrap_err();
            assert_eq!(e.code(), Some(ZipCode::EmptyDir));
            let _ = fs::remove_dir_all(&d);
        }

        #[test]
        fn zip_slip_is_refused_before_writing() {
            let d = tmp("slip");
            for (i, evil) in ["../evil.nt", "/abs/evil.nt", "a/../../evil.nt"]
                .iter()
                .enumerate()
            {
                let z = d.join(format!("e{i}.zip"));
                package(&z, &[("ok.nt", Some(b"x")), (evil, Some(b"y"))]);
                let out = d.join(format!("o{i}"));
                let e = unpack(&z, &out, &ZipLimits::default(), &nt).unwrap_err();
                assert_eq!(e.code(), Some(ZipCode::NotSupported), "{evil}: {e}");
                assert!(!out.exists(), "{evil}");
            }
            assert!(!d.join("evil.nt").exists());
            let _ = fs::remove_dir_all(&d);
        }

        #[test]
        fn export_archives_hold_the_file_by_name() {
            let d = tmp("exp");
            let f = d.join("db_20240101000000.nt");
            fs::write(&f, "<a> <b> <c> .\n").unwrap();
            let z = d.join("db_20240101000000.zip");
            fs::write(&z, b"replaced").unwrap();
            pack_file(&f, &z, "db_20240101000000.nt").unwrap();
            assert!(!d.join("db_20240101000000.zip.part").exists());
            assert_eq!(
                read_entry(&z, "db_20240101000000.nt", 100).unwrap().as_deref(),
                Some(&b"<a> <b> <c> .\n"[..])
            );
            let files = unpack(&z, &d.join("out"), &ZipLimits::default(), &nt).unwrap();
            assert_eq!(files.len(), 1);
            let _ = fs::remove_dir_all(&d);
        }
    }
}
