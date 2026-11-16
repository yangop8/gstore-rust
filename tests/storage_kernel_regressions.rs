//! Storage-kernel regressions (review round 3, package r3-storage-kernel):
//! quad-aware id reuse in `DiskStore::free_key` (disk#3), a poisoned pager
//! lock (conc#8), the advisory lock of `Pager::open` and the one WAL path
//! helper (disk#9), the free-id format of existing and new files (disk#8,
//! design#6) and the RocksDB dictionary's id bounds (db#18).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::dict::Space;
use gstore::kvstore::{DiskStore, Pager};

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_storage_kernel_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
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

/// Every quad of `ds` as strings `(g, s, p, o)`.
fn quads(ds: &DiskStore) -> Vec<(String, String, String, String)> {
    let node = |id| ds.id_to_string(id).unwrap().unwrap_or_default();
    let mut out: Vec<_> = ds
        .quads()
        .unwrap()
        .into_iter()
        .map(|(g, t)| {
            (
                node(g),
                node(t.sub),
                ds.predicate_to_string(t.pred).unwrap().unwrap_or_default(),
                node(t.obj),
            )
        })
        .collect();
    out.sort();
    out
}

// ---- disk#3: a term a named-graph quad uses is never freed --------------------

/// Review disk#3: `free_key` checked the quad index only for a graph IRI
/// (`GSPO` is keyed graph-first), so a term used only as a quad's subject,
/// predicate or object was freed, and the next new term took its id: the
/// quad silently changed meaning.
#[test]
fn free_key_keeps_terms_named_graph_quads_use() {
    let tmp = TmpDir::new("quad_refs");
    let path = tmp.path().join("kvstore.kv");
    let mut ds = DiskStore::open_with(&path, 64, true).unwrap();
    assert!(ds.frees_ids());
    ds.insert_quad_keys("<g>", "<only_in_graph>", "<q>", "\"lit\"", true)
        .unwrap();
    ds.insert_quad_keys("<g>", "<s2>", "<q2>", "<obj_only_in_graph>", false)
        .unwrap();
    ds.insert_keys("<a>", "<p>", "<b>", false).unwrap();
    let before = quads(&ds);
    for (space, key) in [
        (Space::Entity, "<g>"),
        (Space::Entity, "<only_in_graph>"),
        (Space::Predicate, "<q>"),
        (Space::Literal, "\"lit\""),
        (Space::Entity, "<obj_only_in_graph>"),
        (Space::Predicate, "<q2>"),
    ] {
        assert!(
            !ds.free_key(space, key).unwrap(),
            "{space:?} {key} is used by a quad and must not be freed"
        );
    }
    // New terms: had an id above been freed, they would take it.
    ds.insert_keys("<newcomer>", "<newpred>", "<a>", false)
        .unwrap();
    ds.insert_keys("<other>", "<newpred2>", "\"new lit\"", true)
        .unwrap();
    assert_eq!(quads(&ds), before, "id reuse changed a quad");
    // Quads written after the first check count too, and a deleted one
    // stops counting, without a reopen.
    ds.insert_quad_keys("<g2>", "<late>", "<late_p>", "<late_o>", false)
        .unwrap();
    assert!(!ds.free_key(Space::Entity, "<late>").unwrap());
    assert!(!ds.free_key(Space::Predicate, "<late_p>").unwrap());
    assert!(ds
        .delete_quad_keys("<g2>", "<late>", "<late_p>", "<late_o>", false)
        .unwrap());
    for (space, key) in [
        (Space::Entity, "<late>"),
        (Space::Predicate, "<late_p>"),
        (Space::Entity, "<late_o>"),
        (Space::Entity, "<g2>"),
    ] {
        assert!(ds.free_key(space, key).unwrap(), "{key} is unused now");
    }
    ds.verify_free_list().unwrap();
    ds.flush().unwrap();
    drop(ds);
    let ds = DiskStore::open(&path, 64).unwrap();
    assert_eq!(quads(&ds), before, "after a reopen");
    // Once its quad is gone, the term is freed (the check is not a blanket
    // refusal).
    drop(ds);
    let mut ds = DiskStore::open(&path, 64).unwrap();
    assert!(ds
        .delete_quad_keys("<g>", "<only_in_graph>", "<q>", "\"lit\"", true)
        .unwrap());
    assert!(ds.free_key(Space::Entity, "<only_in_graph>").unwrap());
    assert!(ds.free_key(Space::Predicate, "<q>").unwrap());
    assert!(ds.free_key(Space::Literal, "\"lit\"").unwrap());
    assert!(!ds.free_key(Space::Entity, "<g>").unwrap(), "still a graph");
    ds.verify_free_list().unwrap();
}

