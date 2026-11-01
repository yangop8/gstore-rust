//! Database persistence: the three on-disk layouts, the auto-detecting
//! [`Database::open`], write-through to persistent backends, and file-level
//! backup / restore.
//!
//! ## Layouts ([`StorageKind`])
//!
//! | kind | marker in the directory | what is stored where |
//! |------|-------------------------|----------------------|
//! | [`StorageKind::RocksDb`] | `rocksdb/` | everything — triples, statistics, the dictionary (`dict` CF), named graphs (`gspo` CF) and the name (`meta` CF) — inside one RocksDB instance |
//! | [`StorageKind::Disk`] | `kvstore.kv` | triples, dictionary and named graphs in the paged B+ tree file (WAL-protected); `vstree.kv` beside it is the out-of-core VS-tree |
//! | [`StorageKind::Snapshot`] | `meta.bin` | bincode snapshot: `dict.bin`, `store.bin`, `named.bin`, `vstree.bin`, `meta.bin` |
//!
//! Detection precedence is exactly the table order: `rocksdb/` wins over
//! `kvstore.kv`, which wins over a snapshot. Saving a database into a directory
//! makes that directory hold *only* the saved layout (artifacts of another
//! layout are removed), so a directory is never ambiguous after a save.
//!
//! ### Legacy shadow snapshots next to `kvstore.kv`
//!
//! Builds before this module existed loaded an on-disk database into memory and
//! `save()`d it as a bincode snapshot *next to* the untouched `kvstore.kv`, while
//! every later open re-read `kvstore.kv` — so updates looked applied but were
//! never visible again. `kvstore.kv` stays authoritative. When such a "shadow"
//! `store.bin` is found beside it and is at least as new as `kvstore.kv`, its
//! content is the last state the user saw, so it is **reconciled into
//! `kvstore.kv`** (the triple/quad difference is applied) on open; an older
//! shadow is ignored. Either way the shadow files are then renamed to
//! `<file>.legacy` so the decision is made once and the data is never deleted.
//!
//! ## Write-through (disk) and commit points
//!
//! An on-disk database keeps an in-memory working set (dictionary, default and
//! named graphs, as a [`Backend::Memory`]) for query evaluation — streaming
//! evaluation straight off the B+ trees is a later step — and writes every
//! mutation through to its [`DiskStore`] by *dictionary key*, so the working
//! set's ids never have to line up with the store's. Inside a transaction the
//! mutations are queued and only reach the store at [`Database::commit`] (a
//! rollback discards them). The store's dirty pages are committed to
//! `kvstore.kv` (through its WAL) at every **commit point**: the end of each
//! auto-commit SPARQL UPDATE, `commit()`, `save()`/[`checkpoint`](Database::checkpoint),
//! and — best effort — when the database is dropped. If a write-through ever
//! fails, the working set and the store may have diverged, so every later
//! commit point reports the failure instead of claiming durability. (Each pager
//! commit is atomic; a transaction whose dirty pages overflow the page cache is
//! committed in several such batches, so it is durable but not crash-atomic as
//! a whole — see `DiskStore::insert_ids`.)
//!
//! A RocksDB database evaluates directly over RocksDB. New dictionary terms are
//! written to the `dict` CF before the first triple that uses them (also those
//! a reasoner interns), named-graph quads go to the `gspo` CF *before* the
//! in-memory named graph changes, and the RocksDB WAL is fsynced at each commit
//! point. Write errors never panic: they are recorded and returned by the next
//! commit point, and a write that failed is not applied in memory either (a
//! failed term write forgets the freshly interned terms again), so a retry
//! persists it. Inside a transaction every RocksDB write carries an undo record
//! in the same atomic batch (see [`crate::backend::rocks`]): `commit()` deletes
//! the journal, `rollback()` replays it, and an open after a crash replays it —
//! so no uncommitted write survives a crash.
//!
//! ## Directory replacement
//!
//! A `rocksdb/` directory that a save or restore replaces is renamed out of the
//! way before it is deleted, so a crash mid-deletion never leaves a partial
//! store that layout detection would prefer over the newly committed layout.
//! [`Database::restore_into`] stages and validates the copy beside the target
//! and swaps it in as a whole directory.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use super::fsutil::{self, SnapshotWriter};
use super::{
    build_vstree, vstree_entries, Database, Meta, BACKUP_OFFSET_FILE, DICT_FILE, DISK_CACHE_PAGES,
    KV_FILE, META_FILE, NAMED_FILE, REDO_LOG_FILE, ROCKS_SUBDIR, STORE_FILE, UPDATE_LOG_FILE,
    VSTREE_FILE, VSTREE_KV_FILE,
};
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::kvstore::DiskStore;
use crate::model::id::is_literal_id;
use crate::model::{IdTriple, Term, Triple};
use crate::parser::import;
use crate::signature::VsTree;
use crate::store::{Backend, MutableStore, TripleSource, TripleStore};

/// The members of a bincode snapshot, in the order they are staged (`meta.bin`
/// last: it is the file layout detection keys on).
const SNAPSHOT_FILES: &[&str] = &[DICT_FILE, STORE_FILE, NAMED_FILE, VSTREE_FILE, META_FILE];

/// Suffix legacy shadow-snapshot files are renamed to (see the module docs).
const LEGACY_SUFFIX: &str = ".legacy";

/// Which persistent layout a database directory holds.
///
/// | kind | marker in the directory | what is stored where |
/// |------|-------------------------|----------------------|
/// | [`RocksDb`](Self::RocksDb) | `rocksdb/` | everything (triples, statistics, dictionary, named graphs, name) in one RocksDB instance |
/// | [`Disk`](Self::Disk) | `kvstore.kv` | triples, dictionary and named graphs in the WAL-protected B+ tree file; `vstree.kv` is the out-of-core VS-tree |
/// | [`Snapshot`](Self::Snapshot) | `meta.bin` | bincode snapshot: `dict.bin`, `store.bin`, `named.bin`, `vstree.bin`, `meta.bin` |
///
/// [`Database::open`] detects the layout with exactly this precedence
/// (`rocksdb/` over `kvstore.kv` over a snapshot). Saving a database into a
/// directory leaves only the saved layout there.
///
/// **Commit points.** On-disk and RocksDB databases persist every committed
/// update to their own directory: each auto-commit SPARQL UPDATE,
/// [`Database::commit`], [`Database::sync`], [`Database::save`] /
/// [`Database::checkpoint`], and (best effort) dropping the database make the
/// updates applied so far durable; a persistence failure is reported by the
/// next commit point. An on-disk database still evaluates queries over an
/// in-memory working set loaded at open; inside a transaction its writes reach
/// the store only at `commit`. A RocksDB transaction writes through but is
/// journaled, so a crash before `commit` undoes it on the next open.
///
/// **Legacy shadow snapshots.** Builds before this layout handling `save()`d an
/// updated on-disk database as a bincode snapshot next to the untouched
/// `kvstore.kv`. `kvstore.kv` stays authoritative; a shadow `store.bin` at least
/// as new as it is reconciled into it on open (it holds the updates that build
/// reported as applied), an older one is ignored, and either way the shadow
/// files are renamed to `*.legacy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// A bincode snapshot (`meta.bin`, `dict.bin`, `store.bin`, …), loaded fully
    /// into memory and persisted only by [`Database::save`].
    Snapshot,
    /// The on-disk B+ tree KVstore (`kvstore.kv`), updated by write-through.
    Disk,
    /// A RocksDB store (`rocksdb/`); opening one needs the `rocksdb` feature.
    RocksDb,
}

impl fmt::Display for StorageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StorageKind::Snapshot => "in-memory snapshot",
            StorageKind::Disk => "on-disk B+ tree",
            StorageKind::RocksDb => "RocksDB",
        })
    }
}

/// `<path>.wal`, the pager write-ahead log beside a paged file.
fn wal_of(path: &Path) -> PathBuf {
    fsutil::with_suffix(path, ".wal")
}

// ---- disk write-through sink --------------------------------------------------

/// One mutation, in dictionary-key form, destined for the on-disk store.
#[derive(Debug, Clone)]
pub(super) struct DiskOp {
    insert: bool,
    /// Named graph IRI key (`None` ⇒ default graph).
    graph: Option<String>,
    s: String,
    p: String,
    o: String,
    o_is_literal: bool,
}

impl DiskOp {
    /// Resolve an id-level mutation against the working-set dictionary.
    fn resolve(
        dict: &Dictionary,
        insert: bool,
        graph: Option<u32>,
        id: IdTriple,
    ) -> Option<DiskOp> {
        let graph = match graph {
            Some(g) => Some(dict.id_to_string(g)?.to_owned()),
            None => None,
        };
        Some(DiskOp {
            insert,
            graph,
            s: dict.id_to_string(id.sub)?.to_owned(),
            p: dict.predicate_to_string(id.pred)?.to_owned(),
            o: dict.id_to_string(id.obj)?.to_owned(),
            o_is_literal: is_literal_id(id.obj),
        })
    }
}

/// Write-through state of an on-disk (`kvstore.kv`) database.
pub(super) struct DiskSink {
    store: DiskStore,
    /// Mutations of the open transaction, applied at commit.
    pending: Vec<DiskOp>,
    /// Set once a write-through failed: the store may no longer match the
    /// working set, so every later commit point fails.
    broken: Option<String>,
    /// Whether mutations reached `store` since its last flush.
    dirty: bool,
}

