//! Regression tests for the Phase 6 Round 3 review of Stream mode and the
//! directory lock: a broken store that keeps interning terms, deletes that
//! read a dictionary error as "no such term", version views and VS-tree
//! builds that skipped a read error, a rebuilt `vstree.kv` installed before
//! the data it was signed from was durable, statistics rebuilt (but never
//! saved) at every open, a cached CSR served from a broken store, and swap
//! repairs and saves that ran without the directory lock.
//!
//! A child module of `db`, so it can wrap a database's private dictionary in
//! a backend that fails chosen reads.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::open_options::{DiskQueryMode, OpenOptions};
use super::*;
use crate::concurrent::ConcurrentDb;
use crate::dict::{CacheStats, DiskTermSource, Space};

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_stream_review_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Build an on-disk database `<base>/<name>` from N-Triples `text`.
fn build_disk(base: &Path, name: &str, text: &str) -> PathBuf {
    let nt = base.join(format!("{name}.nt"));
    std::fs::write(&nt, text).unwrap();
    let dir = base.join(name);
    Database::build_disk(&dir, &[&nt]).unwrap();
    dir
}

fn stream() -> OpenOptions {
    OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream)
}

fn open_stream(dir: &Path) -> Database {
    let db = Database::open_with(dir, &stream()).unwrap();
    assert!(db.store.as_disk().is_some(), "opened in Stream mode");
    db
}

fn rows(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    let mut out = db.select(q).unwrap().rows;
    out.sort();
    out
}

fn ask(db: &mut Database, q: &str) -> bool {
    match db.query(q).unwrap() {
        crate::QueryResult::Ask(b) => b,
        other => panic!("not an ASK answer: {other:?}"),
    }
}

/// Overwrite page `page` of the paged file at `path` with `byte`.
fn smash_page(path: &Path, page: u64, byte: u8) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(page * crate::kvstore::PAGE_SIZE as u64))
        .unwrap();
    f.write_all(&[byte; crate::kvstore::PAGE_SIZE]).unwrap();
    f.sync_all().unwrap();
}

/// Header root slot `slot` of the store at `kv`.
fn root_of(kv: &Path, slot: usize) -> u64 {
    crate::kvstore::Pager::open(kv, 8).unwrap().root(slot)
}

/// Hold `dir`'s lock as another process would (a second open file
/// description: this process's registry does not know it).
fn foreign_lock(dir: &Path) -> std::fs::File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(super::dirlock::LOCK_FILE))
        .unwrap();
    f.try_lock().unwrap();
    f
}

/// A dictionary backend that fails the reads and writes of one key, as a
/// damaged page on its lookup path would.
#[derive(Debug)]
struct FailingKey {
    inner: Arc<dyn DiskTermSource>,
    key: Mutex<Option<String>>,
}

impl FailingKey {
    fn check(&self, key: &str) -> crate::Result<()> {
        match self.key.lock().unwrap().as_deref() {
            Some(k) if k == key => Err(GStoreError::Database(format!(
                "injected page read error for {key}"
            ))),
            _ => Ok(()),
        }
    }

    fn fail(&self, key: Option<String>) {
        *self.key.lock().unwrap() = key;
    }
}

