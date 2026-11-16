//! Crash-safe file primitives used by database persistence.
//!
//! * **Single files** are replaced atomically: the new content goes to
//!   `<file>.tmp` in the same directory, is flushed and `fsync`ed, renamed over
//!   the target, and the directory is `fsync`ed (unix) so the rename itself is
//!   durable. A crash at any point leaves either the old or the new file — never
//!   a truncated one.
//! * **Snapshot sets** (`dict.bin`, `store.bin`, … — several files that are only
//!   meaningful together) are committed with [`SnapshotWriter`]: every member is
//!   first written to its `.tmp` file and synced, then a small commit record
//!   (`SNAPSHOT.commit`) is written atomically, then the members are renamed into
//!   place and the record is removed. [`recover_snapshot`] — run before every
//!   snapshot read or write — rolls an interrupted commit forward once its
//!   record exists, or discards the half-written temp files of one that never
//!   reached it. So a crash mid-save leaves the old snapshot or the new one,
//!   never a mix.
//! * **Directories** (a RocksDB store copied into a backup) are staged as
//!   `<dir>.tmp`, renamed to `<dir>.new` once complete, then swapped in;
//!   [`recover_dir_swap`] finishes an interrupted swap.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{GStoreError, Result};

/// Suffix of the temporary file/directory a write is staged in.
pub(crate) const TMP_SUFFIX: &str = ".tmp";
/// Suffix of a fully-staged directory waiting to be swapped in.
const NEW_SUFFIX: &str = ".new";
/// Suffix of the directory being swapped out.
const OLD_SUFFIX: &str = ".old";
/// The commit record of a multi-file snapshot write (see [`SnapshotWriter`]).
pub(crate) const SNAPSHOT_COMMIT_FILE: &str = "SNAPSHOT.commit";

/// `path` with `suffix` appended to its final component.
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The staging path of `path` (`<path>.tmp`).
pub(crate) fn tmp_path(path: &Path) -> PathBuf {
    with_suffix(path, TMP_SUFFIX)
}

/// The directory containing `path` (`.` for a bare file name).
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// `fsync` a directory so renames/unlinks inside it are durable. A no-op off
/// unix, where directories cannot be opened for syncing.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Create directory `dir` and every missing ancestor (as
/// `fs::create_dir_all`), then `fsync` the parent of every directory it
/// created, so the new entries survive a power loss once the files written
/// into `dir` are synced (review db#12; `sync_dir(dir)` only makes entries
/// *inside* `dir` durable). A no-op beyond the existence check when `dir`
/// exists.
pub(crate) fn create_dir_durable(dir: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut at = dir;
    while !at.exists() {
        missing.push(at.to_path_buf());
        match at.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => at = parent,
            _ => break,
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    // Deepest first: each new directory's own entry, then its parent's.
    for created in &missing {
        let parent = parent_of(created);
        #[cfg(test)]
        DURABLE_DIR_PARENT_SYNCS.with(|s| s.borrow_mut().push(parent.to_path_buf()));
        sync_dir(parent)?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test hook: the parent directories [`create_dir_durable`] synced on
    /// this thread, in order.
    pub(crate) static DURABLE_DIR_PARENT_SYNCS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Remove a file, treating "not found" as success.
pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Remove a directory tree, treating "not found" as success.
pub(crate) fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write `<path>.tmp` through `fill`, then flush and `fsync` it, returning the
/// temp path. On any failure the temp file is removed and the target untouched.
fn write_tmp<F>(path: &Path, fill: F) -> Result<PathBuf>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    write_tmp_mode(path, None, fill)
}

/// Create `tmp`, the staging file that will replace `path`, with the
/// permissions the replacement must end up with: those of the existing `path`
/// (an operator may have restricted them — renaming a default-mode temp file
/// over it would silently undo that), else `new_mode` (unix mode bits, before
/// the umask) for a file that does not exist yet, else the default. The file is
/// created afresh with that mode, so it is never more permissive than the
/// target, not even while empty.
fn create_tmp(tmp: &Path, path: &Path, new_mode: Option<u32>) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let target = fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777);
        // A stale temp file (from a crash) would keep its own mode.
        remove_file_if_exists(tmp)?;
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if let Some(mode) = target.or(new_mode) {
            opts.mode(mode);
        }
        let file = opts.open(tmp)?;
        if let Some(mode) = target {
            // The umask may have cleared bits the target has: match it exactly.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, new_mode);
        Ok(OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?)
    }
}

/// [`write_tmp`], creating the temp file as [`create_tmp`] does.
fn write_tmp_mode<F>(path: &Path, new_mode: Option<u32>, fill: F) -> Result<PathBuf>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    let tmp = tmp_path(path);
    let written = (|| -> Result<()> {
        let file = create_tmp(&tmp, path, new_mode)?;
        let mut w = BufWriter::new(file);
        fill(&mut w)?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        if tmp.is_file() {
            let _ = fs::remove_file(&tmp);
        }
        return Err(e);
    }
    Ok(tmp)
}

/// Atomically replace `path` with whatever `fill` writes (temp file in the same
/// directory → write → fsync → rename → fsync directory). The replacement
/// keeps the permissions of the file it replaces.
pub(crate) fn atomic_write_with<F>(path: &Path, fill: F) -> Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    atomic_write_mode(path, None, fill)
}

/// [`atomic_write_with`]; a file that does not exist yet is created with unix
/// mode `new_mode` (before the umask) when given.
fn atomic_write_mode<F>(path: &Path, new_mode: Option<u32>, fill: F) -> Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    atomic_replace_mode(path, new_mode, fill).map_err(|e| e.error)
}

/// How an atomic replacement failed: `replaced` when the new file was
/// already renamed over the target (every reader sees the new content) and
/// only the `fsync` of its directory failed — the rename may then not
/// survive a crash; otherwise the target still holds its previous content.
struct ReplaceError {
    replaced: bool,
    error: GStoreError,
}

