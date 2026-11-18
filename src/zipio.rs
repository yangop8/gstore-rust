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
//! entries — refused from its end-of-central-directory records, before the
//! central directory is decoded into memory — or whose files declare more
//! than [`ZipLimits::max_size`] bytes in all, is refused, and the bytes
//! actually written are counted against the
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
use crate::model::term::export_key;
use crate::store::read_scope::ReadScope;
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
    /// No limit at all: for a package the local operator vouches for, such as
    /// a backup `grestore` restores (upstream's `grestore` has no limit).
    pub const NONE: ZipLimits = ZipLimits {
        max_entries: usize::MAX,
        max_size: u64::MAX,
    };

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
    let mut archive = open_archive(zip, limits)?;
    let plan = check(&archive, limits, accept)?;
    fs::create_dir_all(dest).map_err(|e| ZipError::unzip(ZipCode::WriteError, e.to_string()))?;
    let write_err = |what: &Path, e: &dyn fmt::Display| {
        ZipError::unzip(ZipCode::WriteError, format!("'{}': {e}", what.display()))
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
        let written = io::copy(
            &mut io::Read::take(&mut entry, budget.saturating_add(1)),
            &mut w,
        )
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
/// an error), or `None` when it holds no such file. A package that declares
/// more than [`DEFAULT_MAX_ENTRIES`] entries is refused before its central
/// directory is read (see [`unpack`]).
#[cfg(feature = "zip")]
pub fn read_entry(
    zip: &Path,
    name: &str,
    max: u64,
) -> std::result::Result<Option<Vec<u8>>, ZipError> {
    use io::Read;
    // Only the default entry limit: the central directory is read whole.
    let mut archive = open_archive(zip, &ZipLimits::default())?;
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
        .take(max.saturating_add(1))
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

/// Open `zip` (upstream `unzOpen64`: failure is code 1) — once
/// [`declared_entries`] has shown that no end-of-central-directory record
/// the reader may use declares more than `limits.max_entries` entries (code
/// 3, as the entry limit always answered): the zip reader decodes every
/// record of the central directory into memory before it returns, so the
/// limit must hold before it runs, not after (review bulkcli#5: a package
/// of a million empty entries cost about 190 MB before it was refused).
#[cfg(feature = "zip")]
fn open_archive(zip: &Path, limits: &ZipLimits) -> std::result::Result<Archive, ZipError> {
    use io::Seek;
    let not_found = |e: &dyn fmt::Display| {
        ZipError::unzip(ZipCode::NotFound, format!("'{}': {e}", zip.display()))
    };
    let mut file = std::fs::File::open(zip).map_err(|e| not_found(&e))?;
    let declared = declared_entries(&mut file).map_err(|e| not_found(&e))?;
    if declared > limits.max_entries as u64 {
        return Err(too_many_entries(declared, limits));
    }
    file.seek(io::SeekFrom::Start(0))
        .map_err(|e| not_found(&e))?;
    zip::ZipArchive::new(io::BufReader::new(file)).map_err(|e| not_found(&e))
}

/// The entry limit refused a package that holds (or declares) `n` entries.
#[cfg(feature = "zip")]
fn too_many_entries(n: impl fmt::Display, limits: &ZipLimits) -> ZipError {
    ZipError::unzip(
        ZipCode::NotSupported,
        format!(
            "the package holds {n} entries, more than {} (unzip_max_entries)",
            limits.max_entries
        ),
    )
}

#[cfg(feature = "zip")]
/// The end-of-central-directory record's signature (`PK\x05\x06`).
const EOCD_SIG: [u8; 4] = *b"PK\x05\x06";
#[cfg(feature = "zip")]
/// The zip64 end-of-central-directory locator's signature (`PK\x06\x07`).
const EOCD64_LOCATOR_SIG: [u8; 4] = *b"PK\x06\x07";
#[cfg(feature = "zip")]
/// The zip64 end-of-central-directory record's signature (`PK\x06\x06`).
const EOCD64_SIG: [u8; 4] = *b"PK\x06\x06";
#[cfg(feature = "zip")]
/// The fixed part of an end-of-central-directory record.
const EOCD_LEN: u64 = 22;
#[cfg(feature = "zip")]
/// The fixed part of a central-directory file header, its signature
/// included: the least a header takes, what the zip reader assumes when it
/// checks a zip64 record's entry count against the record's offset.
const CDFH_LEN: u64 = 46;
#[cfg(feature = "zip")]
/// The smallest "size of the zip64 end of central directory record" field
/// the zip reader accepts.
const EOCD64_MIN_RECORD_SIZE: u64 = 40;
/// How far from the end an end-of-central-directory record may start: its
/// fixed part, its signature again and the longest comment (64 KiB) — the
/// window zip readers search.
#[cfg(feature = "zip")]
const EOCD_SEARCH: u64 = EOCD_LEN + 4 + (1 << 16);

#[cfg(feature = "zip")]
fn le16(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

#[cfg(feature = "zip")]
fn le32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]))
}

