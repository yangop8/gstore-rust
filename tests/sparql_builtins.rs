//! SPARQL built-ins whose answer belongs to the query execution, and the
//! aggregates: what the review found wrong in them and what SPARQL 1.1
//! requires instead.
//!
//! * `BNODE()` / `BNODE(str)` and template blank nodes are fresh — never a
//!   node of the store, of another solution or of another request
//!   (§17.4.2.9, §16.2.1, Update §3.1.3; review query#5);
//! * `UUID()` / `STRUUID()` / `RAND()` are safe to call from concurrent
//!   queries (review conc#1) and `NOW()` is one instant per query, `GRAPH`
//!   blocks included (§17.4.5.1; review query#16);
//! * `REGEX` / `REPLACE` follow XPath `fn:matches` / `fn:replace` — the
//!   flags, the replacement-string syntax, the empty-match error — and
//!   compile a pattern once per query (§17.4.3.14 / §17.4.3.15; review
//!   query#11, sparqlconf#8);
//! * `MIN` / `MAX` use the `ORDER BY` order and, like `SAMPLE` and
//!   `GROUP_CONCAT`, keep the terms as stored (§18.5.1; review query#8,
//!   sparqlconf#7); `AVG` divides by the values it summed (review query#7);
//!   `DISTINCT` aggregates and `DESCRIBE` de-duplicate in linear time
//!   (review query#12, sparqlconf#10).
//!
//! The spec-defined answers of these functions that do not depend on
//! randomness are also cases of the W3C-style subset (`tests/w3c/`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use gstore::query::Evaluator;
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