impl fmt::Debug for DiskSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskSink")
            .field("triples", &self.store.triple_count())
            .field("quads", &self.store.quad_count())
            .field("pending", &self.pending.len())
            .field("dirty", &self.dirty)
            .field("broken", &self.broken)
            .finish()
    }
}

impl DiskSink {
    fn new(store: DiskStore) -> DiskSink {
        DiskSink {
            store,
            pending: Vec::new(),
            broken: None,
            dirty: false,
        }
    }

    fn fail(&mut self, msg: impl Into<String>) {
        if self.broken.is_none() {
            self.broken = Some(msg.into());
        }
    }

    fn apply(&mut self, op: &DiskOp) {
        if self.broken.is_some() {
            return;
        }
        self.dirty = true;
        let (s, p, o, lit) = (op.s.as_str(), op.p.as_str(), op.o.as_str(), op.o_is_literal);
        let res = match (&op.graph, op.insert) {
            (None, true) => self.store.insert_keys(s, p, o, lit),
            (None, false) => self.store.delete_keys(s, p, o, lit),
            (Some(g), true) => self.store.insert_quad_keys(g, s, p, o, lit),
            (Some(g), false) => self.store.delete_quad_keys(g, s, p, o, lit),
        };
        if let Err(e) = res {
            self.fail(e.to_string());
        }
    }

    /// Apply `op` now (auto-commit) or queue it for the transaction's commit.
    fn record(&mut self, op: DiskOp, in_txn: bool) {
        if in_txn {
            self.pending.push(op);
        } else {
            self.apply(&op);
        }
    }

    fn commit_pending(&mut self) {
        for op in std::mem::take(&mut self.pending) {
            self.apply(&op);
        }
    }

    fn discard_pending(&mut self) {
        self.pending.clear();
    }

    /// Commit the store's dirty pages to `kvstore.kv` (WAL-protected).
    fn flush(&mut self) -> Result<()> {
        if let Some(e) = &self.broken {
            return Err(GStoreError::Database(format!(
                "an update could not be written to the on-disk store ({e}); its contents may \
                 no longer match this session, so nothing more is committed — reopen the database"
            )));
        }
        if self.dirty {
            self.store.flush()?;
            self.dirty = false;
        }
        Ok(())
    }
}

impl Drop for DiskSink {
    /// Best-effort: commit mutations applied since the last commit point, so a
    /// tool that inserts through the API and exits without `save()` loses
    /// nothing. Queued (uncommitted-transaction) mutations are dropped.
    fn drop(&mut self) {
        if self.broken.is_none() && self.dirty {
            let _ = self.store.flush();
        }
    }
}

/// `(s, p, o, o_is_literal)` in dictionary-key form.
type KeyTriple = (String, String, String, bool);
/// `(graph, s, p, o, o_is_literal)` in dictionary-key form.
type KeyQuad = (String, String, String, String, bool);

fn key_triple(dict: &Dictionary, t: IdTriple) -> Option<KeyTriple> {
    Some((
        dict.id_to_string(t.sub)?.to_owned(),
        dict.predicate_to_string(t.pred)?.to_owned(),
        dict.id_to_string(t.obj)?.to_owned(),
        is_literal_id(t.obj),
    ))
}

/// Remove the artifacts of every layout other than `keep` from `dir`.
fn remove_other_layouts(dir: &Path, keep: StorageKind) -> Result<()> {
    if keep != StorageKind::Snapshot {
        for name in SNAPSHOT_FILES {
            let path = dir.join(name);
            fsutil::remove_file_if_exists(&path)?;
            fsutil::remove_file_if_exists(&fsutil::tmp_path(&path))?;
        }
        fsutil::remove_file_if_exists(&dir.join(fsutil::SNAPSHOT_COMMIT_FILE))?;
    }
    if keep != StorageKind::Disk {
        let kv = dir.join(KV_FILE);
        if kv.exists() || wal_of(&kv).exists() {
            // The out-of-core VS-tree file belongs to the disk layout; leave a
            // `vstree.kv` alone otherwise (it may be a memory database's
            // opted-in streaming index).
            for path in [kv, dir.join(VSTREE_KV_FILE)] {
                fsutil::remove_file_if_exists(&wal_of(&path))?;
                fsutil::remove_file_if_exists(&path)?;
            }
        }
    }
    if keep != StorageKind::RocksDb {
        // Crash-safe: `rocksdb/` takes precedence in layout detection, so it is
        // renamed out of the way (durably) before it is deleted — a half-deleted
        // store must never shadow the layout that was just committed.
        fsutil::discard_dir(&dir.join(ROCKS_SUBDIR))?;
    }
    fsutil::sync_dir(dir)
}

/// Build the out-of-core VS-tree file at `path` from `entries`, atomically:
/// built under a temp name, then renamed over `path` (after removing any stale
/// WAL that would otherwise replay into the new file), then reopened.
fn write_vstree_kv(
    path: &Path,
    entries: Vec<(u32, crate::signature::Signature)>,
) -> Result<VsTree> {
    let tmp = fsutil::tmp_path(path);
    fsutil::remove_file_if_exists(&wal_of(&tmp))?;
    fsutil::remove_file_if_exists(&tmp)?;
    drop(VsTree::build_disk(&tmp, entries)?);
    fsutil::remove_file_if_exists(&wal_of(path))?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fsutil::sync_dir(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })?;
    }
    VsTree::open_disk(path)
}

