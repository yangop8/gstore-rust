//! Data test: the bulk build's import semantics are the sequential import's —
//! the same import report, the same `parse_error.log` lines (in order, up to
//! the timestamps of the `Info` lines), the same first error of a strict
//! import, at every thread count — and every RDF format, blank-node scoping
//! and named graphs load as the in-memory build loads them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget};
use gstore::parser::import::{BlankNodes, ImportOptions, ImportReport, ParseErrorLog, Source};
use gstore::{Database, OpenOptions};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_bulkimp_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(path: &Path, content: &str) -> PathBuf {
    fs::write(path, content).unwrap();
    path.to_path_buf()
}

const MIXED_TTL: &str = "@prefix ex: <http://ex.org/> .\n\
ex:a ex:p ex:o1 .\n\
ex:b ex:p undefined:x .\n\
ex:c ex:p \"12\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
ex:d ex:p \"abc\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
\"lit\" ex:p ex:o2 .\n\
ex:e ex:p \"NaN\"^^<http://www.w3.org/2001/XMLSchema#double> .\n\
ex:f ex:p ex:o3 ; ex:q ex:o4 .\n";

const TRIG: &str = "@prefix e: <http://e/> .\n\
e:s e:p e:default .\n\
e:g1 { e:s e:p e:one . e:s e:p e:two }\n\
GRAPH e:g2 { e:s e:p e:three }\n\
_:bg { e:s e:p e:four }\n";

const RDFXML: &str = "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
xmlns:ex=\"http://ex/\"><rdf:Description rdf:about=\"http://ex/a\"><ex:p>v</ex:p>\
<ex:q rdf:resource=\"http://ex/b\"/></rdf:Description></rdf:RDF>";

const JSONLD: &str = r#"{"@context": {"ex": "http://ex/"},
 "@id": "ex:j", "ex:name": "jay", "ex:knows": {"@id": "ex:a"}}"#;

/// A set of sources mixing every format, failures of every kind, blank
/// nodes, named graphs and an unparsable whole document.
fn sources(dir: &Path) -> Vec<Source> {
    let mut big = String::from("@prefix ex: <http://ex.org/> .\n");
    for i in 0..5000 {
        big.push_str(&format!("ex:s{i} ex:p{} \"v{i}\" .\n", i % 5));
        if i % 777 == 3 {
            big.push_str("ex:broken ex:p .\n");
        }
        if i % 1000 == 9 {
            big.push_str(&format!("_:b{} ex:p ex:s{i} .\n", i % 3));
        }
    }
    vec![
        Source::File(write(&dir.join("a_mixed.ttl"), MIXED_TTL)),
        Source::File(write(&dir.join("b_big.ttl"), &big)),
        Source::File(write(&dir.join("c_graphs.trig"), TRIG)),
        Source::File(write(
            &dir.join("d.nq"),
            "<http://e/s> <http://e/p> <http://e/five> <http://e/g1> .\n\
             <http://e/s> <http://e/p> <http://e/six> .\n\
             <http://e/s> <http://e/p> .\n\
             _:x <http://e/p> <http://e/seven> _:gx .\n",
        )),
        Source::File(write(&dir.join("e.rdf"), RDFXML)),
        Source::File(write(
            &dir.join("f_bad.rdf"),
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><unclosed></rdf:RDF>",
        )),
        Source::File(write(&dir.join("g.jsonld"), JSONLD)),
    ]
}

/// The log without the timestamps of its `Info` lines.
fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| match l.strip_prefix("Info ") {
            // "Info <YYYY-mm-dd HH:MM:SS> rest"
            Some(rest) => format!("Info {}", rest.get(20..).unwrap_or(rest)),
            None => l.to_string(),
        })
        .collect()
}

fn bulk_into(
    dir: &Path,
    srcs: &[Source],
    opts: &ImportOptions,
    threads: usize,
) -> (gstore::Result<ImportReport>, Vec<String>) {
    fs::create_dir_all(dir).unwrap();
    let log_path = dir.join("parse_error.log");
    let _ = fs::remove_file(&log_path);
    let mut log = ParseErrorLog::open(&log_path).unwrap();
    let r = bulk::build(
        dir,
        "b",
        BulkInput::Sources(srcs),
        opts,
        &mut log,
        BulkTarget::Disk,
        &BulkOptions {
            threads,
            memory_budget: 1 << 20,
            ..BulkOptions::default()
        },
    )
    .map(|r| r.import);
    drop(log);
    (r, log_lines(&log_path))
}

/// A database's triples and quads as strings.
type Content = (
    BTreeSet<(String, String, String)>,
    BTreeSet<(String, String, String, String)>,
);

/// Triples and quads of a database as strings.
fn content(db: &Database) -> Content {
    let d = db.dict();
    let s = |id| d.id_to_string(id).unwrap().to_string();
    let p = |id| d.predicate_to_string(id).unwrap().to_string();
    let triples = db
        .store()
        .iter_all()
        .map(|t| (s(t.sub), p(t.pred), s(t.obj)))
        .collect();
    let quads = db
        .named_graphs()
        .iter()
        .flat_map(|(&g, st)| st.iter_all().map(move |t| (g, t)))
        .map(|(g, t)| (s(g), s(t.sub), p(t.pred), s(t.obj)))
        .collect();
    (triples, quads)
}

