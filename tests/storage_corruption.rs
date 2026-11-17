//! Damaged secondary on-disk structures are reported, never read as empty
//! and never a panic (review robust#11, disk#4).
//!
//! The pages of `kvstore.kv` and `vstree.kv` carry no checksum, so a flipped
//! byte reaches the decoders. These tests damage one structure on disk at a
//! time, through the public API, and check what a reader then sees:
//!
//! - a VList record of the compacted `sp2o_vlist` index that does not decode,
//!   or carries an unknown tag, is a read error (`InvalidData`) — it used to
//!   read as "no objects", so `<s> <p> ?o` silently had no values although
//!   contract C2 promises that a read error fails the statement; so is an
//!   overflow record whose length no chain of the file can carry, or whose
//!   chain ends before it (that length used to bound nothing: a chain that
//!   cycles was followed for up to 4 GiB);
//! - a damaged node of an out-of-core VS-tree makes the tree filter nothing
//!   (sound), so queries still answer correctly — it used to panic or
//!   overflow the stack.

use std::fs;
use std::path::{Path, PathBuf};

use gstore::kvstore::overflow::CHAIN_PAYLOAD;
use gstore::kvstore::{vlist, DiskStore, PAGE_SIZE};
use gstore::query::{scoped_vs_policy, VsPolicy};
use gstore::signature::{Signature, VsTree};
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult, Term, Triple};

/// A temporary directory, removed on drop (also when an assertion fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!(
            "gstore_storage_corruption_{tag}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Every offset of `needle` in `hay`.
fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    hay.windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .collect()
}

/// A compacted store at `path` whose hub subject has ten objects under one
/// predicate; returns `(hub, p, objects)` and the inline `sp2o_vlist`
/// record that holds them (tag byte 0, then the VList).
fn compacted_hub(path: &Path) -> (u32, u32, Vec<u32>, Vec<u8>) {
    let mut ds = DiskStore::open(path, 64).unwrap();
    for i in 0..10 {
        ds.insert_triple(&Triple::new(
            Term::iri("http://ex/hub"),
            Term::iri("http://ex/p"),
            Term::iri(format!("http://ex/o{i}")),
        ))
        .unwrap();
    }
    ds.compact().unwrap();
    ds.flush().unwrap();
    let hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
    let p = ds
        .predicate_id(&Term::iri("http://ex/p").dict_key())
        .unwrap()
        .unwrap();
    let objects = ds.try_o_by_sp(hub, p).unwrap();
    assert_eq!(objects.len(), 10);
    assert_eq!(ds.compact_value_is_overflow(hub, p).unwrap(), Some(false));
    let mut record = vec![0u8];
    record.extend(vlist::encode_u32s(&objects));
    (hub, p, objects, record)
}

/// Rewrite the one inline copy of `record` in the file at `path` with `f`.
fn damage_record(path: &Path, record: &[u8], f: impl FnOnce(&mut [u8])) {
    let mut bytes = fs::read(path).unwrap();
    let at = find_all(&bytes, record);
    assert_eq!(at.len(), 1, "the record is stored once: {at:?}");
    f(&mut bytes[at[0]..at[0] + record.len()]);
    fs::write(path, &bytes).unwrap();
}

#[test]
fn a_vlist_record_that_does_not_decode_is_a_read_error() {
    let tmp = TempDir::new("vlist_count");
    let path = tmp.0.join("kvstore.kv");
    let (hub, p, _, record) = compacted_hub(&path);
    // The count says 15 objects; only ten deltas follow.
    damage_record(&path, &record, |r| r[1] = 15);
    let ds = DiskStore::open(&path, 64).unwrap();
    assert!(ds.is_compacted());
    let e = ds.try_o_by_sp(hub, p).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    assert!(e.to_string().contains("corrupt"), "{e}");
    assert!(ds.o_by_sp(hub, p).is_err());
}

#[test]
fn a_vlist_record_with_an_unknown_tag_is_a_read_error() {
    let tmp = TempDir::new("vlist_tag");
    let path = tmp.0.join("kvstore.kv");
    let (hub, p, _, record) = compacted_hub(&path);
    damage_record(&path, &record, |r| r[0] = 0x7f);
    let ds = DiskStore::open(&path, 64).unwrap();
    let e = ds.try_o_by_sp(hub, p).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    assert!(e.to_string().contains("tag 127"), "{e}");
}

#[test]
fn an_intact_vlist_record_still_reads() {
    // The control: the same steps without damage read the ten objects.
    let tmp = TempDir::new("vlist_intact");
    let path = tmp.0.join("kvstore.kv");
    let (hub, p, objects, record) = compacted_hub(&path);
    damage_record(&path, &record, |_| {});
    let ds = DiskStore::open(&path, 64).unwrap();
    assert_eq!(ds.try_o_by_sp(hub, p).unwrap(), objects);
}