impl DiskTermSource for FailingKey {
    fn lookup(&self, space: Space, key: &str) -> crate::Result<Option<u32>> {
        self.check(key)?;
        self.inner.lookup(space, key)
    }
    fn resolve(&self, space: Space, id: u32) -> crate::Result<Option<Arc<str>>> {
        self.inner.resolve(space, id)
    }
    fn intern(&self, space: Space, key: &str) -> crate::Result<u32> {
        self.check(key)?;
        self.inner.intern(space, key)
    }
    fn free(&self, space: Space, id: u32) -> crate::Result<bool> {
        self.inner.free(space, id)
    }
    fn resolve_sorted(&self, space: Space, ids: &[u32]) -> crate::Result<Vec<Option<Arc<str>>>> {
        self.inner.resolve_sorted(space, ids)
    }
    fn prefetch(&self, space: Space, ids: &[u32]) -> crate::Result<()> {
        self.inner.prefetch(space, ids)
    }
    fn for_each(
        &self,
        space: Space,
        from: u32,
        f: &mut dyn FnMut(u32, &str) -> ControlFlow<()>,
    ) -> crate::Result<()> {
        self.inner.for_each(space, from, f)
    }
    fn count(&self, space: Space) -> usize {
        self.inner.count(space)
    }
    fn id_limit(&self, space: Space) -> u32 {
        self.inner.id_limit(space)
    }
    fn generation(&self) -> u64 {
        self.inner.generation()
    }
    fn sync(&self) -> crate::Result<()> {
        self.inner.sync()
    }
    fn cache_stats(&self) -> CacheStats {
        self.inner.cache_stats()
    }
}

/// Wrap `db`'s out-of-core dictionary in a [`FailingKey`].
fn failing_dict(db: &mut Database) -> Arc<FailingKey> {
    let inner = Arc::clone(db.dict.backing().expect("an out-of-core dictionary"));
    let failing = Arc::new(FailingKey {
        inner,
        key: Mutex::new(None),
    });
    db.dict = Dictionary::from_backing(Arc::clone(&failing) as Arc<dyn DiskTermSource>);
    failing
}

fn key_of(iri: &str) -> String {
    Term::iri(iri).dict_key()
}