/// [`atomic_write_mode`], telling a failure after the rename apart from one
/// before it ([`ReplaceError::replaced`]).
fn atomic_replace_mode<F>(
    path: &Path,
    new_mode: Option<u32>,
    fill: F,
) -> std::result::Result<(), ReplaceError>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    let not_replaced = |error| ReplaceError {
        replaced: false,
        error,
    };
    let tmp = write_tmp_mode(path, new_mode, fill).map_err(not_replaced)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(not_replaced(e.into()));
    }
    sync_replaced_dir(path).map_err(|error| ReplaceError {
        replaced: true,
        error,
    })
}

/// `fsync` the directory of `path`, a file just renamed into place.
fn sync_replaced_dir(path: &Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_DIR_SYNC_AFTER_RENAME.with(|n| {
        let left = n.get();
        n.set(left.saturating_sub(1));
        left > 0
    }) {
        return Err(GStoreError::Io(io::Error::other(
            "injected directory sync failure",
        )));
    }
    sync_dir(parent_of(path))
}

#[cfg(test)]
thread_local! {
    /// Test-only failpoint: the next this-many directory syncs after an
    /// atomic replacement's rename fail (as `open(2)` of the directory does
    /// on EMFILE, or its `fsync` on EIO).
    pub(crate) static FAIL_DIR_SYNC_AFTER_RENAME: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
}

/// Atomically replace `path` with `bytes` (as [`atomic_write`]; with
/// `private`, for a file holding secrets such as password hashes, a file
/// that does not exist yet is created owner-only, `0600`, while an existing
/// file keeps its permissions), for a caller that keeps the file's content
/// in memory and must know what the file holds afterwards:
///
/// * `Ok(None)`: replaced, durably;
/// * `Ok(Some(e))`: replaced — every reader sees `bytes` — but the `fsync` of
///   the directory failed, also on one retry (`e`), so the rename may not
///   survive a crash. The caller keeps its new state (it is what the file
///   holds) and reports `e` as a durability warning, never as "nothing
///   changed";
/// * `Err`: not replaced; the file holds its previous content.
pub(crate) fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    private: bool,
) -> Result<Option<GStoreError>> {
    let new_mode = private.then_some(0o600);
    match atomic_replace_mode(path, new_mode, |w| {
        w.write_all(bytes)?;
        Ok(())
    }) {
        Ok(()) => Ok(None),
        Err(ReplaceError {
            replaced: false,
            error,
        }) => Err(error),
        Err(ReplaceError { replaced: true, .. }) => Ok(sync_replaced_dir(path).err()),
    }
}

/// Atomically replace `path` with `bytes`.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_with(path, |w| {
        w.write_all(bytes)?;
        Ok(())
    })
}

/// Atomically replace `path` with the bincode encoding of `value`.
#[cfg(test)]
pub(crate) fn write_bincode_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write_with(path, |w| {
        bincode::serialize_into(w, value)?;
        Ok(())
    })
}

/// Read a bincode-encoded value from `path`.
///
/// Same encoding as `bincode::serialize` (fixed-width ints, trailing bytes
/// allowed), but the decoder may read at most the file's length: a corrupt
/// length prefix then fails cleanly instead of asking the allocator for
/// exabytes (which aborts the process).
pub(crate) fn read_bincode<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<T> {
    use bincode::Options;
    let file = File::open(path)?;
    let limit = file.metadata()?.len();
    let reader = io::BufReader::new(file);
    Ok(bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(limit)
        .deserialize_from(reader)?)
}

/// Atomically replace `dst` with a copy of the file `src`.
pub(crate) fn copy_file_atomic(src: &Path, dst: &Path) -> Result<()> {
    let mut input = File::open(src)?;
    atomic_write_with(dst, |w| {
        io::copy(&mut input, w)?;
        Ok(())
    })
}

/// Whether `a` and `b` name the same directory (after resolving symlinks and
/// relative components). A path that cannot be resolved only equals itself.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

// ---- multi-file snapshot commit --------------------------------------------

/// Stages the members of a snapshot set and commits them together (see the
/// module docs). Dropping a writer without [`commit`](Self::commit)ting removes
/// its staged temp files, leaving the previous snapshot untouched.
pub(crate) struct SnapshotWriter<'a> {
    dir: &'a Path,
    /// Every name that may belong to this kind of snapshot (used to validate a
    /// commit record and to clean up stray temp files).
    members: &'a [&'a str],
    staged: Vec<&'a str>,
    committed: bool,
}

impl<'a> SnapshotWriter<'a> {
    /// Start a snapshot write into `dir` (created if needed, durably: see
    /// [`create_dir_durable`]), first finishing or discarding any interrupted
    /// earlier write. That recovery deletes the staged files of a write in
    /// progress too, so the caller excludes every other writer of `dir`
    /// first: `Database::save` holds the directory's lock and its
    /// [`writing`](super::dirlock::DirLock::writing) exclusion (review db#11).
    pub(crate) fn begin(dir: &'a Path, members: &'a [&'a str]) -> Result<SnapshotWriter<'a>> {
        create_dir_durable(dir)?;
        recover_snapshot(dir, members)?;
        Ok(SnapshotWriter {
            dir,
            members,
            staged: Vec::new(),
            committed: false,
        })
    }

    /// Write member `name` (bincode of `value`) to its temp file and sync it.
    pub(crate) fn stage<T: Serialize>(&mut self, name: &'a str, value: &T) -> Result<()> {
        debug_assert!(self.members.contains(&name));
        write_tmp(&self.dir.join(name), |w| {
            bincode::serialize_into(w, value)?;
            Ok(())
        })?;
        self.staged.push(name);
        #[cfg(test)]
        snapshot_probe::staged(self.dir, name);
        Ok(())
    }

    /// Publish every staged member at once.
    pub(crate) fn commit(mut self) -> Result<()> {
        let record = self.staged.join("\n");
        atomic_write(&self.dir.join(SNAPSHOT_COMMIT_FILE), record.as_bytes())?;
        // From here on the new set *is* the snapshot: a crash below is rolled
        // forward by `recover_snapshot`, so the temp files must not be removed.
        self.committed = true;
        let names: Vec<String> = self.staged.iter().map(|s| s.to_string()).collect();
        finish_snapshot_commit(self.dir, &names)
    }
}

impl Drop for SnapshotWriter<'_> {
    fn drop(&mut self) {
        if !self.committed {
            for name in &self.staged {
                let _ = fs::remove_file(tmp_path(&self.dir.join(name)));
            }
        }
    }
}

/// Test hook: lets a test park a snapshot write after it staged a member.
#[cfg(test)]
pub(crate) mod snapshot_probe {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    /// What to call after a member is staged in a watched directory (with the
    /// member's name).
    pub(crate) type Hook = Arc<dyn Fn(&str) + Send + Sync>;

