//! SPARQL built-ins whose answer belongs to the query execution, and the
//! aggregates: what the review found wrong in them and what SPARQL 1.1
//! requires instead.
//!
//! * `BNODE()` / `BNODE(str)` and template blank nodes are fresh — never a
//!   node of the store, of another solution or of another request
//!   (§17.4.2.9, §16.2.1, Update §3.1.3; review query#5);
//! * `UUID()` / `STRUUID()` / `RAND()` are safe to call from concurrent
//!   queries (review conc#1) and `NOW()` is one instant per query, `GRAPH`
//!   blocks included (§17.4.5.1; review query#16).
//!
//! The spec-defined answers of these functions that do not depend on
//! randomness are also cases of the W3C-style subset (`tests/w3c/`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use gstore::{Database, QueryResult, ResultSet, Value};

// ---- helpers ---------------------------------------------------------------

/// A directory under the system temp dir, removed when dropped — also when
/// the test fails (the drop runs while unwinding).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_sparql_builtins_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn db(data: &str) -> Database {
    Database::build_from_str("builtins", data).expect("build the test database")
}

fn select(db: &mut Database, q: &str) -> ResultSet {
    match db.query(q) {
        Ok(QueryResult::Select(rs)) => rs,
        other => panic!("{q}: expected a SELECT result, got {other:?}"),
    }
}

fn update(db: &mut Database, u: &str) {
    match db.query(u) {
        Ok(QueryResult::Update { .. }) => {}
        other => panic!("{u}: expected an update, got {other:?}"),
    }
}

/// The cell of column `var` in `row`.
fn cell<'r>(rs: &'r ResultSet, row: usize, var: &str) -> Option<&'r str> {
    let col = rs
        .vars
        .iter()
        .position(|v| v == var)
        .unwrap_or_else(|| panic!("no column {var} in {:?}", rs.vars));
    rs.rows[row][col].as_deref()
}

fn is_blank(cell: Option<&str>) -> bool {
    cell.is_some_and(|c| c.starts_with("_:"))
}

// ---- BNODE and template blank nodes (review query#5) ----------------------

/// §17.4.2.9: `BNODE(str)` is the same node for the same string within one
/// solution mapping and a different node in every other solution — also
/// when the second call sees the first one's result bound (`BIND` after
/// `BIND`). The labels were a hash of the string, so every solution, query
/// and process shared one node per string.
#[test]
fn bnode_of_a_string_is_one_node_per_solution() {
    let mut db = db("<http://ex/s1> <http://ex/str> \"foo\" .\n\
                     <http://ex/s2> <http://ex/str> \"foo\" .\n\
                     <http://ex/s3> <http://ex/str> \"bar\" .\n");
    for q in [
        "SELECT ?s ?b1 ?b2 WHERE { ?s <http://ex/str> ?v \
         BIND(BNODE(?v) AS ?b1) BIND(BNODE(?v) AS ?b2) }",
        "SELECT ?s (BNODE(?v) AS ?b1) (BNODE(?v) AS ?b2) WHERE { ?s <http://ex/str> ?v }",
        "SELECT ?s ?b1 ?b2 WHERE { ?s <http://ex/str> ?v \
         BIND(BNODE(\"x\") AS ?b1) BIND(BNODE(\"x\") AS ?b2) }",
    ] {
        let rs = select(&mut db, q);
        assert_eq!(rs.row_count(), 3, "{q}");
        let mut nodes = HashSet::new();
        for row in 0..rs.row_count() {
            let (b1, b2) = (cell(&rs, row, "b1"), cell(&rs, row, "b2"));
            assert!(is_blank(b1), "{q}: row {row}: {b1:?}");
            assert_eq!(b1, b2, "{q}: one node per string within a solution");
            nodes.insert(b1.unwrap().to_string());
        }
        assert_eq!(
            nodes.len(),
            3,
            "{q}: a new node in every solution: {nodes:?}"
        );
        // And a new one in the next query.
        let again = select(&mut db, q);
        for row in 0..again.row_count() {
            let b = cell(&again, row, "b1").unwrap();
            assert!(!nodes.contains(b), "{q}: {b} came back in a second query");
        }
    }
    // Two strings in one solution: two nodes.
    let rs = select(
        &mut db,
        "SELECT (BNODE(\"a\") AS ?x) (BNODE(\"b\") AS ?y) (BNODE(\"a\") AS ?z) WHERE {}",
    );
    assert_ne!(cell(&rs, 0, "x"), cell(&rs, 0, "y"));
    assert_eq!(cell(&rs, 0, "x"), cell(&rs, 0, "z"));
}

