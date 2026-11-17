//! Graph management and the inference / bulk-load entry points.
//!
//! * One existence rule for every graph-management operation (review
//!   sparqlconf#16): CREATE GRAPH of a graph that holds triples, and DROP /
//!   CLEAR GRAPH of one that holds none, fail unless SILENT — as COPY / MOVE
//!   / ADD from a missing source already did — on every storage layout,
//!   changing nothing.
//! * Blank nodes are refused in DELETE DATA, DELETE WHERE and DELETE
//!   templates, and SECONDS is an exact `xsd:decimal` (sparqlconf#16); the
//!   SPARQL grammar's PREFIX, `.` separator and solution-modifier rules are
//!   enforced (parser#13).
//! * COPY / MOVE / ADD copy triples by id instead of re-interning their
//!   terms (db#16).
//! * The library inference entry points report a store read error through
//!   `try_materialize_rdfs` / `try_run_rules` instead of inferring less, and
//!   a `reasonManage` rule execution fails before it records or inserts
//!   anything when it cannot read which derived triples exist (design#7).
//! * The in-memory VS-tree is rebuilt after a clear of the default graph and
//!   after reasoning, instead of staying unused for the rest of the session
//!   (db#4).
//! * `build_from_ntriples_batched` does work linear in the file (db#15).
//!
//! The W3C-style cases of `tests/w3c/update-graph-management/` and
//! `tests/w3c/grammar/` check the same rules against the specification text.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::parser::sparql;
use gstore::{Database, DiskQueryMode, GStoreError, OpenOptions, QueryResult};

// ---- temporary directories ------------------------------------------------

/// A directory under the system temp dir, removed when dropped — also when
/// the test fails (the drop runs while unwinding).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_graph_mgmt_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let p = self.path(name);
        fs::write(&p, text).unwrap();
        p
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- storage layouts --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Memory,
    Snapshot,
    DiskMaterialize,
    DiskStream,
    #[cfg(feature = "rocksdb")]
    RocksDb,
}