    static WATCHED: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    /// Call `hook` (on the writing thread) after every member a snapshot
    /// write into `dir` stages.
    pub(crate) fn watch(dir: &Path, hook: Hook) {
        WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((dir.to_path_buf(), hook));
    }

    /// Stop watching `dir`.
    pub(crate) fn unwatch(dir: &Path) {
        WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(d, _)| d != dir);
    }

    /// Called by `SnapshotWriter::stage` once `name` is staged in `dir`.
    pub(super) fn staged(dir: &Path, name: &str) {
        let hooks: Vec<Hook> = WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(d, _)| d == dir)
            .map(|(_, h)| Arc::clone(h))
            .collect();
        for hook in hooks {
            hook(name);
        }
    }
}

/// Rename every staged member into place, then retire the commit record.
fn finish_snapshot_commit(dir: &Path, names: &[String]) -> Result<()> {
    for name in names {
        let tmp = tmp_path(&dir.join(name));
        if tmp.is_file() {
            fs::rename(&tmp, dir.join(name))?;
        }
    }
    sync_dir(dir)?;
    remove_file_if_exists(&dir.join(SNAPSHOT_COMMIT_FILE))?;
    sync_dir(dir)
}

/// Whether [`recover_snapshot`] would act on `dir`: a commit record or a
/// member's temp file is there — what an interrupted snapshot write left, or
/// one in progress (in this or another process; the same names).
pub(crate) fn snapshot_in_progress(dir: &Path, members: &[&str]) -> bool {
    dir.join(SNAPSHOT_COMMIT_FILE).exists()
        || members
            .iter()
            .any(|name| tmp_path(&dir.join(name)).exists())
}

/// Finish (roll forward) a snapshot commit interrupted after its commit record
/// was written, or discard the temp files of one interrupted before it. Only
/// names listed in `members` are ever touched.
pub(crate) fn recover_snapshot(dir: &Path, members: &[&str]) -> Result<()> {
    let record = dir.join(SNAPSHOT_COMMIT_FILE);
    match fs::read_to_string(&record) {
        Ok(text) => {
            let names: Vec<String> = text
                .lines()
                .map(str::trim)
                .filter(|n| members.contains(n))
                .map(str::to_owned)
                .collect();
            finish_snapshot_commit(dir, &names)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            for name in members {
                let tmp = tmp_path(&dir.join(name));
                if tmp.is_file() {
                    let _ = fs::remove_file(&tmp);
                }
            }
            Ok(())
        }
        Err(e) => Err(GStoreError::Database(format!(
            "cannot read snapshot commit record '{}': {e}",
            record.display()
        ))),
    }
}

// ---- directory staging ------------------------------------------------------

/// Recursively copy directory `src` into the (new) directory `dst`. Nothing
/// is synced: the copy is published through [`StagedDir::install`], which
/// fsyncs the whole staged tree before the swap.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only failpoint: [`discard_dir`] stops (as if the process died)
    /// after renaming the directory away, before deleting it.
    pub(crate) static CRASH_BEFORE_DISCARD_DELETE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// The sibling names a directory swap uses for `target`: where the replacement
/// is staged, where a complete replacement waits to be swapped in, and where
/// the replaced directory goes before it is deleted.
#[derive(Debug)]
pub(crate) struct DirSwap {
    tmp: &'static str,
    new: &'static str,
    old: &'static str,
}

/// The swap names of a store directory inside a database directory
/// (`rocksdb/` → `rocksdb.tmp` / `rocksdb.new` / `rocksdb.old`).
pub(crate) const STORE_SWAP: DirSwap = DirSwap {
    tmp: TMP_SUFFIX,
    new: NEW_SUFFIX,
    old: OLD_SUFFIX,
};

/// The swap names of a whole database directory being replaced by a restore
/// (`x.db` → `x.db.restore-tmp` / `-new` / `-old`). Distinctive on purpose: the
/// recovery run before every open deletes leftovers under these names, so they
/// must never collide with a user's own directories (e.g. a manual `x.db.old`).
pub(crate) const RESTORE_SWAP: DirSwap = DirSwap {
    tmp: ".restore-tmp",
    new: ".restore-new",
    old: ".restore-old",
};

impl DirSwap {
    /// Where the replacement of `target` is staged.
    pub(crate) fn staging(&self, target: &Path) -> PathBuf {
        with_suffix(target, self.tmp)
    }

    /// Where the replaced `target` waits to be deleted (during a swap, the
    /// directory itself, moved aside — its lock file with it).
    pub(crate) fn moved_aside(&self, target: &Path) -> PathBuf {
        with_suffix(target, self.old)
    }

    /// Whether a sibling of a swap of `target` exists: what an interrupted
    /// swap left — or a swap in progress, in this or another process (the
    /// same names). [`recover_dir_swap_as`] acts on them only under the lock
    /// of the directory the swap is about.
    pub(crate) fn in_progress(&self, target: &Path) -> bool {
        [self.tmp, self.new, self.old]
            .iter()
            .any(|s| with_suffix(target, s).exists())
    }
}