// ---- conc#8: a poisoned pager lock ------------------------------------------------

/// Poison the pager lock of `ds` the way a panic under its write guard does.
fn poison(ds: &DiskStore) {
    let pager = ds.shared_pager();
    let joined = std::thread::spawn(move || {
        let _guard = pager.write().unwrap();
        panic!("a panic while the pager is locked for writing (test)");
    })
    .join();
    assert!(joined.is_err());
    assert!(ds.shared_pager().is_poisoned());
}

/// Review conc#8: after a panic under the pager's write guard every read of
/// the store panicked (`read().unwrap()` on the poisoned lock) while the
/// out-of-core dictionary sharing the pager tolerated it. Now the reads go
/// on, and a write is refused with an error (the pager is marked failed:
/// nothing the panic may have left half-done reaches the file) instead of a
/// panic.
#[test]
fn a_poisoned_pager_lock_keeps_reads_working_and_refuses_writes() {
    let tmp = TmpDir::new("poison");
    let path = tmp.path().join("kvstore.kv");
    let mut ds = DiskStore::open_with(&path, 64, true).unwrap();
    ds.insert_keys("<a>", "<p>", "<b>", false).unwrap();
    ds.insert_quad_keys("<g>", "<a>", "<p>", "\"x\"", true)
        .unwrap();
    ds.flush().unwrap();
    let a = ds
        .term_id(&gstore::Term::iri("a"))
        .unwrap()
        .expect("<a> is a term");
    poison(&ds);
    let reads = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(ds.id_to_string(a).unwrap().as_deref(), Some("<a>"));
        assert!(ds.predicate_id("<p>").unwrap().is_some());
        assert_eq!(ds.try_po_by_s(a).unwrap().len(), 1);
        assert_eq!(ds.try_iter_all().unwrap().len(), 1);
        assert_eq!(ds.quads().unwrap().len(), 1);
        assert_eq!(ds.subject_degree(a).unwrap(), 1);
        assert!(ds.frees_ids());
        assert_eq!(ds.dictionary().unwrap().entity_num(), ds.entity_num());
    }));
    assert!(reads.is_ok(), "a read panicked on the poisoned lock");
    let write = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ds.insert_keys("<c>", "<p>", "<d>", false)
    }));
    let write = write.expect("a write panicked on the poisoned lock");
    let e = write.expect_err("a write after the panic is refused");
    assert!(matches!(e, gstore::GStoreError::StorageFailed(_)), "{e}");
    let commit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ds.commit()));
    assert!(
        commit.expect("a commit panicked").is_err(),
        "nothing is committed"
    );
    drop(ds);
    // The file holds what was committed before the panic.
    let ds = DiskStore::open(&path, 64).unwrap();
    assert_eq!(ds.triple_count(), 1);
    assert_eq!(ds.quad_count(), 1);
}

// ---- disk#9: one handle per pager file ---------------------------------------------

/// The probe child of [`a_pager_file_open_here_is_refused_to_another_process`]:
/// tries to open the file and reports.
#[test]
fn lock_probe_child() {
    let Ok(path) = std::env::var("GSTORE_KERNEL_LOCK_PROBE") else {
        return;
    };
    match Pager::open(&path, 8) {
        Ok(_) => println!("PROBE-OPENED"),
        Err(e) => println!("PROBE-REFUSED {e}"),
    }
}