/// The fresh-per-solution rule is for the templates that create nodes
/// (CONSTRUCT, INSERT). SPARQL 1.1 Update forbids blank nodes in DELETE
/// templates; this engine reads a labelled `_:b` there as it does in the
/// WHERE pattern and in `DELETE DATA`: the stored node of that label (the
/// label a query returned). Made fresh, it deleted nothing — `DELETE WHERE
/// { <a> <p> _:b }` matched the triple and then left it in place.
#[test]
fn delete_templates_keep_labelled_blank_nodes() {
    let mut db = db("<http://ex/a> <http://ex/p> _:x .\n\
                     <http://ex/a> <http://ex/q> \"1\" .\n\
                     <http://ex/c> <http://ex/p> _:y .\n\
                     <http://ex/c> <http://ex/q> \"2\" .\n");
    let node = |db: &mut Database, s: &str| {
        let rs = select(db, &format!("SELECT ?b WHERE {{ <{s}> <http://ex/p> ?b }}"));
        assert_eq!(rs.row_count(), 1, "{rs:?}");
        let b = cell(&rs, 0, "b").unwrap().to_string();
        assert!(b.starts_with("_:"), "{b}");
        b
    };
    let count = |db: &mut Database, q: &str| select(db, q).row_count();
    // DELETE WHERE with the label a query returned.
    let a = node(&mut db, "http://ex/a");
    update(
        &mut db,
        &format!("DELETE WHERE {{ <http://ex/a> <http://ex/p> {a} }}"),
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT ?b WHERE { <http://ex/a> <http://ex/p> ?b }"
        ),
        0,
        "DELETE WHERE matched {a} but did not delete it"
    );
    // DELETE … INSERT … WHERE: the DELETE template's label is the stored
    // node, the INSERT template's a fresh one.
    let c = node(&mut db, "http://ex/c");
    update(
        &mut db,
        &format!(
            "DELETE {{ ?s <http://ex/p> {c} }} INSERT {{ ?s <http://ex/r> {c} }} \
             WHERE {{ ?s <http://ex/q> \"2\" }}"
        ),
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT ?b WHERE { <http://ex/c> <http://ex/p> ?b }"
        ),
        0,
        "the DELETE template's {c} is the stored node"
    );
    let rs = select(
        &mut db,
        "SELECT ?b WHERE { <http://ex/c> <http://ex/r> ?b }",
    );
    assert_eq!(rs.row_count(), 1, "{rs:?}");
    assert!(is_blank(cell(&rs, 0, "b")), "{rs:?}");
    assert_ne!(
        cell(&rs, 0, "b"),
        Some(c.as_str()),
        "the INSERT template's {c} is a fresh node"
    );
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

/// `NOW()` and the `BNODE(str)` nodes belong to one query *execution*: an
/// `Evaluator` a library caller reuses for a second query must not hand it
/// the first one's instant (frozen for the evaluator's lifetime) or nodes
/// (`BNODE("x")` of the same solution mapping in both).
#[test]
fn a_reused_evaluator_starts_every_query_afresh() {
    let db = db("");
    let ev = Evaluator::new(db.dict(), db.store()).with_function("waitForNextSecond", |_| {
        let start = unix_secs();
        while unix_secs() == start {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Some(Value::Bool(true))
    });
    let q = gstore::parser::sparql::parse(
        "SELECT (NOW() AS ?now) (BNODE(\"x\") AS ?b) (waitForNextSecond(1) AS ?w) WHERE {}",
    )
    .expect("parse");
    let run = || match ev.evaluate(&q).expect("evaluate") {
        QueryResult::Select(rs) => {
            assert_eq!(rs.row_count(), 1, "{rs:?}");
            (
                cell(&rs, 0, "now").unwrap().to_string(),
                cell(&rs, 0, "b").unwrap().to_string(),
            )
        }
        other => panic!("expected SELECT, got {other:?}"),
    };
    let (now1, b1) = run();
    let (now2, b2) = run();
    assert_ne!(now1, now2, "NOW() of the second query is the first one's");
    assert_ne!(
        b1, b2,
        "BNODE(\"x\") of the second query is the first one's"
    );
}

// ---- REGEX / REPLACE (review query#11, sparqlconf#8) -----------------------

/// The one row of `SELECT (e1 AS ?v1) … WHERE {}`: each cell, `None` when
/// the expression is an error.
fn exprs(db: &mut Database, exprs: &[&str]) -> Vec<Option<String>> {
    let items: Vec<String> = exprs
        .iter()
        .enumerate()
        .map(|(i, e)| format!("({e} AS ?v{i})"))
        .collect();
    let rs = select(db, &format!("SELECT {} WHERE {{}}", items.join(" ")));
    assert_eq!(rs.row_count(), 1, "{rs:?}");
    (0..exprs.len())
        .map(|i| cell(&rs, 0, &format!("v{i}")).map(str::to_string))
        .collect()
}

const TRUE: &str = "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>";
const FALSE: &str = "\"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>";

/// XPath flags (F&O §5.6.1.1): `s`, `m`, `x` and `q` were ignored (only
/// `i` was honoured) and any other character was accepted; it is an error.
#[test]
fn regex_flags_follow_xpath() {
    let mut db = db("");
    let got = exprs(
        &mut db,
        &[
            r#"REGEX("a\nb", "a.b", "s")"#,
            r#"REGEX("a\nb", "a.b")"#,
            r#"REGEX("a\nb", "^b$", "m")"#,
            r#"REGEX("ab", "a b", "x")"#,
            r#"REGEX("a b", "a[ ]b", "x")"#,
            r#"REGEX("abc", "a.c", "q")"#,
            r#"REGEX("xA.Cx", "a.c", "qi")"#,
            r#"REGEX("Alice", "^ali", "i")"#,
            r#"REGEX("ab", "a", "z")"#,
            r#"REGEX("ab", "a", "is")"#,
        ],
    );
    let want = [
        Some(TRUE),
        Some(FALSE),
        Some(TRUE),
        Some(TRUE),
        Some(TRUE),
        Some(FALSE),
        Some(TRUE),
        Some(TRUE),
        None, // FORX0001
        Some(TRUE),
    ];
    let got: Vec<Option<&str>> = got.iter().map(Option::as_deref).collect();
    assert_eq!(got, want);
    // An invalid flag is an error, so a FILTER over it keeps nothing.
    let rs = select(
        &mut db,
        r#"SELECT ?x WHERE { VALUES ?x { "ab" } FILTER(REGEX(?x, "a", "u")) }"#,
    );
    assert_eq!(rs.row_count(), 0, "{rs:?}");
}

/// The replacement string is XPath's (F&O §5.6.3), not the regex crate's:
/// `$1x` is group 1 then "x" (it was the group named "1x", so empty),
/// `\$` a literal dollar, a group above the count up to 9 the empty string;
/// a lone `$` or `\` and a pattern that matches the empty string are errors.
#[test]
fn replace_uses_the_xpath_replacement_syntax() {
    let mut db = db("");
    let got = exprs(
        &mut db,
        &[
            r#"REPLACE("abc", "(b)", "$1x")"#,
            r#"REPLACE("abc", "(b)", "[$1]")"#,
            r#"REPLACE("a-b", "-", "\\$")"#,
            r#"REPLACE("a-b", "-", "\\\\")"#,
            r#"REPLACE("abc", "(b)", "$2")"#,
            r#"REPLACE("abc", "(b)", "$12")"#,
            r#"REPLACE("darted", "^(.*?)d(.*)$", "$1c$2")"#,
            r#"REPLACE("abab", "B.", "Z", "i")"#,
            r#"REPLACE("a.b.c", ".", "$", "q")"#,
            r#"REPLACE("abc", "b", "$")"#,
            r#"REPLACE("abc", "b", "\\n")"#,
            r#"REPLACE("abc", "", "-")"#,
            r#"REPLACE("abc", "x*", "-")"#,
            r#"REPLACE("abc", "b", "z", "w")"#,
        ],
    );
    let lit = |s: &str| Some(format!("\"{s}\""));
    let want = vec![
        lit("abxc"),
        lit("a[b]c"),
        lit("a$b"),
        lit("a\\\\b"), // one backslash, escaped in the N-Triples cell
        lit("ac"),
        lit("ab2c"),
        lit("carted"),
        lit("aZb"),
        lit("a$b$c"),
        None, // FORX0004
        None, // FORX0004
        None, // FORX0003
        None, // FORX0003
        None, // FORX0001
    ];
    assert_eq!(got, want);
}

/// A pattern is compiled once per query, not once per solution: a REGEX
/// FILTER over 20,000 rows compiled its pattern 20,000 times (most of the
/// query's time), now once.
#[test]
fn a_pattern_is_compiled_once_per_query() {
    let mut data = String::new();
    for i in 0..20_000 {
        data.push_str(&format!("<http://ex/s{i}> <http://ex/v> \"{i}\" .\n"));
    }
    let db = db(&data);
    let q = gstore::parser::sparql::parse(
        r#"SELECT ?s (REPLACE(?o, "(1)", "[$1]") AS ?r) WHERE { ?s <http://ex/v> ?o
           FILTER(REGEX(?o, "^1.*9$") && !REGEX(?o, "^1.*9$", "i")) }"#,
    )
    .expect("parse");
    let ev = Evaluator::new(db.dict(), db.store());
    let rs = match ev.evaluate(&q).expect("evaluate") {
        QueryResult::Select(rs) => rs,
        other => panic!("expected SELECT, got {other:?}"),
    };
    // A && !A: nothing passes; the point is how often the patterns compiled.
    assert_eq!(rs.row_count(), 0);
    assert_eq!(ev.regex_compilations(), 2, "one per (pattern, flags)");

    let q = gstore::parser::sparql::parse(
        r#"SELECT (COUNT(*) AS ?n) WHERE { ?s <http://ex/v> ?o FILTER(REGEX(?o, "^1.*9$")) }"#,
    )
    .expect("parse");
    let ev = Evaluator::new(db.dict(), db.store());
    match ev.evaluate(&q).expect("evaluate") {
        QueryResult::Select(rs) => assert_eq!(
            rs.rows[0][0].as_deref(),
            Some("\"1111\"^^<http://www.w3.org/2001/XMLSchema#integer>")
        ),
        other => panic!("expected SELECT, got {other:?}"),
    }
    assert_eq!(ev.regex_compilations(), 1);
}

/// The time a REGEX FILTER takes against a STRSTARTS / STRENDS FILTER
/// selecting the same rows: per-solution compilation made it about 30-100
/// times slower (review sparqlconf#8); with the pattern compiled once the
/// two cost about the same. The bound is generous (best of three runs,
/// alternating, so a change of the machine's load hits both alike).
#[test]
fn a_regex_filter_costs_about_what_a_string_filter_costs() {
    let mut data = String::new();
    for i in 0..20_000 {
        data.push_str(&format!("<http://ex/s{i}> <http://ex/v> \"{i}\" .\n"));
    }
    let db = db(&data);
    let regex_q = r#"SELECT ?s WHERE { ?s <http://ex/v> ?o FILTER(REGEX(?o, "^1.*9$")) }"#;
    let plain_q =
        r#"SELECT ?s WHERE { ?s <http://ex/v> ?o FILTER(STRSTARTS(?o, "1") && STRENDS(?o, "9")) }"#;
    for q in [regex_q, plain_q] {
        let parsed = gstore::parser::sparql::parse(q).expect("parse");
        match Evaluator::new(db.dict(), db.store()).evaluate(&parsed) {
            Ok(QueryResult::Select(rs)) => assert_eq!(rs.row_count(), 1111, "{q}"),
            other => panic!("{q}: {other:?}"),
        }
    }
    // A fresh evaluator per run: no query cache, no warm pattern cache.
    let (regex, plain) = best_pair((&db, regex_q), (&db, plain_q));
    eprintln!("REGEX filter {regex:?}, STRSTARTS/STRENDS filter {plain:?}");
    assert!(
        regex < plain * 10,
        "REGEX filter {regex:?} vs STRSTARTS/STRENDS filter {plain:?}"
    );
}

// ---- aggregates (review query#7, query#8, sparqlconf#7, query#12, sparqlconf#10)

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// §18.5.1.7 / §18.5.1.8: MIN and MAX use the ORDER BY order (§15.1:
/// blank nodes, then IRIs, then literals), whatever order the values come
/// in. With the partial `<` the first value seen won whenever two values
/// did not compare (an IRI and a literal, a number and a string).
#[test]
fn min_and_max_follow_the_order_by_order_whatever_the_input_order() {
    let mut values_db = db("");
    let values = ["<http://ex/z>", "3", "\"a\"", "\"b\"@en", "2.5"];
    // Every rotation and its reverse.
    let mut orders = Vec::new();
    for r in 0..values.len() {
        let mut v = values.to_vec();
        v.rotate_left(r);
        orders.push(v.clone());
        v.reverse();
        orders.push(v);
    }
    let sorted = select(
        &mut values_db,
        &format!(
            "SELECT ?v WHERE {{ VALUES ?v {{ {} }} }} ORDER BY ?v",
            values.join(" ")
        ),
    );
    let first = cell(&sorted, 0, "v").map(str::to_string);
    let last = cell(&sorted, sorted.row_count() - 1, "v").map(str::to_string);
    assert_eq!(first.as_deref(), Some("<http://ex/z>"));
    for order in orders {
        let rs = select(
            &mut values_db,
            &format!(
                "SELECT (MIN(?v) AS ?min) (MAX(?v) AS ?max) WHERE {{ VALUES ?v {{ {} }} }}",
                order.join(" ")
            ),
        );
        assert_eq!(cell(&rs, 0, "min").map(str::to_string), first, "{order:?}");
        assert_eq!(cell(&rs, 0, "max").map(str::to_string), last, "{order:?}");
    }
    // Per group, over data whose objects mix kinds.
    let mut db = db("<http://ex/g> <http://ex/v> <http://ex/zzz> .\n\
                     <http://ex/g> <http://ex/v> \"3\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
                     <http://ex/h> <http://ex/v> \"x\" .\n\
                     <http://ex/h> <http://ex/v> \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n");
    let rs = select(
        &mut db,
        "SELECT ?g (MIN(?v) AS ?min) (MAX(?v) AS ?max) WHERE { ?g <http://ex/v> ?v } \
         GROUP BY ?g ORDER BY ?g",
    );
    assert_eq!(cell(&rs, 0, "min"), Some("<http://ex/zzz>"), "{rs:?}");
    assert_eq!(
        cell(&rs, 0, "max"),
        Some(format!("\"3\"^^<{XSD}integer>").as_str())
    );
    assert_eq!(
        cell(&rs, 1, "min"),
        Some(format!("\"7\"^^<{XSD}integer>").as_str())
    );
    assert_eq!(cell(&rs, 1, "max"), Some("\"x\""), "{rs:?}");
}

/// MIN / MAX / SAMPLE return a term of the group as it is stored, and
/// GROUP_CONCAT joins the terms' lexical forms (`STR`): they went through
/// the value and back, so `"5"^^xsd:int` came back an `xsd:integer`,
/// `"01"` as `"1"` and `"1e3"^^xsd:double` as `"1000.0"`.
#[test]
fn min_max_sample_and_group_concat_keep_the_terms() {
    let mut db = db(&format!(
        "<http://ex/a> <http://ex/v> \"5\"^^<{XSD}int> .\n\
         <http://ex/b> <http://ex/v> \"01\"^^<{XSD}integer> .\n\
         <http://ex/c> <http://ex/v> \"1e3\"^^<{XSD}double> .\n"
    ));
    let rs = select(
        &mut db,
        "SELECT ?s (MIN(?v) AS ?min) (MAX(?v) AS ?max) (SAMPLE(?v) AS ?sample) \
         (GROUP_CONCAT(?v) AS ?concat) WHERE { ?s <http://ex/v> ?v } GROUP BY ?s ORDER BY ?s",
    );
    let expect = [
        (format!("\"5\"^^<{XSD}int>"), "\"5\""),
        (format!("\"01\"^^<{XSD}integer>"), "\"01\""),
        (format!("\"1e3\"^^<{XSD}double>"), "\"1e3\""),
    ];
    assert_eq!(rs.row_count(), 3, "{rs:?}");
    for (row, (term, concat)) in expect.iter().enumerate() {
        for var in ["min", "max", "sample"] {
            assert_eq!(cell(&rs, row, var), Some(term.as_str()), "{var}: {rs:?}");
        }
        assert_eq!(cell(&rs, row, "concat"), Some(*concat), "{rs:?}");
    }
}

/// AVG divided the sum of the numeric values by the number of all values,
/// the non-numeric ones it skipped included: 10 and "x" averaged 5.0.
/// Skipping non-numeric values is this engine's extension (SPARQL makes
/// the aggregate an error); the divisor is now the number of values summed.
#[test]
fn avg_divides_by_the_number_of_values_it_summed() {
    let mut db = db("<http://ex/g1> <http://ex/v> 10 .\n\
                     <http://ex/g1> <http://ex/v> \"x\" .\n\
                     <http://ex/g2> <http://ex/v> 10 .\n\
                     <http://ex/g2> <http://ex/v> 20 .\n\
                     <http://ex/g3> <http://ex/v> \"y\" .\n\
                     <http://ex/g4> <http://ex/v> 4 .\n\
                     <http://ex/g4> <http://ex/v> \"4\"^^<http://www.w3.org/2001/XMLSchema#int> .\n");
    let rs = select(
        &mut db,
        "SELECT ?g (AVG(?v) AS ?avg) (AVG(DISTINCT ?v) AS ?davg) (SUM(?v) AS ?sum) \
         (COUNT(?v) AS ?n) WHERE { ?g <http://ex/v> ?v } GROUP BY ?g ORDER BY ?g",
    );
    let dec = |v: &str| format!("\"{v}\"^^<{XSD}decimal>");
    let int = |v: &str| format!("\"{v}\"^^<{XSD}integer>");
    let want = [
        (dec("10.0"), dec("10.0"), int("10"), int("2")),
        (dec("15.0"), dec("15.0"), int("30"), int("2")),
        // Nothing summed: 0, as for an empty group (§18.5.1.4).
        (int("0"), int("0"), int("0"), int("1")),
        (dec("4.0"), dec("4.0"), int("8"), int("2")),
    ];
    assert_eq!(rs.row_count(), 4, "{rs:?}");
    for (row, (avg, davg, sum, n)) in want.iter().enumerate() {
        assert_eq!(
            cell(&rs, row, "avg"),
            Some(avg.as_str()),
            "row {row}: {rs:?}"
        );
        assert_eq!(
            cell(&rs, row, "davg"),
            Some(davg.as_str()),
            "row {row}: {rs:?}"
        );
        assert_eq!(
            cell(&rs, row, "sum"),
            Some(sum.as_str()),
            "row {row}: {rs:?}"
        );
        assert_eq!(cell(&rs, row, "n"), Some(n.as_str()), "row {row}: {rs:?}");
    }
}

/// DISTINCT in an aggregate drops repeated RDF terms (§18.5.1: the
/// multiset is made a set): `"01"` and `"1"` are two terms with one value,
/// so COUNT(DISTINCT) counts both. They were compared as values.
#[test]
fn distinct_aggregates_compare_terms() {
    let mut db = db(&format!(
        "<http://ex/a> <http://ex/v> \"01\"^^<{XSD}integer> .\n\
         <http://ex/b> <http://ex/v> \"1\"^^<{XSD}integer> .\n\
         <http://ex/c> <http://ex/v> \"1\"^^<{XSD}integer> .\n\
         <http://ex/d> <http://ex/v> \"x\" .\n\
         <http://ex/e> <http://ex/v> \"x\" .\n"
    ));
    let rs = select(
        &mut db,
        "SELECT (COUNT(DISTINCT ?v) AS ?n) (SUM(DISTINCT ?v) AS ?sum) \
         (GROUP_CONCAT(DISTINCT ?v; SEPARATOR=\"|\") AS ?all) (COUNT(DISTINCT STR(?v)) AS ?strs) \
         WHERE { ?s <http://ex/v> ?v }",
    );
    assert_eq!(
        cell(&rs, 0, "n"),
        Some(format!("\"3\"^^<{XSD}integer>").as_str())
    );
    assert_eq!(
        cell(&rs, 0, "sum"),
        Some(format!("\"2\"^^<{XSD}integer>").as_str())
    );
    let all = cell(&rs, 0, "all").unwrap().trim_matches('"').to_string();
    let mut parts: Vec<&str> = all.split('|').collect();
    parts.sort();
    assert_eq!(parts, ["01", "1", "x"], "{rs:?}");
    assert_eq!(
        cell(&rs, 0, "strs"),
        Some(format!("\"3\"^^<{XSD}integer>").as_str())
    );
}

/// The best times of two evaluations, each against a fresh evaluator (no
/// query cache), run alternately so a change of the machine's load (other
/// tests of this binary, other builds) hits both alike.
fn best_pair(x: (&Database, &str), y: (&Database, &str)) -> (Duration, Duration) {
    let run = |(db, q): (&Database, &str)| {
        let parsed = gstore::parser::sparql::parse(q).expect("parse");
        let t = Instant::now();
        let r = Evaluator::new(db.dict(), db.store()).evaluate(&parsed);
        let elapsed = t.elapsed();
        assert!(r.is_ok(), "{q}: {r:?}");
        elapsed
    };
    let (mut a, mut b) = (Duration::MAX, Duration::MAX);
    for _ in 0..3 {
        a = a.min(run(x));
        b = b.min(run(y));
    }
    (a, b)
}

/// `n` subjects, each with one distinct string value.
fn distinct_values(n: usize) -> Database {
    let mut data = String::with_capacity(n * 48);
    for i in 0..n {
        data.push_str(&format!("<http://ex/s{i}> <http://ex/v> \"val{i}\" .\n"));
    }
    db(&data)
}

/// D9: an aggregate DISTINCT costs a constant factor over the same
/// aggregate without DISTINCT (the bound allows 20 at 160,000 values), and
/// 16 times the values cost about 16 times as long (the bound allows 160,
/// for a loaded machine). A linear `contains` per value made it quadratic:
/// 160,000 distinct values took 37 s against 0.11 s for 10,000, and some
/// thousand times what COUNT without DISTINCT took.
#[test]
fn distinct_aggregates_scale_linearly() {
    let (small, large) = (distinct_values(10_000), distinct_values(160_000));
    let plain = "SELECT (COUNT(?o) AS ?c) WHERE { ?s <http://ex/v> ?o }";
    for q in [
        "SELECT (COUNT(DISTINCT ?o) AS ?c) WHERE { ?s <http://ex/v> ?o }",
        "SELECT (GROUP_CONCAT(DISTINCT ?o) AS ?c) WHERE { ?s <http://ex/v> ?o }",
        "SELECT (COUNT(DISTINCT STR(?o)) AS ?c) WHERE { ?s <http://ex/v> ?o }",
    ] {
        let (d, p) = best_pair((&large, q), (&large, plain));
        eprintln!("{q}: 160k values {d:?}, without DISTINCT {p:?}");
        assert!(d < p * 20, "{q}: 160k values {d:?}, COUNT(?o) {p:?}");
        let (a, b) = best_pair((&small, q), (&large, q));
        eprintln!("{q}: 10k values {a:?}, 160k values {b:?}");
        assert!(b < a * 160, "{q}: 10k values {a:?}, 160k values {b:?}");
    }
}

/// D9: DESCRIBE costs a constant factor over the SELECT of its WHERE clause
/// (the bound allows 20 at 160,000 resources), and 16 times the resources
/// cost about 16 times as long (the bound allows 160). Its resource list
/// was de-duplicated with a linear `contains` per solution, quadratic in
/// the number of resources: 3.8 s for 160,000 against 12.5 ms for 10,000.
/// The described resources have no triples of their own, so the time is
/// the WHERE clause and the de-duplication.
#[test]
fn describe_scales_linearly() {
    let objects = |n: usize| {
        let mut data = String::with_capacity(n * 48);
        for i in 0..n {
            data.push_str(&format!(
                "<http://ex/s{i}> <http://ex/v> <http://ex/o{i}> .\n"
            ));
        }
        db(&data)
    };
    let (small, large) = (objects(10_000), objects(160_000));
    let q = "DESCRIBE ?o WHERE { ?s <http://ex/v> ?o }";
    let (d, p) = best_pair(
        (&large, q),
        (&large, "SELECT ?o WHERE { ?s <http://ex/v> ?o }"),
    );
    eprintln!("{q}: 160k resources {d:?}, the SELECT {p:?}");
    assert!(d < p * 20, "160k resources {d:?}, the SELECT {p:?}");
    let (a, b) = best_pair((&small, q), (&large, q));
    eprintln!("{q}: 10k resources {a:?}, 160k resources {b:?}");
    assert!(b < a * 160, "10k resources {a:?}, 160k resources {b:?}");
    // DESCRIBE ?s: every subject's triple.
    let parsed =
        gstore::parser::sparql::parse("DESCRIBE ?s WHERE { ?s <http://ex/v> ?o }").unwrap();
    match Evaluator::new(large.dict(), large.store()).evaluate(&parsed) {
        Ok(QueryResult::Construct(ts)) => assert_eq!(ts.len(), 160_000),
        other => panic!("expected a graph, got {other:?}"),
    }
}