/// Install a fully-populated staging directory `staged` as `target`, replacing
/// any existing `target`: `staged` → `target.new` → (`target` → `target.old`)
/// → `target`. Interrupted swaps are completed by [`recover_dir_swap`].
// (Without the `rocksdb` feature nothing swaps a store this way.)
#[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
pub(crate) fn install_staged_dir(staged: &Path, target: &Path) -> Result<()> {
    install_staged_dir_as(staged, target, &STORE_SWAP)
}

/// [`install_staged_dir`] with the sibling names of `swap`; interrupted swaps
/// are completed by [`recover_dir_swap_as`] with the same names.
pub(crate) fn install_staged_dir_as(staged: &Path, target: &Path, swap: &DirSwap) -> Result<()> {
    let new = with_suffix(target, swap.new);
    let old = with_suffix(target, swap.old);
    remove_dir_if_exists(&new)?;
    fs::rename(staged, &new)?;
    sync_dir(parent_of(target))?;
    remove_dir_if_exists(&old)?;
    if target.exists() {
        fs::rename(target, &old)?;
    }
    rename_into_place(&new, target)?;
    sync_dir(parent_of(target))?;
    remove_dir_if_exists(&old)?;
    Ok(())
}

/// The swap's last rename: the complete replacement `new` becomes `target`.
fn rename_into_place(new: &Path, target: &Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_RENAME_INTO_PLACE.with(|n| {
        let left = n.get();
        n.set(left.saturating_sub(1));
        left > 0
    }) {
        return Err(GStoreError::Io(io::Error::other(
            "injected failure of a swap's rename into place",
        )));
    }
    fs::rename(new, target)?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only failpoint: the next this-many renames of a swap's complete
    /// `<target>.new` into place fail (as `rename(2)` does on EIO), in an
    /// install and in a recovery alike.
    pub(crate) static FAIL_RENAME_INTO_PLACE: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
}

/// Remove directory `target` — and a staged `target.new` waiting to be swapped
/// in — such that a crash part-way never leaves a *partially deleted* directory
/// under a name that layout detection or [`recover_dir_swap`] acts on: each is
/// first renamed to `target.old` (ignored by detection, cleaned up by
/// `recover_dir_swap`), the rename is made durable, and only then is the tree
/// deleted. (`fs::remove_dir_all` unlinks one file at a time, so a crash in the
/// middle of it would leave e.g. a RocksDB directory missing its `CURRENT` file
/// — which a later open would mistake for a fresh, empty store.)
pub(crate) fn discard_dir(target: &Path) -> Result<()> {
    let old = with_suffix(target, OLD_SUFFIX);
    for victim in [with_suffix(target, NEW_SUFFIX), target.to_path_buf()] {
        if victim.is_dir() {
            remove_dir_if_exists(&old)?;
            fs::rename(&victim, &old)?;
            sync_dir(parent_of(target))?;
        }
    }
    #[cfg(test)]
    if CRASH_BEFORE_DISCARD_DELETE.with(|c| c.get()) {
        return Err(GStoreError::Database(
            "simulated crash while deleting a discarded directory".into(),
        ));
    }
    remove_dir_if_exists(&old)?;
    remove_dir_if_exists(&tmp_path(target))
}

/// Replace directory `dst` by a recursive copy of `src`: staged and swapped
/// in as a whole ([`StagedDir`] with the [`STORE_SWAP`] names). Any failure
/// is returned, also one after the swap's commit point (the copy may then
/// be in place, but is not known to be durable).
pub(crate) fn replace_dir_with_copy(src: &Path, dst: &Path) -> Result<()> {
    let staged = StagedDir::begin(dst, &STORE_SWAP)?;
    copy_dir_all(src, staged.path())?;
    staged.install().map_err(|e| {
        if e.committed {
            GStoreError::Database(format!(
                "the copy '{}' is in place, but its swap did not finish: {}",
                dst.display(),
                e.error
            ))
        } else {
            e.error
        }
    })
}

/// Complete or clean up an interrupted [`install_staged_dir`] of `target`: a
/// complete `target.new` is moved into place when `target` is missing; stale
/// `target.tmp` / `target.old` leftovers are removed.
pub(crate) fn recover_dir_swap(target: &Path) -> Result<()> {
    recover_dir_swap_as(target, &STORE_SWAP)
}

/// [`recover_dir_swap`] for a swap made with the sibling names of `swap`.
pub(crate) fn recover_dir_swap_as(target: &Path, swap: &DirSwap) -> Result<()> {
    let new = with_suffix(target, swap.new);
    if !target.exists() && new.is_dir() {
        rename_into_place(&new, target)?;
        sync_dir(parent_of(target))?;
    }
    remove_dir_if_exists(&new)?;
    remove_dir_if_exists(&with_suffix(target, swap.old))?;
    remove_dir_if_exists(&with_suffix(target, swap.tmp))?;
    Ok(())
}

/// Whether a (possibly not yet swapped-in) directory exists for `target`.
pub(crate) fn dir_or_staged_exists(target: &Path) -> bool {
    target.is_dir() || with_suffix(target, NEW_SUFFIX).is_dir()
}

// ---- stage + swap + fsync: the one way a directory is replaced or removed --
//
// Every path that replaces a directory as a whole — a RocksDB store a build,
// a save or a backup copy writes, a restored database — stages the new tree
// beside the old one and publishes it with one swap ([`StagedDir`] for the
// incremental RocksDB build and the backup copy; the bulk RocksDB install,
// the RocksDB save into another directory and the restore still stage
// themselves and call [`install_staged_dir_as`], see the review backlog);
// every path that removes a database directory moves it aside durably
// before it deletes it ([`discard_db_dir`]); a renamed database directory
// is made durable before anything records the new name
// ([`rename_dir_durably`]). (Review durability#15; the disk layout's two
// files are installed by the bulk build's install, `crate::bulk::commit`,
// which the incremental disk build uses too.)

/// `fsync` every regular file and every directory of the tree under `dir`
/// (each directory after its entries), so a tree staged for a swap is
/// durable before the rename that publishes it. Symbolic links are not
/// followed.
pub(crate) fn sync_tree(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    sync_dir(dir)
}

