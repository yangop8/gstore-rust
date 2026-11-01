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
    let tmp = write_tmp_mode(path, new_mode, fill)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    sync_dir(parent_of(path))
}

/// Atomically replace `path` with `bytes`.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_with(path, |w| {
        w.write_all(bytes)?;
        Ok(())
    })
}

/// Atomically replace `path` with `bytes`, for a file holding secrets (e.g.
/// password hashes): created owner-only (`0600`) when it does not exist yet;
/// an existing file keeps its permissions.
pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_mode(path, Some(0o600), |w| {
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
    /// Start a snapshot write into `dir` (created if needed), first finishing or
    /// discarding any interrupted earlier write.
    pub(crate) fn begin(dir: &'a Path, members: &'a [&'a str]) -> Result<SnapshotWriter<'a>> {
        fs::create_dir_all(dir)?;
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

/// Recursively copy directory `src` into the (new) directory `dst`, syncing
/// every copied file.
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
            File::open(&to)?.sync_all()?;
        }
    }
    sync_dir(dst)
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
pub(crate) struct DirSwap {
    tmp: &'static str,
    new: &'static str,
    old: &'static str,
}

/// The swap names of a store directory inside a database directory
/// (`rocksdb/` → `rocksdb.tmp` / `rocksdb.new` / `rocksdb.old`).
const STORE_SWAP: DirSwap = DirSwap {
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
}

/// Install a fully-populated staging directory `staged` as `target`, replacing
/// any existing `target`: `staged` → `target.new` → (`target` → `target.old`)
/// → `target`. Interrupted swaps are completed by [`recover_dir_swap`].
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
    fs::rename(&new, target)?;
    sync_dir(parent_of(target))?;
    remove_dir_if_exists(&old)?;
    Ok(())
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

/// Replace directory `dst` by a recursive copy of `src` (staged, then swapped
/// in with [`install_staged_dir`]).
pub(crate) fn replace_dir_with_copy(src: &Path, dst: &Path) -> Result<()> {
    let staged = tmp_path(dst);
    remove_dir_if_exists(&staged)?;
    copy_dir_all(src, &staged)?;
    install_staged_dir(&staged, dst)
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
        fs::rename(&new, target)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gstore_fsutil_{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
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
        atomic_write_private(&secret, b"s").unwrap();
        assert_eq!(mode(&secret), 0o600);
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_write_private(&secret, b"t").unwrap();
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
}
