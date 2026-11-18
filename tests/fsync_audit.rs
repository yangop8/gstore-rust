//! The fsync audit (review tests-b#3, tests-a#13): every acknowledged commit
//! point made the file and directory syncs it relies on before it returned.
//!
//! Every crash test kills its process (`SIGKILL`, `abort`, `mem::forget`), so
//! the kernel's page cache survives the "crash" and a missing `fsync` goes
//! unnoticed — a power loss would lose the acknowledged write, or a renamed
//! file whose directory entry was never made durable. These tests watch the
//! syncs instead, through the library's sync probe
//! ([`gstore::sync_probe`]): each one snapshots a temp directory, runs one
//! commit point, snapshots it again and checks the recorded syncs against the
//! difference ([`Audit::finish`]):
//!
//! * every file the operation created or wrote was synced (`fsync` or
//!   `fdatasync`, through any name it had — a staged file renamed into place
//!   keeps its inode) **after its last write**: its length and modification
//!   time at its last sync are its final ones;
//! * every directory whose entries changed (a file created, renamed or
//!   removed in it) was synced after the last change: the entries recorded at
//!   its last sync are its final ones;
//! * a RocksDB store's write-ahead log was synced after the last write to it.
//!
//! On top of that the protocols that need a sync *between* two steps — which
//! no final state shows — are checked in order ([`durable_steps`]: each
//! intermediate state a protocol documents must have been recorded by a
//! directory sync, in order): the pager's write-ahead log
//! ([`pager_commits`]), the snapshot set's commit record
//! ([`snapshot_commits_at`]), the bulk install's `COMMIT` marker and steps
//! ([`bulk_install`]), directory swaps, the copies of a save elsewhere and
//! a backup, and the reasoning rules' ledgers.
//!
//! Commit points covered: auto-commit SPARQL UPDATE (snapshot save, disk
//! Stream and Materialize, RocksDB), `commit()` and the server's `/commit`,
//! a server's `/update` (single-database and `/api` servers), snapshot
//! saves (into a new directory, over a snapshot, over a disk database, a
//! checkpoint), a disk database saved elsewhere, backups and directory
//! copies, the restore swap, the bulk install (disk and RocksDB) and
//! `build_rocksdb_from_str`, a RocksDB save elsewhere, the write-ahead
//! log's create / rename / delete and its replay at open, a VS-tree rebuilt
//! at open, redo- and update-log appends, rule-directory writes and PFN
//! writes.
//!
//! Checked by removing each sync these commit points make, one at a time
//! (an environment-gated skip in the probe, kept out of the tree): every
//! removal fails a test here, but for three syncs no state can miss —
//! `write_vstree_kv`'s directory sync (the pager's write-ahead-log protocol
//! syncs the same directory before the header that calls the tree fresh is
//! written), and in the RocksDB bulk build the write-ahead-log sync right
//! after a memtable flush (the log is empty) and the staging directory's
//! sync (its entry is renamed away and the staging area is discarded by
//! any later build). See the fragment `changelog.d/r5-test-audit.md`.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use gstore::sync_probe::{FileId, Probe, SyncEvent, SyncKind};
use gstore::{Database, OpenOptions, QueryResult};

// ---------------------------------------------------------------------------
// the audit
// ---------------------------------------------------------------------------

/// A temp directory (canonical path) removed when dropped, also when the
/// test fails.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_fsync_audit_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(fs::canonicalize(&p).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One entry of a scanned tree.
#[derive(Clone, Debug)]
struct Node {
    id: FileId,
    dir: bool,
    len: u64,
    modified: SystemTime,
    /// A directory's entries.
    entries: BTreeMap<OsString, Option<FileId>>,
}

/// Every entry under a root, by path relative to it (the root is `""`).
type Tree = BTreeMap<PathBuf, Node>;

fn scan(root: &Path) -> Tree {
    let mut out = Tree::new();
    scan_into(root, root, &mut out);
    out
}

fn scan_into(root: &Path, path: &Path, out: &mut Tree) {
    let Ok(md) = fs::symlink_metadata(path) else {
        return;
    };
    let mut entries = BTreeMap::new();
    if md.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path).unwrap().flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for e in children {
            let id = fs::symlink_metadata(e.path())
                .ok()
                .as_ref()
                .and_then(FileId::of);
            entries.insert(e.file_name(), id);
            scan_into(root, &e.path(), out);
        }
    }
    out.insert(
        path.strip_prefix(root).unwrap().to_path_buf(),
        Node {
            id: FileId::of(&md).unwrap(),
            dir: md.is_dir(),
            len: md.len(),
            modified: md.modified().unwrap(),
            entries,
        },
    );
}

/// Lock files: `<db>/LOCK` (the directory lock) and `<db>.restore-lock`
/// (a restore's side lock). Advisory `flock` targets holding the owner's
/// pid, never data: one lost or left behind by a power cut costs nothing
/// (the next lock recreates it, and no open reads its content), so their
/// creation, removal and content are not synced and not audited.
fn is_lock_file(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name == "LOCK" || name.ends_with(".restore-lock")
}

/// A directory's entries without its lock files.
fn without_locks(
    entries: impl IntoIterator<Item = (OsString, Option<FileId>)>,
) -> BTreeMap<OsString, Option<FileId>> {
    entries
        .into_iter()
        .filter(|(n, _)| !is_lock_file(n))
        .collect()
}

/// The leftovers of a finished swap or install that every recovery removes
/// again (`recover_dir_swap_as`, `recover_restore`, `remove_drop_leftover`,
/// the next bulk build): a directory may lose them after its last sync —
/// should a power cut bring one back, the next open or build deletes it.
/// (The protocol checks below still require the syncs those steps make.)
fn is_recovered_leftover(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    [".old", ".restore-old", ".drop-old", ".tmp", ".restore-tmp"]
        .iter()
        .any(|s| name.ends_with(s))
        || name == ".bulk-tmp"
}

/// What is exempt from the file-by-file check, with the reason.
struct Exempt {
    /// A path relative to the audited root: everything under it (`dir/`),
    /// or files whose path starts with it.
    prefix: &'static str,
    how: How,
    why: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
enum How {
    /// Not audited file by file.
    Unaudited,
    /// Its removal need not be durable: should a power cut bring it back,
    /// it is ignored.
    MayResurrect,
}

impl Exempt {
    fn covers(&self, rel: &Path, how: How) -> bool {
        self.how == how && rel.to_string_lossy().starts_with(self.prefix)
    }
}

/// A store's own files (`audit.db/rocksdb/`), synced by RocksDB itself:
/// checked through the write-ahead-log sync instead ([`Audit::finish`]).
#[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
const ROCKSDB_FILES: Exempt = Exempt {
    prefix: "audit.db/rocksdb/",
    how: How::Unaudited,
    why: "RocksDB's own files: its write-ahead log sync is checked instead",
};

/// Watches a directory across one commit point (see the module docs).
struct Audit {
    root: PathBuf,
    before: Tree,
    probe: Probe,
}

impl Audit {
    fn begin(root: &Path) -> Audit {
        let probe = Probe::watch(root);
        Audit {
            root: root.to_path_buf(),
            before: scan(root),
            probe,
        }
    }