/// The modification time of `path`, if available.
fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Database {
    // ---- layout detection + the auto-detecting opener ----------------------

    /// Which persistent layout directory `dir` holds, if any (precedence:
    /// RocksDB, then on-disk KVstore, then bincode snapshot — see the
    /// [`StorageKind`]). Works with or without the `rocksdb` feature.
    pub fn storage_kind<P: AsRef<Path>>(dir: P) -> Option<StorageKind> {
        let dir = dir.as_ref();
        if fsutil::dir_or_staged_exists(&dir.join(ROCKS_SUBDIR)) {
            Some(StorageKind::RocksDb)
        } else if dir.join(KV_FILE).is_file() {
            Some(StorageKind::Disk)
        } else if dir.join(META_FILE).is_file() || dir.join(fsutil::SNAPSHOT_COMMIT_FILE).is_file()
        {
            Some(StorageKind::Snapshot)
        } else {
            None
        }
    }

    /// Open the database in directory `dir`, whatever its layout: a bincode
    /// snapshot, an on-disk KVstore (`kvstore.kv`), or a RocksDB store
    /// (`rocksdb/`, which needs the `rocksdb` feature — without it this returns
    /// a clear error rather than misreading the directory). The returned
    /// database remembers `dir` as its [`home`](Self::home): for the persistent
    /// layouts every committed update is written back there.
    pub fn open<P: AsRef<Path>>(dir: P) -> Result<Database> {
        let dir = dir.as_ref();
        Database::recover_restore(dir)?;
        if !dir.is_dir() {
            return Err(GStoreError::Database(format!(
                "database directory '{}' does not exist",
                dir.display()
            )));
        }
        fsutil::recover_dir_swap(&dir.join(ROCKS_SUBDIR))?;
        // The snapshot and on-disk layouts are locked against other processes
        // before anything is read (see `dirlock`); RocksDB locks itself.
        let lock = match Database::storage_kind(dir) {
            Some(StorageKind::RocksDb) | None => None,
            Some(_) => Some(super::dirlock::DirLock::acquire(dir)?),
        };
        let mut db = match Database::storage_kind(dir) {
            Some(StorageKind::RocksDb) => Database::open_rocks_layout(dir),
            Some(StorageKind::Disk) => Database::load_disk(dir),
            Some(StorageKind::Snapshot) => Database::load_snapshot(dir),
            None => Err(GStoreError::Database(format!(
                "'{}' does not contain a gStore database (no meta.bin snapshot, \
                 kvstore.kv or rocksdb/ store)",
                dir.display()
            ))),
        }?;
        db.dir_lock = lock;
        Ok(db)
    }

    /// Hold the inter-process lock of directory `dir` (see
    /// [`dirlock`](super::dirlock)) for as long as this database lives — for a
    /// database built in memory and saved into `dir` that is now served from
    /// there ([`open`](Self::open) takes the lock itself). A no-op when the
    /// database already holds `dir`'s lock; a lock on another directory is
    /// released. Fails when another process holds `dir`.
    pub fn hold_dir_lock<P: AsRef<Path>>(&mut self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        if let (Some(held), Ok(canonical)) = (&self.dir_lock, fs::canonicalize(dir)) {
            if held.dir() == canonical {
                return Ok(());
            }
        }
        self.dir_lock = Some(super::dirlock::DirLock::acquire(dir)?);
        Ok(())
    }

    /// Whether this database holds a directory's inter-process lock (opened
    /// with [`open`](Self::open), or after [`hold_dir_lock`](Self::hold_dir_lock)).
    pub fn holds_dir_lock(&self) -> bool {
        self.dir_lock.is_some()
    }

    /// Load a database from directory `dir`. Equivalent to [`open`](Self::open):
    /// any layout is accepted.
    pub fn load<P: AsRef<Path>>(dir: P) -> Result<Database> {
        Database::open(dir)
    }

    #[cfg(feature = "rocksdb")]
    fn open_rocks_layout(dir: &Path) -> Result<Database> {
        Database::open_rocksdb(dir)
    }

    #[cfg(not(feature = "rocksdb"))]
    fn open_rocks_layout(dir: &Path) -> Result<Database> {
        Err(GStoreError::Database(format!(
            "'{}' is a RocksDB-backed database, but this build of gstore was compiled \
             without the `rocksdb` feature; rebuild with `--features rocksdb` to open it",
            dir.display()
        )))
    }

    /// The directory this database was opened from or built into, if any. For
    /// the persistent layouts (on-disk, RocksDB) this is where every committed
    /// update is written; [`checkpoint`](Self::checkpoint) saves there.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The layout this live database persists as.
    pub fn storage(&self) -> StorageKind {
        if self.disk.borrow().is_some() {
            return StorageKind::Disk;
        }
        #[cfg(feature = "rocksdb")]
        if self.store.as_rocks().is_some() {
            return StorageKind::RocksDb;
        }
        StorageKind::Snapshot
    }

    /// Persist the database to its own [`home`](Self::home) directory: commits
    /// the write-through of an on-disk / RocksDB database, or re-saves the
    /// snapshot of an in-memory one. Errors if the database has no home (it was
    /// built in memory and never opened from a directory) or if an earlier
    /// update could not be persisted.
    pub fn checkpoint(&self) -> Result<()> {
        let Some(home) = self.home.clone() else {
            return Err(GStoreError::Database(format!(
                "database '{}' has no home directory to checkpoint to; use save(dir)",
                self.name
            )));
        };
        self.save(home)
    }

    /// A commit point without a full save: make every mutation applied so far
    /// durable on a persistent layout (commit the on-disk store's dirty pages
    /// through its WAL; fsync RocksDB's WAL) and report any persistence failure
    /// recorded since the previous commit point. Auto-commit SPARQL UPDATEs and
    /// [`commit`](Self::commit) already do this; call it after mutating through
    /// the direct API ([`insert_triple`](Self::insert_triple), …). A no-op for an
    /// in-memory database — persist that with [`save`](Self::save).
    pub fn sync(&self) -> Result<()> {
        self.commit_point()
    }

    /// Detach this database from its on-disk storage, returning a purely
    /// in-memory database with the same content (pending write-through is
    /// committed first). Later updates and saves no longer touch the original
    /// directory. The reasoning rules it kept there (and their ledgers) are
    /// read into memory with it (unless they live in a directory set with
    /// [`set_reason_db_dir`](Self::set_reason_db_dir), which is kept), so
    /// they stay manageable and [`backup`](Self::backup) carries them.
    pub fn into_memory(mut self) -> Result<Database> {
        self.commit_point()?;
        if self.reason_home.dir.is_none() {
            if let Some(home) = &self.home {
                self.reason_home.mem = crate::reason::store::read_rule_dir(home)?;
            }
        }
        #[cfg(feature = "rocksdb")]
        if self.store.as_rocks().is_some() {
            let mut mem = TripleStore::new();
            crate::store::MutableStore::bulk_load(&mut mem, self.store.iter_all());
            self.store = Backend::Memory(mem);
        }
        // Dropping the sink closes the disk store (nothing left to flush).
        *self.disk.get_mut() = None;
        self.home = None;
        self.dir_lock = None;
        if self.vstree.is_disk_backed() {
            // The paged node file belongs to the old directory.
            self.rebuild_index();
        }
        Ok(self)
    }

    // ---- saving --------------------------------------------------------------

    /// Save the database into directory `dir` (created if necessary).
    ///
    /// * In-memory database: writes the bincode snapshot as one crash-atomic set
    ///   (temp files + a commit record, renamed into place) and removes any
    ///   other layout's files
    ///   from `dir`. Reasoning rules it holds in memory (see
    ///   [`reason_db_dir`](Self::reason_db_dir)) replace `dir`'s; holding
    ///   none, it leaves `dir`'s rules alone.
    /// * On-disk database: at its home directory, commits the write-through to
    ///   `kvstore.kv` — no shadow `store.bin` is written. Into another
    ///   directory, it commits and then copies the store files there.
    /// * RocksDB database: at its home directory, commits (flushes) RocksDB.
    ///   Into another directory, it writes a RocksDB checkpoint to
    ///   `dir/rocksdb` — a consistent, openable copy of every column family.
    ///
    /// Inside an open transaction, the persistent layouts save only what has
    /// been committed; an in-memory snapshot captures the live state.
    pub fn save<P: AsRef<Path>>(&self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        let at_home = self
            .home
            .as_deref()
            .is_some_and(|h| fsutil::same_dir(h, dir));
        if self.disk.borrow().is_some() {
            self.commit_point()?;
            return if at_home {
                Ok(())
            } else {
                self.copy_disk_files_to(dir)
            };
        }
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks() {
            self.commit_point()?;
            if at_home {
                return rocks.flush();
            }
            let target = dir.join(ROCKS_SUBDIR);
            let staged = fsutil::tmp_path(&target);
            fsutil::remove_dir_if_exists(&staged)?;
            rocks.checkpoint_to(&staged)?;
            fsutil::install_staged_dir(&staged, &target)?;
            return remove_other_layouts(dir, StorageKind::RocksDb);
        }
        self.save_snapshot(dir)?;
        // Reasoning rules held in memory (see `reason_db_dir`) go with the
        // snapshot; with none, the directory's own are left alone.
        if self.reason_db_dir().is_none() && !self.reason_home.mem.is_empty() {
            crate::reason::store::write_memory_rules(&self.reason_home.mem, dir)?;
        }
        Ok(())
    }

    /// Write the bincode snapshot set into `dir` (crash-atomic as a set).
    fn save_snapshot(&self, dir: &Path) -> Result<()> {
        let Some(mem) = self.store.as_memory() else {
            return Err(GStoreError::Database(
                "only an in-memory store can be saved as a snapshot".into(),
            ));
        };
        let meta = Meta {
            name: self.name.clone(),
            triple_num: mem.triple_count(),
            entity_num: self.dict.entity_num() as u64,
            literal_num: self.dict.literal_num() as u64,
            predicate_num: self.dict.predicate_num() as u64,
        };
        let mut w = SnapshotWriter::begin(dir, SNAPSHOT_FILES)?;
        w.stage(DICT_FILE, &self.dict)?;
        w.stage(STORE_FILE, mem)?;
        w.stage(NAMED_FILE, &self.named)?;
        // Persist a fresh in-memory VS-tree snapshot. Rebuild it from the store
        // when the live tree is stale *or* out-of-core (a disk-backed tree holds
        // no in-memory nodes, so serializing it directly would write an empty
        // tree — rebuild from the store instead so the snapshot is complete).
        if self.index_valid && !self.vstree.is_disk_backed() {
            w.stage(VSTREE_FILE, &self.vstree)?;
        } else {
            w.stage(VSTREE_FILE, &build_vstree(&self.store))?;
        }
        w.stage(META_FILE, &meta)?;
        w.commit()?;
        remove_other_layouts(dir, StorageKind::Snapshot)
    }

    /// Copy a (just committed) on-disk database's store files into `dir`.
    fn copy_disk_files_to(&self, dir: &Path) -> Result<()> {
        let Some(home) = self.home.as_deref() else {
            return Err(GStoreError::Database(
                "on-disk database has no home directory".into(),
            ));
        };
        let stale = self
            .disk
            .borrow()
            .as_ref()
            .is_none_or(|s| s.store.vstree_stale());
        let kv_dst = dir.join(KV_FILE);
        // A stale WAL beside the destination would replay into the new copy.
        fsutil::remove_file_if_exists(&wal_of(&kv_dst))?;
        fsutil::copy_file_atomic(&home.join(KV_FILE), &kv_dst)?;
        let vs_src = home.join(VSTREE_KV_FILE);
        let vs_dst = dir.join(VSTREE_KV_FILE);
        fsutil::remove_file_if_exists(&wal_of(&vs_dst))?;
        if !stale && vs_src.is_file() {
            fsutil::copy_file_atomic(&vs_src, &vs_dst)?;
        } else {
            // The copy's header says the VS-tree is stale; it is rebuilt on open.
            fsutil::remove_file_if_exists(&vs_dst)?;
        }
        remove_other_layouts(dir, StorageKind::Disk)
    }

    // ---- bincode snapshot ----------------------------------------------------

    /// Load a bincode snapshot directory (finishing an interrupted save first).
    pub(super) fn load_snapshot(dir: &Path) -> Result<Database> {
        fsutil::recover_snapshot(dir, SNAPSHOT_FILES)?;
        Database::read_snapshot(dir)
    }

    /// An in-memory copy of the snapshot database in `dir`, read **without**
    /// taking the directory's inter-process lock ([`dirlock`](super::dirlock))
    /// and without touching the directory — to inspect a database another
    /// process (a running `gserver`) may have open. Saves replace a snapshot
    /// atomically, so the copy is either the state before a concurrent save
    /// or the one after it (a read racing a save's final renames fails its
    /// consistency check instead — retry). The copy is detached: it has no
    /// [`home`](Self::home) and never writes back to `dir`. Only the snapshot
    /// layout can be read this way; open an on-disk or RocksDB database with
    /// [`open`](Self::open).
    pub fn read_copy<P: AsRef<Path>>(dir: P) -> Result<Database> {
        let dir = dir.as_ref();
        match Database::storage_kind(dir) {
            Some(StorageKind::Snapshot) => {
                let mut db = Database::read_snapshot(dir)?;
                db.home = None;
                Ok(db)
            }
            Some(kind) => Err(GStoreError::Database(format!(
                "'{}' holds a {kind} database; read_copy reads snapshot databases only",
                dir.display()
            ))),
            None => Err(GStoreError::Database(format!(
                "'{}' does not contain a gStore database",
                dir.display()
            ))),
        }
    }

    /// Read the snapshot files of `dir` (no recovery, see
    /// [`load_snapshot`](Self::load_snapshot)).
    fn read_snapshot(dir: &Path) -> Result<Database> {
        let meta: Meta = fsutil::read_bincode(&dir.join(META_FILE))?;
        let dict: Dictionary = fsutil::read_bincode(&dir.join(DICT_FILE))?;
        let store: TripleStore = fsutil::read_bincode(&dir.join(STORE_FILE))?;
        // Named graphs are optional (older databases / no GRAPH data).
        let named: BTreeMap<u32, TripleStore> =
            fsutil::read_bincode(&dir.join(NAMED_FILE)).unwrap_or_default();
        // Sanity check that the snapshot is internally consistent.
        if store.triple_count() != meta.triple_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} triples but store has {}",
                meta.triple_num,
                store.triple_count()
            )));
        }
        if dict.entity_num() as u64 != meta.entity_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} entities but dict has {}",
                meta.entity_num,
                dict.entity_num()
            )));
        }
        if dict.literal_num() as u64 != meta.literal_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} literals but dict has {}",
                meta.literal_num,
                dict.literal_num()
            )));
        }
        if dict.predicate_num() as u64 != meta.predicate_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} predicates but dict has {}",
                meta.predicate_num,
                dict.predicate_num()
            )));
        }
        // The VS-tree is rebuilt from the store if absent or unreadable.
        let vstree =
            fsutil::read_bincode(&dir.join(VSTREE_FILE)).unwrap_or_else(|_| build_vstree(&store));
        let mut db = Database::new(meta.name);
        db.dict = dict;
        db.store = Backend::Memory(store);
        db.named = named;
        db.vstree = vstree;
        db.index_valid = true;
        db.home = Some(dir.to_path_buf());
        Ok(db)
    }

    // ---- on-disk B+ tree KVstore ---------------------------------------------

    /// Build an on-disk database (B+ tree KVstore) in directory `dir` from RDF
    /// files (and directories / URLs), detecting each file's format by
    /// extension (as [`build_from_files`](Self::build_from_files) does). The
    /// input streams in; the triples, named-graph quads (N-Quads / TriG) and
    /// dictionary are written to `kvstore.kv` through the page cache; the
    /// out-of-core VS-tree is written to `vstree.kv`. Other layouts' files in
    /// `dir` are removed (a fresh build). Strict.
    pub fn build_disk<P: AsRef<Path>, Q: AsRef<Path>>(dir: P, files: &[Q]) -> Result<()> {
        let sources = import::sources_from_paths(files)?;
        Database::build_disk_from_sources(dir, &sources, &import::ImportOptions::strict(), &mut ())
            .map(|_| ())
    }

    /// [`build_disk`](Self::build_disk) from `sources` with explicit
    /// [`import::ImportOptions`] (e.g. tolerant, logging skipped statements
    /// to `log`); returns the import counts.
    pub fn build_disk_from_sources<P: AsRef<Path>>(
        dir: P,
        sources: &[import::Source],
        opts: &import::ImportOptions,
        log: &mut dyn import::ErrorLog,
    ) -> Result<import::ImportReport> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        remove_other_layouts(dir, StorageKind::Disk)?;
        // Fresh build: remove any prior store/VS-tree files *and* their WALs, so a
        // leftover WAL from a crashed earlier build can't replay into the new file.
        let kv = dir.join(KV_FILE);
        super::remove_file_and_wal(&kv);
        let mut ds = DiskStore::open(&kv, DISK_CACHE_PAGES)?;
        let report = ds.import_sources(sources, opts, log)?;
        ds.flush()?;
        // Build the out-of-core VS-tree alongside the store, so a later
        // `load_disk` filters through it on disk instead of materializing it.
        let vskv = dir.join(VSTREE_KV_FILE);
        super::remove_file_and_wal(&vskv);
        drop(write_vstree_kv(&vskv, vstree_entries(&ds))?);
        ds.mark_vstree_fresh();
        ds.flush()?;
        Ok(report)
    }

    /// Whether directory `dir` holds an on-disk KVstore database.
    pub fn is_disk<P: AsRef<Path>>(dir: P) -> bool {
        dir.as_ref().join(KV_FILE).is_file()
    }

    /// Open an on-disk database. The `kvstore.kv` B+ trees are the source of
    /// truth; their content (dictionary, default graph, named graphs) is
    /// materialized as the in-memory working set queries evaluate over, and
    /// every later committed mutation is written through to the store (see the
    /// [`StorageKind`] docs). The out-of-core VS-tree (`vstree.kv`) is opened —
    /// or rebuilt first if an update made it stale.
    pub fn load_disk<P: AsRef<Path>>(dir: P) -> Result<Database> {
        let dir = dir.as_ref();
        let kv = dir.join(KV_FILE);
        if !kv.is_file() {
            return Err(GStoreError::Database(format!(
                "no on-disk KVstore at '{}'",
                kv.display()
            )));
        }
        let mut ds = DiskStore::open(&kv, DISK_CACHE_PAGES)?;
        reconcile_legacy_shadow(dir, &mut ds)?;
        let (dict, store) = ds.to_memory()?;
        let named = ds.named_to_memory()?;
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().trim_end_matches(".db").to_string())
            .unwrap_or_else(|| "disk".to_string());
        // Out-of-core VS-tree: open the persisted paged node file, unless an
        // update since it was built made it stale (or it is missing) — then
        // rebuild it from the just-loaded store, whose ids are the store's.
        let vskv = dir.join(VSTREE_KV_FILE);
        let vstree = if vskv.is_file() && !ds.vstree_stale() {
            VsTree::open_disk(&vskv)?
        } else {
            let tree = write_vstree_kv(&vskv, vstree_entries(&store))?;
            ds.mark_vstree_fresh();
            ds.flush()?;
            tree
        };
        let mut db = Database::new(name);
        db.dict = dict;
        db.store = Backend::Memory(store);
        db.named = named;
        db.vstree = vstree;
        // The disk-backed tree is consistent with the just-loaded store, so
        // the query path may use it as a candidate filter.
        db.index_valid = true;
        db.home = Some(dir.to_path_buf());
        *db.disk.get_mut() = Some(DiskSink::new(ds));
        Ok(db)
    }

    // ---- RocksDB -------------------------------------------------------------

    /// Whether directory `dir` holds a RocksDB-backed database. A plain
    /// directory check, available with or without the `rocksdb` feature.
    pub fn is_rocksdb<P: AsRef<Path>>(dir: P) -> bool {
        fsutil::dir_or_staged_exists(&dir.as_ref().join(ROCKS_SUBDIR))
    }

    /// Build a **persistent**, RocksDB-backed database in directory `dir` from an
    /// in-memory RDF document (Turtle / N-Triples). Everything — triples,
    /// statistics, dictionary, named graphs, name — lives in the RocksDB store
    /// under `dir/rocksdb`; reopen it with [`open`](Self::open) /
    /// [`open_rocksdb`](Self::open_rocksdb). The full query engine (optimizer,
    /// VS-tree, analytics) runs over it unchanged via the [`Backend`] seam.
    #[cfg(feature = "rocksdb")]
    pub fn build_rocksdb_from_str<P: AsRef<Path>>(
        name: impl Into<String>,
        dir: P,
        content: &str,
    ) -> Result<Database> {
        let opts = import::ImportOptions {
            blank_nodes: import::BlankNodes::Keep,
            ..import::ImportOptions::strict()
        };
        let mut db = Database::new(name);
        let mut acc = super::BuildAccumulator::default();
        import::import_reader(
            content.as_bytes(),
            import::RdfFormat::Turtle,
            "<string>",
            &opts,
            &mut (),
            &mut |batch: &mut Vec<crate::parser::nquads::Quad>| acc.add(&mut db, batch),
        )?;
        Database::create_rocksdb(db, acc, dir.as_ref())
    }

    /// Build a RocksDB-backed database in `dir` from RDF files (and
    /// directories / URLs), detecting each file's format by extension (see
    /// [`build_from_files`](Self::build_from_files)). The input streams in;
    /// N-Quads / TriG statements keep their named graph (the `gspo` CF).
    #[cfg(feature = "rocksdb")]
    pub fn build_rocksdb_from_files<P: AsRef<Path>, Q: AsRef<Path>>(
        name: impl Into<String>,
        dir: P,
        files: &[Q],
    ) -> Result<Database> {
        let sources = import::sources_from_paths(files)?;
        let (db, _) = Database::build_rocksdb_from_sources(
            name,
            dir,
            &sources,
            &import::ImportOptions::strict(),
            &mut (),
        )?;
        Ok(db)
    }

    /// [`build_rocksdb_from_files`](Self::build_rocksdb_from_files) from
    /// `sources` with explicit [`import::ImportOptions`]; returns the import
    /// counts too.
    #[cfg(feature = "rocksdb")]
    pub fn build_rocksdb_from_sources<P: AsRef<Path>>(
        name: impl Into<String>,
        dir: P,
        sources: &[import::Source],
        opts: &import::ImportOptions,
        log: &mut dyn import::ErrorLog,
    ) -> Result<(Database, import::ImportReport)> {
        let mut db = Database::new(name);
        let mut acc = super::BuildAccumulator::default();
        let report = import::import_sources(
            sources,
            "build",
            opts,
            log,
            &mut |batch: &mut Vec<crate::parser::nquads::Quad>| acc.add(&mut db, batch),
        )?;
        Ok((Database::create_rocksdb(db, acc, dir.as_ref())?, report))
    }

    /// Write an encoded build (`db`'s dictionary, `acc`'s ids) into a fresh
    /// RocksDB store in `dir`.
    #[cfg(feature = "rocksdb")]
    fn create_rocksdb(
        mut db: Database,
        acc: super::BuildAccumulator,
        dir: &Path,
    ) -> Result<Database> {
        use crate::backend::rocks::RocksStore;
        // Everything is encoded already: an exhausted id space has failed
        // before anything in `dir` is replaced.
        let super::BuildAccumulator { default: ids, named } = acc;
        fs::create_dir_all(dir)?;
        // Fresh build: replace any previous database in `dir`.
        remove_other_layouts(dir, StorageKind::RocksDb)?;
        let rocks_dir = dir.join(ROCKS_SUBDIR);
        fsutil::discard_dir(&rocks_dir)?;

        let mut rocks = RocksStore::open(&rocks_dir)?;
        // Dictionary first: a term without a triple is harmless, the reverse not.
        rocks.replace_dictionary(rocks_layout::dict_entries(&db.dict))?;
        crate::store::MutableStore::bulk_load(&mut rocks, ids);
        if let Some(e) = rocks.take_error() {
            return Err(GStoreError::Database(e));
        }
        // Named graphs: the `gspo` CF and the in-memory map.
        for (gid, quads) in named {
            for &id in &quads {
                rocks.insert_quad(gid, id)?;
            }
            let mut graph = TripleStore::new();
            graph.bulk_load(quads);
            db.named.insert(gid, graph);
        }
        rocks.put_meta(rocks_layout::NAME_KEY, db.name.as_bytes())?;
        rocks.put_meta(rocks_layout::LAYOUT_KEY, rocks_layout::LAYOUT_VERSION)?;
        rocks.flush()?;
        db.store = Backend::Rocks(rocks);
        db.home = Some(dir.to_path_buf());
        db.rebuild_index();
        Ok(db)
    }

    /// Reopen a RocksDB-backed database. The triples are read from RocksDB on
    /// demand; the dictionary and named graphs are reloaded from their column
    /// families and the VS-tree is rebuilt. A database written by an older build
    /// (dictionary in `dict.bin`) is migrated into the column families first —
    /// unless that build had applied updates whose new terms it never saved, in
    /// which case it cannot be opened safely and an error says so.
    #[cfg(feature = "rocksdb")]
    pub fn open_rocksdb<P: AsRef<Path>>(dir: P) -> Result<Database> {
        use crate::backend::rocks::RocksStore;
        let dir = dir.as_ref();
        let rocks_dir = dir.join(ROCKS_SUBDIR);
        fsutil::recover_dir_swap(&rocks_dir)?;
        if !rocks_dir.is_dir() {
            return Err(GStoreError::Database(format!(
                "no RocksDB store at '{}'",
                rocks_dir.display()
            )));
        }
        let rocks = RocksStore::open(&rocks_dir)?;
        let (dict, named, name) = if rocks.get_meta(rocks_layout::LAYOUT_KEY)?.is_some() {
            let dict = rocks_layout::dictionary_from_terms(rocks.load_terms()?)?;
            let named = rocks_layout::named_from_quads(rocks.quads()?);
            let name = match rocks.get_meta(rocks_layout::NAME_KEY)? {
                Some(n) => String::from_utf8_lossy(&n).into_owned(),
                None => rocks_layout::dir_name(dir),
            };
            (dict, named, name)
        } else {
            rocks_layout::migrate_legacy(dir, &rocks)?
        };
        let mut db = Database::new(name);
        db.dict = dict;
        db.store = Backend::Rocks(rocks);
        db.named = named;
        db.home = Some(dir.to_path_buf());
        db.rebuild_index();
        Ok(db)
    }

    // ---- file-level backup / restore -----------------------------------------

    /// File-level backup of a *persisted* database directory `src` into `dst`,
    /// copying every database file present — the snapshot files, the on-disk
    /// KVstore and VS-tree files **with their WALs**, the `rocksdb/` store
    /// (recursively), and the update/redo logs — without loading anything into
    /// memory. Database files of `dst` that `src` lacks are removed, so `dst`
    /// mirrors `src`. Intended for a database no process is writing; for a live
    /// database prefer [`backup`](Self::backup) (a RocksDB checkpoint or a
    /// committed copy).
    pub fn backup_dir<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> Result<()> {
        let (src, dst) = (src.as_ref(), dst.as_ref());
        if !src.is_dir() {
            return Err(GStoreError::Database(format!(
                "backup source '{}' is not a directory",
                src.display()
            )));
        }
        // Finish any interrupted save in the source so the copy is coherent.
        fsutil::recover_snapshot(src, SNAPSHOT_FILES)?;
        fsutil::recover_dir_swap(&src.join(ROCKS_SUBDIR))?;
        fs::create_dir_all(dst)?;

        let kv = Path::new(KV_FILE);
        let vs = Path::new(VSTREE_KV_FILE);
        let mut names: Vec<PathBuf> = SNAPSHOT_FILES.iter().map(PathBuf::from).collect();
        // Each paged file before its WAL, so a stale destination WAL is gone
        // before the new main file lands (see `wal_of`).
        for paged in [kv, vs] {
            names.push(paged.to_path_buf());
            names.push(wal_of(paged));
        }
        for extra in [UPDATE_LOG_FILE, REDO_LOG_FILE, BACKUP_OFFSET_FILE] {
            names.push(PathBuf::from(extra));
        }
        // A destination WAL must never meet a newly copied main file: drop them
        // all first; the source's own WALs (if any) are copied after their file.
        for paged in [kv, vs] {
            fsutil::remove_file_if_exists(&dst.join(wal_of(paged)))?;
        }
        fsutil::sync_dir(dst)?;
        for name in &names {
            let (from, to) = (src.join(name), dst.join(name));
            if from.is_file() {
                fsutil::copy_file_atomic(&from, &to)?;
            } else {
                fsutil::remove_file_if_exists(&to)?;
            }
        }
        let (rocks_src, rocks_dst) = (src.join(ROCKS_SUBDIR), dst.join(ROCKS_SUBDIR));
        if rocks_src.is_dir() {
            fsutil::replace_dir_with_copy(&rocks_src, &rocks_dst)?;
        } else {
            fsutil::discard_dir(&rocks_dst)?;
        }
        // The reasoning rules (`reason_rule_files/`), as upstream's
        // whole-directory copy carries them.
        crate::reason::store::copy_rule_dir(src, dst)?;
        fsutil::sync_dir(dst)
    }

    /// Restore the database held in `backup` (any layout) into directory
    /// `target`, replacing whatever database `target` held. Crash-safe and
    /// validated: the backup is first copied into a staging directory beside
    /// `target` (a snapshot backup is loaded — which validates it — and saved
    /// there; an on-disk or RocksDB backup is copied file by file, see
    /// [`backup_dir`](Self::backup_dir)), the staged copy is opened to check it
    /// is a readable database, and only then is it swapped in for `target` as a
    /// whole directory (an interrupted swap is completed by the next
    /// [`open`](Self::open)). A corrupt or truncated backup therefore leaves
    /// `target` untouched. `target` is replaced as a whole, so it must be a
    /// directory dedicated to the database (as `<name>.db` is). Open the result
    /// with [`open`](Self::open).
    ///
    /// Without the `rocksdb` feature a RocksDB backup can be restored but not
    /// opened, so it is only checked for a RocksDB `CURRENT` file.
    pub fn restore_into<P: AsRef<Path>, Q: AsRef<Path>>(backup: P, target: Q) -> Result<()> {
        let target = target.as_ref();
        let staged = Database::stage_restore(backup.as_ref(), target)?;
        Database::install_restore(&staged, target)
    }

    /// The first half of [`restore_into`](Self::restore_into): copy `backup`
    /// into the staging directory of `target` and validate the copy, without
    /// touching `target` itself. Returns the staged directory, to be passed to
    /// [`install_restore`](Self::install_restore) (or
    /// [`discard_staged_restore`](Self::discard_staged_restore)).
    pub(crate) fn stage_restore(backup: &Path, target: &Path) -> Result<PathBuf> {
        let Some(kind) = Database::storage_kind(backup) else {
            return Err(GStoreError::Database(format!(
                "'{}' does not contain a database backup",
                backup.display()
            )));
        };
        // Finish (or clean up after) an interrupted earlier restore of `target`.
        Database::recover_restore(target)?;
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let staged = fsutil::RESTORE_SWAP.staging(target);
        fsutil::remove_dir_if_exists(&staged)?;
        let built = (|| -> Result<()> {
            match kind {
                // Loading validates the snapshot (meta vs. contents).
                StorageKind::Snapshot => {
                    Database::load_snapshot(backup)?.save(&staged)?;
                    crate::reason::store::copy_rule_dir(backup, &staged)
                }
                _ => {
                    Database::backup_dir(backup, &staged)?;
                    remove_other_layouts(&staged, kind)?;
                    validate_restored_copy(&staged, kind)
                }
            }
        })();
        if let Err(e) = built {
            let _ = fsutil::remove_dir_if_exists(&staged);
            return Err(e);
        }
        Ok(staged)
    }

    /// The second half of [`restore_into`](Self::restore_into): swap the staged
    /// copy in for `target` (crash-safe; see [`recover_restore`](Self::recover_restore)).
    /// No process may have a database open in `target` while this runs.
    pub(crate) fn install_restore(staged: &Path, target: &Path) -> Result<()> {
        fsutil::install_staged_dir_as(staged, target, &fsutil::RESTORE_SWAP)
    }

    /// Drop a staged restore that will not be installed.
    pub(crate) fn discard_staged_restore(staged: &Path) {
        let _ = fsutil::remove_dir_if_exists(staged);
    }

    /// Complete an interrupted [`restore_into`](Self::restore_into) of directory
    /// `dir` (the staged copy had fully replaced nothing yet, or was half way
    /// through the swap), or remove its leftovers. Run by [`open`](Self::open);
    /// only the restore's own `<dir>.restore-*` siblings are ever touched.
    pub(crate) fn recover_restore(dir: &Path) -> Result<()> {
        fsutil::recover_dir_swap_as(dir, &fsutil::RESTORE_SWAP)
    }

    // ---- zipped backups (feature `zip`, see `crate::zipio`) ------------------

    /// Pack the backup directory `dir` (a [`backup`](Self::backup) or
    /// [`backup_dir`](Self::backup_dir) result) into the archive `zip`, then
    /// remove `dir` — upstream's `backup compress=true` /
    /// `gbackup -z 1`: `<db>.db_<YYYYmmddHHMMSS>.zip`, the backup's files at
    /// the archive root. `dir` is kept when packing fails. Without the `zip`
    /// feature this fails, naming the feature.
    pub fn pack_backup<P: AsRef<Path>, Q: AsRef<Path>>(dir: P, zip: Q) -> Result<()> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            return Err(GStoreError::Database(format!(
                "backup '{}' is not a directory",
                dir.display()
            )));
        }
        crate::zipio::pack_dir(dir, zip.as_ref())?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Unpack the zipped backup `zip` into `scratch` (a fresh directory the
    /// caller removes afterwards) under `limits`, and return the directory
    /// holding the database: `scratch` itself (the upstream layout), or its
    /// one sub-directory when the archive wraps the backup in a directory
    /// (`zip -r x.zip lubm.db_…`). Whether that holds a database is the
    /// caller's to check ([`storage_kind`](Self::storage_kind)).
    pub fn unpack_backup(
        zip: &Path,
        scratch: &Path,
        limits: &crate::zipio::ZipLimits,
    ) -> std::result::Result<PathBuf, crate::zipio::ZipError> {
        crate::zipio::unpack(zip, scratch, limits, &|_| true)?;
        if Database::storage_kind(scratch).is_some() {
            return Ok(scratch.to_path_buf());
        }
        let entries: Vec<PathBuf> = fs::read_dir(scratch)?.flatten().map(|e| e.path()).collect();
        match entries.as_slice() {
            [only] if only.is_dir() && Database::storage_kind(only).is_some() => Ok(only.clone()),
            _ => Ok(scratch.to_path_buf()),
        }
    }

    /// [`restore_into`](Self::restore_into) from a zipped backup (`grestore
    /// x.zip`): unpacked under `limits` into `<target>.unzip-tmp` beside
    /// `target` (removed afterwards), then validated and swapped in like a
    /// backup directory. Without the `zip` feature this fails, naming it.
    pub fn restore_from_zip<P: AsRef<Path>, Q: AsRef<Path>>(
        zip: P,
        target: Q,
        limits: &crate::zipio::ZipLimits,
    ) -> Result<()> {
        let target = target.as_ref();
        if !crate::zipio::AVAILABLE {
            return Err(crate::zipio::ZipError::FeatureMissing.into());
        }
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut scratch = target.as_os_str().to_owned();
        scratch.push(".unzip-tmp");
        let scratch = crate::zipio::ScratchDir::fresh(PathBuf::from(scratch))?;
        let backup = Database::unpack_backup(zip.as_ref(), scratch.path(), limits)?;
        Database::restore_into(&backup, target)
    }

    // ---- write-through hooks (called by the mutation paths) ----------------

    /// Record a persistence failure to report at the next commit point.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn note_persist_error(&self, e: impl fmt::Display) {
        let mut slot = self.persist_error.borrow_mut();
        if slot.is_none() {
            *slot = Some(e.to_string());
        }
    }

    /// Write one *applied* mutation of the default graph (`graph == None`) or
    /// of a named graph through to an on-disk store (queued inside a
    /// transaction). The other layouts persist elsewhere: RocksDB default-graph
    /// triples go to RocksDB through the [`Backend`], and named-graph quads
    /// through [`persist_named_quad`](Self::persist_named_quad) *before* the
    /// in-memory graph is changed. In-memory: nothing to do.
    pub(super) fn persist_mutation(&mut self, insert: bool, graph: Option<u32>, id: IdTriple) {
        let in_txn = self.txn.is_some();
        if let Some(sink) = self.disk.get_mut() {
            match DiskOp::resolve(&self.dict, insert, graph, id) {
                Some(op) => sink.record(op, in_txn),
                None => sink.fail("a mutated triple references an id missing from the dictionary"),
            }
        }
    }

    /// Mirror a named-graph change to RocksDB's `gspo` CF (journaled inside a
    /// transaction). Returns `false` — the error noted for the next commit
    /// point — when the write failed; the caller must then leave the in-memory
    /// graph unchanged, so memory never holds a change RocksDB lacks (a retry
    /// then writes it again). Always `true` for the other layouts.
    #[cfg_attr(not(feature = "rocksdb"), allow(unused_variables))]
    pub(super) fn persist_named_quad(&mut self, insert: bool, graph: u32, id: IdTriple) -> bool {
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks_mut() {
            // A stale undo journal blocks writes until it is cleaned up.
            if let Err(e) = rocks.heal_journal() {
                self.note_persist_error(e);
                return false;
            }
            let res = if insert {
                rocks.insert_quad(graph, id)
            } else {
                rocks.remove_quad(graph, id)
            };
            if let Err(e) = res {
                self.note_persist_error(e);
                return false;
            }
        }
        true
    }

    // ---- RocksDB transaction journal ---------------------------------------

    /// Start journaling a transaction's RocksDB writes (no-op otherwise).
    pub(super) fn rocks_begin_journal(&mut self) -> Result<()> {
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks_mut() {
            return rocks.begin_journal();
        }
        Ok(())
    }

    /// Whether this is a RocksDB database journaling an open transaction.
    pub(super) fn rocks_journaling(&self) -> bool {
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks() {
            return rocks.journaling();
        }
        false
    }

    /// Commit the journaled transaction (delete its undo journal).
    pub(super) fn rocks_commit_journal(&mut self) -> Result<()> {
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks_mut() {
            return rocks.commit_journal();
        }
        Ok(())
    }

    /// Roll back the journaled transaction (replay its undo journal).
    pub(super) fn rocks_rollback_journal(&mut self) -> Result<()> {
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks_mut() {
            return rocks.rollback_journal();
        }
        Ok(())
    }

    /// Apply the queued write-through of a committing transaction.
    pub(super) fn disk_commit_pending(&mut self) {
        if let Some(sink) = self.disk.get_mut() {
            sink.commit_pending();
        }
    }

    /// Drop the queued write-through of a rolled-back transaction.
    pub(super) fn disk_discard_pending(&mut self) {
        if let Some(sink) = self.disk.get_mut() {
            sink.discard_pending();
        }
    }

    /// Whether mutations are written through to an on-disk store.
    pub(super) fn is_disk_backed(&self) -> bool {
        self.disk.borrow().is_some()
    }

    /// A commit point: make every mutation applied so far durable (commit the
    /// on-disk store's dirty pages / fsync RocksDB's WAL), then report the first
    /// persistence failure recorded since the previous commit point.
    pub(super) fn commit_point(&self) -> Result<()> {
        let mut failure: Option<GStoreError> = None;
        if let Some(sink) = self.disk.borrow_mut().as_mut() {
            if let Err(e) = sink.flush() {
                failure = Some(e);
            }
        }
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks() {
            if let Err(e) = rocks.sync_wal() {
                failure.get_or_insert(e);
            }
            // Not `take`n: a stale journal keeps failing commit points until
            // it is cleaned up.
            if let Some(e) = rocks.journal_error() {
                failure.get_or_insert(GStoreError::Database(format!("rocksdb: {e}")));
            }
        }
        let recorded = self
            .persist_error
            .borrow_mut()
            .take()
            .or_else(|| self.store.take_write_error());
        if let Some(msg) = recorded {
            failure.get_or_insert(GStoreError::Database(format!(
                "an update could not be persisted: {msg}"
            )));
        }
        failure.map_or(Ok(()), Err)
    }

    /// Intern a triple's terms (and its graph IRI) for an insert. Errs once a
    /// dictionary id space is exhausted. For a RocksDB database, brand-new
    /// dictionary terms are written to the `dict` CF *before* the triple; if
    /// that fails the insert is abandoned (`Ok(None)`) and the error is
    /// reported at the next commit point.
    pub(super) fn intern_for_insert(
        &mut self,
        t: &Triple,
        graph: Option<&str>,
    ) -> Result<Option<(Option<u32>, IdTriple)>> {
        #[cfg(feature = "rocksdb")]
        if self.store.as_rocks().is_some() {
            return self.intern_persisting_terms(t, graph);
        }
        let gid = match graph {
            Some(g) => Some(self.dict.try_intern_entity(&Term::iri(g).dict_key())?),
            None => None,
        };
        Ok(Some((gid, self.encode_triple(t)?)))
    }

    #[cfg(feature = "rocksdb")]
    fn intern_persisting_terms(
        &mut self,
        t: &Triple,
        graph: Option<&str>,
    ) -> Result<Option<(Option<u32>, IdTriple)>> {
        use crate::backend::rocks::TermSpace;
        use rocks_layout::intern_tracked;
        let mut fresh: Vec<(TermSpace, u32, String)> = Vec::new();
        let dict = &mut self.dict;
        let gid = match graph {
            Some(g) => Some(intern_tracked(
                dict,
                TermSpace::Entity,
                Term::iri(g).dict_key(),
                &mut fresh,
            )?),
            None => None,
        };
        let s = intern_tracked(dict, TermSpace::Entity, t.subject.dict_key(), &mut fresh)?;
        let p = intern_tracked(
            dict,
            TermSpace::Predicate,
            t.predicate.dict_key(),
            &mut fresh,
        )?;
        let o_space = if t.object.is_literal() {
            TermSpace::Literal
        } else {
            TermSpace::Entity
        };
        let o = intern_tracked(dict, o_space, t.object.dict_key(), &mut fresh)?;
        // Known terms whose earlier write failed are written again now.
        let mut retried: Vec<(TermSpace, u32, String)> = Vec::new();
        if !self.unpersisted_terms.is_empty() {
            let used = [
                gid.map(|g| (TermSpace::Entity, g)),
                Some((TermSpace::Entity, s)),
                Some((TermSpace::Predicate, p)),
                Some((o_space, o)),
            ];
            for (space, id) in used.into_iter().flatten() {
                let pending = self.unpersisted_terms.contains(&(space, id))
                    && !fresh.iter().chain(&retried).any(|(sp, i, _)| (*sp, *i) == (space, id));
                let key = match space {
                    TermSpace::Predicate => self.dict.predicate_to_string(id),
                    _ => self.dict.id_to_string(id),
                };
                if let (true, Some(key)) = (pending, key) {
                    retried.push((space, id, key.to_owned()));
                }
            }
        }
        if !fresh.is_empty() || !retried.is_empty() {
            let terms: Vec<(TermSpace, u32, &str)> = fresh
                .iter()
                .chain(&retried)
                .map(|(sp, id, k)| (*sp, *id, k.as_str()))
                .collect();
            if let Some(rocks) = self.store.as_rocks() {
                if let Err(e) = rocks.put_terms(&terms) {
                    // Forget the terms again, so that a retry interns them
                    // afresh and persists them: a known-but-unpersisted term
                    // would let a later triple reference an id RocksDB has no
                    // dictionary entry for.
                    for (space, _, key) in &fresh {
                        match space {
                            TermSpace::Entity => self.dict.free_entity(key),
                            TermSpace::Literal => self.dict.free_literal(key),
                            TermSpace::Predicate => self.dict.free_predicate(key),
                        };
                    }
                    self.note_persist_error(e);
                    return Ok(None);
                }
            }
            for (space, id, _) in &retried {
                self.unpersisted_terms.remove(&(*space, *id));
            }
        }
        Ok(Some((gid, IdTriple::new(s, p, o))))
    }

    /// After reasoning inserted `added` into the default graph: write the new
    /// triples through to an on-disk store (RocksDB already persisted them,
    /// terms first, see [`infer`](Self::infer)).
    pub(super) fn persist_inferred(&mut self, added: &[IdTriple]) {
        if self.is_disk_backed() {
            for &t in added {
                self.persist_mutation(true, None, t);
            }
        }
    }

    /// Run a reasoner over the dictionary and default graph. `run` gets the
    /// insert function to use for every inferred triple: a plain store insert,
    /// except on RocksDB, where the dictionary terms the triple uses (which the
    /// reasoner may just have interned, e.g. `rdf:type` or a rule-head
    /// constant) are written to the `dict` CF *before* the triple — the
    /// invariant every insert keeps, so a crash never leaves a persisted triple
    /// whose terms are missing. A triple whose terms cannot be persisted is not
    /// inserted (the error is reported at the next commit point).
    pub(super) fn infer<R>(&mut self, run: R) -> Vec<IdTriple>
    where
        R: FnOnce(
            &mut Dictionary,
            &mut Backend,
            &mut dyn FnMut(&Dictionary, &mut Backend, IdTriple) -> bool,
        ) -> Vec<IdTriple>,
    {
        #[cfg(feature = "rocksdb")]
        if self.store.as_rocks().is_some() {
            use crate::backend::rocks::TermSpace;
            // Terms already written during this run (re-writing one is a
            // harmless overwrite; this just avoids doing it per triple).
            let mut written: HashSet<(TermSpace, u32)> = HashSet::new();
            // Terms whose write failed: the reasoner may have interned them,
            // so the next insert that uses one must write it first.
            let mut unwritten: HashSet<(TermSpace, u32)> = HashSet::new();
            let mut failure: Option<String> = None;
            let added = run(&mut self.dict, &mut self.store, &mut |dict, store, t| {
                let terms: Vec<(TermSpace, u32, String)> = rocks_layout::terms_of(dict, &[t])
                    .into_iter()
                    .filter(|(space, id, _)| !written.contains(&(*space, *id)))
                    .collect();
                if !terms.is_empty() {
                    let borrowed: Vec<(TermSpace, u32, &str)> =
                        terms.iter().map(|(s, i, k)| (*s, *i, k.as_str())).collect();
                    let res = match store.as_rocks() {
                        Some(rocks) => rocks.put_terms(&borrowed),
                        None => Ok(()),
                    };
                    if let Err(e) = res {
                        failure.get_or_insert_with(|| e.to_string());
                        unwritten.extend(terms.iter().map(|(s, i, _)| (*s, *i)));
                        return false;
                    }
                    written.extend(terms.iter().map(|(s, i, _)| (*s, *i)));
                }
                store.insert(t)
            });
            self.unpersisted_terms.retain(|k| !written.contains(k));
            self.unpersisted_terms
                .extend(unwritten.into_iter().filter(|k| !written.contains(k)));
            if let Some(e) = failure {
                self.note_persist_error(e);
            }
            return added;
        }
        run(&mut self.dict, &mut self.store, &mut |_, store, t| store.insert(t))
    }

    /// After id reclamation: rewrite RocksDB's stored dictionary so freed ids
    /// cannot resurface on reopen (no-op for other layouts, which key their
    /// persistent state by term string).
    pub(super) fn persist_dictionary_rewrite(&mut self) {
        #[cfg(feature = "rocksdb")]
        if let Some(rocks) = self.store.as_rocks() {
            match rocks.replace_dictionary(rocks_layout::dict_entries(&self.dict)) {
                // Every live term is stored now.
                Ok(()) => self.unpersisted_terms.clear(),
                Err(e) => self.note_persist_error(e),
            }
        }
    }
}

