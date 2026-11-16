//! Storage-kernel regressions (review round 3, package r3-storage-kernel):
//! quad-aware id reuse in `DiskStore::free_key` (disk#3), a poisoned pager
//! lock (conc#8), the advisory lock of `Pager::open` and the one WAL path
//! helper (disk#9), the free-id format of existing and new files (disk#8,
//! design#6) and the RocksDB dictionary's id bounds (db#18).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::dict::Space;
use gstore::kvstore::DiskStore;

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