/// Whether another process can open the pager file at `path` now.
fn probe_from_another_process(path: &Path) -> Result<(), String> {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_probe_child", "--nocapture", "--test-threads=1"])
        .env("GSTORE_KERNEL_LOCK_PROBE", path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if stdout.contains("PROBE-OPENED") {
        Ok(())
    } else {
        Err(stdout)
    }
}

/// Review disk#9: `Pager::open` took no lock, so a store open here could be
/// opened by another process too — each replaying and deleting the other's
/// write-ahead log and overwriting its pages. Now the file is locked while
/// any pager of this process has it open (a second pager of this process
/// shares the lock); another process is refused, touching nothing, until
/// the last one is dropped.
#[test]
fn a_pager_file_open_here_is_refused_to_another_process() {
    let tmp = TmpDir::new("pager_lock");
    let path = tmp.path().join("kvstore.kv");
    let mut ds = DiskStore::open_with(&path, 64, true).unwrap();
    ds.insert_keys("<a>", "<p>", "<b>", false).unwrap();
    ds.flush().unwrap();
    let e = probe_from_another_process(&path).expect_err("refused while open here");
    assert!(e.contains("PROBE-REFUSED") && e.contains("in use"), "{e}");
    assert!(e.contains("kvstore.kv"), "{e}");
    // A second pager of this process shares the lock: still refused after
    // the first one is gone, while the second lives.
    let second = Pager::open(&path, 8).unwrap();
    drop(ds);
    probe_from_another_process(&path).expect_err("the second pager holds it");
    drop(second);
    probe_from_another_process(&path).expect("released with the last pager");
    let ds = DiskStore::open(&path, 64).unwrap();
    assert_eq!(ds.triple_count(), 1);
}

/// The other direction: a file another process holds fails this process's
/// open; once that process exits, the open succeeds.
#[cfg(unix)]
#[test]
fn a_pager_file_held_by_another_process_is_refused() {
    if let Ok(path) = std::env::var("GSTORE_KERNEL_LOCK_HOLDER") {
        // The child: hold the file until stdin closes.
        let _pager = Pager::open(&path, 8).unwrap();
        println!("HELD");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        return;
    }
    use std::io::{BufRead, BufReader};
    let tmp = TmpDir::new("pager_lock_proc");
    let path = tmp.path().join("kvstore.kv");
    drop(Pager::open(&path, 8).unwrap());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_pager_file_held_by_another_process_is_refused",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GSTORE_KERNEL_LOCK_HOLDER", &path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            out.read_line(&mut line).unwrap() > 0,
            "the child exited before holding the file"
        );
        if line.contains("HELD") {
            break;
        }
    }
    let e = Pager::open(&path, 8).err().expect("held by the child");
    assert!(e.to_string().contains("in use"), "{e}");
    drop(child.stdin.take()); // the child exits
    assert!(child.wait().unwrap().success());
    Pager::open(&path, 8).expect("released when the child exited");
}

/// Review disk#9: `use_streaming_vstree` removed a stale node file and its
/// write-ahead log ignoring every error, so a log it could not remove was
/// left beside the fresh file. Now an error other than "not found" fails the
/// call before anything is built.
#[test]
fn a_wal_that_cannot_be_removed_fails_the_streaming_vstree_build() {
    let tmp = TmpDir::new("vstree_wal");
    let mut db = gstore::Database::build_from_str(
        "v",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    )
    .unwrap();
    let path = tmp.path().join("vs.kv");
    // A "log" that is a non-empty directory cannot be removed as a file.
    let wal = tmp.path().join("vs.kv.wal");
    fs::create_dir_all(wal.join("x")).unwrap();
    let e = db
        .use_streaming_vstree(&path)
        .expect_err("the stale log could not be removed");
    let msg = e.to_string();
    assert!(msg.contains("vs.kv.wal"), "{msg}");
    assert!(msg.contains("remove"), "{msg}");
    assert!(!path.exists(), "nothing was built");
    fs::remove_dir_all(&wal).unwrap();
    db.use_streaming_vstree(&path).unwrap();
}

// ---- disk#8 / design#6: the free-id format of a file ----------------------------

const FORMAT_CHILD: &str = "GSTORE_KERNEL_FORMAT_CHILD";

/// The magic of the store file at `path` (`GSTOREKV` version 1,
/// `GSTOREK2` version 2 with the free-id list).
fn magic(path: &Path) -> String {
    String::from_utf8_lossy(&fs::read(path).unwrap()[0..8]).into_owned()
}