/// `BNODE()` minted labels from a per-process counter starting at 0, so
/// two processes (or one server before and after a restart) inserting
/// `BIND(BNODE() AS ?b)` wrote the same stored node `_:b0`, merging two
/// resources for good.
#[test]
fn bnode_without_argument_is_fresh_across_processes() {
    let tmp = TempDir::new("bnode_processes");
    let nt = tmp.path().join("data.nt");
    std::fs::write(&nt, "<http://ex/c> <http://ex/p> <http://ex/d> .\n").unwrap();
    let name = tmp.path().join("db");
    let build = Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .arg(&name)
        .arg(&nt)
        .output()
        .expect("run gbuild");
    assert!(build.status.success(), "gbuild failed: {build:?}");
    for subject in ["http://ex/a", "http://ex/b"] {
        let out = Command::new(env!("CARGO_BIN_EXE_gquery"))
            .arg(&name)
            .arg("-e")
            .arg(format!(
                "INSERT {{ <{subject}> <http://ex/new> ?b }} WHERE {{ BIND(BNODE() AS ?b) }}"
            ))
            .output()
            .expect("run gquery");
        assert!(out.status.success(), "gquery failed: {out:?}");
    }
    let mut db = Database::open(format!("{}.db", name.display())).expect("open the database");
    let rs = select(
        &mut db,
        "SELECT ?s ?b WHERE { ?s <http://ex/new> ?b } ORDER BY ?s",
    );
    assert_eq!(rs.row_count(), 2, "{rs:?}");
    assert!(
        is_blank(cell(&rs, 0, "b")) && is_blank(cell(&rs, 1, "b")),
        "{rs:?}"
    );
    assert_ne!(
        cell(&rs, 0, "b"),
        cell(&rs, 1, "b"),
        "two processes minted the same blank node"
    );
    let shared = select(
        &mut db,
        "SELECT ?s1 ?s2 WHERE { ?s1 <http://ex/new> ?b . ?s2 <http://ex/new> ?b \
         FILTER(?s1 != ?s2) }",
    );
    assert_eq!(shared.row_count(), 0, "{shared:?}");
}

/// The query's blank-node state reaches `GRAPH` blocks: `BNODE()` inside a
/// `GRAPH` block is a node of its own, and `BNODE("x")` inside and after
/// the block are one node for one solution.
#[test]
fn bnode_state_reaches_graph_blocks() {
    let mut db = db("");
    update(
        &mut db,
        "INSERT DATA { GRAPH <http://ex/g> { <http://ex/s1> <http://ex/p> <http://ex/o> . \
         <http://ex/s2> <http://ex/p> <http://ex/o> } }",
    );
    let rs = select(
        &mut db,
        "SELECT ?s ?in ?out ?x1 ?x2 WHERE { BIND(BNODE() AS ?out) \
         GRAPH <http://ex/g> { ?s <http://ex/p> ?o BIND(BNODE() AS ?in) BIND(BNODE(\"x\") AS ?x1) } \
         BIND(BNODE(\"x\") AS ?x2) }",
    );
    assert_eq!(rs.row_count(), 2, "{rs:?}");
    let mut fresh = HashSet::new();
    let mut xs = HashSet::new();
    for row in 0..2 {
        assert!(is_blank(cell(&rs, row, "in")), "{rs:?}");
        fresh.insert(cell(&rs, row, "in").unwrap().to_string());
        assert_eq!(
            cell(&rs, row, "x1"),
            cell(&rs, row, "x2"),
            "BNODE(\"x\") inside and after GRAPH in one solution: {rs:?}"
        );
        xs.insert(cell(&rs, row, "x1").unwrap().to_string());
    }
    let out = cell(&rs, 0, "out").unwrap().to_string();
    assert_eq!(fresh.len(), 2, "{rs:?}");
    assert!(!fresh.contains(&out), "{rs:?}");
    assert_eq!(xs.len(), 2, "one BNODE(\"x\") node per solution: {rs:?}");
    assert!(xs.is_disjoint(&fresh) && !xs.contains(&out), "{rs:?}");
}