/// A compacted store at `path` whose hub subject has so many objects under
/// one predicate that their VList spills into an overflow chain; returns
/// `(hub, p, objects)` and the VList's encoded length, the last four bytes
/// of its 9-byte head-pointer record (tag 1, head page, length).
fn compacted_overflow_hub(path: &Path) -> (u32, u32, Vec<u32>, u32) {
    let mut ds = DiskStore::open(path, 256).unwrap();
    for i in 0..3000 {
        ds.insert_triple(&Triple::new(
            Term::iri("http://ex/hub"),
            Term::iri("http://ex/p"),
            Term::iri(format!("http://ex/o{i}")),
        ))
        .unwrap();
    }
    ds.compact().unwrap();
    ds.flush().unwrap();
    let hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
    let p = ds
        .predicate_id(&Term::iri("http://ex/p").dict_key())
        .unwrap()
        .unwrap();
    let objects = ds.try_o_by_sp(hub, p).unwrap();
    assert_eq!(objects.len(), 3000);
    assert_eq!(ds.compact_value_is_overflow(hub, p).unwrap(), Some(true));
    let len = ds.compact_value_len(hub, p).unwrap().unwrap() as u32;
    assert_eq!(len as usize, vlist::encode_u32s(&objects).len());
    (hub, p, objects, len)
}

/// Rewrite the length of the one overflow record of length `len` in the
/// file at `path` to `new_len`.
fn damage_overflow_len(path: &Path, len: u32, new_len: u32) {
    let mut bytes = fs::read(path).unwrap();
    let at: Vec<usize> = bytes
        .windows(9)
        .enumerate()
        .filter(|(_, w)| w[0] == 1 && w[5..9] == len.to_le_bytes())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "the overflow record is stored once: {at:?}");
    bytes[at[0] + 5..at[0] + 9].copy_from_slice(&new_len.to_le_bytes());
    fs::write(path, &bytes).unwrap();
}

#[test]
fn a_vlist_overflow_record_whose_length_the_file_cannot_hold_is_a_read_error() {
    let tmp = TempDir::new("vlist_overflow_len");
    let path = tmp.0.join("kvstore.kv");
    let (hub, p, objects, len) = compacted_overflow_hub(&path);
    // The control: an intact record reads back.
    damage_overflow_len(&path, len, len);
    assert_eq!(
        DiskStore::open(&path, 64)
            .unwrap()
            .try_o_by_sp(hub, p)
            .unwrap(),
        objects
    );
    // More bytes than every page of the file holds: refused before the
    // chain is followed (a chain that cycles would otherwise be read for
    // up to 4 GiB).
    damage_overflow_len(&path, len, u32::MAX);
    let e = DiskStore::open(&path, 64)
        .unwrap()
        .try_o_by_sp(hub, p)
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    assert!(
        e.to_string().contains("of 4294967295 bytes in a file"),
        "{e}"
    );
    // One byte more than the chain's pages hold: the chain ends first.
    let chain = (len as usize).div_ceil(CHAIN_PAYLOAD) * CHAIN_PAYLOAD;
    damage_overflow_len(&path, u32::MAX, chain as u32 + 1);
    let e = DiskStore::open(&path, 64)
        .unwrap()
        .try_o_by_sp(hub, p)
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    assert!(e.to_string().contains("ends after"), "{e}");
}

/// The rows of `q`, sorted.
fn rows(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    match db.query(q) {
        Ok(QueryResult::Select(rs)) => {
            let mut r = rs.rows;
            r.sort();
            r
        }
        other => panic!("{q}: {other:?}"),
    }
}

#[test]
fn a_damaged_vstree_kv_node_filters_nothing_and_queries_stay_correct() {
    let tmp = TempDir::new("vstree_node");
    let nt = tmp.0.join("data.nt");
    let mut text = String::new();
    for i in 0..200 {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{}> .\n",
            i % 7,
            i % 13
        ));
        if i % 3 == 0 {
            text.push_str(&format!("<http://ex/s{i}> <http://ex/q> \"v{i}\" .\n"));
        }
    }
    fs::write(&nt, text).unwrap();
    let dir = tmp.0.join("db");
    Database::build_disk(&dir, &[&nt]).unwrap();
    let q = "SELECT ?s ?o WHERE { ?s <http://ex/p3> ?o . ?s <http://ex/q> ?v }";
    let expected = {
        let mut db = Database::open_with(
            &dir,
            &OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream),
        )
        .unwrap();
        let _tree = scoped_vs_policy(VsPolicy::Tree);
        rows(&mut db, q)
    };
    assert!(!expected.is_empty());
    // Give the root node of vstree.kv an entry count no page can hold.
    let vs = dir.join("vstree.kv");
    // The bulk build's file passes the reader's validation: a scan of every
    // node answers every entity.
    {
        let tree = VsTree::open_disk(&vs).unwrap();
        assert!(tree.entity_count() > 0);
        assert_eq!(
            tree.candidates(&Signature::new()).map(|c| c.len()),
            Some(tree.entity_count())
        );
    }
    let mut bytes = fs::read(&vs).unwrap();
    let root = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    assert!(root > 0, "a non-empty tree");
    bytes[root * PAGE_SIZE + 1..root * PAGE_SIZE + 3].copy_from_slice(&40u16.to_le_bytes());
    fs::write(&vs, &bytes).unwrap();
    assert_eq!(
        VsTree::open_disk(&vs)
            .unwrap()
            .candidates(&Signature::new()),
        None,
        "the damaged tree filters nothing"
    );
    for mode in [DiskQueryMode::Stream, DiskQueryMode::Materialize] {
        let mut db =
            Database::open_with(&dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap();
        assert!(db.stats().index_valid, "{mode:?}");
        for policy in [VsPolicy::Tree, VsPolicy::Auto, VsPolicy::Off] {
            let _p = scoped_vs_policy(policy);
            assert_eq!(rows(&mut db, q), expected, "{mode:?} {policy:?}");
        }
    }
}