/// Run this test binary's `child` test with `GSTORE_ID_RECLAIM=policy` and
/// `FORMAT_CHILD=dir`.
fn run_with_policy(child: &str, policy: &str, dir: &Path) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", child, "--nocapture", "--test-threads=1"])
        .env("GSTORE_ID_RECLAIM", policy)
        .env(FORMAT_CHILD, dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{child} with GSTORE_ID_RECLAIM={policy}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The child of [`existing_files_keep_their_format_under_either_policy`]:
/// open `v1/` and `v2/` both ways, write through them, close.
#[test]
fn format_child_writes_through_existing_files() {
    let Ok(dir) = std::env::var(FORMAT_CHILD) else {
        return;
    };
    let dir = PathBuf::from(dir);
    for name in ["v1", "v2"] {
        let kv = dir.join(name).join("kvstore.kv");
        {
            let mut ds = DiskStore::open(&kv, 64).unwrap();
            ds.insert_keys("<x>", "<p>", "<y>", false).unwrap();
            ds.flush().unwrap();
        }
        for opts in [
            gstore::db::OpenOptions::stream(),
            gstore::db::OpenOptions::materialize(),
        ] {
            let mut db = gstore::Database::open_with(dir.join(name), &opts).unwrap();
            db.query("INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/m> }")
                .unwrap();
            db.query("DELETE DATA { <http://ex/n> <http://ex/p> <http://ex/m> }")
                .unwrap();
            db.sync().unwrap();
        }
        let mut db = gstore::Database::open(dir.join(name)).unwrap();
        db.query("INSERT DATA { <http://ex/k> <http://ex/p> \"v\" }")
            .unwrap();
        db.sync().unwrap();
    }
}

/// Review disk#8 / design#6: an existing `kvstore.kv` keeps the format its
/// header records — version 1 without the free-id list, version 2 with it —
/// whatever the process's id-reclamation policy (`GSTORE_ID_RECLAIM`), when
/// it is opened, written through and saved by the store, a Stream-mode and
/// a Materialize-mode database. Only `gcompact --enable-reclaim` upgrades.
#[test]
fn existing_files_keep_their_format_under_either_policy() {
    if std::env::var(FORMAT_CHILD).is_ok() {
        return;
    }
    let tmp = TmpDir::new("format");
    for (name, free_ids) in [("v1", false), ("v2", true)] {
        fs::create_dir_all(tmp.path().join(name)).unwrap();
        let kv = tmp.path().join(name).join("kvstore.kv");
        let mut ds = DiskStore::open_with(&kv, 64, free_ids).unwrap();
        ds.insert_keys("<http://ex/a>", "<http://ex/p>", "<http://ex/b>", false)
            .unwrap();
        ds.flush().unwrap();
    }
    let v1 = tmp.path().join("v1/kvstore.kv");
    let v2 = tmp.path().join("v2/kvstore.kv");
    assert_eq!(magic(&v1), "GSTOREKV");
    assert_eq!(magic(&v2), "GSTOREK2");
    for policy in ["on", "off"] {
        run_with_policy("format_child_writes_through_existing_files", policy, tmp.path());
        assert_eq!(magic(&v1), "GSTOREKV", "policy {policy} upgraded a v1 file");
        assert_eq!(magic(&v2), "GSTOREK2", "policy {policy} downgraded a v2 file");
    }
}

const EXPLICIT_CHILD: &str = "GSTORE_KERNEL_EXPLICIT_CHILD";

/// The child of [`new_files_take_the_explicitly_chosen_format`]: build and
/// create new stores with the format chosen against the process policy.
#[test]
fn explicit_format_child() {
    let Ok(dir) = std::env::var(EXPLICIT_CHILD) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let policy_on = gstore::db::default_id_reclaim() == gstore::db::IdReclaim::Immediate;
    // The defaults follow the process policy...
    assert_eq!(gstore::bulk::BulkOptions::default().free_ids, policy_on);
    assert_eq!(gstore::db::default_open_options().free_ids_if_new, policy_on);
    // ...and an explicit choice wins over it: the opposite format.
    let nt = dir.join("d.nt");
    fs::write(&nt, "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    let sources = gstore::parser::import::sources_from_paths(&[&nt]).unwrap();
    gstore::bulk::build(
        &dir.join("bulk.db"),
        "bulk",
        gstore::bulk::BulkInput::Sources(&sources),
        &gstore::parser::import::ImportOptions::strict(),
        &mut (),
        gstore::bulk::BulkTarget::Disk,
        &gstore::bulk::BulkOptions {
            free_ids: !policy_on,
            ..gstore::bulk::BulkOptions::default()
        },
    )
    .unwrap();
    // An open that creates the store file (an empty kvstore.kv).
    fs::create_dir_all(dir.join("open.db")).unwrap();
    fs::write(dir.join("open.db/kvstore.kv"), b"").unwrap();
    let opts = gstore::db::OpenOptions::stream().with_free_ids_if_new(!policy_on);
    let mut db = gstore::Database::open_with(dir.join("open.db"), &opts).unwrap();
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    db.sync().unwrap();
    drop(db);
    let db = gstore::Database::open_with(dir.join("bulk.db"), &opts).unwrap();
    assert_eq!(db.triple_num(), 1);
}

/// Review disk#8 / design#6: the format of a *new* store file is an
/// explicit choice — `BulkOptions::free_ids` for a build,
/// `OpenOptions::free_ids_if_new` for an open that creates the file —
/// which the process's id-reclamation policy only defaults (the build used
/// to read `GSTORE_ID_RECLAIM` itself, whatever its caller wanted).
#[test]
fn new_files_take_the_explicitly_chosen_format() {
    if std::env::var(EXPLICIT_CHILD).is_ok() {
        return;
    }
    for (policy, want) in [("on", "GSTOREKV"), ("off", "GSTOREK2")] {
        let tmp = TmpDir::new(&format!("explicit_{policy}"));
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "explicit_format_child", "--nocapture", "--test-threads=1"])
            .env("GSTORE_ID_RECLAIM", policy)
            .env(EXPLICIT_CHILD, tmp.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "policy {policy}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for db in ["bulk.db", "open.db"] {
            assert_eq!(
                magic(&tmp.path().join(db).join("kvstore.kv")),
                want,
                "{db} under policy {policy}"
            );
        }
    }
}