/// Tolerant: the report and the log equal the sequential import's (the
/// incremental disk build and the in-memory build), at 1 to 8 threads.
#[test]
fn tolerant_report_and_log_equal_the_sequential_import() {
    let base = tmp("tolerant");
    let srcs = sources(&base);
    let opts = ImportOptions {
        blank_nodes: BlankNodes::Keep,
        ..ImportOptions::tolerant()
    };
    // The reference: the incremental disk build's log and report.
    let inc = base.join("inc.db");
    fs::create_dir_all(&inc).unwrap();
    let mut log = ParseErrorLog::open(inc.join("parse_error.log")).unwrap();
    let want = Database::build_disk_from_sources_incremental(&inc, &srcs, &opts, &mut log).unwrap();
    drop(log);
    let want_log = log_lines(&inc.join("parse_error.log"));
    assert!(want.failed >= 9, "{want:?}");
    assert_eq!(
        want_log.iter().filter(|l| l.starts_with("Info ")).count(),
        srcs.len()
    );
    let (mem, mem_report) = Database::build_from_sources("m", &srcs, &opts, &mut ()).unwrap();
    assert_eq!(mem_report, want);
    for threads in [1, 2, 3, 8] {
        let dir = base.join(format!("bulk{threads}.db"));
        let (r, lines) = bulk_into(&dir, &srcs, &opts, threads);
        assert_eq!(r.unwrap(), want, "threads {threads}");
        assert_eq!(lines, want_log, "threads {threads}");
        // Read through the materialized store and dictionary (`content`
        // uses the in-memory accessors), whatever the process's disk query
        // mode.
        let db = Database::open_with(&dir, &OpenOptions::materialize()).unwrap();
        assert_eq!(content(&db), content(&mem), "threads {threads}");
        assert_eq!(db.triple_num(), mem.triple_num());
    }
    let _ = fs::remove_dir_all(&base);
}

/// Strict: the first failure is the error, the same at every thread count
/// and the same as the sequential import's; an existing database stays.
#[test]
fn strict_first_error_equals_the_sequential_import() {
    let base = tmp("strict");
    let srcs = sources(&base);
    let opts = ImportOptions::strict();
    let want = Database::build_from_sources("m", &srcs, &opts, &mut ())
        .unwrap_err()
        .to_string();
    let inc =
        Database::build_disk_from_sources_incremental(base.join("inc.db"), &srcs, &opts, &mut ())
            .unwrap_err()
            .to_string();
    assert_eq!(inc, want);
    // Only the later sources: a failure in a later file.
    let later = &srcs[1..];
    let want_later = Database::build_from_sources("m", later, &opts, &mut ())
        .unwrap_err()
        .to_string();
    assert_ne!(want_later, want);
    let dir = base.join("bulk.db");
    let ok = write(
        &base.join("ok.nt"),
        "<http://e/x> <http://e/y> <http://e/z> .\n",
    );
    for threads in [1, 2, 4] {
        // A database to protect.
        bulk_into(&dir, &[Source::File(ok.clone())], &opts, 1)
            .0
            .unwrap();
        for (input, expect) in [(&srcs[..], &want), (later, &want_later)] {
            let (r, _) = bulk_into(&dir, input, &opts, threads);
            assert_eq!(&r.unwrap_err().to_string(), expect, "threads {threads}");
            assert!(!dir.join(bulk::STAGING_DIR).exists());
            assert_eq!(Database::open(&dir).unwrap().triple_num(), 1);
        }
    }
    // A missing source fails the same way too.
    let missing = vec![Source::File(base.join("missing.nt"))];
    let want = Database::build_from_sources("m", &missing, &opts, &mut ())
        .unwrap_err()
        .to_string();
    for threads in [1, 4] {
        let (r, _) = bulk_into(&base.join("missing.db"), &missing, &opts, threads);
        assert_eq!(r.unwrap_err().to_string(), want);
    }
    let _ = fs::remove_dir_all(&base);
}

/// Blank nodes: each document is its own scope by default (two files'
/// `_:b` are two nodes), as for the in-memory build; a blank graph label is
/// the graph's IRI.
#[test]
fn blank_nodes_and_graph_labels_follow_the_memory_build() {
    let base = tmp("blank");
    let a = write(
        &base.join("a.nt"),
        "_:b <http://e/p> \"one\" .\n_:b <http://e/q> _:c .\n",
    );
    let b = write(&base.join("b.nt"), "_:b <http://e/p> \"two\" .\n");
    let g = write(
        &base.join("g.nq"),
        "_:x <http://e/p> \"three\" _:g .\n_:g <http://e/r> \"about g\" .\n",
    );
    let srcs = vec![Source::File(a), Source::File(b), Source::File(g)];
    let dir = base.join("bulk.db");
    let (r, _) = bulk_into(&dir, &srcs, &ImportOptions::strict(), 2);
    r.unwrap();
    let (mem, _) =
        Database::build_from_sources("m", &srcs, &ImportOptions::strict(), &mut ()).unwrap();
    let mut db = Database::open(&dir).unwrap();
    let mut mem = mem;
    for q in [
        "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s <http://e/p> ?o }",
        "SELECT ?o WHERE { ?s <http://e/p> ?o } ORDER BY ?o",
        "SELECT ?o WHERE { GRAPH ?g { ?s <http://e/p> ?o } . ?g <http://e/r> ?about }",
    ] {
        let x = db.select(q).unwrap().rows;
        assert_eq!(x.len(), mem.select(q).unwrap().rows.len(), "{q}");
        assert!(!x.is_empty(), "{q}");
    }
    assert_eq!(db.entity_num(), mem.entity_num());
    assert_eq!(db.triple_num(), 4);
    let _ = fs::remove_dir_all(&base);
}
