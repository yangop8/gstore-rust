//! Deleting the triples of one stored blank node (round-4 review of
//! sparqlconf#16 / parser#13's fix).
//!
//! SPARQL 1.1 Update forbids blank nodes in `DELETE DATA`, `DELETE WHERE`
//! and DELETE templates, so `DELETE DATA { <a> <p> _:x }` — which earlier
//! gstore-rust versions read as "the stored node `_:x`", as upstream gStore
//! does — is refused now. The documented replacement deletes exactly that
//! node: a variable in the template, bound in the WHERE clause to the
//! stored node by `FILTER(sameTerm(?o, _:x))` (a label names the stored
//! node in a WHERE clause, as in a pattern; the label is the one a query
//! result shows, as a loader may relabel the blank nodes of a file). The
//! `isBlank` form the
//! changelog first gave deletes every blank node that matches; this test
//! pins both, on every storage layout.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::{Database, DiskQueryMode, GStoreError, OpenOptions, QueryResult};

/// A directory under the system temp dir, removed when dropped — also when
/// the test fails (the drop runs while unwinding).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_delete_bnode_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Memory,
    Snapshot,
    DiskMaterialize,
    DiskStream,
    #[cfg(feature = "rocksdb")]
    RocksDb,
}

fn layouts() -> Vec<Layout> {
    #[allow(unused_mut)]
    let mut v = vec![
        Layout::Memory,
        Layout::Snapshot,
        Layout::DiskMaterialize,
        Layout::DiskStream,
    ];
    #[cfg(feature = "rocksdb")]
    v.push(Layout::RocksDb);
    v
}

/// Two blank nodes with the same property value, and an IRI with it too.
const DATA: &str = "<http://ex/a> <http://ex/p> _:x .\n\
                    <http://ex/a> <http://ex/p> _:y .\n\
                    _:x <http://ex/name> \"same\" .\n\
                    _:y <http://ex/name> \"same\" .\n\
                    <http://ex/i> <http://ex/name> \"same\" .\n";

fn open(layout: Layout, tmp: &TmpDir) -> Database {
    let data = tmp.0.join("data.nt");
    fs::write(&data, DATA).unwrap();
    let mut db = match layout {
        Layout::Memory => Database::build_from_files("b", &[&data]).unwrap(),
        Layout::Snapshot => {
            let dir = tmp.0.join("snapshot");
            Database::build_from_files("b", &[&data])
                .unwrap()
                .save(&dir)
                .unwrap();
            Database::open(&dir).unwrap()
        }
        Layout::DiskMaterialize | Layout::DiskStream => {
            let dir = tmp.0.join("disk");
            Database::build_disk(&dir, &[&data]).unwrap();
            let mode = if layout == Layout::DiskStream {
                DiskQueryMode::Stream
            } else {
                DiskQueryMode::Materialize
            };
            Database::open_with(&dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap()
        }
        #[cfg(feature = "rocksdb")]
        Layout::RocksDb => {
            let dir = tmp.0.join("rocks");
            drop(Database::build_rocksdb_from_files("b", &dir, &[&data]).unwrap());
            Database::open_rocksdb(&dir).unwrap()
        }
    };
    db.set_query_cache_size(0);
    db
}

fn changed(db: &mut Database, u: &str) -> usize {
    match db.query(u).unwrap_or_else(|e| panic!("{u}: {e}")) {
        QueryResult::Update { changed } => changed,
        other => panic!("not an update result: {other:?}"),
    }
}

/// The column-0 values of a SELECT, sorted.
fn col0(db: &mut Database, q: &str) -> Vec<String> {
    let mut v: Vec<String> = db
        .select(q)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

/// How many triples match `pattern` (a WHERE body).
fn count(db: &mut Database, pattern: &str) -> usize {
    db.select(&format!("SELECT * WHERE {{ {pattern} }}"))
        .unwrap()
        .rows
        .len()
}

#[test]
fn same_term_deletes_exactly_the_named_blank_node_on_every_layout() {
    for layout in layouts() {
        let tmp = TmpDir::new("exact");
        let mut db = open(layout, &tmp);
        assert_eq!(db.triple_num(), 5, "{layout:?}");

        // The upstream form is refused, changing nothing.
        let e = db
            .query("DELETE DATA { <http://ex/a> <http://ex/p> _:x }")
            .unwrap_err();
        assert!(
            matches!(e, GStoreError::SparqlParse(_)),
            "{layout:?}: {e:?}"
        );
        assert_eq!(db.triple_num(), 5, "{layout:?}");

        // The stored labels, as a client reads them from a result (a
        // loader may relabel the blank nodes of a file).
        let labels = col0(
            &mut db,
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
        );
        assert_eq!(labels.len(), 2, "{layout:?}: {labels:?}");
        assert!(labels.iter().all(|l| l.starts_with("_:")), "{labels:?}");
        let (x, y) = (&labels[0], &labels[1]);

        // The documented replacement deletes the one triple naming `x`.
        assert_eq!(
            changed(
                &mut db,
                &format!(
                    "DELETE {{ <http://ex/a> <http://ex/p> ?o }} \
                     WHERE {{ <http://ex/a> <http://ex/p> ?o FILTER(sameTerm(?o, {x})) }}"
                ),
            ),
            1,
            "{layout:?}"
        );
        assert_eq!(
            col0(
                &mut db,
                "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"
            ),
            std::slice::from_ref(y),
            "{layout:?}"
        );
        assert_eq!(db.triple_num(), 4, "{layout:?}");

        // And a node's own triples, with the node as the subject.
        assert_eq!(
            changed(
                &mut db,
                &format!(
                    "DELETE {{ ?b <http://ex/name> \"same\" }} \
                     WHERE {{ ?b <http://ex/name> \"same\" FILTER(sameTerm(?b, {y})) }}"
                ),
            ),
            1,
            "{layout:?}"
        );
        assert_eq!(count(&mut db, &format!("{y} <http://ex/name> ?n")), 0);
        assert_eq!(count(&mut db, &format!("{x} <http://ex/name> ?n")), 1);
        assert_eq!(
            count(&mut db, "<http://ex/i> <http://ex/name> ?n"),
            1,
            "{layout:?}"
        );
    }
}

/// `FILTER isBlank(?b)` is not a replacement for a single-node delete: it
/// deletes every blank node that matches (the review's probe).
#[test]
fn is_blank_deletes_every_matching_blank_node() {
    let tmp = TmpDir::new("isblank");
    let mut db = open(Layout::Memory, &tmp);
    assert_eq!(
        changed(
            &mut db,
            "DELETE { ?b <http://ex/name> \"same\" } \
             WHERE { ?b <http://ex/name> \"same\" FILTER isBlank(?b) }",
        ),
        2
    );
    assert_eq!(count(&mut db, "<http://ex/i> <http://ex/name> \"same\""), 1);
}