/// A labelled blank node in an INSERT or CONSTRUCT template (`_:n`) is a
/// fresh node for every solution (and so for every request), the same one
/// across the template triples of a solution. It was copied verbatim, so
/// every solution and every later request wrote the one stored node `_:n`.
#[test]
fn template_blank_nodes_are_fresh_per_solution_and_request() {
    let mut db = db("<http://ex/a> <http://ex/q> \"1\" .\n\
                     <http://ex/b> <http://ex/q> \"2\" .\n");
    let insert = "INSERT { ?s <http://ex/p> _:n . _:n <http://ex/v> ?o } \
                  WHERE { ?s <http://ex/q> ?o }";
    update(&mut db, insert);
    update(&mut db, insert);
    let rs = select(
        &mut db,
        "SELECT ?s ?n ?o WHERE { ?s <http://ex/p> ?n . ?n <http://ex/v> ?o }",
    );
    // Each insert links each subject to its own node, which carries only
    // that subject's value.
    assert_eq!(rs.row_count(), 4, "{rs:?}");
    let nodes: HashSet<_> = (0..4).map(|r| cell(&rs, r, "n").unwrap()).collect();
    assert_eq!(nodes.len(), 4, "{rs:?}");
    for r in 0..4 {
        let s = cell(&rs, r, "s").unwrap();
        let o = cell(&rs, r, "o").unwrap();
        assert_eq!(s == "<http://ex/a>", o.contains('1'), "{rs:?}");
    }
    // CONSTRUCT: a fresh node per solution, and per run.
    let construct = "CONSTRUCT { ?s <http://ex/p> _:v . _:v <http://ex/v> ?o } \
                     WHERE { ?s <http://ex/q> ?o }";
    let mut seen = HashSet::new();
    for _ in 0..2 {
        let ts = match db.query(construct) {
            Ok(QueryResult::Construct(ts)) => ts,
            other => panic!("expected CONSTRUCT, got {other:?}"),
        };
        assert_eq!(ts.len(), 4, "{ts:?}");
        let nodes: HashSet<String> = ts
            .iter()
            .filter(|t| t.predicate.to_string() == "<http://ex/p>")
            .map(|t| t.object.to_string())
            .collect();
        assert_eq!(nodes.len(), 2, "{ts:?}");
        for t in ts
            .iter()
            .filter(|t| t.predicate.to_string() == "<http://ex/v>")
        {
            assert!(nodes.contains(&t.subject.to_string()), "{ts:?}");
        }
        for n in nodes {
            assert!(seen.insert(n), "a CONSTRUCT run reused a blank node");
        }
    }
}

// ---- RAND / UUID / STRUUID / NOW (review conc#1, query#16) -----------------