/// How a [`StagedDir::install`] failed: `committed` when the staged tree is
/// what an open of the target sees from now on (it is in place, or waits
/// as `<target>.new`, which every [`recover_dir_swap_as`] moves into place)
/// and only a step after that failed; otherwise the target is untouched.
#[derive(Debug)]
pub(crate) struct InstallError {
    pub(crate) committed: bool,
    pub(crate) error: GStoreError,
}

impl From<InstallError> for GStoreError {
    fn from(e: InstallError) -> GStoreError {
        e.error
    }
}

/// The replacement of directory `target`, staged beside it under the
/// staging name of a [`DirSwap`] and published as a whole by
/// [`install`](Self::install): the staged tree is fsynced, renamed to
/// `<target>.new`, the parent fsynced, the old `target` moved aside, the new
/// one renamed into place, the parent fsynced again and the old tree
/// deleted ([`install_staged_dir_as`]). Until the rename into place the old
/// directory is what every open sees; an interrupted swap is finished by
/// [`recover_dir_swap_as`]. Dropped without being installed, the staging
/// directory is removed and `target` is untouched.
#[derive(Debug)]
pub(crate) struct StagedDir {
    staged: PathBuf,
    target: PathBuf,
    swap: &'static DirSwap,
    /// Set once `install` took over the staged tree (it must then never be
    /// deleted by `drop`: it may be the committed copy).
    handed_over: bool,
}

impl StagedDir {
    /// Start staging a replacement of `target`: a fresh, empty staging
    /// directory (one a crash left there is removed first). The caller holds
    /// the lock of the directory `target` belongs to.
    pub(crate) fn begin(target: &Path, swap: &'static DirSwap) -> Result<StagedDir> {
        let staged = swap.staging(target);
        remove_dir_if_exists(&staged)?;
        fs::create_dir_all(&staged)?;
        Ok(StagedDir {
            staged,
            target: target.to_path_buf(),
            swap,
            handed_over: false,
        })
    }

    /// The staging directory to fill.
    pub(crate) fn path(&self) -> &Path {
        &self.staged
    }

    /// Publish the staged tree as `target` (see the type docs).
    pub(crate) fn install(mut self) -> std::result::Result<(), InstallError> {
        let not_committed = |error| InstallError {
            committed: false,
            error,
        };
        sync_tree(&self.staged).map_err(not_committed)?;
        self.handed_over = true;
        let Err(error) = install_staged_dir_as(&self.staged, &self.target, self.swap) else {
            return Ok(());
        };
        // Where did the swap stop? `staged` → `.new` → (`target` → `.old`)
        // → `target`.
        if self.staged.exists() {
            let _ = remove_dir_if_exists(&self.staged);
            return Err(not_committed(error));
        }
        let new = with_suffix(&self.target, self.swap.new);
        if new.is_dir() && self.target.is_dir() {
            // The old directory is still in place (the next recovery would
            // discard the new one): undo.
            let _ = remove_dir_if_exists(&new);
            return Err(not_committed(error));
        }
        // The new tree is `target`, or `.new` with no `target`, which every
        // recovery moves into place: the commit point is passed. Finish what
        // can be finished; the caller decides what the failure means.
        let _ = recover_dir_swap_as(&self.target, self.swap);
        Err(InstallError {
            committed: true,
            error,
        })
    }
}

impl Drop for StagedDir {
    fn drop(&mut self) {
        if !self.handed_over {
            let _ = remove_dir_if_exists(&self.staged);
        }
    }
}

/// The sibling names of a database directory a drop removes
/// ([`discard_db_dir`]): `x.db` is renamed to `x.db.drop-old` before it is
/// deleted. No database scan reads such a name (it does not end in `.db`),
/// and the next drop of the same name removes a leftover one.
pub(crate) const DROP_SWAP: DirSwap = DirSwap {
    tmp: ".drop-tmp",
    new: ".drop-new",
    old: ".drop-old",
};

/// Test hook of the drop crash tests: with this environment variable set
/// (non-empty), [`discard_db_dir`] parks its thread for good right after
/// the directory was durably renamed aside — the state a crash there
/// leaves — and says so on stderr, for the test to kill the process. Read
/// once per drop, never on a hot path.
const DROP_PARK_ENV: &str = "GSTORE_DROP_PARK";

/// Park the calling thread for good after telling stderr `what` (a crash
/// test's hook: the test kills the process at this point).
pub(crate) fn park_for_crash_test(env: &str, what: &str) -> ! {
    let mut err = io::stderr();
    // Written straight to the stream (not through the test harness's
    // output capture, which a parked thread would never flush).
    let _ = writeln!(err, "{env}: parked {what}");
    let _ = err.flush();
    loop {
        std::thread::park();
    }
}

/// Delete database directory `target` such that neither a crash nor an
/// error part-way leaves a partially deleted directory under its live name
/// (review durability#10): `target` is renamed to `<target>.drop-old`
/// ([`DROP_SWAP`]), the parent is fsynced — from here on the database is
/// gone — and only then is the tree deleted. A leftover `<target>.drop-old`
/// of an earlier interrupted drop is removed first, also when `target` no
/// longer exists. Returns whether `target` existed.
///
/// Once the rename is durable the drop has happened: a failure deleting the
/// moved-aside tree is logged (the leftover is ignored by every scan and
/// removed by the next drop of the name), not returned. The caller holds
/// `target`'s directory lock (it moves with the directory). A symbolic link
/// to a database directory is renamed and removed as a link, never
/// followed.
pub(crate) fn discard_db_dir(target: &Path) -> Result<bool> {
    let aside = DROP_SWAP.moved_aside(target);
    remove_drop_leftover(target)?;
    if !target.is_dir() {
        return Ok(false);
    }
    fs::rename(target, &aside)?;
    sync_dir(parent_of(target))?;
    if std::env::var_os(DROP_PARK_ENV).is_some_and(|v| !v.is_empty()) {
        park_for_crash_test(
            DROP_PARK_ENV,
            &format!("after renaming '{}' aside", target.display()),
        );
    }
    #[cfg(test)]
    if CRASH_BEFORE_DISCARD_DELETE.with(|c| c.get()) {
        return Err(GStoreError::Database(
            "simulated crash while deleting a discarded directory".into(),
        ));
    }
    if let Err(e) = remove_dir_if_exists(&aside) {
        crate::gs_warn!(
            "dropped '{}', but its files could not all be deleted ({e}); '{}' is left \
             behind (no database scan reads it) and the next drop of the name removes it",
            target.display(),
            aside.display()
        );
    }
    Ok(true)
}