/// Check that a restored on-disk / RocksDB copy in `dir` opens as a database.
/// Without the `rocksdb` feature a RocksDB copy cannot be opened, so it is only
/// checked for the `CURRENT` file every RocksDB directory has.
fn validate_restored_copy(dir: &Path, kind: StorageKind) -> Result<()> {
    if kind == StorageKind::RocksDb && !cfg!(feature = "rocksdb") {
        let current = dir.join(ROCKS_SUBDIR).join("CURRENT");
        return if current.is_file() {
            Ok(())
        } else {
            Err(GStoreError::Database(format!(
                "the RocksDB backup has no '{}'",
                current.display()
            )))
        };
    }
    Database::open(dir).map(drop).map_err(|e| {
        GStoreError::Database(format!("the backup is not a readable database: {e}"))
    })
}

/// Reconcile a legacy shadow snapshot found next to `kvstore.kv` (see the
/// module docs), then retire the shadow files.
fn reconcile_legacy_shadow(dir: &Path, ds: &mut DiskStore) -> Result<()> {
    let shadow = dir.join(STORE_FILE);
    if !shadow.is_file() {
        return Ok(());
    }
    let newer = match (mtime(&shadow), mtime(&dir.join(KV_FILE))) {
        (Some(s), Some(k)) => s >= k,
        _ => false,
    };
    if newer {
        // An unreadable / inconsistent shadow cannot be trusted: keep the store.
        if let Ok(snap) = Database::load_snapshot(dir) {
            apply_snapshot_to_disk(&snap, ds)?;
            ds.flush()?;
        }
    }
    for name in SNAPSHOT_FILES {
        let path = dir.join(name);
        if path.is_file() {
            let _ = fs::rename(&path, fsutil::with_suffix(&path, LEGACY_SUFFIX));
        }
    }
    fsutil::sync_dir(dir)
}