// ---- db#18: a RocksDB dictionary id beyond its space ---------------------------

/// Review db#18: loading the RocksDB `dict` column family sized a vector by
/// an untrusted id before checking it against its id space, so one corrupt
/// entry with an id near `u32::MAX` made the open allocate ~100 GB and abort
/// the process. Now the open fails with "corrupt RocksDB dictionary".
#[cfg(feature = "rocksdb")]
#[test]
fn a_rocksdb_dictionary_id_beyond_its_space_fails_the_open() {
    use gstore::dict::{ENTITY_ID_LIMIT, LITERAL_INDEX_LIMIT, PREDICATE_ID_LIMIT};
    use gstore::model::id::LITERAL_FIRST_ID;
    const CFS: [&str; 7] = ["spo", "pos", "osp", "stats", "dict", "gspo", "meta"];
    let cases: [(u8, u32); 5] = [
        (b'E', u32::MAX - 1),
        (b'E', ENTITY_ID_LIMIT),
        (b'P', PREDICATE_ID_LIMIT),
        (b'L', u32::MAX),
        (b'L', LITERAL_FIRST_ID + LITERAL_INDEX_LIMIT),
    ];
    for (tag, id) in cases {
        let tmp = TmpDir::new("rocks_dict_id");
        let dir = tmp.path().join("d.db");
        drop(
            gstore::Database::build_rocksdb_from_str(
                "d",
                &dir,
                "<http://ex/a> <http://ex/p> \"x\" .\n",
            )
            .unwrap(),
        );
        {
            let db =
                rocksdb::DB::open_cf(&rocksdb::Options::default(), dir.join("rocksdb"), CFS)
                    .unwrap();
            let mut key = vec![tag];
            key.extend_from_slice(&id.to_be_bytes());
            db.put_cf(db.cf_handle("dict").unwrap(), &key, b"<http://ex/far>")
                .unwrap();
            db.flush().unwrap();
        }
        let e = gstore::Database::open(&dir)
            .err()
            .unwrap_or_else(|| panic!("{} id {id} was accepted", tag as char));
        let msg = e.to_string();
        assert!(msg.contains("corrupt RocksDB dictionary"), "{msg}");
        assert!(msg.contains(&id.to_string()), "{msg}");
    }
}