/// Remove what an interrupted [`discard_db_dir`] of `target` left behind
/// (`<target>.drop-old`), leaving `target` itself alone. Returns whether
/// there was such a leftover.
pub(crate) fn remove_drop_leftover(target: &Path) -> Result<bool> {
    let aside = DROP_SWAP.moved_aside(target);
    let found = fs::symlink_metadata(&aside).is_ok();
    remove_dir_if_exists(&aside)?;
    Ok(found)
}

/// Rename directory `from` to `to` and make the rename durable (the parent
/// directory, or both parents, fsynced) before anything records the new
/// name (review durability#14).
pub(crate) fn rename_dir_durably(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    sync_dir(parent_of(to))?;
    if parent_of(from) != parent_of(to) {
        sync_dir(parent_of(from))?;
    }
    Ok(())
}

/// `path` with its longest existing leading part resolved
/// ([`fs::canonicalize`]: symbolic links, `.` and `..`) and the parts that
/// do not exist yet appended as they are (`..` taking away a component).
fn resolve_leniently(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut existing = absolute.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(mut out) = fs::canonicalize(&existing) {
            for part in rest.iter().rev() {
                match Path::new(part).components().next() {
                    Some(std::path::Component::ParentDir) => {
                        out.pop();
                    }
                    Some(std::path::Component::CurDir) | None => {}
                    _ => out.push(part),
                }
            }
            return out;
        }
        let Some(last) = existing.components().next_back() else {
            return absolute;
        };
        rest.push(last.as_os_str().to_owned());
        if !existing.pop() {
            return absolute;
        }
    }
}