#[cfg(feature = "zip")]
fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// The most entries any end-of-central-directory record of the package in
/// `r` declares, among the records a zip reader may use: it searches the
/// last [`EOCD_SEARCH`] bytes backwards and falls back to an earlier
/// candidate when one is inconsistent, so every plausible candidate counts
/// (one on a single disk, its comment inside the file, its directory before
/// it). A zip64 record is read where its locator says; one that is not
/// there, or is inconsistent in a way that would make a reader search the
/// file for another, makes the package unsupported (`InvalidData`) rather
/// than unchecked. Reads at most the search window plus one zip64 record —
/// never the central directory itself.
#[cfg(feature = "zip")]
fn declared_entries<R: io::Read + io::Seek>(r: &mut R) -> io::Result<u64> {
    use io::SeekFrom;
    let len = r.seek(SeekFrom::End(0))?;
    let start = len.saturating_sub(EOCD_SEARCH);
    let mut tail = Vec::with_capacity((len - start) as usize);
    r.seek(SeekFrom::Start(start))?;
    io::Read::read_to_end(&mut io::Read::take(&mut *r, len - start), &mut tail)?;
    let unsupported = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let mut most = 0u64;
    let mut found = false;
    for pos in (0..tail.len().saturating_sub(EOCD_LEN as usize - 1)).rev() {
        if tail[pos..pos + 4] != EOCD_SIG {
            continue;
        }
        let e = &tail[pos..pos + EOCD_LEN as usize];
        let at = start + pos as u64;
        let (disk, cd_disk) = (le16(e, 4), le16(e, 6));
        let (on_disk, total) = (le16(e, 8), le16(e, 10));
        let (cd_size, cd_offset, comment) = (le32(e, 12), le32(e, 16), le16(e, 20));
        if at + EOCD_LEN + comment > len {
            continue; // the comment runs past the end: not a record
        }
        found = true;
        let zip64 = on_disk == 0xFFFF
            || total == 0xFFFF
            || cd_size == 0xFFFF_FFFF
            || cd_offset == 0xFFFF_FFFF;
        if zip64 && at >= 20 {
            let mut loc = [0u8; 20];
            r.seek(SeekFrom::Start(at - 20))?;
            io::Read::read_exact(r, &mut loc)?;
            if loc[..4] == EOCD64_LOCATOR_SIG {
                let (eocd64_at, disks) = (le64(&loc, 8), le32(&loc, 16));
                if disks > 1 || eocd64_at >= at - 20 {
                    continue; // multi-disk or misplaced: no reader uses it
                }
                let mut z = [0u8; 56];
                r.seek(SeekFrom::Start(eocd64_at))?;
                io::Read::read_exact(r, &mut z)
                    .map_err(|_| unsupported("truncated zip64 record"))?;
                let (size, z_on_disk, z_total) = (le64(&z, 4), le64(&z, 24), le64(&z, 32));
                let (z_disk, z_cd_disk, z_cd_offset) = (le32(&z, 16), le32(&z, 20), le64(&z, 48));
                // Every check the zip reader (zip 9.2,
                // `find_central_directory_end`) applies to the record the
                // locator names. A record that fails one is not simply
                // skipped by the reader: it scans the bytes from there to the
                // locator for another zip64 record and takes the first
                // consistent one — which may sit inside this record's
                // extensible data and declare any count (review of
                // bulkcli#5's fix). That scan is not followed here: such a
                // package is refused.
                let fits = z[..4] == EOCD64_SIG
                    && size >= EOCD64_MIN_RECORD_SIZE
                    && size.checked_add(12) == Some(at - 20 - eocd64_at)
                    && z_disk == 0
                    && z_cd_disk == le32(&loc, 4)
                    && z_on_disk == z_total
                    && z_total
                        .checked_mul(CDFH_LEN)
                        .and_then(|n| n.checked_add(z_cd_offset))
                        .is_some_and(|end| end <= eocd64_at);
                if !fits {
                    return Err(unsupported(
                        "the zip64 end of central directory is not where its locator says, \
                         or is inconsistent",
                    ));
                }
                most = most.max(z_total);
                continue;
            }
        }
        // A single-disk record with its directory before it is what a
        // reader takes (an empty one too).
        if disk == 0 && cd_disk == 0 && on_disk == total && (total == 0 || cd_offset < at) {
            most = most.max(total);
        }
    }
    if !found {
        return Err(unsupported("no end of central directory record"));
    }
    Ok(most)
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
    // `open_archive` refused a package declaring more already; this
    // guards what the reader actually decoded.
    if n > limits.max_entries {
        return Err(too_many_entries(n, limits));
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
/// Symbolic links are skipped. Written to a temporary file beside `dest`
/// (`<dest>.part-<pid>-<n>`, created exclusively), synced and renamed into
/// place (a `dest` that is a symbolic link: over the file it names).
/// Returns the number of files packed.
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

/// [`pack_file`] into `w` (an empty file, or anything that can be written
/// and sought like one) instead of a path: for a caller that creates and
/// commits the archive file itself — `gexport -z` writes it through the
/// output file of a plain export (an exclusively created temporary name, a
/// symbolic link's file replaced keeping its mode). Returns `w`, flushed
/// by the archive writer but not synced.
#[cfg(feature = "zip")]
pub fn pack_file_to<W: io::Write + io::Seek>(
    src: &Path,
    w: W,
    entry: &str,
) -> std::result::Result<W, ZipError> {
    let mut zw = zip::ZipWriter::new(w);
    add_file(&mut zw, entry, src)?;
    zw.finish().map_err(zip_io)
}

/// Without the `zip` feature: [`ZipError::FeatureMissing`].
#[cfg(not(feature = "zip"))]
pub fn pack_file_to<W: io::Write + io::Seek>(
    _src: &Path,
    _w: W,
    _entry: &str,
) -> std::result::Result<W, ZipError> {
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
fn add_file<W: io::Write + io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    name: &str,
    path: &Path,
) -> std::result::Result<(), ZipError> {
    let opts = options_for(path)?;
    zw.start_file(name, opts).map_err(zip_io)?;
    let mut f = std::fs::File::open(path)?;
    io::copy(&mut f, zw)?;
    Ok(())
}

/// Write an archive to a temporary file beside `dest` with `fill`, then sync
/// and rename it to `dest`; the partial file is removed on error.
///
/// The temporary file is `<dest>.part-<pid>-<n>`, created exclusively, and
/// a `dest` that is a symbolic link to a file is resolved first, so the
/// file it names is replaced and the link stays (round-4 review of
/// bulkcli#11: a fixed `<dest>.part` was opened truncating and following a
/// symbolic link — a link planted there had its target overwritten, two
/// writers of one archive shared the file — and the rename replaced the
/// link itself).
#[cfg(feature = "zip")]
fn write_archive<T>(
    dest: &Path,
    fill: impl FnOnce(&mut Writer) -> std::result::Result<T, ZipError>,
) -> std::result::Result<T, ZipError> {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dest = match fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf())
        }
        _ => dest.to_path_buf(),
    };
    let dest = dest.as_path();
    let mut attempt = 0;
    let (part, file) = loop {
        let mut part = dest.as_os_str().to_owned();
        part.push(format!(
            ".part-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let part = PathBuf::from(part);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
        {
            Ok(file) => break (part, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            Err(e) => return Err(ZipError::Io(e)),
        }
    };
    let result = (|| {
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

    /// Create a new, empty directory named `<prefix>-<pid>-<random>`
    /// **exclusively** — an existing directory is never reused or removed,
    /// whoever made it — and private to this user (mode `0700` on Unix).
    /// The name cannot be predicted, so no other process can prepare it or
    /// pick it for its own scratch work (review bulkcli#6: a fixed name was
    /// taken over — wiped and refilled — by a concurrent tool). `prefix`'s
    /// parent must exist.
    pub fn create_unique(prefix: &Path) -> io::Result<ScratchDir> {
        use std::hash::BuildHasher;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut attempt = 0u32;
        loop {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            // `RandomState` keys come from the operating system's randomness.
            let nonce = std::collections::hash_map::RandomState::new().hash_one((
                nanos,
                SEQ.fetch_add(1, Ordering::Relaxed),
                std::process::id(),
            ));
            let mut name = prefix.as_os_str().to_owned();
            name.push(format!("-{}-{nonce:016x}", std::process::id()));
            let path = PathBuf::from(name);
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(ScratchDir { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => {
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
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
/// Every term is written through [`export_key`] (the escaping N-Triples
/// serializer, `Term::to_ntriples`), so the export loads back.
///
/// The walk runs inside a [`ReadScope`] (contract C2): a read error a backend
/// reports while exporting fails the export instead of silently dropping the
/// statements it could not read.
pub fn write_export(db: &Database, format: ExportFormat, out: &mut dyn Write) -> Result<u64> {
    let scope = ReadScope::enter();
    let count = write_statements(db, format, out)?;
    if let Some(e) = scope.take_error() {
        return Err(GStoreError::Database(format!(
            "a read from the store failed while exporting: {e}"
        )));
    }
    drop(scope);
    out.flush()?;
    Ok(count)
}

/// The statements of [`write_export`], without the read scope or the flush.
fn write_statements(db: &Database, format: ExportFormat, out: &mut dyn Write) -> Result<u64> {
    let dict = db.dict();
    let mut count = 0u64;
    for t in db.backend().iter_all() {
        let (Some(s), Some(p), Some(o)) = (dict.term(t.sub), dict.pred(t.pred), dict.term(t.obj))
        else {
            continue;
        };
        let (s, p, o) = (export_key(&s), export_key(&p), export_key(&o));
        writeln!(out, "{s} {p} {o} .")?;
        count += 1;
    }
    if format == ExportFormat::NQuads {
        for (&gid, graph) in db.named_graphs() {
            let Some(g) = dict.term(gid) else {
                continue;
            };
            let g = export_key(&g);
            for t in graph.iter_all() {
                let (Some(s), Some(p), Some(o)) =
                    (dict.term(t.sub), dict.pred(t.pred), dict.term(t.obj))
                else {
                    continue;
                };
                let (s, p, o) = (export_key(&s), export_key(&p), export_key(&o));
                writeln!(out, "{s} {p} {o} {g} .")?;
                count += 1;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package made by the zip writer, with `n` small files.
    #[cfg(feature = "zip")]
    fn written(n: usize, comment: &[u8]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for i in 0..n {
            zw.start_file(format!("f{i}.nt"), zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"<a> <b> <c> .\n").unwrap();
        }
        zw.set_raw_comment(comment.into()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    /// Review bulkcli#5: the declared entry count comes from the end of the
    /// file — the end record, a zip64 record where its locator says — and
    /// every candidate record a reader may fall back to counts.
    #[cfg(feature = "zip")]
    #[test]
    fn declared_entries_reads_the_end_records_only() {
        let count = |b: &[u8]| declared_entries(&mut io::Cursor::new(b));
        assert_eq!(count(&written(3, b"")).unwrap(), 3);
        assert_eq!(count(&written(0, b"")).unwrap(), 0);
        assert_eq!(count(&written(5, b"a comment")).unwrap(), 5);
        // Data prepended (a self-extracting stub): same records.
        let mut stub = b"#!/bin/sh\nexit 0\n".to_vec();
        stub.extend(written(4, b""));
        assert_eq!(count(&stub).unwrap(), 4);
        // A forged record inside the comment, declaring more entries, is a
        // candidate a reader may fall back to: it counts.
        let mut forged = Vec::new();
        forged.extend_from_slice(&EOCD_SIG);
        forged.extend_from_slice(&[0, 0, 0, 0]);
        forged.extend_from_slice(&60_000u16.to_le_bytes());
        forged.extend_from_slice(&60_000u16.to_le_bytes());
        forged.extend_from_slice(&[0; 10]);
        assert_eq!(count(&written(2, &forged)).unwrap(), 60_000);
        // Not a zip, or truncated before its end record.
        assert!(count(b"not a zip at all").is_err());
        let whole = written(3, b"");
        assert!(count(&whole[..whole.len() - 10]).is_err());
        // A zip64 end record that is not where its locator says.
        let mut z64 = whole[..whole.len() - 22].to_vec();
        let locator_at = z64.len() as u64;
        z64.extend_from_slice(&EOCD64_LOCATOR_SIG);
        z64.extend_from_slice(&[0; 4]);
        z64.extend_from_slice(&(locator_at - 56).to_le_bytes());
        z64.extend_from_slice(&1u32.to_le_bytes());
        z64.extend_from_slice(&EOCD_SIG);
        z64.extend_from_slice(&[0; 4]);
        z64.extend_from_slice(&[0xFF; 12]);
        z64.extend_from_slice(&[0; 2]);
        let e = count(&z64).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// The limit is applied to what the end records declare: one entry over
    /// it is refused with the entry-limit error, before the archive opens.
    #[cfg(feature = "zip")]
    #[test]
    fn open_archive_refuses_a_declared_count_over_the_limit() {
        let dir =
            std::env::temp_dir().join(format!("gstore_zipio_declared_{}", std::process::id()));
        fs_reset(&dir);
        let zip = dir.join("p.zip");
        std::fs::write(&zip, written(11, b"")).unwrap();
        let limits = ZipLimits {
            max_entries: 10,
            ..ZipLimits::default()
        };
        let e = open_archive(&zip, &limits).map(drop).unwrap_err();
        assert_eq!(e.code(), Some(ZipCode::NotSupported), "{e}");
        assert!(e.to_string().contains("holds 11 entries"), "{e}");
        let ok = ZipLimits {
            max_entries: 11,
            ..ZipLimits::default()
        };
        assert_eq!(open_archive(&zip, &ok).unwrap().len(), 11);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review bulkcli#5 (review of the fix): when the zip64 end record at
    /// the locator's offset fails one of the zip reader's own consistency
    /// checks, the reader does not stop there — it scans the bytes between
    /// that offset and the locator for another zip64 end record and uses the
    /// first consistent one. A package whose named record declares one entry
    /// but is inconsistent (multi-disk counts, a disk number, a disk that
    /// disagrees with the locator), with a consistent record declaring the
    /// real central directory hidden in its extensible data, was counted
    /// from the named record and passed the limit, and the reader then
    /// decoded the whole directory. The declared count must cover the
    /// record the reader uses, or the package is refused.
    #[cfg(feature = "zip")]
    #[test]
    fn a_zip64_record_the_reader_would_skip_is_not_taken_for_the_count() {
        const N: u64 = 20;
        let plain = written(N as usize, b"");
        let eocd = plain.len() - EOCD_LEN as usize;
        let (cd_size, cd_offset) = (le32(&plain, eocd + 12), le32(&plain, eocd + 16));
        // (disk, disk with the directory, entries on this disk, entries) of
        // the record the locator names, and the locator's disk with the
        // directory.
        for (disk, cd_disk, on_disk, total, loc_cd_disk) in [
            (0u32, 0u32, 1u64, 2u64, 0u32),
            (1, 0, 1, 1, 0),
            (0, 1, 1, 1, 0),
        ] {
            let mut z = plain[..eocd].to_vec();
            let named_at = z.len() as u64;
            let record =
                |z: &mut Vec<u8>, size: u64, disks: (u32, u32), n: (u64, u64), off: u64| {
                    z.extend_from_slice(&EOCD64_SIG);
                    z.extend_from_slice(&size.to_le_bytes());
                    z.extend_from_slice(&45u16.to_le_bytes()); // made by
                    z.extend_from_slice(&45u16.to_le_bytes()); // needed
                    z.extend_from_slice(&disks.0.to_le_bytes());
                    z.extend_from_slice(&disks.1.to_le_bytes());
                    z.extend_from_slice(&n.0.to_le_bytes());
                    z.extend_from_slice(&n.1.to_le_bytes());
                    z.extend_from_slice(&cd_size.to_le_bytes());
                    z.extend_from_slice(&off.to_le_bytes());
                };
            // The named record (56 bytes) with 56 bytes of extensible data…
            record(&mut z, 100, (disk, cd_disk), (on_disk, total), 0);
            // …holding a consistent record of the real directory, relative to
            // the archive offset the reader then assumes (56).
            record(&mut z, 44, (0, 0), (N, N), cd_offset - 56);
            z.extend_from_slice(&EOCD64_LOCATOR_SIG);
            z.extend_from_slice(&loc_cd_disk.to_le_bytes());
            z.extend_from_slice(&named_at.to_le_bytes());
            z.extend_from_slice(&1u32.to_le_bytes());
            z.extend_from_slice(&EOCD_SIG);
            z.extend_from_slice(&[0; 4]);
            z.extend_from_slice(&[0xFF; 12]); // counts, size, offset: zip64
            z.extend_from_slice(&[0; 2]);
            let case = (disk, cd_disk, on_disk, total);
            // The reader takes the hidden record: it decodes all N entries.
            let read = zip::ZipArchive::new(io::Cursor::new(&z))
                .map(|a| a.len() as u64)
                .unwrap_or_else(|e| panic!("{case:?}: the reader refused it: {e}"));
            assert_eq!(read, N, "{case:?}");
            match declared_entries(&mut io::Cursor::new(&z)) {
                Ok(n) => assert!(n >= N, "{case:?}: declared {n}, the reader decodes {N}"),
                Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{case:?}: {e}"),
            }
        }
    }

    /// A zip64 package of empty stored entries named `names`, written by
    /// hand: local headers, the central directory, a zip64 end record and
    /// its locator right after it, and an end record that defers to them.
    #[cfg(feature = "zip")]
    fn zip64_of(names: &[&str]) -> Vec<u8> {
        let mut z = Vec::new();
        let mut offsets = Vec::new();
        for name in names {
            offsets.push(z.len() as u32);
            z.extend_from_slice(b"PK\x03\x04");
            z.extend_from_slice(&20u16.to_le_bytes()); // version needed
            z.extend_from_slice(&[0; 4]); // flags, method (stored)
            z.extend_from_slice(&[0, 0, 0x21, 0]); // time, date (1980-01-01)
            z.extend_from_slice(&[0; 12]); // crc, sizes
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&[0; 2]); // extra
            z.extend_from_slice(name.as_bytes());
        }
        let cd_start = z.len() as u64;
        for (name, off) in names.iter().zip(&offsets) {
            z.extend_from_slice(b"PK\x01\x02");
            z.extend_from_slice(&20u16.to_le_bytes()); // made by
            z.extend_from_slice(&20u16.to_le_bytes()); // needed
            z.extend_from_slice(&[0; 4]); // flags, method
            z.extend_from_slice(&[0, 0, 0x21, 0]); // time, date
            z.extend_from_slice(&[0; 12]); // crc, sizes
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&[0; 12]); // extra, comment, disk, attributes
            z.extend_from_slice(&off.to_le_bytes());
            z.extend_from_slice(name.as_bytes());
        }
        let eocd64_at = z.len() as u64;
        let n = names.len() as u64;
        z.extend_from_slice(&EOCD64_SIG);
        z.extend_from_slice(&44u64.to_le_bytes());
        z.extend_from_slice(&45u16.to_le_bytes());
        z.extend_from_slice(&45u16.to_le_bytes());
        z.extend_from_slice(&[0; 8]); // disk, disk with the directory
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(eocd64_at - cd_start).to_le_bytes());
        z.extend_from_slice(&cd_start.to_le_bytes());
        z.extend_from_slice(&EOCD64_LOCATOR_SIG);
        z.extend_from_slice(&[0; 4]);
        z.extend_from_slice(&eocd64_at.to_le_bytes());
        z.extend_from_slice(&1u32.to_le_bytes());
        z.extend_from_slice(&EOCD_SIG);
        z.extend_from_slice(&[0; 4]);
        z.extend_from_slice(&[0xFF; 12]); // counts, size, offset: zip64
        z.extend_from_slice(&[0; 2]);
        z
    }

    /// Review of bulkcli#5's fix: the entry-count check of the zip64 record
    /// assumed 50 bytes per central-directory header where the reader (and
    /// the format) assume 46, so a valid zip64 package with short entry
    /// names (a directory of fewer than 50 bytes per entry) was refused as
    /// unsupported although the reader reads it.
    #[cfg(feature = "zip")]
    #[test]
    fn a_zip64_package_with_short_names_is_counted_not_refused() {
        let z = zip64_of(&["a", "b", "c"]);
        let read = zip::ZipArchive::new(io::Cursor::new(&z))
            .map(|a| a.len())
            .unwrap_or_else(|e| panic!("the reader refused it: {e}"));
        assert_eq!(read, 3);
        assert_eq!(declared_entries(&mut io::Cursor::new(&z)).unwrap(), 3);
    }

    #[cfg(feature = "zip")]
    fn fs_reset(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
    }

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
    fn exported_iris_load_back_even_with_characters_iriref_excludes() {
        use crate::model::{Term, Triple};
        let triples = vec![
            Triple::new(
                Term::iri("http://ex/a b"),
                Term::iri("http://ex/p"),
                Term::iri("http://ex/o\t{x}|\"<>`^\\"),
            ),
            Triple::new(
                Term::iri("http://ex/c"),
                Term::iri("http://ex/p"),
                Term::typed_literal("v \"q\"^^<x y>", "http://ex/dt z"),
            ),
            Triple::new(
                Term::iri("http://ex/c"),
                Term::iri("http://ex/p"),
                Term::plain_literal("a b <c>"),
            ),
        ];
        let db = Database::build_from_triples("x", triples.clone()).unwrap();
        let mut out = Vec::new();
        assert_eq!(
            write_export(&db, ExportFormat::NTriples, &mut out).unwrap(),
            3
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("<http://ex/a\\u0020b>"), "{text}");
        let mut want = triples.clone();
        want.sort_by_key(|t| t.to_string());
        // `.nt` files load through the Turtle reader, `.nq` through N-Quads.
        for mut back in [
            crate::parser::turtle::parse_str(&text).unwrap(),
            crate::parser::nquads::parse_str_triples(&text).unwrap(),
        ] {
            back.sort_by_key(|t| t.to_string());
            assert_eq!(back, want);
        }
        let mut out = Vec::new();
        db.export_ntriples(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), text);
        assert_eq!(export_key("_:b"), "_:b");
        assert_eq!(export_key("\"x\"@en"), "\"x\"@en");
        assert!(matches!(
            export_key("<http://ok>"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// A sink that stands in for a backend whose reads fail: on the export's
    /// thread, during the walk, it reports a read error (contract C2).
    struct FailingReads(Vec<u8>);
    impl Write for FailingReads {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            crate::store::read_scope::note_read_error("injected export read failure");
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_read_error_fails_the_export() {
        use crate::store::read_scope::in_scope;
        let db = Database::build_from_str(
            "x",
            "<http://a> <http://p> <http://b> .\n<http://b> <http://p> \"l\" .\n",
        )
        .unwrap();
        for format in [ExportFormat::NTriples, ExportFormat::NQuads] {
            let e = write_export(&db, format, &mut FailingReads(Vec::new())).unwrap_err();
            assert!(matches!(e, GStoreError::Database(_)), "{e}");
            assert!(e.to_string().contains("exporting"), "{e}");
            assert!(
                e.to_string().contains("injected export read failure"),
                "{e}"
            );
        }
        let e = db
            .export_ntriples(&mut FailingReads(Vec::new()))
            .unwrap_err();
        assert!(
            e.to_string().contains("injected export read failure"),
            "{e}"
        );
        // The errors landed in the export's scope, which is gone again.
        assert!(!in_scope());
        // Without an error the same export succeeds.
        let mut out = Vec::new();
        assert_eq!(db.export_ntriples(&mut out).unwrap(), 2);
    }

    #[test]
    fn formats_and_names() {
        assert_eq!(ExportFormat::parse("NQ"), Some(ExportFormat::NQuads));
        assert_eq!(
            ExportFormat::parse("n-triples"),
            Some(ExportFormat::NTriples)
        );
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
        let e = unpack(
            Path::new("x.zip"),
            Path::new("d"),
            &ZipLimits::default(),
            &|_| true,
        )
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

        /// The archive temporary files (`*.part*`) in `d`.
        fn part_files(d: &Path) -> Vec<String> {
            let mut left: Vec<String> = fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains(".part"))
                .collect();
            left.sort();
            left
        }

        /// Round-4 review of bulkcli#11: an archive is written to a
        /// temporary name created exclusively beside it — never through
        /// what sits at the old fixed `<dest>.part` (a symbolic link planted
        /// there had its target overwritten) — and a destination that is a
        /// symbolic link stays one (the file it names is replaced).
        #[cfg(unix)]
        #[test]
        fn archives_never_write_through_a_planted_part_and_keep_a_link() {
            struct Gone(PathBuf);
            impl Drop for Gone {
                fn drop(&mut self) {
                    let _ = fs::remove_dir_all(&self.0);
                }
            }
            let d = Gone(tmp("part"));
            let d = d.0.as_path();
            let src = d.join("src.nt");
            fs::write(&src, "<a> <b> <c> .\n").unwrap();
            let victim = d.join("victim.txt");
            fs::write(&victim, "keep me").unwrap();
            std::os::unix::fs::symlink(&victim, d.join("out.zip.part")).unwrap();
            pack_file(&src, &d.join("out.zip"), "src.nt").unwrap();
            assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
            assert_eq!(
                read_entry(&d.join("out.zip"), "src.nt", 100)
                    .unwrap()
                    .as_deref(),
                Some(&b"<a> <b> <c> .\n"[..])
            );

            let real = d.join("real.zip");
            fs::write(&real, "old").unwrap();
            std::os::unix::fs::symlink("real.zip", d.join("link.zip")).unwrap();
            let sub = d.join("sub");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("a.nt"), "<a> <b> <c> .\n").unwrap();
            assert_eq!(pack_dir(&sub, &d.join("link.zip")).unwrap(), 1);
            assert!(
                fs::symlink_metadata(d.join("link.zip"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the link was replaced by the archive"
            );
            assert!(read_entry(&real, "a.nt", 100).unwrap().is_some());
            assert_eq!(part_files(d), vec!["out.zip.part".to_string()]);
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
            // The temporary name is `p.zip.part-<pid>-<n>` (round-4 review
            // of bulkcli#11), so whether one was left takes a scan.
            assert_eq!(part_files(&d), Vec::<String>::new());
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
        fn symlinks_are_ignored() {
            let d = tmp("link");
            let z = d.join("l.zip");
            let mut zw = zip::ZipWriter::new(fs::File::create(&z).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zw.add_symlink("evil.nt", "/etc/passwd", opts).unwrap();
            zw.start_file("ok.nt", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.finish().unwrap();
            let out = d.join("out");
            let files = unpack(&z, &out, &ZipLimits::default(), &nt).unwrap();
            assert_eq!(files.len(), 1);
            assert!(fs::symlink_metadata(out.join("evil.nt")).is_err());
            // A package of links only holds no file.
            let z2 = d.join("l2.zip");
            let mut zw = zip::ZipWriter::new(fs::File::create(&z2).unwrap());
            zw.add_symlink("a.nt", "/etc/passwd", opts).unwrap();
            zw.finish().unwrap();
            let e = unpack(&z2, &d.join("o2"), &ZipLimits::default(), &nt).unwrap_err();
            assert_eq!(e.code(), Some(ZipCode::EmptyDir));
            let _ = fs::remove_dir_all(&d);
        }

        /// Overwrite every 4-byte little-endian `from` at the size fields of
        /// the local (offset 22) and central (offset 24) headers with `to`.
        fn lie_about_sizes(bytes: &mut [u8], from: u32, to: u32) {
            let mut patched = 0;
            for i in 0..bytes.len().saturating_sub(4) {
                let sig = &bytes[i..i + 4];
                let at = if sig == b"PK\x03\x04" {
                    i + 22
                } else if sig == b"PK\x01\x02" {
                    i + 24
                } else {
                    continue;
                };
                if bytes[at..at + 4] == from.to_le_bytes() {
                    bytes[at..at + 4].copy_from_slice(&to.to_le_bytes());
                    patched += 1;
                }
            }
            assert_eq!(patched, 2, "both headers patched");
        }

        #[test]
        fn a_header_that_lies_does_not_unpack_more_than_the_limit() {
            let d = tmp("lie");
            let z = d.join("bomb.zip");
            let big = vec![b' '; 1 << 20];
            let mut zw = zip::ZipWriter::new(fs::File::create(&z).unwrap());
            zw.start_file(
                "bomb.nt",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            zw.write_all(&big).unwrap();
            zw.finish().unwrap();
            let mut bytes = fs::read(&z).unwrap();
            lie_about_sizes(&mut bytes, 1 << 20, 100);
            fs::write(&z, &bytes).unwrap();
            let limits = ZipLimits {
                max_size: 4096,
                ..ZipLimits::default()
            };
            let out = d.join("out");
            // The header claims 100 bytes; whatever happens, no more than the
            // limit reaches the disk.
            let res = unpack(&z, &out, &limits, &nt);
            let on_disk: u64 = fs::read_dir(&out)
                .map(|rd| rd.flatten().map(|e| e.metadata().unwrap().len()).sum())
                .unwrap_or(0);
            assert!(on_disk <= limits.max_size, "{on_disk} bytes written");
            match res {
                Err(e) => assert_eq!(e.code(), Some(ZipCode::WriteError), "{e}"),
                Ok(files) => assert!(files.iter().all(|f| f.size <= 100)),
            }
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
                read_entry(&z, "db_20240101000000.nt", 100)
                    .unwrap()
                    .as_deref(),
                Some(&b"<a> <b> <c> .\n"[..])
            );
            let files = unpack(&z, &d.join("out"), &ZipLimits::default(), &nt).unwrap();
            assert_eq!(files.len(), 1);
            let _ = fs::remove_dir_all(&d);
        }
    }
}