/// Make the disk store's default and named graphs equal those of `snap`.
fn apply_snapshot_to_disk(snap: &Database, ds: &mut DiskStore) -> Result<()> {
    let disk_dict = ds.dictionary()?;
    let disk_triples: HashSet<KeyTriple> = ds
        .iter_all()?
        .into_iter()
        .filter_map(|t| key_triple(&disk_dict, t))
        .collect();
    let snap_triples: HashSet<KeyTriple> = snap
        .store
        .iter_all()
        .into_iter()
        .filter_map(|t| key_triple(&snap.dict, t))
        .collect();
    for (s, p, o, lit) in disk_triples.difference(&snap_triples) {
        ds.delete_keys(s, p, o, *lit)?;
    }
    for (s, p, o, lit) in snap_triples.difference(&disk_triples) {
        ds.insert_keys(s, p, o, *lit)?;
    }

    let quad = |dict: &Dictionary, g: u32, t: IdTriple| -> Option<KeyQuad> {
        let (s, p, o, lit) = key_triple(dict, t)?;
        Some((dict.id_to_string(g)?.to_owned(), s, p, o, lit))
    };
    let disk_quads: HashSet<KeyQuad> = ds
        .quads()?
        .into_iter()
        .filter_map(|(g, t)| quad(&disk_dict, g, t))
        .collect();
    let snap_quads: HashSet<KeyQuad> = snap
        .named
        .iter()
        .flat_map(|(&g, st)| st.iter_all().map(move |t| (g, t)))
        .filter_map(|(g, t)| quad(&snap.dict, g, t))
        .collect();
    for (g, s, p, o, lit) in disk_quads.difference(&snap_quads) {
        ds.delete_quad_keys(g, s, p, o, *lit)?;
    }
    for (g, s, p, o, lit) in snap_quads.difference(&disk_quads) {
        ds.insert_quad_keys(g, s, p, o, *lit)?;
    }
    Ok(())
}