/// Whether `path` is directory `dir` or lies inside it, after resolving
/// symbolic links and relative components; a `path` that does not exist
/// yet is resolved through its nearest existing ancestor (review
/// bulkcli#4: a backup into the database's own directory).
pub(crate) fn is_within(path: &Path, dir: &Path) -> bool {
    match fs::canonicalize(dir) {
        Ok(dir) => resolve_leniently(path).starts_with(dir),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temporary directory, removed when dropped (also when the
    /// test fails); unique per process and call.
    struct TmpDir(PathBuf);

    impl std::ops::Deref for TmpDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TmpDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_dir(tag: &str) -> TmpDir {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("gstore_fsutil_{tag}_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    /// A value whose serialization writes some bytes and then fails — the
    /// stand-in for a crash or I/O error in the middle of a save.
    struct FailsMidway;
    impl Serialize for FailsMidway {
        fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
            use serde::ser::{Error, SerializeSeq};
            let mut seq = s.serialize_seq(Some(3))?;
            seq.serialize_element(&1u64)?;
            seq.serialize_element(&2u64)?;
            Err(S::Error::custom("simulated failure mid-write"))
        }
    }

    #[test]
    fn atomic_write_failure_leaves_old_file_intact() {
        let dir = tmp_dir("atomic_fail");
        let target = dir.join("store.bin");
        write_bincode_atomic(&target, &vec![7u32, 8, 9]).unwrap();
        let before = fs::read(&target).unwrap();

        assert!(write_bincode_atomic(&target, &FailsMidway).is_err());
        assert_eq!(fs::read(&target).unwrap(), before, "old content survives");
        assert!(!tmp_path(&target).exists(), "the temp file is cleaned up");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_writer_abort_keeps_previous_set() {
        let dir = tmp_dir("snap_abort");
        let members: &[&str] = &["a.bin", "b.bin"];
        let mut w = SnapshotWriter::begin(&dir, members).unwrap();
        w.stage("a.bin", &1u32).unwrap();
        w.stage("b.bin", &1u32).unwrap();
        w.commit().unwrap();

        let mut w = SnapshotWriter::begin(&dir, members).unwrap();
        w.stage("a.bin", &2u32).unwrap();
        assert!(w.stage("b.bin", &FailsMidway).is_err());
        drop(w); // aborted: nothing published

        let a: u32 = bincode::deserialize(&fs::read(dir.join("a.bin")).unwrap()).unwrap();
        let b: u32 = bincode::deserialize(&fs::read(dir.join("b.bin")).unwrap()).unwrap();
        assert_eq!((a, b), (1, 1), "the previous set is untouched");
        assert!(!tmp_path(&dir.join("a.bin")).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_snapshot_commit_rolls_forward() {
        let dir = tmp_dir("snap_rollfwd");
        let members: &[&str] = &["a.bin", "b.bin"];
        let mut w = SnapshotWriter::begin(&dir, members).unwrap();
        w.stage("a.bin", &1u32).unwrap();
        w.stage("b.bin", &1u32).unwrap();
        w.commit().unwrap();

        // Simulate a crash after the commit record was written and one member
        // was renamed, but before the other was.
        write_tmp(&dir.join("a.bin"), |w| {
            Ok(bincode::serialize_into(w, &2u32)?)
        })
        .unwrap();
        write_tmp(&dir.join("b.bin"), |w| {
            Ok(bincode::serialize_into(w, &2u32)?)
        })
        .unwrap();
        atomic_write(&dir.join(SNAPSHOT_COMMIT_FILE), b"a.bin\nb.bin").unwrap();
        fs::rename(tmp_path(&dir.join("a.bin")), dir.join("a.bin")).unwrap();

        recover_snapshot(&dir, members).unwrap();
        let a: u32 = bincode::deserialize(&fs::read(dir.join("a.bin")).unwrap()).unwrap();
        let b: u32 = bincode::deserialize(&fs::read(dir.join("b.bin")).unwrap()).unwrap();
        assert_eq!((a, b), (2, 2), "the committed set is completed");
        assert!(!dir.join(SNAPSHOT_COMMIT_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// An atomic replacement keeps the replaced file's permissions (renaming
    /// a default-mode temp file over it would silently widen a restricted
    /// one); a private file is created owner-only.
    #[cfg(unix)]
    #[test]
    fn atomic_replacement_keeps_the_targets_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perms");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let f = dir.join("f");
        atomic_write(&f, b"1").unwrap();
        for m in [0o600, 0o640, 0o664] {
            fs::set_permissions(&f, fs::Permissions::from_mode(m)).unwrap();
            atomic_write(&f, b"2").unwrap();
            assert_eq!(mode(&f), m);
            // Also when a stale temp file with a wider mode is lying around.
            fs::write(tmp_path(&f), b"stale").unwrap();
            fs::set_permissions(tmp_path(&f), fs::Permissions::from_mode(0o666)).unwrap();
            atomic_write(&f, b"3").unwrap();
            assert_eq!(mode(&f), m);
            assert_eq!(fs::read(&f).unwrap(), b"3");
        }
        let secret = dir.join("secret");
        assert!(atomic_replace(&secret, b"s", true).unwrap().is_none());
        assert_eq!(mode(&secret), 0o600);
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(atomic_replace(&secret, b"t", true).unwrap().is_none());
        assert_eq!(mode(&secret), 0o640, "an existing file keeps its mode");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_prepare_is_discarded() {
        let dir = tmp_dir("snap_discard");
        let members: &[&str] = &["a.bin"];
        write_bincode_atomic(&dir.join("a.bin"), &1u32).unwrap();
        // A temp file without a commit record: the prepare never finished.
        write_tmp(&dir.join("a.bin"), |w| {
            Ok(bincode::serialize_into(w, &2u32)?)
        })
        .unwrap();
        recover_snapshot(&dir, members).unwrap();
        let a: u32 = bincode::deserialize(&fs::read(dir.join("a.bin")).unwrap()).unwrap();
        assert_eq!(a, 1);
        assert!(!tmp_path(&dir.join("a.bin")).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_directory_swap_and_recovery() {
        let dir = tmp_dir("dir_swap");
        let src = dir.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("f"), b"new").unwrap();
        fs::write(src.join("sub/g"), b"deep").unwrap();
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("f"), b"old").unwrap();

        replace_dir_with_copy(&src, &target).unwrap();
        assert_eq!(fs::read(target.join("f")).unwrap(), b"new");
        assert_eq!(fs::read(target.join("sub/g")).unwrap(), b"deep");

        // Interrupted between "target → target.old" and "target.new → target".
        fs::rename(&target, with_suffix(&target, OLD_SUFFIX)).unwrap();
        copy_dir_all(&src, &with_suffix(&target, NEW_SUFFIX)).unwrap();
        assert!(dir_or_staged_exists(&target));
        recover_dir_swap(&target).unwrap();
        assert_eq!(fs::read(target.join("f")).unwrap(), b"new");
        assert!(!with_suffix(&target, OLD_SUFFIX).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_dir_removes_target_and_staged_copy_via_ignored_name() {
        let dir = tmp_dir("discard");
        let target = dir.join("rocksdb");
        fs::create_dir_all(target.join("sub")).unwrap();
        fs::write(target.join("CURRENT"), b"x").unwrap();
        fs::create_dir_all(with_suffix(&target, NEW_SUFFIX)).unwrap();
        // A leftover of an earlier interrupted discard is ignored by detection.
        fs::create_dir_all(with_suffix(&target, OLD_SUFFIX)).unwrap();
        discard_dir(&target).unwrap();
        for p in [
            target.clone(),
            with_suffix(&target, NEW_SUFFIX),
            with_suffix(&target, OLD_SUFFIX),
            tmp_path(&target),
        ] {
            assert!(!p.exists(), "{} must be gone", p.display());
        }
        // A crash right after the rename leaves only `.old`, which layout
        // detection ignores and recovery deletes.
        fs::create_dir_all(with_suffix(&target, OLD_SUFFIX)).unwrap();
        assert!(!dir_or_staged_exists(&target));
        recover_dir_swap(&target).unwrap();
        assert!(!with_suffix(&target, OLD_SUFFIX).exists());
        discard_dir(&target).unwrap(); // nothing there: a no-op
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_swap_names_are_distinct_from_store_swap_names() {
        let dir = tmp_dir("restore_swap");
        let target = dir.join("x.db");
        // A user's own `x.db.old` / `x.db.new` / `x.db.tmp` are never touched.
        for s in [OLD_SUFFIX, NEW_SUFFIX, TMP_SUFFIX] {
            fs::create_dir_all(with_suffix(&target, s)).unwrap();
        }
        let staged = RESTORE_SWAP.staging(&target);
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("f"), b"restored").unwrap();
        install_staged_dir_as(&staged, &target, &RESTORE_SWAP).unwrap();
        recover_dir_swap_as(&target, &RESTORE_SWAP).unwrap();
        assert_eq!(fs::read(target.join("f")).unwrap(), b"restored");
        for s in [OLD_SUFFIX, NEW_SUFFIX, TMP_SUFFIX] {
            assert!(with_suffix(&target, s).is_dir(), "{s} sibling must survive");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staged_dir_is_installed_whole_or_discarded() {
        let dir = tmp_dir("staged_dir");
        let target = dir.join("rocksdb");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("CURRENT"), b"old").unwrap();
        // Dropped without installing: the staging goes, the target stays.
        let staged = StagedDir::begin(&target, &STORE_SWAP).unwrap();
        fs::create_dir_all(staged.path().join("sub")).unwrap();
        fs::write(staged.path().join("CURRENT"), b"new").unwrap();
        assert_eq!(staged.path(), tmp_path(&target));
        drop(staged);
        assert!(!tmp_path(&target).exists());
        assert_eq!(fs::read(target.join("CURRENT")).unwrap(), b"old");
        // A stale staging directory (a crash) is replaced by a fresh one.
        fs::create_dir_all(tmp_path(&target).join("junk")).unwrap();
        let staged = StagedDir::begin(&target, &STORE_SWAP).unwrap();
        assert!(!staged.path().join("junk").exists());
        fs::create_dir_all(staged.path().join("sub")).unwrap();
        fs::write(staged.path().join("sub/f"), b"deep").unwrap();
        fs::write(staged.path().join("CURRENT"), b"new").unwrap();
        staged.install().unwrap();
        assert_eq!(fs::read(target.join("CURRENT")).unwrap(), b"new");
        assert_eq!(fs::read(target.join("sub/f")).unwrap(), b"deep");
        assert!(!STORE_SWAP.in_progress(&target), "no sibling is left");
        // An install that cannot even begin its swap (here: no parent for
        // the target) is not committed and leaves nothing behind.
        let orphan = dir.join("gone/rocksdb");
        let staged = StagedDir::begin(&orphan, &STORE_SWAP).unwrap();
        fs::remove_dir_all(dir.join("gone")).unwrap();
        let e = staged.install().unwrap_err();
        assert!(!e.committed, "{e:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dropped_database_directory_is_moved_aside_before_it_is_deleted() {
        let dir = tmp_dir("discard_db");
        let target = dir.join("x.db");
        fs::create_dir_all(target.join("rocksdb")).unwrap();
        fs::write(target.join("meta.bin"), b"m").unwrap();
        let aside = DROP_SWAP.moved_aside(&target);
        assert!(
            !aside.to_string_lossy().ends_with(".db"),
            "scanners skip it"
        );
        // "Crash" right after the durable rename: only the aside name is
        // left, never a half-deleted `x.db`.
        CRASH_BEFORE_DISCARD_DELETE.with(|c| c.set(true));
        let r = discard_db_dir(&target);
        CRASH_BEFORE_DISCARD_DELETE.with(|c| c.set(false));
        assert!(r.is_err());
        assert!(!target.exists());
        assert!(aside.join("meta.bin").is_file(), "moved whole");
        // The next drop of the name removes the leftover, also with no
        // database there.
        assert!(!discard_db_dir(&target).unwrap());
        assert!(!aside.exists());
        fs::create_dir_all(&aside).unwrap();
        assert!(remove_drop_leftover(&target).unwrap());
        assert!(!remove_drop_leftover(&target).unwrap());
        // A plain drop.
        fs::create_dir_all(target.join("sub")).unwrap();
        assert!(discard_db_dir(&target).unwrap());
        assert!(!target.exists() && !aside.exists());
        // A durable rename.
        fs::create_dir_all(dir.join("a.db")).unwrap();
        rename_dir_durably(&dir.join("a.db"), &dir.join("b.db")).unwrap();
        assert!(dir.join("b.db").is_dir() && !dir.join("a.db").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_within_resolves_links_dots_and_missing_parts() {
        let dir = tmp_dir("is_within");
        let db = dir.join("x.db");
        fs::create_dir_all(db.join("sub")).unwrap();
        assert!(is_within(&db, &db));
        assert!(is_within(&db.join("sub"), &db));
        assert!(is_within(&db.join("missing/deeper"), &db));
        assert!(is_within(&db.join("sub/../other"), &db));
        assert!(!is_within(&db.join("../x.db2"), &db));
        assert!(!is_within(&dir.join("bk"), &db));
        assert!(!is_within(&dir, &db));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&db, dir.join("link")).unwrap();
            assert!(is_within(&dir.join("link"), &db));
            assert!(is_within(&dir.join("link/new"), &db));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_bincode_matches_serialize_and_rejects_huge_length_prefixes() {
        let dir = tmp_dir("bincode_limit");
        let ok = dir.join("ok.bin");
        let value = (7u32, String::from("seven"), vec![1u64, 2, 3]);
        fs::write(&ok, bincode::serialize(&value).unwrap()).unwrap();
        let back: (u32, String, Vec<u64>) = read_bincode(&ok).unwrap();
        assert_eq!(back, value);

        // A string whose u64 length prefix claims ~7 EB: must be an error, not
        // an allocation failure that aborts the process.
        let bad = dir.join("bad.bin");
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        fs::write(&bad, &bytes).unwrap();
        assert!(read_bincode::<String>(&bad).is_err());
        fs::write(&bad, b"definitely not bincode").unwrap();
        assert!(read_bincode::<(String, u64)>(&bad).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Round-2 review (durability): a RocksDB build whose swap failed past
    /// its commit point with the new store still `rocksdb.new` (the rename
    /// into place failed in the install, in its recovery and in the
    /// build's retry) is refused — it neither opens `rocksdb/` (which would
    /// create an empty store there, which the next open's recovery keeps
    /// while it deletes `rocksdb.new`, the real build) nor removes the other
    /// layouts — and the next open moves the build into place.
    #[cfg(feature = "rocksdb")]
    #[test]
    fn a_rocksdb_build_left_as_new_is_refused_and_the_next_open_finishes_it() {
        let dir = tmp_dir("rocks_left_new");
        let db_dir = dir.join("x.db");
        FAIL_RENAME_INTO_PLACE.with(|n| n.set(3));
        let built = crate::Database::build_rocksdb_from_str(
            "x",
            &db_dir,
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
             <http://ex/b> <http://ex/p> <http://ex/c> .\n",
        );
        FAIL_RENAME_INTO_PLACE.with(|n| n.set(0));
        let Err(err) = built else {
            panic!("a build whose store is not in place is refused");
        };
        assert!(
            err.to_string().contains("could not be moved into place"),
            "{err}"
        );
        assert!(!db_dir.join("rocksdb").exists(), "no store was created");
        assert!(db_dir.join("rocksdb.new").is_dir(), "the build is kept");
        let db = crate::Database::open(&db_dir).unwrap();
        assert_eq!(db.stats().triple_num, 2, "the next open serves the build");
    }
}