/// `UUID()` / `STRUUID()` / `RAND()` from four threads at once on one
/// shared database: 800,000 UUIDs, no two alike. The generator was one
/// process-wide state advanced by a load and a store, so threads in step
/// drew the same numbers (hundreds of duplicate UUIDs per 200,000).
#[test]
fn uuids_and_rand_are_unique_across_threads() {
    // 450 x 450 = 202,500 solutions per query, from 450 triples.
    let mut data = String::new();
    for i in 0..450 {
        data.push_str(&format!("<http://ex/s{i}> <http://ex/p> \"{i}\" .\n"));
    }
    let db = Arc::new(db(&data));
    let q = "SELECT (STRUUID() AS ?u) (UUID() AS ?i) (STR(RAND()) AS ?r) \
             WHERE { ?a <http://ex/p> ?x . ?b <http://ex/p> ?y }";
    let start = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (db, start) = (Arc::clone(&db), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                match db.query_read(q, None, None) {
                    Ok(QueryResult::Select(rs)) => rs,
                    other => panic!("expected SELECT, got {other:?}"),
                }
            })
        })
        .collect();
    let mut uuids = HashSet::new();
    let mut rands = HashSet::new();
    let mut total = 0;
    for h in handles {
        let rs = h.join().expect("query thread");
        for row in &rs.rows {
            let (u, i, r) = (row[0].as_deref(), row[1].as_deref(), row[2].as_deref());
            let u = u.expect("STRUUID bound");
            let i = i.expect("UUID bound");
            // "xxxxxxxx-xxxx-4xxx-[89ab]xxx-xxxxxxxxxxxx" in a simple literal.
            let text = u.trim_matches('"');
            assert_eq!(text.len(), 36, "{u}");
            assert_eq!(&text[14..15], "4", "version 4: {u}");
            assert!("89ab".contains(&text[19..20]), "RFC 4122 variant: {u}");
            assert!(i.starts_with("<urn:uuid:") && i.len() == 47, "{i}");
            assert!(uuids.insert(text.to_string()), "duplicate STRUUID {u}");
            assert!(uuids.insert(i[10..46].to_string()), "duplicate UUID {i}");
            rands.insert(r.expect("RAND bound").to_string());
            total += 1;
        }
    }
    assert_eq!(total, 4 * 450 * 450);
    assert_eq!(uuids.len(), 2 * total);
    // 810,000 draws of 53 random bits: a repeat has a chance of about 4e-5.
    assert!(
        rands.len() + 2 >= total,
        "{} distinct RAND() of {total}",
        rands.len()
    );
}

/// Seconds since the Unix epoch.
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A database with the user function `waitForNextSecond(x)`: true, once the
/// wall clock has moved to the next whole second (`NOW()` has a resolution
/// of one second), so a `NOW()` called after it in the same query would see
/// a later instant unless the query fixes one.
fn db_with_a_clock_tick(data: &str) -> Database {
    let mut db = db(data);
    db.register_function("waitForNextSecond", |_| {
        let start = unix_secs();
        while unix_secs() == start {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Some(Value::Bool(true))
    });
    db
}

/// §17.4.5.1: "All calls to this function in any one query execution must
/// return the same value." `NOW()` read the clock at every call.
#[test]
fn now_is_one_instant_per_query() {
    let mut db = db_with_a_clock_tick("<http://ex/s> <http://ex/p> <http://ex/o> .\n");
    let rs = select(
        &mut db,
        "SELECT ?a ?b WHERE { { BIND(NOW() AS ?a) FILTER(waitForNextSecond(?a)) } \
         BIND(NOW() AS ?b) }",
    );
    assert_eq!(rs.row_count(), 1, "{rs:?}");
    assert!(cell(&rs, 0, "a").is_some(), "{rs:?}");
    assert_eq!(cell(&rs, 0, "a"), cell(&rs, 0, "b"), "{rs:?}");
    // Projection expressions too.
    let rs = select(
        &mut db,
        "SELECT (NOW() AS ?a) (waitForNextSecond(1) AS ?w) (NOW() AS ?b) WHERE {}",
    );
    assert_eq!(cell(&rs, 0, "a"), cell(&rs, 0, "b"), "{rs:?}");
}

/// The instant is the query's, so a `GRAPH` block (a sub-evaluator over the
/// named graph) sees the same `NOW()` as the rest of the query.
#[test]
fn now_inside_and_outside_a_graph_block_agree() {
    let mut db = db_with_a_clock_tick("");
    update(
        &mut db,
        "INSERT DATA { GRAPH <http://ex/g> { <http://ex/s> <http://ex/p> <http://ex/o> } }",
    );
    let rs = select(
        &mut db,
        "SELECT ?in ?out WHERE { \
         GRAPH <http://ex/g> { ?s ?p ?o BIND(NOW() AS ?in) FILTER(waitForNextSecond(?in)) } \
         BIND(NOW() AS ?out) }",
    );
    assert_eq!(rs.row_count(), 1, "{rs:?}");
    assert!(cell(&rs, 0, "in").is_some(), "{rs:?}");
    assert_eq!(cell(&rs, 0, "in"), cell(&rs, 0, "out"), "{rs:?}");
}