    /// Check what changed under the root since [`begin`](Self::begin) against
    /// the syncs recorded meanwhile (panics with every problem found), and
    /// return those syncs for the protocol checks.
    fn finish(self, what: &str, exempt: &[Exempt]) -> Vec<SyncEvent> {
        let events = self.probe.events();
        let after = scan(&self.root);
        let mut problems = Vec::new();
        let last = |kinds: &[SyncKind], id: FileId| {
            events
                .iter()
                .rev()
                .find(|e| kinds.contains(&e.kind) && e.id == Some(id))
        };
        for (rel, node) in &after {
            let shown = format!("{}", self.root.join(rel).display());
            let as_dir = PathBuf::from(format!("{}/", rel.display()));
            if exempt.iter().any(|x| x.covers(&as_dir, How::Unaudited))
                || rel.file_name().is_some_and(is_lock_file)
            {
                continue;
            }
            let before = self.before.get(rel);
            let same_object = before.is_some_and(|b| b.id == node.id && b.dir == node.dir);
            if node.dir {
                // Entries that are not audited at all (an exemption's whole
                // tree, e.g. a log directory) are left out on every side.
                let audited = |entries: BTreeMap<OsString, Option<FileId>>| {
                    entries
                        .into_iter()
                        .filter(|(n, _)| {
                            let path = rel.join(n);
                            let as_dir = PathBuf::from(format!("{}/", path.display()));
                            !exempt.iter().any(|x| {
                                x.covers(&path, How::Unaudited) || x.covers(&as_dir, How::Unaudited)
                            })
                        })
                        .collect::<BTreeMap<_, _>>()
                };
                let without_locks = |e| audited(without_locks(e));
                let entries = without_locks(node.entries.clone());
                // Entries gone since a state that may come back after a
                // power cut, harmlessly: a recovered leftover, or what an
                // exemption says is ignored.
                let benign = |entries_then: BTreeMap<OsString, Option<FileId>>| {
                    entries_then
                        .into_iter()
                        .filter(|(n, _)| {
                            entries.contains_key(n)
                                || !(is_recovered_leftover(n)
                                    || exempt
                                        .iter()
                                        .any(|x| x.covers(&rel.join(n), How::MayResurrect)))
                        })
                        .collect::<BTreeMap<_, _>>()
                };
                let changed = !same_object
                    || before.is_some_and(|b| benign(without_locks(b.entries.clone())) != entries);
                if !changed {
                    continue;
                }
                match last(&[SyncKind::Dir], node.id) {
                    None => problems.push(format!(
                        "directory {shown}: its entries changed but it was never synced \
                         (before: {:?}, now: {:?})",
                        before.map(|b| without_locks(b.entries.clone())
                            .into_keys()
                            .collect::<Vec<_>>()),
                        entries.keys().collect::<Vec<_>>()
                    )),
                    Some(e) => {
                        let synced = benign(without_locks(e.entries.iter().cloned().collect()));
                        if synced != entries {
                            problems.push(format!(
                                "directory {shown}: its entries changed after its last sync \
                                 (at {}): synced {:?}, now {:?}",
                                e.site(),
                                synced.keys().collect::<Vec<_>>(),
                                entries.keys().collect::<Vec<_>>()
                            ));
                        }
                    }
                }
            } else {
                let changed = !same_object
                    || before.is_some_and(|b| b.len != node.len || b.modified != node.modified);
                if !changed {
                    continue;
                }
                match last(&[SyncKind::File, SyncKind::FileData], node.id) {
                    None => problems.push(format!(
                        "file {shown}: written ({} bytes) but never synced",
                        node.len
                    )),
                    Some(e) if e.len != node.len || e.modified != Some(node.modified) => problems
                        .push(format!(
                            "file {shown}: written after its last sync (at {}: {} bytes, now {})",
                            e.site(),
                            e.len,
                            node.len
                        )),
                    Some(_) => {}
                }
            }
        }
        // A RocksDB store: every write-ahead-log byte written before the
        // commit point returned was covered by its last WAL sync.
        for (rel, node) in &after {
            if rel.file_name().is_none_or(|n| n != "rocksdb") || !node.dir {
                continue;
            }
            let logs: Vec<(PathBuf, u64)> = after
                .iter()
                .filter(|(p, n)| {
                    p.parent() == Some(rel.as_path())
                        && !n.dir
                        && p.extension().is_some_and(|x| x == "log")
                })
                .map(|(p, n)| (self.root.join(p), n.len))
                .collect();
            let changed = logs.iter().any(|(p, len)| {
                let rel = p.strip_prefix(&self.root).unwrap();
                self.before
                    .get(rel)
                    .is_none_or(|b| b.len != *len || b.id != after[rel].id)
            });
            if !changed {
                continue;
            }
            // Every log byte there now was covered by the last sync: a log
            // the sync saw (by file name — a staged store is renamed into
            // place) at least as long. An empty log (a fresh one after a
            // memtable flush) holds nothing to lose.
            let covered = |e: &SyncEvent| {
                logs.iter().filter(|(_, len)| *len > 0).all(|(p, len)| {
                    e.wal_files
                        .iter()
                        .any(|(q, l)| q.file_name() == p.file_name() && l >= len)
                })
            };
            match events
                .iter()
                .rev()
                .find(|e| e.kind == SyncKind::RocksWal && e.id == Some(node.id))
            {
                None if logs.iter().any(|(_, len)| *len > 0) => problems.push(format!(
                    "RocksDB store {}: its write-ahead log grew but was never synced",
                    self.root.join(rel).display()
                )),
                None => {}
                Some(e) if !covered(e) => problems.push(format!(
                    "RocksDB store {}: its write-ahead log was written after its last sync \
                     (at {}): synced {:?}, now {:?}",
                    self.root.join(rel).display(),
                    e.site(),
                    e.wal_files,
                    logs
                )),
                Some(_) => {}
            }
        }
        assert!(
            problems.is_empty(),
            "{what}: the commit point returned with unsynced changes:\n  {}\nsyncs made:\n  {}",
            problems.join("\n  "),
            events
                .iter()
                .map(|e| format!("{:?} {} ({})", e.kind, e.path.display(), e.site()))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
        for x in exempt {
            assert!(!x.why.is_empty());
        }
        events
    }
}

fn is(e: &SyncEvent, kind: SyncKind, path: &Path) -> bool {
    e.kind == kind && e.path == path
}

fn listing(events: &[SyncEvent]) -> String {
    events
        .iter()
        .map(|e| format!("{:?} {} ({})", e.kind, e.path.display(), e.site()))
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// A directory's entries (lock files left out), as a sync recorded them or
/// as they are now.
type Entries = BTreeMap<OsString, Option<FileId>>;

/// The identity of the file or directory at `path` now (`None`: absent).
fn id_of(path: &Path) -> Option<FileId> {
    fs::symlink_metadata(path)
        .ok()
        .as_ref()
        .and_then(FileId::of)
}

/// The identity entry `name` has in `entries` (`None`: absent).
fn id_in(entries: &Entries, name: &str) -> Option<FileId> {
    entries.get(&OsString::from(name)).copied().flatten()
}

fn has(entries: &Entries, name: &str) -> bool {
    entries.contains_key(&OsString::from(name))
}

/// One durable state a protocol must pass through: what it is, and which
/// directory entries show it.
type Step<'a> = (&'a str, Box<dyn Fn(&Entries) -> bool + 'a>);

/// Check that the syncs of directory `dir` among `events` made `steps`
/// durable in order: for each step, a later sync of `dir` than the previous
/// step's recorded entries that show it. This is what the final state
/// cannot show — a sync a protocol needs *between* two of its changes (the
/// old VS-tree gone before the new store arrives, …): with it removed, the
/// next sync shows the later state, and the step is missing. Returns the
/// indexes (into `events`) of the syncs that matched.
fn durable_steps(events: &[SyncEvent], dir: &Path, what: &str, steps: &[Step]) -> Vec<usize> {
    let mut from = 0;
    let mut found = Vec::new();
    for (k, (step, shows)) in steps.iter().enumerate() {
        let hit = events[from..].iter().position(|e| {
            e.kind == SyncKind::Dir
                && e.path == dir
                && shows(&without_locks(e.entries.iter().cloned()))
        });
        let Some(i) = hit else {
            panic!(
                "{what}: step {k} ({step}) was never made durable: no sync of {} after \
                 step {} recorded it. The syncs from there on:\n  {}",
                dir.display(),
                k.saturating_sub(1),
                listing_entries(&events[from..], dir)
            );
        };
        found.push(from + i);
        from += i + 1;
    }
    found
}

/// [`listing`], with the entries each sync of `dir` recorded.
fn listing_entries(events: &[SyncEvent], dir: &Path) -> String {
    events
        .iter()
        .map(|e| {
            let mut line = format!("{:?} {} ({})", e.kind, e.path.display(), e.site());
            if e.kind == SyncKind::Dir && e.path == dir {
                let names: Vec<_> = e
                    .entries
                    .iter()
                    .map(|(n, _)| n.to_string_lossy().into_owned())
                    .collect();
                line.push_str(&format!(" entries {names:?}"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// The page-batch commits of the paged file `file` among `events`, each
/// checked to follow the write-ahead-log protocol exactly: the log staged
/// and synced (`<file>.wal.tmp`), the directory synced (the log's name is
/// durable before the file is touched), the file synced, the directory
/// synced (the log's removal is durable).
fn pager_commits(events: &[SyncEvent], file: &Path) -> usize {
    let staged = PathBuf::from(format!("{}.wal.tmp", file.display()));
    let dir = file.parent().unwrap();
    let mut n = 0;
    for (i, e) in events.iter().enumerate() {
        if !is(e, SyncKind::File, &staged) {
            continue;
        }
        let next = &events[i + 1..];
        let ok = next.len() >= 3
            && is(&next[0], SyncKind::Dir, dir)
            && is(&next[1], SyncKind::File, file)
            && is(&next[2], SyncKind::Dir, dir);
        assert!(
            ok,
            "a page commit of {} broke the write-ahead-log protocol (log synced, directory \
             synced, file synced, directory synced):\n  {}",
            file.display(),
            listing(&events[i..])
        );
        n += 1;
    }
    n
}

/// The members of a snapshot set (`SNAPSHOT_FILES`).
const SNAPSHOT_MEMBERS: [&str; 5] = [
    "dict.bin",
    "store.bin",
    "named.bin",
    "vstree.bin",
    "meta.bin",
];

/// The snapshot-set commits into `dir` among `events`, each checked in
/// order: every member staged and synced before the commit record, then
/// three syncs of the directory right after the record's own — the record
/// named while every member is still staged, every member renamed into
/// place while the record still names them, the record retired (a crash
/// between any two leaves the old set or rolls the new one forward whole).
/// Returns the commits and, for each, the index of its last sync.
fn snapshot_commits_at(events: &[SyncEvent], dir: &Path) -> Vec<usize> {
    let record = dir.join("SNAPSHOT.commit.tmp");
    let mut done = Vec::new();
    let mut from = 0;
    for (i, e) in events.iter().enumerate() {
        if !is(e, SyncKind::File, &record) {
            continue;
        }
        for member in SNAPSHOT_MEMBERS {
            let tmp = dir.join(format!("{member}.tmp"));
            assert!(
                events[from..i].iter().any(|e| is(e, SyncKind::File, &tmp)),
                "snapshot member {member} was not synced before the commit record:\n  {}",
                listing(&events[from..])
            );
        }
        let staged = |x: &Entries, all: bool| {
            SNAPSHOT_MEMBERS
                .iter()
                .all(|m| has(x, &format!("{m}.tmp")) == all)
        };
        let steps: [Step; 3] = [
            (
                "the commit record named, every member still staged",
                Box::new(|x| has(x, "SNAPSHOT.commit") && staged(x, true)),
            ),
            (
                "every member renamed into place, the record still there",
                Box::new(|x| {
                    has(x, "SNAPSHOT.commit")
                        && staged(x, false)
                        && SNAPSHOT_MEMBERS.iter().all(|m| has(x, m))
                }),
            ),
            (
                "the commit record retired",
                Box::new(|x| !has(x, "SNAPSHOT.commit") && staged(x, false)),
            ),
        ];
        let found = durable_steps(&events[i + 1..], dir, "a snapshot commit", &steps);
        assert_eq!(
            found,
            [0, 1, 2],
            "a snapshot commit into {} must sync its directory right after the record, \
             after the members' renames and after the record's removal:\n  {}",
            dir.display(),
            listing_entries(&events[i..], dir)
        );
        done.push(i + 3);
        from = i + 4;
    }
    done
}

/// How many snapshot-set commits into `dir` `events` hold (each checked as
/// [`snapshot_commits_at`] says).
fn snapshot_commits(events: &[SyncEvent], dir: &Path) -> usize {
    snapshot_commits_at(events, dir).len()
}

/// The bulk install into `dir` among `events` (`crate::bulk::commit`'s
/// steps; call right after the build), checked in order: both staged files
/// and the staging directory synced, the `COMMIT` marker synced and the
/// staging directory again; then each step the module docs list as synced
/// made durable on its own — `cleared` (the old `vstree.kv` and log gone,
/// the old `kvstore.kv` still in place), `kv-synced` (the commit point: the
/// new `kvstore.kv`, no VS-tree yet), the replaced rules removed (when
/// `old_rules`), `vstree-placed` (the staging area still there, and the
/// snapshot files when `old_snapshot`), `layouts-removed` (when
/// `old_snapshot`) and `cleaned`.
fn bulk_install(events: &[SyncEvent], dir: &Path, old_rules: bool, old_snapshot: bool) {
    let staging = dir.join(".bulk-tmp");
    let marker = staging.join("COMMIT");
    let i = events
        .iter()
        .position(|e| is(e, SyncKind::File, &staging.join("kvstore.kv")))
        .unwrap_or_else(|| {
            panic!(
                "the staged kvstore.kv was never synced:\n  {}",
                listing(events)
            )
        });
    let rest = &events[i..];
    let expect = [
        (SyncKind::File, staging.join("kvstore.kv")),
        (SyncKind::File, staging.join("vstree.kv")),
        (SyncKind::Dir, staging.clone()),
        (SyncKind::File, marker),
        (SyncKind::Dir, staging.clone()),
    ];
    for (k, (kind, path)) in expect.iter().enumerate() {
        assert!(
            rest.len() > k && is(&rest[k], *kind, path),
            "bulk install step {k} of {}: expected {kind:?} {}:\n  {}",
            dir.display(),
            path.display(),
            listing(rest)
        );
    }
    let new_kv = id_of(&dir.join("kvstore.kv"));
    let new_vs = id_of(&dir.join("vstree.kv"));
    assert!(
        new_kv.is_some() && new_vs.is_some(),
        "the build installed both files"
    );
    let mut steps: Vec<Step> = vec![
        (
            "cleared: the old vstree.kv and kvstore.kv.wal removed, the old kvstore.kv in place",
            Box::new(|x| {
                !has(x, "vstree.kv")
                    && !has(x, "kvstore.kv.wal")
                    && id_in(x, "kvstore.kv") != new_kv
            }),
        ),
        (
            "kv-synced (the commit point): the new kvstore.kv in place, no vstree.kv yet",
            Box::new(|x| id_in(x, "kvstore.kv") == new_kv && !has(x, "vstree.kv")),
        ),
    ];
    if old_rules {
        steps.push((
            "the replaced database's rules removed, before the new vstree.kv is placed",
            Box::new(|x| {
                id_in(x, "kvstore.kv") == new_kv
                    && !has(x, "reason_rule_files")
                    && !has(x, "vstree.kv")
            }),
        ));
    }
    steps.push((
        "vstree-placed: the new vstree.kv in place, the staging area (and any other layout) \
         still there",
        Box::new(move |x| {
            id_in(x, "vstree.kv") == new_vs
                && has(x, ".bulk-tmp")
                && (!old_snapshot || has(x, "meta.bin"))
        }),
    ));
    if old_snapshot {
        steps.push((
            "layouts-removed: the snapshot files gone, the staging area still there",
            Box::new(|x| !has(x, "meta.bin") && !has(x, "store.bin") && has(x, ".bulk-tmp")),
        ));
    }
    steps.push((
        "cleaned: the staging area removed",
        Box::new(|x| {
            !has(x, ".bulk-tmp")
                && id_in(x, "kvstore.kv") == new_kv
                && id_in(x, "vstree.kv") == new_vs
        }),
    ));
    durable_steps(&rest[expect.len()..], dir, "the bulk install", &steps);
}

/// How many times the parent of `target` was synced after the last sync
/// of anything inside `staged` (a staged directory swapped in for `target`:
/// once after its rename to `<target>.new`, once after its rename into
/// place).
fn swaps(events: &[SyncEvent], staged: &Path, target: &Path) -> usize {
    let parent = target.parent().unwrap();
    let last_staged = events
        .iter()
        .rposition(|e| e.path.starts_with(staged))
        .unwrap_or_else(|| panic!("nothing in {} was synced", staged.display()));
    events[last_staged + 1..]
        .iter()
        .filter(|e| is(e, SyncKind::Dir, parent))
        .count()
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const BASE: &str = "<http://ex/s0> <http://ex/p> \"zero\" .\n";

/// A bulk-built disk database at `dir` holding [`BASE`].
fn disk_db(dir: &Path) {
    let rdf = dir.with_extension("nt");
    fs::write(&rdf, BASE).unwrap();
    Database::build_disk(dir, &[&rdf]).unwrap();
    fs::remove_file(&rdf).unwrap();
}

/// A saved snapshot database at `dir` holding [`BASE`].
fn snapshot_db(dir: &Path) {
    Database::build_from_str("audit", BASE)
        .unwrap()
        .save(dir)
        .unwrap();
}

fn insert(i: u32) -> String {
    format!("INSERT DATA {{ <http://ex/s{i}> <http://ex/p> \"v{i}\" }}")
}

fn count(db: &mut Database) -> usize {
    match db.query("SELECT ?s WHERE { ?s <http://ex/p> ?o }").unwrap() {
        QueryResult::Select(rs) => rs.rows.len(),
        other => panic!("expected Select, got {other:?}"),
    }
}

/// One HTTP/1.1 request (`Connection: close`) with a read timeout.
fn request(
    addr: SocketAddr,
    method: &str,
    target: &str,
    auth: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
    if let Some(a) = auth {
        raw.push_str(&format!("Authorization: {a}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (status, body)
}

// ---------------------------------------------------------------------------
// auto-commit SPARQL UPDATE and commit(), library
// ---------------------------------------------------------------------------

fn disk_auto_commit(opts: &OpenOptions, tag: &str) {
    let tmp = TmpDir::new(tag);
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let mut db = Database::open_with(&dir, opts).unwrap();
    for i in 1..=3 {
        let audit = Audit::begin(tmp.path());
        db.query(&insert(i)).unwrap();
        let events = audit.finish(&format!("{tag}: INSERT DATA #{i}"), &[]);
        assert!(
            pager_commits(&events, &dir.join("kvstore.kv")) >= 1,
            "{tag}: the update committed no page batch of kvstore.kv:\n  {}",
            listing(&events)
        );
    }
    assert_eq!(count(&mut db), 4);
}

#[test]
fn stream_auto_commit_updates_sync_their_page_batch() {
    disk_auto_commit(&OpenOptions::stream(), "stream_auto");
}

#[test]
fn materialize_auto_commit_updates_sync_their_page_batch() {
    disk_auto_commit(&OpenOptions::materialize(), "mat_auto");
}

fn disk_commit(opts: &OpenOptions, tag: &str) {
    let tmp = TmpDir::new(tag);
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let mut db = Database::open_with(&dir, opts).unwrap();
    db.begin().unwrap();
    for i in 1..=3 {
        db.query(&insert(i)).unwrap();
    }
    let audit = Audit::begin(tmp.path());
    db.commit().unwrap();
    let events = audit.finish(&format!("{tag}: commit()"), &[]);
    assert!(
        pager_commits(&events, &dir.join("kvstore.kv")) >= 1,
        "{tag}: commit() committed no page batch:\n  {}",
        listing(&events)
    );
    assert_eq!(count(&mut db), 4);
}

#[test]
fn stream_commit_syncs_the_transaction() {
    disk_commit(&OpenOptions::stream(), "stream_commit");
}

#[test]
fn materialize_commit_syncs_the_transaction() {
    disk_commit(&OpenOptions::materialize(), "mat_commit");
}

/// A database whose dirty pages outgrow its cache commits page batches in
/// the middle of one update: each of them follows the protocol, and the
/// update's own commit point syncs what is left.
#[test]
fn a_large_update_syncs_every_intermediate_batch() {
    let tmp = TmpDir::new("large");
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let opts = OpenOptions::stream().with_disk_cache_pages(8);
    let mut db = Database::open_with(&dir, &opts).unwrap();
    let mut update = String::from("INSERT DATA {");
    for i in 1..=3000 {
        update.push_str(&format!(" <http://ex/s{i}> <http://ex/p> \"value {i}\" ."));
    }
    update.push_str(" }");
    let audit = Audit::begin(tmp.path());
    db.query(&update).unwrap();
    let events = audit.finish("a large INSERT DATA", &[]);
    let n = pager_commits(&events, &dir.join("kvstore.kv"));
    assert!(n >= 2, "expected several page batches, got {n}");
}

// ---------------------------------------------------------------------------
// snapshot saves
// ---------------------------------------------------------------------------

/// A save into a new directory makes the directory's own entry durable and
/// commits the snapshot set; a second save replaces it the same way.
#[test]
fn a_snapshot_save_syncs_its_members_record_and_directories() {
    let tmp = TmpDir::new("snapsave");
    let dir = tmp.path().join("nested").join("audit.db");
    let mut db = Database::build_from_str("audit", BASE).unwrap();
    let audit = Audit::begin(tmp.path());
    db.save(&dir).unwrap();
    let events = audit.finish("save() into a new directory", &[]);
    assert_eq!(snapshot_commits(&events, &dir), 1);

    db.query(&insert(1)).unwrap();
    let audit = Audit::begin(tmp.path());
    db.save(&dir).unwrap();
    let events = audit.finish("save() over a snapshot", &[]);
    assert_eq!(snapshot_commits(&events, &dir), 1);
}

/// An opened snapshot database: `checkpoint()` (what a server's check_point
/// save does) re-saves it at home.
#[test]
fn a_snapshot_checkpoint_syncs_the_snapshot_set() {
    let tmp = TmpDir::new("snapckpt");
    let dir = tmp.path().join("audit.db");
    snapshot_db(&dir);
    let mut db = Database::open(&dir).unwrap();
    db.query(&insert(1)).unwrap();
    let audit = Audit::begin(tmp.path());
    db.checkpoint().unwrap();
    let events = audit.finish("checkpoint() of a snapshot database", &[]);
    assert_eq!(snapshot_commits(&events, &dir), 1);
    drop(db);
    assert_eq!(count(&mut Database::open(&dir).unwrap()), 2);
}

// ---------------------------------------------------------------------------
// the HTTP servers: an acknowledged write was synced before the reply
// ---------------------------------------------------------------------------

/// The single-database server: `/update` is answered after its commit
/// point (disk) or check_point save (snapshot) synced it.
#[test]
fn the_single_database_server_syncs_before_it_acknowledges() {
    for (layout, snapshot) in [("disk", false), ("snapshot", true)] {
        let tmp = TmpDir::new(&format!("single_{layout}"));
        let dir = tmp.path().join("audit.db");
        if snapshot {
            snapshot_db(&dir);
        } else {
            disk_db(&dir);
        }
        let db = Database::open(&dir).unwrap();
        let server = Arc::new(
            gstore::server::Server::bind(db, "127.0.0.1:0")
                .unwrap()
                .with_db_dir(&dir),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());
        let audit = Audit::begin(tmp.path());
        let (status, body) = request(addr, "POST", "/update", None, &insert(1));
        assert_eq!(status, 200, "{layout}: {body}");
        let events = audit.finish(&format!("{layout}: POST /update"), &[]);
        if snapshot {
            assert_eq!(snapshot_commits(&events, &dir), 1, "{layout}");
        } else {
            assert!(
                pager_commits(&events, &dir.join("kvstore.kv")) >= 1,
                "{layout}"
            );
        }
        server.shutdown();
        serving.join().unwrap();
    }
}

/// `Basic root:pw`.
const ROOT: &str = "Basic cm9vdDpwdw==";

/// The `/api` server: `/update` and a transaction's `/commit` are answered
/// after their commit points synced them, on both layouts.
#[test]
fn the_api_server_syncs_updates_and_commits_before_it_acknowledges() {
    let tmp = TmpDir::new("api");
    let root = tmp.path().join("root");
    let mut cfg = gstore::config::GStoreConfig::default();
    cfg.ip_deny_path = None;
    let server = Arc::new(
        gstore::http_api::ApiServer::bind(&root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(cfg),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    for layout in ["disk", "snapshot"] {
        let name = format!("a{layout}");
        let dir = root.join(format!("{name}.db"));
        let (s, b) = request(
            addr,
            "POST",
            &format!("/build?db_name={name}&layout={layout}"),
            Some(ROOT),
            BASE,
        );
        assert_eq!(s, 200, "build {layout}: {b}");

        let audit = Audit::begin(&dir);
        let (s, b) = request(
            addr,
            "POST",
            &format!("/update?db_name={name}"),
            Some(ROOT),
            &insert(1),
        );
        assert_eq!(s, 200, "{layout}: {b}");
        let events = audit.finish(&format!("/api server, {layout}: POST /update"), &[]);
        if layout == "snapshot" {
            assert_eq!(snapshot_commits(&events, &dir), 1, "{layout}");
        } else {
            assert!(
                pager_commits(&events, &dir.join("kvstore.kv")) >= 1,
                "{layout}"
            );
        }

        let (s, b) = request(
            addr,
            "POST",
            &format!("/begin?db_name={name}"),
            Some(ROOT),
            "",
        );
        assert_eq!(s, 200, "begin {layout}: {b}");
        let i = b.find("\"TID\":\"").expect("TID") + "\"TID\":\"".len();
        let tid: String = b[i..].chars().take_while(char::is_ascii_digit).collect();
        let (s, b) = request(
            addr,
            "POST",
            &format!("/tquery?db_name={name}&tid={tid}"),
            Some(ROOT),
            &insert(2),
        );
        assert_eq!(s, 200, "tquery {layout}: {b}");
        let audit = Audit::begin(&dir);
        let (s, b) = request(
            addr,
            "POST",
            &format!("/commit?db_name={name}&tid={tid}"),
            Some(ROOT),
            "",
        );
        assert_eq!(s, 200, "commit {layout}: {b}");
        let events = audit.finish(&format!("/api server, {layout}: POST /commit"), &[]);
        if layout == "snapshot" {
            assert_eq!(snapshot_commits(&events, &dir), 1, "{layout}");
        } else {
            assert!(
                pager_commits(&events, &dir.join("kvstore.kv")) >= 1,
                "{layout}"
            );
        }
    }
    server.shutdown();
    serving.join().unwrap();
}

/// The server's own logs (under `root/`): appended, not commit points.
const SERVER_LOGS: [Exempt; 2] = [
    Exempt {
        prefix: "root/access.log",
        how: How::Unaudited,
        why: "the access log: appended, never acknowledged as durable",
    },
    Exempt {
        prefix: "root/logs/",
        how: How::Unaudited,
        why: "the query and API logs: appended, never acknowledged as durable",
    },
];

/// A server's backups, REST `/backup` and the `/api` `backup` operation: the
/// backup (its files, its directory's own entry, the backups directory's)
/// is durable before the server answers success.
#[test]
fn the_api_server_syncs_a_backup_before_it_acknowledges() {
    let tmp = TmpDir::new("api_backup");
    let root = tmp.path().join("root");
    let mut cfg = gstore::config::GStoreConfig::default();
    cfg.ip_deny_path = None;
    let server = Arc::new(
        gstore::http_api::ApiServer::bind(&root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(cfg),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (s, b) = request(
        addr,
        "POST",
        "/build?db_name=bak&layout=disk",
        Some(ROOT),
        BASE,
    );
    assert_eq!(s, 200, "build: {b}");
    let audit = Audit::begin(tmp.path());
    let (s, b) = request(addr, "POST", "/backup?db_name=bak", Some(ROOT), "");
    assert_eq!(s, 200, "REST /backup: {b}");
    audit.finish("REST /backup", &SERVER_LOGS);
    // The `/api` operation creates the backups directory itself (both
    // protocols back up into `<root>/backups`): each path's directory
    // creation is checked on its own.
    fs::remove_dir_all(root.join("backups")).unwrap();
    let audit = Audit::begin(tmp.path());
    let (s, b) = request(
        addr,
        "POST",
        "/api",
        None,
        r#"{"operation":"backup","db_name":"bak","username":"root","password":"pw"}"#,
    );
    assert_eq!(s, 200, "/api backup: {b}");
    assert!(b.contains("\"StatusCode\":0"), "/api backup: {b}");
    audit.finish("/api backup", &SERVER_LOGS);
    server.shutdown();
    serving.join().unwrap();
}

/// What an `/api` `build` writes beside the database, not part of it:
/// upstream's build-cost file and the parse error log (both written once the
/// database is complete, neither read back as data).
const BUILD_NOTES: [Exempt; 4] = [
    Exempt {
        prefix: "root/apisnap.db/success",
        how: How::Unaudited,
        why: "the build's cost in milliseconds (shown by monitor, 0 when missing)",
    },
    Exempt {
        prefix: "root/apisnap.db/parse_error.log",
        how: How::Unaudited,
        why: "the build's parse error log: diagnostics, not data",
    },
    Exempt {
        prefix: "root/apidisk.db/success",
        how: How::Unaudited,
        why: "the build's cost in milliseconds (shown by monitor, 0 when missing)",
    },
    Exempt {
        prefix: "root/apidisk.db/parse_error.log",
        how: How::Unaudited,
        why: "the build's parse error log: diagnostics, not data",
    },
];

/// A database the `/api` server creates — REST `/build` (both layouts),
/// REST `/upload` into a new name, the `/api` `build` operation (both
/// layouts) — or restores from a backup (REST `/restore` over a database
/// and into a new name, the `/api` `restore` operation) is durable, its
/// directory's entry included, before the server answers success. (review
/// tests-b#3: `/upload` into a new name created the directory without
/// syncing its parent.)
#[test]
fn databases_the_api_server_creates_or_restores_are_durable_before_it_answers() {
    let tmp = TmpDir::new("api_create");
    let root = tmp.path().join("root");
    let mut cfg = gstore::config::GStoreConfig::default();
    cfg.ip_deny_path = None;
    let server = Arc::new(
        gstore::http_api::ApiServer::bind(&root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(cfg),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    // The `/api` build reads its source from the upload directory.
    fs::create_dir_all(root.join("upload")).unwrap();
    let rdf = root.join("upload").join("base.nt");
    fs::write(&rdf, BASE).unwrap();
    let api_build = |db: &str, layout: &str| {
        format!(
            r#"{{"operation":"build","db_name":"{db}","db_path":"{}","layout":"{layout}","username":"root","password":"pw"}}"#,
            rdf.display()
        )
    };
    let backup = root.join("backups").join("restdisk.backup");
    let steps: [(&str, String, Option<&str>, String); 8] = [
        (
            "REST /build, snapshot layout",
            "/build?db_name=restsnap&layout=snapshot".into(),
            Some(ROOT),
            BASE.into(),
        ),
        (
            "REST /build, disk layout",
            "/build?db_name=restdisk&layout=disk".into(),
            Some(ROOT),
            BASE.into(),
        ),
        (
            "REST /upload into a new name",
            "/upload?db_name=uploaded&format=ntriples".into(),
            Some(ROOT),
            BASE.into(),
        ),
        (
            "/api build, snapshot layout",
            "/api".into(),
            None,
            api_build("apisnap", "snapshot"),
        ),
        (
            "/api build, disk layout",
            "/api".into(),
            None,
            api_build("apidisk", "disk"),
        ),
        (
            "REST /restore over the database",
            format!("/restore?db_name=restdisk&path={}", backup.display()),
            Some(ROOT),
            String::new(),
        ),
        (
            "REST /restore into a new name",
            format!("/restore?db_name=restored&path={}", backup.display()),
            Some(ROOT),
            String::new(),
        ),
        (
            "/api restore over the database",
            "/api".into(),
            None,
            format!(
                r#"{{"operation":"restore","db_name":"restdisk","backup_path":"{}","username":"root","password":"pw"}}"#,
                backup.display()
            ),
        ),
    ];
    let exempt: Vec<Exempt> = SERVER_LOGS.into_iter().chain(BUILD_NOTES).collect();
    for (what, target, auth, body) in steps {
        if what == "REST /restore over the database" {
            let (s, b) = request(addr, "POST", "/backup?db_name=restdisk", Some(ROOT), "");
            assert_eq!(s, 200, "REST /backup: {b}");
            assert!(backup.is_dir(), "the backup is at {}", backup.display());
        }
        let audit = Audit::begin(tmp.path());
        let (s, b) = request(addr, "POST", &target, auth, &body);
        assert_eq!(s, 200, "{what}: {b}");
        assert!(
            target != "/api" || b.contains("\"StatusCode\":0"),
            "{what}: {b}"
        );
        audit.finish(what, &exempt);
    }
    server.shutdown();
    serving.join().unwrap();
}

// ---------------------------------------------------------------------------
// restore swap, bulk install
// ---------------------------------------------------------------------------

/// A restore stages the backup beside the target, synced, and swaps it in
/// with the parent synced after each of the swap's renames.
#[test]
fn a_restore_syncs_the_staged_copy_and_both_swap_renames() {
    for layout in ["snapshot", "disk"] {
        let tmp = TmpDir::new(&format!("restore_{layout}"));
        let src = tmp.path().join("src.db");
        if layout == "snapshot" {
            snapshot_db(&src);
        } else {
            disk_db(&src);
        }
        let bak = tmp.path().join("bak");
        Database::open(&src).unwrap().backup(&bak).unwrap();
        let target = tmp.path().join("target.db");
        for round in ["into a new directory", "over the database"] {
            let audit = Audit::begin(tmp.path());
            Database::restore_into(&bak, &target).unwrap();
            let events = audit.finish(&format!("{layout}: restore {round}"), &[]);
            let staged = tmp.path().join("target.db.restore-tmp");
            assert_eq!(
                swaps(&events, &staged, &target),
                2,
                "{layout}: restore {round}: the parent was not synced after both renames \
                 of the swap:\n  {}",
                listing(&events)
            );
            assert_eq!(count(&mut Database::open(&target).unwrap()), 1);
        }
    }
}

/// The bulk install (review durability#12: the `COMMIT` marker), into a
/// new directory, over a disk database, over one with reasoning rules
/// (which the install removes) and over a snapshot database (whose files
/// the install removes, after making the staged store newer).
#[test]
fn a_bulk_install_syncs_every_step() {
    let tmp = TmpDir::new("bulk");
    let rdf = tmp.path().join("data.nt");
    fs::write(
        &rdf,
        format!("{BASE}<http://ex/s1> <http://ex/p> \"one\" .\n"),
    )
    .unwrap();
    let dir = tmp.path().join("audit.db");
    for round in [
        "a new directory",
        "a disk database",
        "a disk database with rules",
        "a snapshot database",
    ] {
        let rules = round == "a disk database with rules";
        if rules {
            let mut db = Database::open(&dir).unwrap();
            db.reason_add(&gstore::json::parse(RULE).unwrap()).unwrap();
            assert!(dir.join("reason_rule_files").is_dir());
        }
        if round == "a snapshot database" {
            fs::remove_dir_all(&dir).unwrap();
            snapshot_db(&dir);
        }
        let audit = Audit::begin(tmp.path());
        Database::build_disk(&dir, &[&rdf]).unwrap();
        let events = audit.finish(&format!("bulk build into {round}"), &[]);
        bulk_install(&events, &dir, rules, round == "a snapshot database");
        assert_eq!(count(&mut Database::open(&dir).unwrap()), 2, "{round}");
    }
}

/// An in-memory database saved over a disk database: the snapshot set is
/// committed (its record retired durably) while the disk layout is still
/// there, and only then is the disk layout removed — durably.
#[test]
fn a_snapshot_save_over_a_disk_database_syncs_each_step() {
    let tmp = TmpDir::new("snap_over_disk");
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let mut db = Database::build_from_str("audit", BASE).unwrap();
    db.query(&insert(1)).unwrap();
    let audit = Audit::begin(tmp.path());
    db.save(&dir).unwrap();
    let events = audit.finish("save() over a disk database", &[]);
    let at = snapshot_commits_at(&events, &dir);
    assert_eq!(at.len(), 1, "{}", listing(&events));
    let steps: [Step; 2] = [
        (
            "the snapshot committed, its record retired, the disk layout still there",
            Box::new(|x| !has(x, "SNAPSHOT.commit") && has(x, "meta.bin") && has(x, "kvstore.kv")),
        ),
        (
            "the disk layout removed",
            Box::new(|x| !has(x, "kvstore.kv") && !has(x, "vstree.kv") && has(x, "meta.bin")),
        ),
    ];
    durable_steps(&events, &dir, "save() over a disk database", &steps);
    drop(db);
    assert_eq!(count(&mut Database::open(&dir).unwrap()), 2);
}

/// A disk database saved into another directory (twice: the second save
/// replaces the first copy): what would pair with the new store — the old
/// copy's VS-tree and logs — is removed durably before the new store lands.
#[test]
fn a_disk_save_elsewhere_syncs_each_step() {
    let tmp = TmpDir::new("disk_save");
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let mut db = Database::open(&dir).unwrap();
    let copy = tmp.path().join("copy.db");
    for round in ["into a new directory", "over the earlier copy"] {
        db.query(&insert(if round == "into a new directory" {
            1
        } else {
            2
        }))
        .unwrap();
        let old_kv = id_of(&copy.join("kvstore.kv"));
        let audit = Audit::begin(tmp.path());
        db.save(&copy).unwrap();
        let events = audit.finish(&format!("disk save() {round}"), &[]);
        let new_kv = id_of(&copy.join("kvstore.kv"));
        let steps: [Step; 2] = [
            (
                "the old copy's vstree.kv and logs removed, its kvstore.kv still there",
                Box::new(|x| {
                    !has(x, "vstree.kv")
                        && !has(x, "kvstore.kv.wal")
                        && id_in(x, "kvstore.kv") == old_kv
                }),
            ),
            (
                "the new kvstore.kv in place",
                Box::new(|x| id_in(x, "kvstore.kv") == new_kv),
            ),
        ];
        durable_steps(&events, &copy, &format!("disk save() {round}"), &steps);
    }
    let n = count(&mut db);
    drop(db);
    assert_eq!(count(&mut Database::open(&copy).unwrap()), n);
}

/// A backup (`Database::backup`) and a directory copy
/// (`Database::backup_dir`, what a restore stages and `/api` backups use):
/// into a new directory, the directory's own entry is durable; over an older
/// backup, the stale VS-tree and logs go durably before the new store lands.
#[test]
fn backups_sync_the_copy_and_its_directory() {
    let tmp = TmpDir::new("backup");
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let mut db = Database::open(&dir).unwrap();
    // With reasoning rules, which a copy replaces through a directory swap.
    db.reason_add(&gstore::json::parse(RULE).unwrap()).unwrap();
    let bak = tmp.path().join("nested").join("bak");
    let audit = Audit::begin(tmp.path());
    db.backup(&bak).unwrap();
    audit.finish("backup() into a new directory", &[]);
    db.query(&insert(1)).unwrap();
    db.sync().unwrap();
    drop(db);
    let copy = tmp.path().join("copies").join("copy");
    for round in ["into a new directory", "over an older copy"] {
        let src = if round == "into a new directory" {
            &bak
        } else {
            &dir
        };
        let old_kv = id_of(&copy.join("kvstore.kv"));
        let audit = Audit::begin(tmp.path());
        Database::backup_dir(src, &copy).unwrap();
        let events = audit.finish(&format!("backup_dir() {round}"), &[]);
        let new_kv = id_of(&copy.join("kvstore.kv"));
        let steps: [Step; 2] = [
            (
                "stale files removed, the old kvstore.kv still there",
                Box::new(|x| !has(x, "vstree.kv") && id_in(x, "kvstore.kv") == old_kv),
            ),
            (
                "the new kvstore.kv in place",
                Box::new(|x| id_in(x, "kvstore.kv") == new_kv),
            ),
        ];
        durable_steps(&events, &copy, &format!("backup_dir() {round}"), &steps);
        // The copy returns with its directory exactly as durable as it is —
        // the rule swap's leftover (`reason_rule_files.old`) removal too.
        let last = events
            .iter()
            .rev()
            .find(|e| e.kind == SyncKind::Dir && e.path == copy)
            .expect("the copy's directory was synced");
        let now: Entries = without_locks(
            fs::read_dir(&copy)
                .unwrap()
                .flatten()
                .map(|e| (e.file_name(), id_of(&e.path()))),
        );
        assert_eq!(
            without_locks(last.entries.iter().cloned()),
            now,
            "backup_dir() {round}: the copy's directory changed after its last sync:\n  {}",
            listing_entries(&events, &copy)
        );
    }
    assert_eq!(count(&mut Database::open(&copy).unwrap()), 2);
}

/// An open that rebuilds a missing `vstree.kv` (Stream mode, `vstree_on_open`)
/// syncs the new tree file before the store's header calls it fresh.
#[test]
fn a_vstree_rebuild_at_open_syncs_the_tree() {
    let tmp = TmpDir::new("vsrebuild");
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    fs::remove_file(dir.join("vstree.kv")).unwrap();
    let opts = OpenOptions::stream().with_vstree_on_open(true);
    let audit = Audit::begin(tmp.path());
    let mut db = Database::open_with(&dir, &opts).unwrap();
    let events = audit.finish("an open rebuilding vstree.kv", &[]);
    assert!(dir.join("vstree.kv").is_file());
    assert!(
        pager_commits(&events, &dir.join("kvstore.kv")) >= 1,
        "the fresh flag was committed:\n  {}",
        listing(&events)
    );
    assert_eq!(count(&mut db), 1);
}

// ---------------------------------------------------------------------------
// redo / update logs, rules, PFN
// ---------------------------------------------------------------------------

/// The redo and update logs: each commit's records are `fdatasync`ed, and
/// a log's creation makes its directory entry durable.
#[test]
fn redo_and_update_log_appends_are_synced() {
    for layout in ["disk", "snapshot"] {
        let tmp = TmpDir::new(&format!("logs_{layout}"));
        let dir = tmp.path().join("audit.db");
        if layout == "disk" {
            disk_db(&dir);
        } else {
            snapshot_db(&dir);
        }
        let mut db = Database::open(&dir).unwrap();
        db.enable_redo_log_in(&dir).unwrap();
        db.enable_update_log_in(&dir).unwrap();
        for i in 1..=2 {
            let audit = Audit::begin(tmp.path());
            db.query(&insert(i)).unwrap();
            db.sync().unwrap();
            let events = audit.finish(&format!("{layout}: INSERT DATA #{i} with logs"), &[]);
            assert!(
                events.iter().any(|e| e.kind == SyncKind::FileData),
                "{layout}: no log append was synced:\n  {}",
                listing(&events)
            );
        }
    }
}

const RULE: &str = r#"{"rulename":"R","type":1,"logic":1,
    "conditions":[{"patterns":[{"subject":"?x","predicate":"<http://ex/p>","object":"?y"}]}],
    "return":{"source":"?x","target":"?y","label":"R"}}"#;

/// Reasoning rules kept in the database directory: adding one creates
/// `reason_rule_files/` and the rule's file, compiling rewrites it,
/// executing writes its ledger (`inferred/`), deleting removes it — each
/// durably.
#[test]
fn rule_directory_writes_are_synced() {
    let tmp = TmpDir::new("rules");
    let dir = tmp.path().join("audit.db");
    disk_db(&dir);
    let mut db = Database::open(&dir).unwrap();
    let rule = gstore::json::parse(RULE).unwrap();

    let rules = dir.join("reason_rule_files");
    let ledgers = rules.join("inferred");
    let audit = Audit::begin(tmp.path());
    db.reason_add(&rule).unwrap();
    let events = audit.finish("reason_add", &[]);
    // The rule's (empty) ledger is durable — its file and the new
    // `inferred/` directory's entry — before the rule file is created: a
    // rule file without its ledger would be taken for an upstream rule.
    let ledgers_id = id_of(&ledgers);
    let ledger = durable_steps(
        &events,
        &ledgers,
        "reason_add",
        &[("the ledger written", Box::new(|x| has(x, "R.nt")))],
    )[0];
    let in_rules = durable_steps(
        &events,
        &rules,
        "reason_add",
        &[
            (
                "inferred/ created, no rule file yet",
                Box::new(|x| id_in(x, "inferred") == ledgers_id && !has(x, "R.json")),
            ),
            ("the rule file created", Box::new(|x| has(x, "R.json"))),
        ],
    );
    assert!(
        ledger < in_rules[1],
        "reason_add: the rule file was made durable before its ledger:\n  {}",
        listing(&events)
    );
    let audit = Audit::begin(tmp.path());
    db.reason_compile("R").unwrap();
    audit.finish("reason_compile", &[]);
    let ledger_before: Entries = without_locks(
        fs::read_dir(&ledgers)
            .unwrap()
            .flatten()
            .map(|e| (e.file_name(), id_of(&e.path()))),
    );
    let audit = Audit::begin(tmp.path());
    db.reason_execute("R").unwrap();
    let events = audit.finish("reason_execute", &[]);
    // The intended ledger is durable before the insert's first page batch:
    // a power cut after the insert must never leave an inferred triple no
    // rule owns.
    let insert = events
        .iter()
        .position(|e| is(e, SyncKind::File, &dir.join("kvstore.kv.wal.tmp")))
        .unwrap_or_else(|| {
            panic!(
                "the inferred triples were never committed:\n  {}",
                listing(&events)
            )
        });
    let empty_ledger = id_in(&ledger_before, "R.nt");
    durable_steps(
        &events[..insert],
        &ledgers,
        "reason_execute",
        &[(
            "the intended ledger written",
            Box::new(|x| has(x, "R.nt") && id_in(x, "R.nt") != empty_ledger),
        )],
    );
    let audit = Audit::begin(tmp.path());
    db.reason_disable("R").unwrap();
    audit.finish("reason_disable", &[]);
    let audit = Audit::begin(tmp.path());
    db.reason_delete("R").unwrap();
    audit.finish("reason_delete", &[ORPHAN_LEDGERS]);
}

/// `RuleStore::remove`: the rule file goes first (its removal synced); a
/// ledger left without its rule file is ignored and replaced when a rule
/// of the name is added.
const ORPHAN_LEDGERS: Exempt = Exempt {
    prefix: "audit.db/reason_rule_files/inferred/",
    how: How::MayResurrect,
    why: "a ledger without its rule file is ignored (RuleStore::remove)",
};

/// PFN definitions: creating a user's first function creates its
/// directories and files durably; updating and deleting rewrite them.
#[test]
fn pfn_writes_are_synced() {
    use gstore::pfn::{url_encode, PfnInfo, PfnRegistry};
    let tmp = TmpDir::new("pfn");
    let reg = PfnRegistry::new(tmp.path().join("pfn"));
    let info = |body: &str| PfnInfo {
        name: "f".into(),
        body: url_encode(body),
        subs: url_encode(""),
        ..PfnInfo::default()
    };
    let audit = Audit::begin(tmp.path());
    reg.create("alice", info("(module)")).unwrap();
    audit.finish("PFN create", &[]);
    let audit = Audit::begin(tmp.path());
    reg.update("alice", info("(module (func))")).unwrap();
    audit.finish("PFN update", &[]);
    // Compiling writes the user's first module: its new `lib/` directory
    // is durable too (only with a runtime to compile with).
    #[cfg(feature = "pfn-wasm")]
    {
        let g = PfnInfo {
            name: "g".into(),
            body: url_encode("(i32.const 1)"),
            subs: url_encode(""),
            ..PfnInfo::default()
        };
        reg.create("alice", g).unwrap();
        let audit = Audit::begin(tmp.path());
        reg.compile("alice", "g").unwrap().unwrap();
        audit.finish("PFN compile, the user's first module", &[]);
    }
    let audit = Audit::begin(tmp.path());
    reg.delete("alice", "f").unwrap();
    audit.finish(
        "PFN delete",
        &[Exempt {
            prefix: "pfn/",
            how: How::MayResurrect,
            why: "data.json decides: a source or module file no entry names is ignored, \
                  and replaced when a function of the name is created",
        }],
    );
}

// ---------------------------------------------------------------------------
// write-ahead-log recovery
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod wal_recovery {
    use super::*;

    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }

    extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        fn signal(signum: i32, handler: usize) -> usize;
    }

    const RLIMIT_FSIZE: i32 = 1;
    const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;

    /// The environment variable that makes the test binary the child of
    /// [`an_open_syncs_the_batch_it_replays_from_the_log`].
    const WAL_CHILD: &str = "GSTORE_FSYNC_AUDIT_WAL_CHILD";

    /// The child: open the disk database at `dir`, cap the file size at
    /// `kvstore.kv`'s (`RLIMIT_FSIZE`, `SIGXFSZ` ignored — what a full disk
    /// does to a file that must grow) and commit an update that appends pages.
    /// Its write-ahead log fits under the cap, so the commit gets as far as
    /// naming the log; applying the batch then fails, leaving the log for the
    /// next open to replay. Exits 0 once the log is there.
    fn wal_child(dir: &Path) -> ! {
        let mut db = Database::open_with(dir, &OpenOptions::stream()).unwrap();
        let size = fs::metadata(dir.join("kvstore.kv")).unwrap().len();
        let rl = RLimit {
            cur: size,
            max: size,
        };
        // SAFETY: plain system calls on values this function owns.
        unsafe {
            assert_eq!(setrlimit(RLIMIT_FSIZE, &rl), 0);
            signal(SIGXFSZ, SIG_IGN);
        }
        let mut update = String::from("INSERT DATA {");
        for i in 0..300 {
            update.push_str(&format!(
                " <http://ex/new{i}> <http://ex/p> \"{}\" .",
                "x".repeat(200)
            ));
        }
        update.push_str(" }");
        let r = db.query(&update);
        let logged = dir.join("kvstore.kv.wal").is_file();
        eprintln!("child: update {r:?}, log left: {logged}");
        std::process::exit(if r.is_err() && logged { 0 } else { 3 });
    }

    /// Opening a store whose last commit failed after its write-ahead log got
    /// its name replays the batch: the replayed file is synced before the log
    /// is removed, and the removal is made durable (a log resurrected by a
    /// power cut would replay old pages over later commits).
    #[test]
    fn an_open_syncs_the_batch_it_replays_from_the_log() {
        if let Some(dir) = std::env::var_os(WAL_CHILD) {
            wal_child(Path::new(&dir));
        }
        let tmp = TmpDir::new("walrec");
        let dir = tmp.path().join("audit.db");
        let rdf = tmp.path().join("base.nt");
        let mut base = String::new();
        for i in 0..20_000 {
            base.push_str(&format!("<http://ex/s{i}> <http://ex/p> \"value {i}\" .\n"));
        }
        fs::write(&rdf, base).unwrap();
        Database::build_disk(&dir, &[&rdf]).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wal_recovery::an_open_syncs_the_batch_it_replays_from_the_log",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(WAL_CHILD, &dir)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "the child left no write-ahead log: {status}"
        );
        assert!(
            dir.join("kvstore.kv.wal").is_file(),
            "the child left its log"
        );
        let audit = Audit::begin(tmp.path());
        let mut db = Database::open_with(&dir, &OpenOptions::stream()).unwrap();
        let events = audit.finish("an open replaying a write-ahead log", &[]);
        let kv = dir.join("kvstore.kv");
        let replayed = events
            .iter()
            .position(|e| is(e, SyncKind::File, &kv))
            .unwrap_or_else(|| panic!("the replayed file was not synced:\n  {}", listing(&events)));
        assert!(
            events[replayed + 1..]
                .iter()
                .any(|e| is(e, SyncKind::Dir, &dir)),
            "the log's removal was not made durable:\n  {}",
            listing(&events)
        );
        assert!(!dir.join("kvstore.kv.wal").exists());
        assert_eq!(count(&mut db), 20_300, "the replay applied the whole batch");
    }
}

// ---------------------------------------------------------------------------
// RocksDB
// ---------------------------------------------------------------------------

#[cfg(feature = "rocksdb")]
mod rocks {
    use super::*;

    /// An auto-commit update and a transaction's commit sync RocksDB's
    /// write-ahead log after their last write.
    #[test]
    fn rocksdb_commit_points_sync_the_write_ahead_log() {
        let tmp = TmpDir::new("rocks");
        let dir = tmp.path().join("audit.db");
        Database::build_rocksdb_from_str("audit", &dir, BASE).unwrap();
        let mut db = Database::open(&dir).unwrap();
        for i in 1..=2 {
            let audit = Audit::begin(tmp.path());
            db.query(&insert(i)).unwrap();
            let events = audit.finish(&format!("RocksDB: INSERT DATA #{i}"), &[ROCKSDB_FILES]);
            assert!(
                events.iter().any(|e| e.kind == SyncKind::RocksWal),
                "no WAL sync:\n  {}",
                listing(&events)
            );
        }
        db.begin().unwrap();
        db.query(&insert(3)).unwrap();
        let audit = Audit::begin(tmp.path());
        db.commit().unwrap();
        let events = audit.finish("RocksDB: commit()", &[ROCKSDB_FILES]);
        assert!(
            events.iter().any(|e| e.kind == SyncKind::RocksWal),
            "no WAL sync:\n  {}",
            listing(&events)
        );
        assert_eq!(count(&mut db), 4);
    }

    /// The two RocksDB builds, into a new directory and over the database
    /// they built before: the directory's own entry is durable, the staged
    /// store synced (RocksDB's write-ahead log, and the staged tree) before
    /// it is swapped in, the parent synced after the rename to
    /// `rocksdb.new` (the old store still in place) and after the rename
    /// into place.
    ///
    /// `Database::build_rocksdb_from_str` stages the store in `rocksdb.tmp`;
    /// the bulk build (`gstore::bulk::build`) in `.bulk-tmp/rocksdb`, and
    /// removes the staging area durably at the end.
    #[test]
    fn rocksdb_builds_sync_their_install() {
        use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget};
        use gstore::parser::import::{ImportOptions, RdfFormat};
        for path in ["build_rocksdb_from_str", "bulk::build"] {
            let tmp = TmpDir::new("rocks_build");
            let dir = tmp.path().join("audit.db");
            for round in ["into a new directory", "over the database"] {
                let old = id_of(&dir.join("rocksdb"));
                let what = format!("RocksDB {path} {round}");
                let audit = Audit::begin(tmp.path());
                if path == "bulk::build" {
                    bulk::build(
                        &dir,
                        "audit",
                        BulkInput::Text {
                            text: BASE,
                            format: RdfFormat::Turtle,
                            name: "base.nt",
                        },
                        &ImportOptions::strict(),
                        &mut (),
                        BulkTarget::RocksDb,
                        &BulkOptions::default(),
                    )
                    .unwrap();
                } else {
                    drop(Database::build_rocksdb_from_str("audit", &dir, BASE).unwrap());
                }
                let events = audit.finish(&what, &[ROCKSDB_FILES]);
                let new = id_of(&dir.join("rocksdb"));
                assert!(
                    new.is_some() && new != old,
                    "{what}: a new store is in place"
                );
                assert!(
                    events.iter().any(|e| e.kind == SyncKind::RocksWal),
                    "{what}: the staged store's log was never synced:\n  {}",
                    listing(&events)
                );
                let mut steps: Vec<Step> = vec![
                    (
                        "the staged store named rocksdb.new, the old store (if any) in place",
                        Box::new(|x| id_in(x, "rocksdb.new") == new && id_in(x, "rocksdb") == old),
                    ),
                    (
                        "the new store in place",
                        Box::new(|x| id_in(x, "rocksdb") == new && !has(x, "rocksdb.new")),
                    ),
                ];
                if path == "bulk::build" {
                    steps.push((
                        "the staging area removed",
                        Box::new(|x| !has(x, ".bulk-tmp")),
                    ));
                }
                let at = durable_steps(&events, &dir, &what, &steps);
                // The staged tree (the directory that became `rocksdb/`, and
                // the files in it) synced before it was published.
                let staged = if path == "bulk::build" {
                    dir.join(".bulk-tmp").join("rocksdb")
                } else {
                    dir.join("rocksdb.tmp")
                };
                let staged_dir_synced = events[..at[0]]
                    .iter()
                    .any(|e| is(e, SyncKind::Dir, &staged) && e.id == new);
                let staged_files_synced = events[..at[0]]
                    .iter()
                    .any(|e| e.kind == SyncKind::File && e.path.starts_with(&staged));
                assert!(
                    staged_dir_synced && staged_files_synced,
                    "{what}: the staged store was not synced before its swap \
                     (directory: {staged_dir_synced}, files: {staged_files_synced}):\n  {}",
                    listing(&events)
                );
                assert_eq!(count(&mut Database::open(&dir).unwrap()), 1, "{what}");
            }
        }
    }

    /// A RocksDB database saved into another directory (twice: the second
    /// save replaces the first copy): the checkpoint is written to
    /// `rocksdb.tmp` — RocksDB's `CreateCheckpoint` syncs its files and its
    /// directory itself, out of the probe's sight — and swapped in, the
    /// parent synced after the rename to `rocksdb.new` (the old copy still
    /// in place) and after the rename into place.
    #[test]
    fn a_rocksdb_save_elsewhere_syncs_the_swap() {
        let tmp = TmpDir::new("rocks_save");
        let dir = tmp.path().join("audit.db");
        Database::build_rocksdb_from_str("audit", &dir, BASE).unwrap();
        let mut db = Database::open(&dir).unwrap();
        let copy = tmp.path().join("copy.db");
        for round in ["into a new directory", "over the earlier copy"] {
            db.query(&insert(if round == "into a new directory" {
                1
            } else {
                2
            }))
            .unwrap();
            let old = id_of(&copy.join("rocksdb"));
            let audit = Audit::begin(tmp.path());
            db.save(&copy).unwrap();
            let events = audit.finish(
                &format!("RocksDB: save() {round}"),
                &[
                    // The checkpoint flushes the source's memtables.
                    ROCKSDB_FILES,
                    Exempt {
                        prefix: "copy.db/rocksdb/",
                        how: How::Unaudited,
                        why: "the checkpoint's files, written and synced by RocksDB",
                    },
                ],
            );
            let new = id_of(&copy.join("rocksdb"));
            let steps: [Step; 2] = [
                (
                    "the checkpoint named rocksdb.new, the old copy (if any) in place",
                    Box::new(|x| id_in(x, "rocksdb.new") == new && id_in(x, "rocksdb") == old),
                ),
                (
                    "the checkpoint in place",
                    Box::new(|x| id_in(x, "rocksdb") == new && !has(x, "rocksdb.new")),
                ),
            ];
            durable_steps(&events, &copy, &format!("RocksDB: save() {round}"), &steps);
        }
        let n = count(&mut db);
        drop(db);
        assert_eq!(count(&mut Database::open(&copy).unwrap()), n);
    }
}