/// RocksDB layout helpers: dictionary (de)serialization into the `dict` CF and
/// migration of databases written by older builds.
#[cfg(feature = "rocksdb")]
mod rocks_layout {
    use std::collections::{BTreeMap, HashSet};
    use std::path::Path;

    use super::{fsutil, Meta, DICT_FILE, LEGACY_SUFFIX, META_FILE, NAMED_FILE};
    use crate::backend::rocks::{RocksStore, TermSpace};
    use crate::dict::Dictionary;
    use crate::error::{GStoreError, Result};
    use crate::model::id::{is_literal_id, LITERAL_FIRST_ID};
    use crate::model::IdTriple;
    use crate::store::{MutableStore, TripleSource, TripleStore};

    /// `meta` CF key holding the database name.
    pub(super) const NAME_KEY: &str = "name";
    /// `meta` CF key marking that dictionary + named graphs live in their CFs.
    pub(super) const LAYOUT_KEY: &str = "layout";
    pub(super) const LAYOUT_VERSION: &[u8] = b"1";

    pub(super) fn dir_name(dir: &Path) -> String {
        dir.file_name()
            .map(|s| s.to_string_lossy().trim_end_matches(".db").to_string())
            .unwrap_or_else(|| "rocksdb".to_string())
    }

    /// Intern `key` in `space`, recording it in `fresh` if it was new.
    pub(super) fn intern_tracked(
        dict: &mut Dictionary,
        space: TermSpace,
        key: String,
        fresh: &mut Vec<(TermSpace, u32, String)>,
    ) -> Result<u32> {
        let known = match space {
            TermSpace::Entity => dict.entity_id(&key),
            TermSpace::Literal => dict.literal_id(&key),
            TermSpace::Predicate => dict.predicate_id(&key),
        };
        if let Some(id) = known {
            return Ok(id);
        }
        let id = intern(dict, space, &key)?;
        fresh.push((space, id, key));
        Ok(id)
    }

