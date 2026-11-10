//! Where a database's reasoning rules live.
//!
//! * A database with a directory keeps them, as upstream, in
//!   `<db dir>/reason_rule_files/`: one `<rulename>.json` per rule (the rule
//!   JSON, compact with sorted keys like upstream's `nlohmann::json::dump()`).
//!   This implementation adds one file per rule it added (or disabled or
//!   executed), `reason_rule_files/inferred/<rulename>.nt`: the N-Triples
//!   ledger of the triples the rule inferred, empty when it owns none (see
//!   [`super::manage`]; a rule file without one was written by upstream
//!   gStore). Upstream's listing only reads names containing `.json`, so it
//!   ignores the ledgers.
//! * A database without a directory (built in memory, never opened from or
//!   saved to a directory) keeps them in memory ([`MemoryRules`]); they live
//!   as long as the [`Database`](crate::Database) value.
//!
//! Every read-modify-write of a rule directory happens under a process-wide
//! lock for that directory ([`RuleStore::lock`]), so concurrent
//! `reasonManage` requests on one database never interleave; copying a rule
//! directory ([`copy_rule_dir`], for backups and restores) holds it too, and
//! so does a server removing or replacing a database directory. Rule files are
//! created with `create_new` (two concurrent adds of a name cannot both
//! succeed) and replaced atomically (temp file + rename). The database
//! directory itself is never created here: a rule cannot resurrect a dropped
//! database.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use super::rules::{MAX_RULE_FILE_BYTES, RULE_DIR};
use crate::db::fsutil;

/// The ledger sub-directory inside the rule directory.
pub const LEDGER_DIR: &str = "inferred";

/// The rules (and ledgers) of a database that has no directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryRules {
    /// Rule name → rule JSON text.
    rules: BTreeMap<String, String>,
    /// Rule name → its ledger (N-Triples lines).
    ledgers: BTreeMap<String, BTreeSet<String>>,
}

impl MemoryRules {
    /// Whether no rule is held.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.ledgers.is_empty()
    }
}

/// Where a [`Database`](crate::Database) keeps its upstream-format rules:
/// the directory set with
/// [`Database::set_reason_db_dir`](crate::Database::set_reason_db_dir) (the
/// server sets it to the database's directory), else the database's own
/// [`home`](crate::Database::home), else memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleHome {
    pub(crate) dir: Option<PathBuf>,
    pub(crate) mem: MemoryRules,
}

/// A rule store failure: an I/O error, or a missing database directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> StoreError {
        StoreError(e.to_string())
    }
}

impl From<crate::GStoreError> for StoreError {
    fn from(e: crate::GStoreError) -> StoreError {
        StoreError(e.to_string())
    }
}

type StoreResult<T> = std::result::Result<T, StoreError>;

/// Upstream's message when the database directory is missing.
pub const NO_DB_DIR: &str = "the database directory is not exists";

/// A database's rule storage (see the module docs).
#[derive(Debug)]
pub enum RuleStore<'a> {
    /// `<db dir>/reason_rule_files/`.
    Dir {
        /// The database directory.
        db_dir: PathBuf,
    },
    /// In memory.
    Memory(&'a mut MemoryRules),
}

/// One rule directory's process-wide lock: a flag guarded by a mutex, so a
/// held lock ([`StoreLock`]) owns its state without borrowing it.
#[derive(Default)]
struct DirLockState {
    held: Mutex<bool>,
    freed: Condvar,
}

/// The process-wide lock state of each rule directory (kept for the life of
/// the process; one small entry per database directory ever used).
fn dir_lock(dir: &Path) -> Arc<DirLockState> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<DirLockState>>>> = OnceLock::new();
    let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    Arc::clone(map.entry(key).or_default())
}

/// A held rule-directory lock (nothing for an in-memory store), released
/// when dropped.
pub struct StoreLock {
    state: Option<Arc<DirLockState>>,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if let Some(s) = &self.state {
            *s.held.lock().unwrap_or_else(|p| p.into_inner()) = false;
            s.freed.notify_one();
        }
    }
}