/// Once a commit failed part-way (the store broken, its applied pages still
/// in the cache), inserts are refused before they intern anything: the
/// dictionary shares the pager, and ending its write operation could flush
/// the cache — making part of the failed transaction durable.
#[test]
fn a_broken_stream_store_interns_no_terms() {
    let base = tmp_dir("broken_intern");
    let dir = build_disk(&base, "b", "<http://ex/a> <http://ex/p> <http://ex/b> .\n");
    let small = stream().with_disk_cache_pages(16);
    {
        let mut db = Database::open_with(&dir, &small).unwrap();
        // The transaction's terms interned (and durable) beforehand — kept
        // by the delete below (R4: with immediate reclamation it would free
        // them, and the transaction would intern them again).
        db.set_id_reclaim(crate::db::IdReclaim::Off);
        let data = "<http://ex/x1> <http://ex/q> <http://ex/y1> . \
                    <http://ex/x2> <http://ex/q> <http://ex/y2>";
        db.query(&format!("INSERT DATA {{ {data} }}")).unwrap();
        db.query(&format!("DELETE DATA {{ {data} }}")).unwrap();
        let entities = db.entity_num();
        db.begin().unwrap();
        db.query(&format!("INSERT DATA {{ {data} }}")).unwrap();
        db.store.as_disk().unwrap().debug_fail_write_after(1);
        assert!(db.commit().is_err());
        assert!(db.store.as_disk().unwrap().broken().is_some());
        for i in 0..400 {
            let t = Triple::new(
                Term::iri(format!("http://ex/fresh_subject_{i}")),
                Term::iri("http://ex/q"),
                Term::iri(format!("http://ex/fresh_object_{i}")),
            );
            let e = db
                .try_insert_triple(&t)
                .expect_err("a broken store refuses");
            assert!(e.to_string().contains("broken"), "{e}");
        }
        assert_eq!(db.entity_num(), entities, "nothing was interned");
    }
    let mut db = open_stream(&dir);
    assert_eq!(
        db.triple_num(),
        1,
        "nothing of the failed commit is durable"
    );
    assert!(!ask(
        &mut db,
        "ASK { <http://ex/x1> <http://ex/q> <http://ex/y1> }"
    ));
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// A DELETE, a CLEAR / DROP of a graph and a batch removal whose terms
/// cannot be looked up (a damaged dictionary page) fail: they used to read
/// the error as "no such term" and succeed with nothing removed.
#[test]
fn stream_deletes_fail_on_a_dictionary_read_error() {
    let base = tmp_dir("delete_read_error");
    let dir = build_disk(
        &base,
        "d",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
    );
    {
        let mut db = open_stream(&dir);
        db.query(
            "INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/c> } }",
        )
        .unwrap();
    }
    let kv = dir.join(KV_FILE);
    smash_page(&kv, root_of(&kv, crate::kvstore::format::ENTITY2ID), 0xAB);
    let mut db = open_stream(&dir);
    assert!(db
        .query("SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }")
        .is_err());
    for update in [
        "DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }",
        "DELETE WHERE { ?s <http://ex/p> ?o }",
        "CLEAR GRAPH <http://ex/g>",
        "DROP GRAPH <http://ex/g>",
        "DELETE DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/c> } }",
    ] {
        let r = db.query(update);
        assert!(r.is_err(), "{update} answered {r:?} despite a read error");
    }
    assert_eq!(db.triple_num(), 2);
    assert_eq!(db.named_graphs().len(), 1);
    // gsub / batchRemove.
    let nt = base.join("remove.nt");
    std::fs::write(&nt, "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    let mut applied = 0;
    let removed = db.remove_sources(
        &[import::Source::File(nt)],
        &import::ImportOptions::strict().for_removal(),
        &mut (),
        &mut applied,
    );
    assert!(
        removed.is_err(),
        "a removal answered {removed:?} despite a read error"
    );
    // REST /batchRemove's single-triple removal.
    let t = Triple::new(
        Term::iri("http://ex/a"),
        Term::iri("http://ex/p"),
        Term::iri("http://ex/b"),
    );
    let removed = db.try_remove_triple(&t);
    assert!(
        removed.is_err(),
        "a removal answered {removed:?} despite a read error"
    );
    assert_eq!(db.triple_num(), 2);
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// A version view materializes its chains in a read scope: a dictionary read
/// error while it resolves a tombstone or interns an insert fails the query
/// (it used to answer with the deleted triple back, or the insert missing),
/// and the garbage collection that cannot read the base keeps the record.
#[test]
fn a_version_view_fails_on_a_dictionary_read_error() {
    let base = tmp_dir("version_view");
    let dir = build_disk(
        &base,
        "v",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/c> <http://ex/p> <http://ex/d> .\n",
    );
    let mut db = open_stream(&dir);
    assert!(db.dict.is_disk_backed());
    let failing = failing_dict(&mut db);
    let cdb = ConcurrentDb::new(db);
    let t = |s: &str, o: &str| {
        Triple::new(
            Term::iri(format!("http://ex/{s}")),
            Term::iri("http://ex/p"),
            Term::iri(format!("http://ex/{o}")),
        )
    };
    let mut txn = cdb.begin();
    txn.delete(t("a", "b"));
    txn.insert(t("e", "f"));
    txn.commit().unwrap();
    let q = "SELECT ?s WHERE { ?s <http://ex/p> ?o }";
    let subjects = |r: crate::Result<crate::QueryResult>| -> crate::Result<Vec<String>> {
        match r? {
            crate::QueryResult::Select(rs) => {
                let mut v: Vec<String> = rs.rows.into_iter().flatten().flatten().collect();
                v.sort();
                Ok(v)
            }
            other => panic!("{other:?}"),
        }
    };
    let clean = vec!["<http://ex/c>".to_string(), "<http://ex/e>".to_string()];
    assert_eq!(subjects(cdb.version_view().query(q)).unwrap(), clean);
    for key in ["http://ex/a", "http://ex/e"] {
        failing.fail(Some(key_of(key)));
        let r = subjects(cdb.version_view().query(q));
        assert!(r.is_err(), "{key}: answered {r:?} despite a read error");
        failing.fail(None);
        assert_eq!(
            subjects(cdb.version_view().query(q)).unwrap(),
            clean,
            "{key}"
        );
    }
    drop(cdb);
    let _ = std::fs::remove_dir_all(&base);
}

/// After a failed commit the store is broken: the CSR cached while the
/// transaction ran (built from its overlay) is not served any more.
#[test]
fn a_broken_stream_store_serves_no_cached_csr() {
    let base = tmp_dir("csr_broken");
    let dir = build_disk(
        &base,
        "c",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
    );
    let mut db = open_stream(&dir);
    assert_eq!(db.csr().unwrap().num_edges(), 2);
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/x> <http://ex/q> <http://ex/y> }")
        .unwrap();
    assert_eq!(db.csr().unwrap().num_edges(), 3);
    assert!(db.csr_is_cached());
    db.store.as_disk().unwrap().debug_fail_next_write();
    assert!(db.commit().is_err());
    assert!(db.csr().is_err());
    assert!(db.preload_csr().is_err());
    assert!(!db.csr_is_cached());
    assert!(db.cached_csr().is_none());
    assert!(!db.csr_status().is_built());
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// `rebuild_index` makes the data it signs durable before the new
/// `vstree.kv` replaces the old one: a crash (here a failed flush, then the
/// database leaked like a killed process) can no longer leave a tree without
/// a deleted edge beside a store that still holds it under a header that
/// says the tree is fresh.
#[test]
fn a_stream_rebuild_never_installs_a_tree_ahead_of_the_data() {
    let base = tmp_dir("rebuild_durable");
    let dir = build_disk(
        &base,
        "r",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/a> <http://ex/q> <http://ex/c> .\n\
         <http://ex/x> <http://ex/p> <http://ex/b> .\n<http://ex/x> <http://ex/q> <http://ex/c> .\n",
    );
    let q = "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/b> }";
    {
        let mut db = open_stream(&dir);
        assert!(db.stats().index_valid);
        let t = Triple::new(
            Term::iri("http://ex/a"),
            Term::iri("http://ex/p"),
            Term::iri("http://ex/b"),
        );
        assert!(db.remove_triple(&t), "removed, not synced");
        db.store.as_disk().unwrap().debug_fail_next_write();
        assert!(db.try_rebuild_index().is_err());
        std::mem::forget(db);
    }
    for options in [stream(), OpenOptions::new()] {
        let mut db = Database::open_with(&dir, &options).unwrap();
        // The unsynced delete is lost with the crash; its edge must not be
        // pruned away by the tree.
        assert_eq!(db.triple_num(), 4);
        assert_eq!(
            rows(&mut db, q),
            vec![
                vec![Some("<http://ex/a>".to_string())],
                vec![Some("<http://ex/x>".to_string())]
            ],
            "{:?}",
            options.disk_query_mode
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// `use_streaming_vstree` signs in a read scope: a read error fails it and
/// leaves the current tree, instead of building one with entities missing
/// edges (and marking it valid).
#[test]
fn a_streaming_vstree_build_fails_on_a_read_error() {
    let base = tmp_dir("streaming_vstree");
    let mut text = String::new();
    for i in 0..3000 {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n<http://ex/o{i}> <http://ex/q> \"v{i}\" .\n"
        ));
    }
    let dir = build_disk(&base, "s", &text);
    let mut db = Database::open_with(&dir, &stream().with_disk_cache_pages(16)).unwrap();
    let q = "SELECT ?s WHERE { ?s <http://ex/p> ?o . ?o <http://ex/q> \"v1\" }";
    let want = rows(&mut db, q);
    assert_eq!(want.len(), 1);
    let kv = dir.join(KV_FILE);
    let bytes = std::fs::read(&kv).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&kv)
        .unwrap()
        .set_len(bytes.len() as u64 / 10)
        .unwrap();
    let built = db.use_streaming_vstree(base.join("side.vstree"));
    std::fs::write(&kv, &bytes).unwrap();
    assert!(built.is_err(), "a tree was built despite read errors");
    assert_eq!(rows(&mut db, q), want);
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// Statistics an open rebuilt for a file written before they were
/// maintained are written by the first commit point (here the close of a
/// read-only session): the next open does not rebuild them again.
#[test]
fn statistics_rebuilt_at_open_are_saved() {
    let base = tmp_dir("stats_saved");
    let dir = build_disk(
        &base,
        "t",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/q> <http://ex/c> .\n",
    );
    let kv = dir.join(KV_FILE);
    let pred_stats = crate::kvstore::format::PRED_STATS;
    assert_ne!(root_of(&kv, pred_stats), 0);
    {
        let mut p = crate::kvstore::Pager::open(&kv, 8).unwrap();
        p.set_root(pred_stats, 0);
        p.flush().unwrap();
    }
    {
        let mut db = open_stream(&dir);
        assert_eq!(rows(&mut db, "SELECT ?s WHERE { ?s ?p ?o }").len(), 2);
    }
    assert_ne!(
        root_of(&kv, pred_stats),
        0,
        "the rebuilt statistics were not saved"
    );
    let mut db = open_stream(&dir);
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"
        )
        .len(),
        1
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// An open repairs an interrupted swap only under the directory's lock:
/// while another process holds it (installing a store, restoring), the open
/// fails and touches nothing — it used to delete the staged `rocksdb.new` /
/// `<dir>.restore-new` (or the moved-aside database) first.
#[test]
fn an_open_repairs_no_swap_another_process_holds() {
    let base = tmp_dir("repair_locked");
    // A bulk install in progress: the old store and the staged new one.
    let dir = base.join("i.db");
    std::fs::create_dir_all(dir.join("rocksdb")).unwrap();
    std::fs::write(dir.join("rocksdb/CURRENT"), b"old").unwrap();
    std::fs::create_dir_all(dir.join("rocksdb.new")).unwrap();
    std::fs::write(dir.join("rocksdb.new/CURRENT"), b"new").unwrap();
    let held = foreign_lock(&dir);
    let e = Database::open(&dir)
        .expect_err("refused while locked")
        .to_string();
    assert!(e.contains("locked by another process"), "{e}");
    assert!(
        dir.join("rocksdb.new/CURRENT").is_file(),
        "the staged store was deleted"
    );
    drop(held);

    // A restore in progress: the database and its staged replacement ...
    let dir = build_disk(
        &base,
        "r.db",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let staged = fsutil::with_suffix(&dir, ".restore-new");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("meta.bin"), b"staged").unwrap();
    let held = foreign_lock(&dir);
    assert!(Database::open(&dir).is_err());
    assert!(
        staged.join("meta.bin").is_file(),
        "the staged restore was deleted"
    );
    drop(held);
    // ... and the database moved aside (its lock file with it).
    let aside = fsutil::with_suffix(&dir, ".restore-old");
    std::fs::rename(&dir, &aside).unwrap();
    let held = foreign_lock(&aside);
    assert!(Database::open(&dir).is_err());
    assert!(
        aside.join(KV_FILE).is_file(),
        "the moved-aside database was deleted"
    );
    assert!(staged.is_dir(), "the staged restore was moved or deleted");
    drop(held);
    // With nobody holding them, an open repairs both (the swap completes).
    std::fs::remove_dir_all(&staged).unwrap();
    std::fs::rename(&aside, &dir).unwrap();
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 1);
    let _ = std::fs::remove_dir_all(&base);
}

/// Saving a snapshot over a database directory, the incremental disk build
/// and a restore take the directory's lock first: with another process
/// holding it they fail and leave its files alone (they used to delete or
/// replace them under the process serving them).
#[test]
fn saves_builds_and_restores_over_a_locked_directory_are_refused() {
    let base = tmp_dir("locked_writes");
    let dir = build_disk(
        &base,
        "l.db",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let kv = dir.join(KV_FILE);
    let before = std::fs::read(&kv).unwrap();
    let held = foreign_lock(&dir);

    let snapshot =
        Database::build_from_str("l", "<http://ex/x> <http://ex/p> <http://ex/y> .").unwrap();
    let e = snapshot
        .save(&dir)
        .expect_err("saved over a locked directory");
    assert!(e.to_string().contains("locked by another process"), "{e}");

    let nt = base.join("new.nt");
    std::fs::write(&nt, "<http://ex/n> <http://ex/p> <http://ex/m> .\n").unwrap();
    let sources = import::sources_from_paths(&[&nt]).unwrap();
    let e = Database::build_disk_from_sources_incremental(
        &dir,
        &sources,
        &import::ImportOptions::strict(),
        &mut (),
    )
    .expect_err("rebuilt a locked directory");
    assert!(e.to_string().contains("locked by another process"), "{e}");

    let backup = build_disk(
        &base,
        "bak.db",
        "<http://ex/k> <http://ex/p> <http://ex/j> .\n",
    );
    let e = Database::restore_into(&backup, &dir).expect_err("restored over a locked directory");
    assert!(e.to_string().contains("locked by another process"), "{e}");

    assert_eq!(
        std::fs::read(&kv).unwrap(),
        before,
        "the served store was touched"
    );
    assert!(!dir.join(super::META_FILE).exists());
    drop(held);
    // Free again: all three work.
    snapshot.save(&dir).unwrap();
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 1);
    Database::build_disk_from_sources_incremental(
        &dir,
        &sources,
        &import::ImportOptions::strict(),
        &mut (),
    )
    .unwrap();
    Database::restore_into(&backup, &dir).unwrap();
    let mut db = Database::open(&dir).unwrap();
    assert!(ask(
        &mut db,
        "ASK { <http://ex/k> <http://ex/p> <http://ex/j> }"
    ));
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// A file-level backup (`gbackup`, the `/api` backup of an unloaded
/// database) repairs an interrupted save in its source only under the
/// source's lock: with another process holding it — a `gbuild --force`
/// staging its snapshot, a bulk install between its renames — it fails and
/// leaves the staged files alone. It used to delete the staged snapshot
/// members (the save then committed without them and removed the old
/// store) or move the staged store into place under the installer.
#[test]
fn a_file_backup_repairs_no_save_another_process_holds() {
    let base = tmp_dir("backup_dir_locked");
    let dir = build_disk(
        &base,
        "s.db",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let member = dir.join("dict.bin.tmp");
    std::fs::write(&member, b"staged").unwrap();
    let store_new = dir.join("rocksdb.new");
    std::fs::create_dir_all(&store_new).unwrap();
    std::fs::write(store_new.join("CURRENT"), b"new").unwrap();
    let copy = base.join("copy");
    let held = foreign_lock(&dir);
    let e = Database::backup_dir(&dir, &copy).expect_err("repaired a held directory");
    assert!(e.to_string().contains("locked by another process"), "{e}");
    assert!(member.is_file(), "the staged snapshot member was deleted");
    assert!(store_new.is_dir(), "the staged store was moved");
    assert!(!dir.join("rocksdb").exists());
    drop(held);
    // Free: the leftovers of the (now interrupted) saves are repaired.
    Database::backup_dir(&dir, &copy).unwrap();
    assert!(!member.exists());
    assert!(dir.join("rocksdb/CURRENT").is_file());
    assert!(copy.join("rocksdb/CURRENT").is_file());
    assert!(copy.join(KV_FILE).is_file());
    let _ = std::fs::remove_dir_all(&base);
}

/// A restore that creates its database (no directory to lock yet) holds
/// the lock file beside it, `<dir>.restore-lock`, from before it stages
/// until the swap — and removes it afterwards; another process's repair of
/// the directory (its `open`) takes that lock too, so it cannot remove the
/// staged copy of a restore in progress (R3 review, second pass).
#[test]
fn a_restore_creating_its_database_holds_the_lock_beside_it() {
    let base = tmp_dir("restore_creates");
    let dir = base.join("n.db");
    let side = fsutil::with_suffix(&dir, ".restore-lock");
    let backup = build_disk(
        &base,
        "bak.db",
        "<http://ex/k> <http://ex/p> <http://ex/j> .\n",
    );
    persist::restore_probe::watch(&dir);
    Database::restore_into(&backup, &dir).unwrap();
    assert_eq!(
        persist::restore_probe::lock_was_free(&dir),
        [false],
        "another process could repair the staged copy away"
    );
    assert!(!side.exists(), "the lock file is removed with the lock");
    let mut db = Database::open(&dir).unwrap();
    assert!(ask(
        &mut db,
        "ASK { <http://ex/k> <http://ex/p> <http://ex/j> }"
    ));
    drop(db);

    // Another process restoring into a new `m.db` holds the lock beside it
    // while its copy is staged: an open of `m.db` here is refused, the
    // staged copy untouched; once it is gone, the leftovers are repaired.
    let other = base.join("m.db");
    let staged = fsutil::with_suffix(&other, ".restore-tmp");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("x"), b"staged").unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fsutil::with_suffix(&other, ".restore-lock"))
        .unwrap();
    held.try_lock().unwrap();
    let e = Database::open(&other).expect_err("repaired a staged restore in progress");
    assert!(e.to_string().contains("locked by another process"), "{e}");
    assert!(staged.join("x").is_file(), "the staged copy was removed");
    let e = Database::lock_restore_target(&other).expect_err("two restores of one database");
    assert!(e.to_string().contains("locked by another process"), "{e}");
    drop(held);
    Database::recover_restore(&other).unwrap();
    assert!(
        !staged.exists(),
        "the leftovers of an ended restore are removed"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A restore into a directory an interrupted earlier restore moved aside
/// holds the directory that finishing the earlier one recreates, from then
/// until the swap: another process opening it while the backup is staged
/// (it used to be free then) is refused instead of being served — and
/// acknowledging writes to — a database the swap replaces.
#[test]
fn a_restore_after_an_interrupted_one_holds_the_recreated_directory() {
    let base = tmp_dir("restore_recreated");
    let dir = base.join("t.db");
    let aside = fsutil::with_suffix(&dir, ".restore-old");
    let waiting = fsutil::with_suffix(&dir, ".restore-new");
    // The crash state: the database moved aside, its replacement complete.
    build_disk(
        &base,
        "t.db.restore-old",
        "<http://ex/old> <http://ex/p> <http://ex/o> .\n",
    );
    build_disk(
        &base,
        "t.db.restore-new",
        "<http://ex/new> <http://ex/p> <http://ex/o> .\n",
    );
    let backup = build_disk(
        &base,
        "bak.db",
        "<http://ex/k> <http://ex/p> <http://ex/j> .\n",
    );
    persist::restore_probe::watch(&dir);
    Database::restore_into(&backup, &dir).unwrap();
    assert_eq!(
        persist::restore_probe::lock_was_free(&dir),
        [false],
        "another process could open the directory while the backup was staged"
    );
    assert!(!aside.exists() && !waiting.exists());
    let mut db = Database::open(&dir).unwrap();
    assert!(ask(
        &mut db,
        "ASK { <http://ex/k> <http://ex/p> <http://ex/j> }"
    ));
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// With the `rocksdb` feature: a VS-tree rebuild that hits a read error
/// fails and leaves the database without a tree, instead of marking one
/// missing the unread edges valid.
#[cfg(feature = "rocksdb")]
#[test]
fn a_rocksdb_vstree_rebuild_fails_on_a_read_error() {
    let base = tmp_dir("rocks_rebuild");
    let dir = base.join("k.db");
    drop(
        Database::build_rocksdb_from_str(
            "k",
            &dir,
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
        )
        .unwrap(),
    );
    let mut db = Database::open(&dir).unwrap();
    db.backend().as_rocks().unwrap().debug_fail_reads(true);
    assert!(db.try_rebuild_index().is_err());
    assert!(!db.stats().index_valid);
    db.backend().as_rocks().unwrap().debug_fail_reads(false);
    db.try_rebuild_index().unwrap();
    assert!(db.stats().index_valid);
    assert_eq!(
        rows(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }").len(),
        2
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}