    fn space_of(obj: u32) -> TermSpace {
        if is_literal_id(obj) {
            TermSpace::Literal
        } else {
            TermSpace::Entity
        }
    }

    /// Every live dictionary term as `(space, id, key)`.
    pub(super) fn dict_entries(dict: &Dictionary) -> Vec<(TermSpace, u32, &str)> {
        let mut out: Vec<(TermSpace, u32, &str)> = dict
            .entity_literal_entries()
            .into_iter()
            .map(|(id, k)| (space_of(id), id, k))
            .collect();
        // Predicates have no entry iterator; walk ids until every live one is
        // found (freed ids leave holes).
        let mut found = 0usize;
        let mut id = 0u32;
        while found < dict.predicate_num() && id < u32::MAX {
            if let Some(k) = dict.predicate_to_string(id) {
                out.push((TermSpace::Predicate, id, k));
                found += 1;
            }
            id += 1;
        }
        out
    }

    /// The terms `(space, id, key)` the triples in `added` use, de-duplicated.
    pub(super) fn terms_of(dict: &Dictionary, added: &[IdTriple]) -> Vec<(TermSpace, u32, String)> {
        let mut seen: HashSet<(TermSpace, u32)> = HashSet::new();
        let mut out = Vec::new();
        for t in added {
            for (space, id) in [
                (TermSpace::Entity, t.sub),
                (TermSpace::Predicate, t.pred),
                (space_of(t.obj), t.obj),
            ] {
                if !seen.insert((space, id)) {
                    continue;
                }
                let key = match space {
                    TermSpace::Predicate => dict.predicate_to_string(id),
                    _ => dict.id_to_string(id),
                };
                if let Some(k) = key {
                    out.push((space, id, k.to_owned()));
                }
            }
        }
        out
    }