impl<'a> RuleStore<'a> {
    /// The store of the database in directory `db_dir`.
    pub fn dir(db_dir: impl Into<PathBuf>) -> RuleStore<'static> {
        RuleStore::Dir {
            db_dir: db_dir.into(),
        }
    }

    /// Take this store's process-wide lock (a no-op in memory). Hold it for
    /// the whole of an operation.
    pub fn lock(&self) -> StoreLock {
        match self {
            RuleStore::Dir { db_dir } => lock_dir(db_dir),
            RuleStore::Memory(_) => StoreLock { state: None },
        }
    }

    fn rule_dir(db_dir: &Path) -> PathBuf {
        db_dir.join(RULE_DIR)
    }

    fn rule_file(db_dir: &Path, name: &str) -> PathBuf {
        Self::rule_dir(db_dir).join(format!("{name}.json"))
    }

    fn ledger_file(db_dir: &Path, name: &str) -> PathBuf {
        Self::rule_dir(db_dir)
            .join(LEDGER_DIR)
            .join(format!("{name}.nt"))
    }

    /// The path a rule's file is shown as in messages (upstream prints
    /// `<db_home><db>.db/reason_rule_files/<name>.json`).
    pub fn shown_path(&self, name: &str) -> String {
        match self {
            RuleStore::Dir { db_dir } => Self::rule_file(db_dir, name).display().to_string(),
            RuleStore::Memory(_) => format!("(memory)/{RULE_DIR}/{name}.json"),
        }
    }

    /// Whether the database directory exists (always true in memory).
    pub fn db_exists(&self) -> bool {
        match self {
            RuleStore::Dir { db_dir } => db_dir.is_dir(),
            RuleStore::Memory(_) => true,
        }
    }

    /// The rule names, sorted.
    pub fn names(&self) -> StoreResult<Vec<String>> {
        match self {
            RuleStore::Memory(m) => Ok(m.rules.keys().cloned().collect()),
            RuleStore::Dir { db_dir } => {
                let dir = Self::rule_dir(db_dir);
                let entries = match fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
                    Err(e) => return Err(e.into()),
                };
                let mut names = Vec::new();
                for entry in entries {
                    let entry = entry?;
                    // Regular files only (no symlinks, no directories).
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let Some(file) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if let Some(name) = file.strip_suffix(".json") {
                        if !name.is_empty() {
                            names.push(name.to_string());
                        }
                    }
                }
                names.sort();
                Ok(names)
            }
        }
    }

    /// The text of rule `name`'s file, `None` when it does not exist. A file
    /// larger than [`MAX_RULE_FILE_BYTES`] is an error.
    pub fn read(&self, name: &str) -> StoreResult<Option<String>> {
        match self {
            RuleStore::Memory(m) => Ok(m.rules.get(name).cloned()),
            RuleStore::Dir { db_dir } => {
                let path = Self::rule_file(db_dir, name);
                match fs::symlink_metadata(&path) {
                    Ok(md) if md.is_file() => {}
                    Ok(_) => return Ok(None),
                    Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
                let mut text = String::new();
                let f = fs::File::open(&path)?;
                let read = f
                    .take(MAX_RULE_FILE_BYTES as u64 + 1)
                    .read_to_string(&mut text);
                match read {
                    Ok(n) if n > MAX_RULE_FILE_BYTES => Err(StoreError(format!(
                        "the reason file is larger than {MAX_RULE_FILE_BYTES} bytes"
                    ))),
                    Ok(_) => Ok(Some(text)),
                    Err(e) if e.kind() == ErrorKind::InvalidData => {
                        Err(StoreError("the reason file is not valid UTF-8".to_string()))
                    }
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Whether rule `name`'s file exists (on disk: anything at its path).
    pub fn exists(&self, name: &str) -> StoreResult<bool> {
        match self {
            RuleStore::Memory(m) => Ok(m.rules.contains_key(name)),
            RuleStore::Dir { db_dir } => {
                match fs::symlink_metadata(Self::rule_file(db_dir, name)) {
                    Ok(_) => Ok(true),
                    Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Create rule `name` with `text`. `Ok(false)` when it already exists.
    pub fn create(&mut self, name: &str, text: &str) -> StoreResult<bool> {
        match self {
            RuleStore::Memory(m) => {
                if m.rules.contains_key(name) {
                    return Ok(false);
                }
                m.rules.insert(name.to_string(), text.to_string());
                Ok(true)
            }
            RuleStore::Dir { db_dir } => {
                Self::ensure_dir(db_dir)?;
                let path = Self::rule_file(db_dir, name);
                #[cfg(test)]
                crash::point();
                let mut f = match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(f) => f,
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
                    Err(e) => return Err(e.into()),
                };
                let written = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
                if let Err(e) = written {
                    drop(f);
                    let _ = fs::remove_file(&path);
                    return Err(e.into());
                }
                fsutil::sync_dir(&Self::rule_dir(db_dir))?;
                Ok(true)
            }
        }
    }

    /// Replace rule `name`'s file with `text` (atomically on disk).
    pub fn write(&mut self, name: &str, text: &str) -> StoreResult<()> {
        match self {
            RuleStore::Memory(m) => {
                m.rules.insert(name.to_string(), text.to_string());
                Ok(())
            }
            RuleStore::Dir { db_dir } => {
                Self::ensure_dir(db_dir)?;
                #[cfg(test)]
                crash::point();
                fsutil::atomic_write(&Self::rule_file(db_dir, name), text.as_bytes())?;
                Ok(())
            }
        }
    }

    /// Remove rule `name` (and its ledger). `Ok(false)` when it did not exist.
    /// The rule file goes first: a rule file is never left without its
    /// ledger (which would make it an upstream rule, see
    /// [`super::manage`]); a ledger left without its rule file is ignored,
    /// and replaced when a rule of the name is added.
    pub fn remove(&mut self, name: &str) -> StoreResult<bool> {
        match self {
            RuleStore::Memory(m) => {
                m.ledgers.remove(name);
                Ok(m.rules.remove(name).is_some())
            }
            RuleStore::Dir { db_dir } => {
                let path = Self::rule_file(db_dir, name);
                #[cfg(test)]
                crash::point();
                let existed = match fs::remove_file(&path) {
                    Ok(()) => {
                        fsutil::sync_dir(&Self::rule_dir(db_dir))?;
                        true
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => false,
                    Err(e) => return Err(e.into()),
                };
                #[cfg(test)]
                crash::point();
                fsutil::remove_file_if_exists(&Self::ledger_file(db_dir, name))?;
                Ok(existed)
            }
        }
    }

    /// Rule `name`'s ledger — the N-Triples lines of the triples it inferred
    /// — or `None` when it has none (never executed here, or disabled).
    pub fn ledger(&self, name: &str) -> StoreResult<Option<BTreeSet<String>>> {
        match self {
            RuleStore::Memory(m) => Ok(m.ledgers.get(name).cloned()),
            RuleStore::Dir { db_dir } => {
                let path = Self::ledger_file(db_dir, name);
                match fs::symlink_metadata(&path) {
                    Ok(md) if md.is_file() => {}
                    Ok(_) => return Ok(None),
                    Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
                let text = fs::read_to_string(&path)?;
                Ok(Some(
                    text.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(str::to_string)
                        .collect(),
                ))
            }
        }
    }

    /// Replace (`Some`) or remove (`None`) rule `name`'s ledger.
    pub fn set_ledger(&mut self, name: &str, ledger: Option<&BTreeSet<String>>) -> StoreResult<()> {
        match self {
            RuleStore::Memory(m) => {
                match ledger {
                    Some(l) => m.ledgers.insert(name.to_string(), l.clone()),
                    None => m.ledgers.remove(name),
                };
                Ok(())
            }
            RuleStore::Dir { db_dir } => {
                let path = Self::ledger_file(db_dir, name);
                #[cfg(test)]
                crash::point();
                let Some(lines) = ledger else {
                    fsutil::remove_file_if_exists(&path)?;
                    return Ok(());
                };
                Self::ensure_dir(db_dir)?;
                let dir = Self::rule_dir(db_dir).join(LEDGER_DIR);
                match fs::create_dir(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
                fsutil::atomic_write_with(&path, |w| {
                    for l in lines {
                        w.write_all(l.as_bytes())?;
                        w.write_all(b"\n")?;
                    }
                    Ok(())
                })?;
                Ok(())
            }
        }
    }

    /// Create `reason_rule_files/` inside the (existing) database directory.
    fn ensure_dir(db_dir: &Path) -> StoreResult<()> {
        if !db_dir.is_dir() {
            return Err(StoreError(NO_DB_DIR.to_string()));
        }
        match fs::create_dir(Self::rule_dir(db_dir)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Take the process-wide rule lock of the database in directory `db_dir`
/// (see [`RuleStore::lock`]).
fn lock_dir(db_dir: &Path) -> StoreLock {
    let state = dir_lock(db_dir);
    {
        let mut held = state.held.lock().unwrap_or_else(|p| p.into_inner());
        while *held {
            held = state.freed.wait(held).unwrap_or_else(|p| p.into_inner());
        }
        *held = true;
    }
    StoreLock { state: Some(state) }
}

/// Make `dst_db_dir/reason_rule_files` a copy of `src_db_dir`'s (removing
/// the destination's when the source has none) — for backups and restores,
/// which carry a database's rules like upstream's whole-directory copies do.
/// Both directories' rule locks are held throughout (taken in a fixed order),
/// so a concurrent `reasonManage` operation is never copied half-done.
pub fn copy_rule_dir(src_db_dir: &Path, dst_db_dir: &Path) -> crate::Result<()> {
    let (src, dst) = (src_db_dir.join(RULE_DIR), dst_db_dir.join(RULE_DIR));
    let key = |d: &Path| fs::canonicalize(d).unwrap_or_else(|_| d.to_path_buf());
    let (src_key, dst_key) = (key(src_db_dir), key(dst_db_dir));
    if src_key == dst_key {
        return Ok(());
    }
    let _locks = if src_key < dst_key {
        (lock_dir(src_db_dir), lock_dir(dst_db_dir))
    } else {
        let d = lock_dir(dst_db_dir);
        (lock_dir(src_db_dir), d)
    };
    if fs::canonicalize(&src)
        .ok()
        .is_some_and(|s| fs::canonicalize(&dst).ok() == Some(s))
    {
        return Ok(());
    }
    fsutil::remove_dir_if_exists(&dst)?;
    if !src.is_dir() {
        return Ok(());
    }
    copy_tree(&src, &dst)?;
    fsutil::sync_dir(dst_db_dir)
}

/// Copy the regular files of `src` (and of its sub-directories) into `dst`:
/// the files first, then the sub-directories — so a rule file another
/// process adds meanwhile (its ledger is written first) is never copied
/// without its ledger.
fn copy_tree(src: &Path, dst: &Path) -> crate::Result<()> {
    fs::create_dir_all(dst)?;
    let mut dirs = Vec::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            dirs.push((entry.path(), to));
        } else if ty.is_file() {
            fs::copy(entry.path(), &to)?;
        }
    }
    for (from, to) in dirs {
        copy_tree(&from, &to)?;
    }
    fsutil::sync_dir(dst)
}

/// The rules (and ledgers) of the database in directory `db_dir`, read into
/// memory under its rule lock — for a database detached from its directory
/// ([`Database::into_memory`](crate::Database::into_memory)), which keeps
/// them in memory from then on. A rule file that cannot be read (larger
/// than [`MAX_RULE_FILE_BYTES`], not UTF-8) is left out, as no operation
/// could use it.
pub fn read_rule_dir(db_dir: &Path) -> crate::Result<MemoryRules> {
    let store = RuleStore::dir(db_dir);
    let _lock = store.lock();
    let mut m = MemoryRules::default();
    let err = |e: StoreError| crate::GStoreError::Database(e.0);
    for name in store.names().map_err(err)? {
        let Ok(Some(text)) = store.read(&name) else {
            continue;
        };
        if let Some(ledger) = store.ledger(&name).map_err(err)? {
            m.ledgers.insert(name.clone(), ledger);
        }
        m.rules.insert(name, text);
    }
    Ok(m)
}

/// Write the in-memory rules `m` into `dst_db_dir/reason_rule_files` (the
/// backup of a database that has no directory).
pub fn write_memory_rules(m: &MemoryRules, dst_db_dir: &Path) -> crate::Result<()> {
    let dir = dst_db_dir.join(RULE_DIR);
    fsutil::remove_dir_if_exists(&dir)?;
    if m.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir.join(LEDGER_DIR))?;
    for (name, text) in &m.rules {
        fsutil::atomic_write(&dir.join(format!("{name}.json")), text.as_bytes())?;
    }
    for (name, lines) in &m.ledgers {
        let mut text = String::new();
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        fsutil::atomic_write(
            &dir.join(LEDGER_DIR).join(format!("{name}.nt")),
            text.as_bytes(),
        )?;
    }
    Ok(())
}

/// A simulated crash for tests: [`arm`](crash::arm)`(n)` makes the `n`-th
/// next change a directory store makes on disk (0: the next one) panic
/// before it happens, on this thread.
#[cfg(test)]
pub(crate) mod crash {
    use std::cell::Cell;

    thread_local! {
        static AT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(crate) fn arm(n: usize) {
        AT.with(|a| a.set(Some(n)));
    }

    pub(crate) fn disarm() {
        AT.with(|a| a.set(None));
    }

    /// Before a change on disk: the simulated crash, when it is due.
    pub(crate) fn point() {
        AT.with(|a| match a.get() {
            Some(0) => {
                a.set(None);
                panic!("simulated crash");
            }
            Some(n) => a.set(Some(n - 1)),
            None => {}
        });
    }
}