impl Layout {
    fn all() -> Vec<Layout> {
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
}

/// A database of `layout` built from `data` (a TriG file) under `tmp`.
fn open(layout: Layout, data: &Path, tmp: &TmpDir) -> Database {
    open_with(layout, data, tmp, false)
}

/// [`open`], with RocksDB building its VS-tree at open when `vstree`.
#[cfg_attr(not(feature = "rocksdb"), allow(unused_variables))]
fn open_with(layout: Layout, data: &Path, tmp: &TmpDir, vstree: bool) -> Database {
    let mut db = match layout {
        Layout::Memory => Database::build_from_files("g", &[data]).unwrap(),
        Layout::Snapshot => {
            let dir = tmp.path("snapshot");
            Database::build_from_files("g", &[data])
                .unwrap()
                .save(&dir)
                .unwrap();
            Database::open(&dir).unwrap()
        }
        Layout::DiskMaterialize | Layout::DiskStream => {
            let dir = tmp.path("disk");
            Database::build_disk(&dir, &[data]).unwrap();
            let mode = if layout == Layout::DiskStream {
                DiskQueryMode::Stream
            } else {
                DiskQueryMode::Materialize
            };
            let db =
                Database::open_with(&dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap();
            assert_eq!(db.disk_query_mode(), Some(mode));
            db
        }
        #[cfg(feature = "rocksdb")]
        Layout::RocksDb => {
            let dir = tmp.path("rocks");
            drop(Database::build_rocksdb_from_files("g", &dir, &[data]).unwrap());
            Database::open_rocksdb_with(&dir, &OpenOptions::new().with_vstree_on_open(vstree))
                .unwrap()
        }
    };
    db.set_query_cache_size(0);
    db
}

/// Every quad of `db` as sorted `s p o [g]` lines (the N-Quads export).
fn dataset(db: &Database) -> Vec<String> {
    let mut out = Vec::new();
    gstore::zipio::write_export(db, gstore::zipio::ExportFormat::NQuads, &mut out).unwrap();
    let mut lines: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
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

fn changed(r: gstore::Result<QueryResult>) -> gstore::Result<usize> {
    match r? {
        QueryResult::Update { changed } => Ok(changed),
        other => panic!("not an update result: {other:?}"),
    }
}

const GRAPHS: &str = "@prefix : <http://ex/> .
:x :p :y .
:g1 { :a :p :b . :a :q \"v\"@en . }
";

// ---- sparqlconf#16: one existence rule --------------------------------------

#[test]
fn graph_management_follows_one_existence_rule_on_every_layout() {
    for layout in Layout::all() {
        let tmp = TmpDir::new("exist");
        let data = tmp.write("data.trig", GRAPHS);
        let mut db = open(layout, &data, &tmp);
        let before = dataset(&db);
        let fails = |db: &mut Database, u: &str| {
            let e = changed(db.query(u)).expect_err(u);
            assert!(
                matches!(e, GStoreError::Query(_)),
                "{layout:?} {u}: a request error, not {e:?}"
            );
            let msg = e.to_string();
            assert!(msg.contains("SILENT"), "{layout:?} {u}: {msg}");
        };
        // CREATE of a graph that exists; DROP / CLEAR of one that does not.
        fails(&mut db, "CREATE GRAPH <http://ex/g1>");
        fails(&mut db, "DROP GRAPH <http://ex/none>");
        fails(&mut db, "CLEAR GRAPH <http://ex/none>");
        fails(&mut db, "COPY <http://ex/none> TO <http://ex/g1>");
        fails(&mut db, "MOVE <http://ex/none> TO DEFAULT");
        fails(&mut db, "ADD GRAPH <http://ex/none> TO <http://ex/g1>");
        // The failing operation ends the request: the INSERT after it is
        // not applied.
        fails(
            &mut db,
            "DROP GRAPH <http://ex/none> ; INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/o> }",
        );
        assert_eq!(dataset(&db), before, "{layout:?}: a failure changed data");
        // SILENT turns each into a no-op.
        for u in [
            "CREATE SILENT GRAPH <http://ex/g1>",
            "DROP SILENT GRAPH <http://ex/none>",
            "CLEAR SILENT GRAPH <http://ex/none>",
            "COPY SILENT <http://ex/none> TO <http://ex/g1>",
        ] {
            assert_eq!(changed(db.query(u)).unwrap(), 0, "{layout:?} {u}");
        }
        assert_eq!(dataset(&db), before, "{layout:?}: SILENT changed data");
        // DEFAULT / NAMED / ALL never fail; a graph that does not exist may
        // be created.
        for u in [
            "CREATE GRAPH <http://ex/new>",
            "CLEAR NAMED",
            "DROP NAMED",
            "DROP DEFAULT",
            "CLEAR ALL",
            "DROP ALL",
        ] {
            changed(db.query(u)).unwrap_or_else(|e| panic!("{layout:?} {u}: {e}"));
        }
        assert!(dataset(&db).is_empty(), "{layout:?}");
        // A dropped graph does not exist any more: dropping it again fails,
        // creating it succeeds.
        changed(db.query("INSERT DATA { GRAPH <http://ex/g2> { <http://ex/s> <http://ex/p> 1 } }"))
            .unwrap();
        assert_eq!(changed(db.query("DROP GRAPH <http://ex/g2>")).unwrap(), 1);
        fails(&mut db, "DROP GRAPH <http://ex/g2>");
        assert_eq!(changed(db.query("CREATE GRAPH <http://ex/g2>")).unwrap(), 0);
    }
}

#[test]
fn a_refused_graph_operation_leaves_the_transaction_usable() {
    for layout in Layout::all() {
        let tmp = TmpDir::new("txn");
        let data = tmp.write("data.trig", GRAPHS);
        let mut db = open(layout, &data, &tmp);
        db.begin().unwrap();
        changed(db.query("INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/u> }")).unwrap();
        assert!(changed(db.query("CREATE GRAPH <http://ex/g1>")).is_err());
        assert!(changed(db.query("DROP GRAPH <http://ex/none>")).is_err());
        assert_eq!(changed(db.query("DROP GRAPH <http://ex/g1>")).unwrap(), 2);
        db.commit().unwrap();
        assert_eq!(
            col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"),
            vec!["<http://ex/t>", "<http://ex/x>"],
            "{layout:?}"
        );
        assert!(
            col0(&mut db, "SELECT ?g WHERE { GRAPH ?g { ?s ?p ?o } }").is_empty(),
            "{layout:?}"
        );
    }
}

#[test]
fn same_graph_transfers_stay_no_ops() {
    // §3.2.3-3.2.5: the operation has no effect when source and destination
    // are the same graph — also for a graph that does not exist.
    let tmp = TmpDir::new("same");
    let data = tmp.write("data.trig", GRAPHS);
    let mut db = open(Layout::Memory, &data, &tmp);
    for op in ["ADD", "MOVE", "COPY"] {
        for u in [
            format!("{op} <http://ex/g1> TO <http://ex/g1>"),
            format!("{op} <http://ex/none> TO <http://ex/none>"),
            format!("{op} DEFAULT TO DEFAULT"),
        ] {
            assert_eq!(changed(db.query(&u)).unwrap(), 0, "{u}");
        }
    }
}

// ---- sparqlconf#16 / parser#13: blank nodes in DELETE ------------------------

fn parse_error(text: &str) -> String {
    match sparql::parse(text) {
        Err(GStoreError::SparqlParse(m)) => m,
        other => panic!("{text}: expected a parse error, got {other:?}"),
    }
}

#[test]
fn blank_nodes_are_refused_where_sparql_update_forbids_them() {
    for (u, ctx) in [
        ("DELETE DATA { _:b <http://ex/p> \"x\" }", "DELETE DATA"),
        (
            "DELETE DATA { GRAPH <http://ex/g> { <http://ex/s> <http://ex/p> _:b } }",
            "DELETE DATA",
        ),
        ("DELETE WHERE { _:b <http://ex/p> ?o }", "DELETE WHERE"),
        (
            "DELETE { ?s <http://ex/p> _:b } WHERE { ?s <http://ex/p> ?o }",
            "a DELETE template",
        ),
        (
            "DELETE { GRAPH <http://ex/g> { _:b <http://ex/p> ?o } } WHERE { ?s <http://ex/p> ?o }",
            "a DELETE template",
        ),
        (
            "WITH <http://ex/g> DELETE { ?s <http://ex/p> _:b } INSERT { ?s <http://ex/q> 1 } \
             WHERE { ?s <http://ex/p> ?o }",
            "a DELETE template",
        ),
    ] {
        let m = parse_error(u);
        assert!(
            m.contains("blank nodes are not allowed in") && m.contains(ctx) && m.contains("_:b"),
            "{u}: {m}"
        );
    }
    // INSERT DATA, INSERT templates and WHERE patterns keep them.
    for ok in [
        "INSERT DATA { _:b <http://ex/p> \"x\" }",
        "INSERT { ?s <http://ex/q> _:b } WHERE { ?s <http://ex/p> ?o }",
        "DELETE { ?s <http://ex/p> ?o } INSERT { _:n <http://ex/p> ?o } WHERE { ?s <http://ex/p> ?o }",
        "DELETE { ?s <http://ex/p> ?o } WHERE { _:b <http://ex/p> ?o . ?s <http://ex/p> ?o }",
        "SELECT * WHERE { _:b <http://ex/p> ?o }",
    ] {
        sparql::parse(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    // Through a database, before anything is applied.
    let mut db = Database::build_from_str(
        "b",
        "_:b <http://ex/p> \"x\" .\n<http://ex/s> <http://ex/p> \"x\" .\n",
    )
    .unwrap();
    let e = db
        .query("DELETE DATA { _:b <http://ex/p> \"x\" }")
        .unwrap_err();
    assert!(matches!(e, GStoreError::SparqlParse(_)), "{e:?}");
    assert_eq!(db.triple_num(), 2);
}

// ---- sparqlconf#16: SECONDS -------------------------------------------------

#[test]
fn seconds_is_an_exact_decimal() {
    let mut db = Database::new("s");
    for (topk, name) in [
        (gstore::query::TopKOptions::NONE, "full"),
        (gstore::query::TopKOptions::default(), "stream"),
    ] {
        db.set_topk_options(topk);
        let rs = db
            .select(
                "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
                 SELECT ?a ?b ?c ?d ?e ?f WHERE {
                   BIND(SECONDS(\"2011-01-10T14:45:13.815-05:00\"^^xsd:dateTime) AS ?a)
                   BIND(DATATYPE(SECONDS(\"2020-03-04T05:06:07Z\"^^xsd:dateTime)) AS ?b)
                   BIND(SECONDS(\"2020-03-04T05:06:07Z\"^^xsd:dateTime) = 7 AS ?c)
                   BIND(SECONDS(\"2020-03-04T05:06:07.5Z\"^^xsd:dateTime) AS ?d)
                   BIND(SECONDS(\"2020-03-04T05:06:00.25\"^^xsd:dateTime) AS ?e)
                   BIND(SECONDS(\"2020-03-04\"^^xsd:date) AS ?f)
                 } LIMIT 10",
            )
            .unwrap();
        let dec = "^^<http://www.w3.org/2001/XMLSchema#decimal>";
        let row = &rs.rows[0];
        assert_eq!(
            row[0].as_deref(),
            Some(&*format!("\"13.815\"{dec}")),
            "{name}"
        );
        assert_eq!(
            row[1].as_deref(),
            Some("<http://www.w3.org/2001/XMLSchema#decimal>"),
            "{name}"
        );
        assert!(row[2].as_deref().unwrap().starts_with("\"true\""), "{name}");
        assert_eq!(row[3].as_deref(), Some(&*format!("\"7.5\"{dec}")), "{name}");
        assert_eq!(
            row[4].as_deref(),
            Some(&*format!("\"0.25\"{dec}")),
            "{name}"
        );
        // An xsd:date has no time, so no seconds: an error, unbound.
        assert_eq!(row[5], None, "{name}");
    }
}

// ---- parser#13: grammar leniencies ------------------------------------------

#[test]
fn a_prefix_declaration_has_no_local_part() {
    let m = parse_error("PREFIX foaf:knows <http://ex/ns#>\nSELECT ?n WHERE { ?s foaf:name ?n }");
    assert!(m.contains("foaf:knows"), "{m}");
    // Also in an update's prologue.
    parse_error("PREFIX ex:x <http://ex/>\nINSERT DATA { ex:a ex:b ex:c }");
    // The empty prefix and ordinary declarations still parse.
    sparql::parse("PREFIX : <http://ex/> PREFIX foaf: <http://xmlns.com/foaf/0.1/>\nSELECT * WHERE { ?s foaf:name :n }")
        .unwrap();
}

#[test]
fn triple_patterns_are_separated_by_a_dot() {
    for bad in [
        "SELECT * WHERE { ?s ?p ?o ?a ?b ?c }",
        "SELECT * WHERE { ?s ?p ?o . ?a ?b ?c ?d ?e ?f }",
        "SELECT * WHERE { GRAPH ?g { ?s ?p ?o ?a ?b ?c } }",
        "SELECT * WHERE { [ <http://ex/p> 1 ] [ <http://ex/p> 2 ] }",
        "CONSTRUCT { ?s ?p ?o ?o ?p ?s } WHERE { ?s ?p ?o }",
        "CONSTRUCT WHERE { ?s ?p ?o ?a ?b ?c }",
        "INSERT DATA { <http://ex/a> <http://ex/b> <http://ex/c> <http://ex/d> <http://ex/e> <http://ex/f> }",
        "INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/b> 1 <http://ex/d> <http://ex/e> 2 } }",
        "INSERT { ?s ?p ?o ?o ?p ?s } WHERE { ?s ?p ?o }",
        "DELETE { ?s ?p ?o ?o ?p ?s } WHERE { ?s ?p ?o }",
        "DELETE WHERE { ?s ?p ?o ?o ?p ?s }",
    ] {
        let m = parse_error(bad);
        assert!(m.contains("expected '.' between two triple patterns"), "{bad}: {m}");
    }
    for ok in [
        "SELECT * WHERE { ?s ?p ?o . ?a ?b ?c }",
        "SELECT * WHERE { ?s ?p ?o . ?a ?b ?c . }",
        "SELECT * WHERE { ?s ?p ?o ; ?q ?r , ?t . ?a ?b ?c }",
        "SELECT * WHERE { ?s ?p ?o FILTER(?o != 1) ?a ?b ?c }",
        "SELECT * WHERE { ?s ?p ?o OPTIONAL { ?s ?q ?r } ?a ?b ?c }",
        "SELECT * WHERE { ?s ?p ?o { ?a ?b ?c } ?d ?e ?f }",
        "SELECT * WHERE { ?s ?p ?o BIND(1 AS ?x) ?a ?b ?c }",
        "SELECT * WHERE { [ <http://ex/p> 1 ] . [ <http://ex/p> 2 ] }",
        "CONSTRUCT { ?s ?p ?o . ?o ?p ?s } WHERE { ?s ?p ?o }",
        "INSERT DATA { <http://ex/a> <http://ex/b> 1 . GRAPH <http://ex/g> { <http://ex/a> <http://ex/b> 2 } <http://ex/c> <http://ex/b> 3 }",
        "DELETE { ?s ?p ?o . GRAPH ?g { ?s ?p ?o } ?o ?p ?s } WHERE { GRAPH ?g { ?s ?p ?o } }",
    ] {
        sparql::parse(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn solution_modifiers_come_once_each_and_in_order() {
    for bad in [
        "SELECT * WHERE { ?s ?p ?o } LIMIT 1 LIMIT 2",
        "SELECT * WHERE { ?s ?p ?o } OFFSET 1 OFFSET 2",
        "SELECT * WHERE { ?s ?p ?o } LIMIT 1 OFFSET 2 LIMIT 3",
        "SELECT * WHERE { ?s ?p ?o } ORDER BY ?s ORDER BY ?o",
        "SELECT * WHERE { ?s ?p ?o } OFFSET 3 ORDER BY ?s LIMIT 1",
        "SELECT * WHERE { ?s ?p ?o } LIMIT 1 ORDER BY ?s",
        "SELECT * WHERE { { SELECT ?s WHERE { ?s ?p ?o } LIMIT 1 LIMIT 2 } }",
    ] {
        let m = parse_error(bad);
        assert!(m.contains("misplaced solution modifier"), "{bad}: {m}");
    }
    let slice = |q: &str| match sparql::parse(q).unwrap() {
        sparql::ast::Query::Select(s) => (s.order_by.len(), s.limit, s.offset),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        slice("SELECT * WHERE { ?s ?p ?o } ORDER BY ?s LIMIT 2 OFFSET 1"),
        (1, Some(2), Some(1))
    );
    assert_eq!(
        slice("SELECT * WHERE { ?s ?p ?o } ORDER BY ?s OFFSET 1 LIMIT 2"),
        (1, Some(2), Some(1))
    );
}

#[test]
fn a_labelled_blank_node_in_a_where_pattern_stays_a_constant() {
    // The documented reading (CHANGELOG Phase 3 known gaps) is unchanged:
    // `_:b` names the stored node in a pattern.
    let mut db = Database::build_from_str(
        "c",
        "_:b <http://ex/p> \"x\" .\n<http://ex/s> <http://ex/p> \"y\" .\n",
    )
    .unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?o WHERE { _:b <http://ex/p> ?o }"),
        vec!["\"x\""]
    );
}

// ---- db#4: the in-memory VS-tree comes back ---------------------------------

/// A dataset whose queries the VS-tree prunes: every `:e<i>` has a `:kind`
/// and a `:v`; `:t` holds the RDFS schema.
fn vs_data() -> String {
    let mut s = String::from(
        "@prefix : <http://ex/> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         :C rdfs:subClassOf :D .\n\
         :g1 { :n0 :kind :k0 . :n1 :kind :k1 . }\n",
    );
    for i in 0..60 {
        s.push_str(&format!(":e{i} :kind :k{} ; :v {i} ; a :C .\n", i % 3));
    }
    s
}

const KIND0: &str = "SELECT ?s WHERE { ?s <http://ex/kind> <http://ex/k0> . ?s <http://ex/v> ?v }";

#[test]
fn the_in_memory_vstree_is_rebuilt_after_clears_copies_moves_and_reasoning() {
    #[allow(unused_mut)]
    let mut layouts = vec![Layout::Memory, Layout::Snapshot, Layout::DiskMaterialize];
    #[cfg(feature = "rocksdb")]
    layouts.push(Layout::RocksDb);
    for layout in layouts {
        let tmp = TmpDir::new("vstree");
        let data = tmp.write("data.trig", &vs_data());
        let mut db = open_with(layout, &data, &tmp, true);
        assert!(db.stats().index_valid, "{layout:?}: built with a tree");
        let k0 = col0(&mut db, KIND0);
        assert_eq!(k0.len(), 20, "{layout:?}");

        let step = |db: &mut Database, u: &str, expect: &[String]| {
            changed(db.query(u)).unwrap_or_else(|e| panic!("{layout:?} {u}: {e}"));
            assert!(
                db.stats().index_valid,
                "{layout:?} after {u}: tree not in use"
            );
            assert_eq!(col0(db, KIND0), expect, "{layout:?} after {u}");
        };
        // COPY DEFAULT away and back: the clear rebuilds an empty tree, the
        // copy's inserts are folded into it.
        step(&mut db, "COPY DEFAULT TO <http://ex/saved>", &k0);
        step(&mut db, "CLEAR DEFAULT", &[]);
        step(&mut db, "COPY <http://ex/saved> TO DEFAULT", &k0);
        step(&mut db, "MOVE DEFAULT TO <http://ex/moved>", &[]);
        step(&mut db, "MOVE <http://ex/moved> TO DEFAULT", &k0);
        step(&mut db, "DROP DEFAULT", &[]);
        step(
            &mut db,
            "INSERT DATA { <http://ex/z> <http://ex/kind> <http://ex/k0> ; <http://ex/v> 1 }",
            &["<http://ex/z>".to_string()],
        );
        // Inside a transaction the rebuild waits for its end; a rollback
        // brings the old graph and a valid tree back.
        db.begin().unwrap();
        changed(db.query("CLEAR DEFAULT")).unwrap();
        db.rollback().unwrap();
        assert!(db.stats().index_valid, "{layout:?} after a rollback");
        assert_eq!(col0(&mut db, KIND0), vec!["<http://ex/z>"], "{layout:?}");
        db.begin().unwrap();
        changed(db.query("CLEAR DEFAULT")).unwrap();
        changed(db.query("COPY <http://ex/saved> TO DEFAULT")).unwrap();
        db.commit().unwrap();
        assert!(db.stats().index_valid, "{layout:?} after a commit");
        assert_eq!(col0(&mut db, KIND0), k0, "{layout:?}");
        assert_eq!(db.vs_shrink_debt(), 0, "{layout:?}: the tree was rebuilt");
        step(&mut db, "DROP ALL", &[]);
    }
}

#[test]
fn reasoning_keeps_the_in_memory_vstree_in_use() {
    #[allow(unused_mut)]
    let mut layouts = vec![Layout::Memory, Layout::Snapshot, Layout::DiskMaterialize];
    #[cfg(feature = "rocksdb")]
    layouts.push(Layout::RocksDb);
    let typed_d = "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
                   <http://ex/D> . ?s <http://ex/kind> <http://ex/k1> }";
    let knows = "SELECT ?s WHERE { ?s <http://ex/knows> <http://ex/e0> . ?s <http://ex/v> ?v }";
    for layout in layouts {
        let tmp = TmpDir::new("vsinfer");
        let data = tmp.write("data.trig", &vs_data());
        let mut db = open_with(layout, &data, &tmp, true);
        assert!(col0(&mut db, typed_d).is_empty());
        assert_eq!(db.try_materialize_rdfs().unwrap(), 60, "{layout:?}");
        assert!(db.stats().index_valid, "{layout:?}: tree unused after RDFS");
        assert_eq!(col0(&mut db, typed_d).len(), 20, "{layout:?}");
        db.add_rule(
            "link",
            "?x <http://ex/kind> <http://ex/k2> => ?x <http://ex/knows> <http://ex/e0>",
        )
        .unwrap();
        assert_eq!(db.try_run_rules().unwrap(), 20, "{layout:?}");
        assert!(
            db.stats().index_valid,
            "{layout:?}: tree unused after rules"
        );
        assert_eq!(col0(&mut db, knows).len(), 20, "{layout:?}");
    }
}

// ---- design#7: inference reports store read errors ---------------------------

const RDFS: &str = "<http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/C> .
<http://ex/C> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://ex/D> .
<http://ex/a> <http://ex/knows> <http://ex/b> .
";

const SYMMETRIC: &str = "?x <http://ex/knows> ?y => ?y <http://ex/knows> ?x";

/// Overwrite the root page of the tree in header slot `slot` of `kvstore.kv`
/// with garbage (as `tests/dt_overlay.rs` does): every read of that tree
/// fails.
fn corrupt_root(path: &Path, slot: usize) {
    let mut bytes = fs::read(path).unwrap();
    let off = 16 + slot * 8;
    let page = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
    assert!(page > 0, "the tree has a root page");
    bytes[page * 4096..(page + 1) * 4096].fill(0xFF);
    fs::write(path, bytes).unwrap();
}

#[test]
fn stream_mode_inference_reports_a_read_error() {
    const POS: usize = 1; // header slot of the POS tree
    let tmp = TmpDir::new("infer_stream");
    let data = tmp.write("rdfs.nt", RDFS);
    let stream = OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream);
    // A healthy store infers.
    let healthy = tmp.path("healthy");
    Database::build_disk(&healthy, &[&data]).unwrap();
    {
        let mut db = Database::open_with(&healthy, &stream).unwrap();
        assert_eq!(db.try_materialize_rdfs().unwrap(), 1);
        db.add_rule("sym", SYMMETRIC).unwrap();
        assert_eq!(db.try_run_rules().unwrap(), 1);
    }
    // A store whose POS tree cannot be read fails the run instead of
    // inferring nothing.
    let broken = tmp.path("broken");
    Database::build_disk(&broken, &[&data]).unwrap();
    corrupt_root(&broken.join("kvstore.kv"), POS);
    let mut db = Database::open_with(&broken, &stream).unwrap();
    let e = db.try_materialize_rdfs().unwrap_err();
    assert!(
        e.to_string()
            .contains("a read from the store failed while materializing the RDFS closure"),
        "{e}"
    );
    assert_eq!(e.kind(), gstore::error::ErrorKind::Storage, "{e}");
    db.add_rule("sym", SYMMETRIC).unwrap();
    let e = db.try_run_rules().unwrap_err();
    assert!(
        e.to_string()
            .contains("a read from the store failed while running the reasoning rules"),
        "{e}"
    );
    // The old entry points keep their signatures (the error is logged).
    let _: usize = db.materialize_rdfs();
    let _: usize = db.run_rules();
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocksdb_inference_reports_a_read_error() {
    let tmp = TmpDir::new("infer_rocks");
    let dir = tmp.path("r.db");
    drop(Database::build_rocksdb_from_str("r", &dir, RDFS).unwrap());
    let mut db = Database::open(&dir).unwrap();
    db.add_rule("sym", SYMMETRIC).unwrap();
    db.backend().as_rocks().unwrap().debug_fail_reads(true);
    let e = db.try_materialize_rdfs().unwrap_err();
    assert!(
        e.to_string().contains("materializing the RDFS closure"),
        "{e}"
    );
    let e = db.try_run_rules().unwrap_err();
    assert!(e.to_string().contains("running the reasoning rules"), "{e}");
    db.backend().as_rocks().unwrap().debug_fail_reads(false);
    assert_eq!(db.try_materialize_rdfs().unwrap(), 1);
    assert_eq!(db.try_run_rules().unwrap(), 1);
}

/// An inverse-relation rule (`reasonManage`): `?x :p ?y` derives
/// `?y <Rule:inv> ?x`.
const INVERSE_RULE: &str = r#"{"rulename": "inv", "description": "", "isenable": 1,
    "type": 1, "logic": 1,
    "conditions": [{"patterns": [{"subject": "?x", "predicate": "<http://ex/p>", "object": "?y"}],
                    "filters": [], "count_info": {}}],
    "return": {"source": "?y", "target": "?x", "label": "inv"}}"#;

#[test]
fn rule_execution_fails_before_recording_when_existence_cannot_be_read() {
    // review design#7 (reason_infer): whether each derived triple already
    // exists is read in a read scope. Here the SPO tree is unreadable, while
    // the rule's WHERE pattern (predicate bound) reads the POS tree: the run
    // must fail before the ledger or the store change. Read outside a scope,
    // every derived triple counted as new, the insert went ahead on the
    // failing store, broke it with a write error and left the rule marked
    // executed.
    const SPO: usize = 0; // header slot of the SPO tree
    let tmp = TmpDir::new("rule_spo");
    let data = tmp.write(
        "relations.nt",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
         <http://ex/b> <Rule:inv> <http://ex/a> .\n\
         <http://ex/c> <http://ex/p> <http://ex/d> .\n",
    );
    let dir = tmp.path("db");
    Database::build_disk(&dir, &[&data]).unwrap();
    corrupt_root(&dir.join("kvstore.kv"), SPO);
    let stream = OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream);
    let mut db = Database::open_with(&dir, &stream).unwrap();
    db.reason_add(&gstore::json::parse(INVERSE_RULE).unwrap())
        .unwrap();
    db.reason_compile("inv").unwrap();
    let e = db.reason_execute("inv").unwrap_err();
    assert!(
        e.message
            .contains("a read from the store failed while checking which derived triples exist"),
        "{e:?}"
    );
    let status = db.reason_show("inv").unwrap();
    assert_eq!(
        status.get("status").and_then(|s| s.as_str()),
        Some("已编译"),
        "the rule stays compiled, not executed"
    );
    // Nothing was written: the store still answers (through POS) and holds
    // only the triple that was there.
    let derived = col0(&mut db, "SELECT ?s WHERE { ?s <Rule:inv> ?o }");
    assert_eq!(derived, vec!["<http://ex/b>"]);
}

// ---- db#15: the batched N-Triples build is linear ----------------------------

/// The triples a batched build of `n` generated triples sorted into the
/// store's indexes, and the database.
fn batched_build_work(tmp: &TmpDir, n: usize, batch: usize) -> (u64, Database) {
    let mut text = String::new();
    for i in 0..n {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{}> .\n",
            i % 8,
            i % 997
        ));
    }
    let file = tmp.write(&format!("batched_{n}.nt"), &text);
    let before = gstore::store::bulk_load_sorted();
    let db = Database::build_from_ntriples_batched("b", &file, batch).unwrap();
    (gstore::store::bulk_load_sorted() - before, db)
}

#[test]
fn the_batched_ntriples_build_sorts_each_triple_a_bounded_number_of_times() {
    let tmp = TmpDir::new("batched");
    let (w1, db1) = batched_build_work(&tmp, 3_000, 100);
    let (w2, db2) = batched_build_work(&tmp, 6_000, 100);
    assert_eq!(db1.triple_num(), 3_000);
    assert_eq!(db2.triple_num(), 6_000);
    // Linear: a constant number of sorts per triple, whatever the batch
    // (loading every batch into the growing store re-sorted all of it:
    // 3000 triples in batches of 100 cost ~46 500 triple sorts).
    assert!(w1 <= 2 * 3_000, "work {w1} for 3000 triples");
    assert!(w2 <= 2 * 6_000, "work {w2} for 6000 triples");
    assert!(
        w2 <= 2 * w1 + 100,
        "doubling the input more than doubled the work: {w1} -> {w2}"
    );
    // Same answers as the whole-file build.
    let whole = Database::build_from_files("w", &[tmp.path("batched_3000.nt")]).unwrap();
    assert_eq!(dataset(&db1), dataset(&whole));
}

// ---- db#16: COPY / MOVE / ADD by id -------------------------------------------

#[test]
fn transfers_copy_by_id_without_reading_the_dictionary_per_triple() {
    // Stream mode reads its dictionary through counted caches: a transfer
    // that turned every triple into terms and interned them again did six
    // look-ups per triple.
    let tmp = TmpDir::new("byid");
    let mut text = String::from("@prefix : <http://ex/> .\n");
    for i in 0..300 {
        text.push_str(&format!(":s{i} :p \"v{i}\"@en ; :q _:b{i} .\n"));
    }
    let data = tmp.write("data.trig", &text);
    let mut db = open(Layout::DiskStream, &data, &tmp);
    let before = dataset(&db);
    let lookups = |db: &Database| {
        let s = db.dict().cache_stats();
        s.hits + s.misses
    };
    for (u, n) in [
        ("COPY DEFAULT TO <http://ex/g>", 600),
        ("ADD <http://ex/g> TO <http://ex/h>", 600),
        // MOVE: the default graph's 600 cleared, 600 added, :h's 600 dropped.
        ("MOVE <http://ex/h> TO DEFAULT", 1800),
    ] {
        let l0 = lookups(&db);
        assert_eq!(changed(db.query(u)).unwrap(), n, "{u}");
        let used = lookups(&db) - l0;
        assert!(
            used < 100,
            "{u}: {used} dictionary look-ups for 600 triples"
        );
    }
    // The default graph is back as it was; :g holds a copy of it, the same
    // blank nodes included.
    let after = dataset(&db);
    let default: Vec<&String> = after
        .iter()
        .filter(|l| !l.contains("<http://ex/g>"))
        .collect();
    assert_eq!(default, before.iter().collect::<Vec<_>>());
    assert_eq!(after.len(), 1200);
}

#[test]
fn transfers_give_the_same_dataset_on_every_layout() {
    let data_text = "@prefix : <http://ex/> .
:a :name \"Alice\"@en ; :age 30 ; :knows _:b .
_:b :name \"Bob\" .
:g1 { :c :p \"c\"^^<http://ex/dt> . :c :q _:n . }
";
    let ops = "COPY DEFAULT TO <http://ex/g2> ; ADD <http://ex/g1> TO <http://ex/g2> ; \
               MOVE <http://ex/g1> TO <http://ex/g3> ; ADD <http://ex/g3> TO DEFAULT";
    let mut reference: Option<Vec<String>> = None;
    for layout in Layout::all() {
        let tmp = TmpDir::new("same_ds");
        let data = tmp.write("data.trig", data_text);
        let mut db = open(layout, &data, &tmp);
        changed(db.query(ops)).unwrap_or_else(|e| panic!("{layout:?}: {e}"));
        // Blank-node labels may differ between layouts.
        let mut ds: Vec<String> = dataset(&db)
            .iter()
            .map(|l| {
                l.split(' ')
                    .map(|t| if t.starts_with("_:") { "_:B" } else { t })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        ds.sort();
        // Default graph 4 + 2 (from :g3), :g2 4 + 2, :g3 2.
        assert_eq!(ds.len(), 14, "{layout:?}: {ds:#?}");
        match &reference {
            None => reference = Some(ds),
            Some(r) => assert_eq!(&ds, r, "{layout:?}"),
        }
    }
}