    /// Rebuild the in-memory dictionary from the `dict` CF, keeping every id
    /// exactly as stored. Gaps (ids freed by reclamation, or interned but never
    /// used by a persisted triple) become free-list holes, as they were.
    pub(super) fn dictionary_from_terms(
        terms: Vec<(TermSpace, u32, String)>,
    ) -> Result<Dictionary> {
        let mut dict = Dictionary::new();
        let mut holes: Vec<(TermSpace, String)> = Vec::new();
        let mut next_entity = 0u32;
        let mut next_literal = LITERAL_FIRST_ID;
        let mut next_pred = 0u32;
        for (space, id, key) in terms {
            let next = match space {
                TermSpace::Entity => &mut next_entity,
                TermSpace::Literal => &mut next_literal,
                TermSpace::Predicate => &mut next_pred,
            };
            if id < *next {
                return Err(GStoreError::Database(format!(
                    "corrupt RocksDB dictionary: id {id} stored out of order"
                )));
            }
            while *next < id {
                let hole = format!("\u{0}gstore-free-slot-{}", *next);
                intern(&mut dict, space, &hole)?;
                holes.push((space, hole));
                *next += 1;
            }
            let got = intern(&mut dict, space, &key)?;
            if got != id {
                return Err(GStoreError::Database(format!(
                    "corrupt RocksDB dictionary: term {key} stored under two ids"
                )));
            }
            *next = id + 1;
        }
        for (space, hole) in holes {
            match space {
                TermSpace::Entity => dict.free_entity(&hole),
                TermSpace::Literal => dict.free_literal(&hole),
                TermSpace::Predicate => dict.free_predicate(&hole),
            };
        }
        Ok(dict)
    }

    /// Intern `key` in `space`, erring (instead of corrupting ids) once that
    /// id space is exhausted.
    fn intern(dict: &mut Dictionary, space: TermSpace, key: &str) -> Result<u32> {
        match space {
            TermSpace::Entity => dict.try_intern_entity(key),
            TermSpace::Literal => dict.try_intern_literal(key),
            TermSpace::Predicate => dict.try_intern_predicate(key),
        }
    }

    pub(super) fn named_from_quads(quads: Vec<(u32, IdTriple)>) -> BTreeMap<u32, TripleStore> {
        let mut grouped: BTreeMap<u32, Vec<IdTriple>> = BTreeMap::new();
        for (g, t) in quads {
            grouped.entry(g).or_default().push(t);
        }
        grouped
            .into_iter()
            .map(|(g, ts)| {
                let mut st = TripleStore::new();
                MutableStore::bulk_load(&mut st, ts);
                (g, st)
            })
            .collect()
    }

    /// Migrate a RocksDB database written by an older build — dictionary in a
    /// `dict.bin` snapshot, named graphs in `named.bin`, name in `meta.bin` —
    /// into the column families, then retire those files as `*.legacy`.
    pub(super) fn migrate_legacy(
        dir: &Path,
        rocks: &RocksStore,
    ) -> Result<(Dictionary, BTreeMap<u32, TripleStore>, String)> {
        let dict: Dictionary = fsutil::read_bincode(&dir.join(DICT_FILE)).map_err(|e| {
            GStoreError::Database(format!(
                "RocksDB database at '{}' has no dictionary (neither a `dict` column family \
                 nor a readable dict.bin): {e}",
                dir.display()
            ))
        })?;
        let meta: Option<Meta> = fsutil::read_bincode(&dir.join(META_FILE)).ok();
        let named: BTreeMap<u32, TripleStore> =
            fsutil::read_bincode(&dir.join(NAMED_FILE)).unwrap_or_default();
        if meta.as_ref().map(|m| m.triple_num) != Some(rocks.triple_count()) {
            // The old build applied updates to RocksDB but saved its dictionary
            // only on an explicit save; triples with terms it never saved cannot
            // be decoded.
            let dangling = rocks
                .iter_all()
                .into_iter()
                .filter(|t| {
                    dict.id_to_string(t.sub).is_none()
                        || dict.predicate_to_string(t.pred).is_none()
                        || dict.id_to_string(t.obj).is_none()
                })
                .count();
            if dangling > 0 {
                return Err(GStoreError::Database(format!(
                    "RocksDB database at '{}' (written by an older build) holds {dangling} \
                     triple(s) whose dictionary terms were never saved; it cannot be opened \
                     safely",
                    dir.display()
                )));
            }
        }
        rocks.replace_dictionary(dict_entries(&dict))?;
        for (&g, st) in &named {
            for t in st.iter_all() {
                rocks.insert_quad(g, t)?;
            }
        }
        let name = meta.map(|m| m.name).unwrap_or_else(|| dir_name(dir));
        rocks.put_meta(NAME_KEY, name.as_bytes())?;
        rocks.put_meta(LAYOUT_KEY, LAYOUT_VERSION)?;
        rocks.flush()?;
        for file in [DICT_FILE, META_FILE, NAMED_FILE] {
            let path = dir.join(file);
            if path.is_file() {
                let _ = std::fs::rename(&path, fsutil::with_suffix(&path, LEGACY_SUFFIX));
            }
        }
        Ok((dict, named, name))
    }
}
