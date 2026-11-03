//! Data test (Phase 6 R3-STREAM): an on-disk database opened in **Stream**
//! mode — queries straight off the B+ trees (`Backend::Disk`) with an
//! out-of-core dictionary — answers exactly what the same directory answers
//! in **Materialize** mode (the in-memory working set), rows in the same
//! order, with the same VS-tree state; and both answer the in-memory build
//! of the same data (as a multiset).
//!
//! - LUBM Q1–Q14 plus a hand-written list covering LIMIT / OFFSET / Top-K,
//!   property paths (both ends unbound), `?s ?p ?o LIMIT`, `GRAPH ?g` and
//!   `GRAPH <g>`, CONTAINS / REGEX, DESCRIBE, CONSTRUCT, aggregates and the
//!   graph functions (which run over the CSR built from the store).
//! - The read queries and data of the dt_sparql11, dt_sparql_semantics and
//!   dt_sparql_syntax corpora (extracted from those files' string literals).
//! - A seeded update workload (INSERT / DELETE DATA, DELETE WHERE, DELETE …
//!   INSERT … WHERE, CLEAR, DROP, ADD / COPY / MOVE, quads, LOAD,
//!   `reason_execute`) against an in-memory oracle; `mutation_count`
//!   advances exactly when the data changed; the database is reopened
//!   alternately in Stream, Materialize and Stream mode.
//! - The VS-tree is used only while fresh, never rebuilt at open unless
//!   `vstree_on_open`, and `rebuild_index` makes it fresh again.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use gstore::dict::Space;
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult, Statement, StorageKind};

const UB: &str = "PREFIX ub: <http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#>\n\
                  PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n";

/// Named graphs over LUBM entities and some of their own.
const QUADS: &str = "\
<http://www.Department0.University0.edu/FullProfessor0> <http://ex/rates> <http://www.Department0.University0.edu/Course0> <http://ex/g1> .
<http://www.Department0.University0.edu/FullProfessor0> <http://ex/rates> \"excellent\" <http://ex/g1> .
<http://ex/a> <http://ex/p> <http://ex/b> <http://ex/g1> .
<http://ex/b> <http://ex/p> <http://ex/c> <http://ex/g2> .
<http://ex/c> <http://ex/p> \"in g2\" <http://ex/g2> .
<http://ex/a> <http://ex/q> <http://ex/c> .
";

/// The temp directories the current test thread made, removed when the
/// thread ends — also when the test fails (a thread-local's destructor runs
/// after the unwinding).
struct TmpDirs(std::cell::RefCell<Vec<std::path::PathBuf>>);

impl Drop for TmpDirs {
    fn drop(&mut self) {
        for p in self.0.borrow().iter() {
            // A directory, or a file (a store path).
            let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
        }
    }
}

thread_local! {
    static TMP_DIRS: TmpDirs = const { TmpDirs(std::cell::RefCell::new(Vec::new())) };
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_stream_{tag}_{}", std::process::id()));
    TMP_DIRS.with(|t| t.0.borrow_mut().push(d.clone()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_queries() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    (1..=14)
        .map(|i| fs::read_to_string(dir.join(format!("lubm_q{i}.rq"))).unwrap())
        .collect()
}

fn open(dir: &Path, mode: DiskQueryMode) -> Database {
    let db = Database::open_with(dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap();
    assert_eq!(db.disk_query_mode(), Some(mode));
    assert_eq!(db.storage(), StorageKind::Disk);
    db
}

/// The outcome of one query, comparable across databases (`Err` keeps only
/// the fact that it failed).
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Rows(Vec<String>, Vec<Vec<Option<String>>>),
    Ask(bool),
    Triples(Vec<String>),
    Update(usize),
    Failed,
}

fn run(db: &mut Database, q: &str) -> Outcome {
    match db.query(q) {
        Ok(QueryResult::Select(rs)) => Outcome::Rows(rs.vars, rs.rows),
        Ok(QueryResult::Ask(b)) => Outcome::Ask(b),
        Ok(QueryResult::Construct(ts)) => {
            Outcome::Triples(ts.iter().map(|t| format!("{t:?}")).collect())
        }
        Ok(QueryResult::Update { changed }) => Outcome::Update(changed),
        Err(_) => Outcome::Failed,
    }
}

/// `o` as a multiset (rows and triples sorted).
fn multiset(o: &Outcome) -> Outcome {
    match o {
        Outcome::Rows(v, rows) => {
            let mut rows = rows.clone();
            rows.sort();
            Outcome::Rows(v.clone(), rows)
        }
        Outcome::Triples(ts) => {
            let mut ts = ts.clone();
            ts.sort();
            Outcome::Triples(ts)
        }
        other => other.clone(),
    }
}

/// `s` with every blank-node label erased (two imports of one document
/// label its blank nodes differently).
fn erase_blank_labels(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("_:") {
        out.push_str(&rest[..i + 2]);
        rest = &rest[i + 2..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'))
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// `o` as a multiset with blank-node labels erased (for comparing two
/// builds of the same data).
fn multiset_unlabelled(o: &Outcome) -> Outcome {
    let erase = |v: &Option<String>| v.as_deref().map(erase_blank_labels);
    multiset(&match o {
        Outcome::Rows(v, rows) => Outcome::Rows(
            v.clone(),
            rows.iter().map(|r| r.iter().map(erase).collect()).collect(),
        ),
        Outcome::Triples(ts) => {
            Outcome::Triples(ts.iter().map(|t| erase_blank_labels(t)).collect())
        }
        other => other.clone(),
    })
}

/// What two outcomes differ in (for an assertion message).
fn diff(a: &Outcome, b: &Outcome) -> String {
    let lines = |o: &Outcome| -> Vec<String> {
        match o {
            Outcome::Rows(_, rows) => rows.iter().map(|r| format!("{r:?}")).collect(),
            Outcome::Triples(ts) => ts.clone(),
            other => vec![format!("{other:?}")],
        }
    };
    let (la, lb) = (lines(a), lines(b));
    let only_a: Vec<&String> = la.iter().filter(|l| !lb.contains(l)).take(10).collect();
    let only_b: Vec<&String> = lb.iter().filter(|l| !la.contains(l)).take(10).collect();
    format!(
        "{} vs {} rows; only left: {only_a:?}; only right: {only_b:?}",
        la.len(),
        lb.len()
    )
}

fn row_count(o: &Outcome) -> usize {
    match o {
        Outcome::Rows(_, rows) => rows.len(),
        Outcome::Triples(ts) => ts.len(),
        _ => 1,
    }
}

/// A query whose answer may legitimately differ between two evaluations.
fn nondeterministic(q: &str) -> bool {
    let u = q.to_ascii_uppercase();
    ["RAND(", "NOW(", "UUID(", "STRUUID(", "BNODE(", "SERVICE"]
        .iter()
        .any(|f| u.contains(f))
}

/// Whether `q` uses a property path: the engine evaluates paths through hash
/// sets, so their solutions come in no defined order (in either mode, or in
/// two runs of one mode) unless the query sorts them.
fn unordered_paths(q: &str) -> bool {
    let Ok(stmt) = Statement::parse(q) else {
        return false;
    };
    let ast = format!("{:?}", stmt.query());
    let path = [
        "ZeroOrMore",
        "OneOrMore",
        "ZeroOrOne",
        "Seq(",
        "Alt(",
        "Inverse(",
        "NegatedSet",
    ]
    .iter()
    .any(|k| ast.contains(k));
    path && !q.to_ascii_uppercase().contains("ORDER BY")
}

/// Compare `q` over the Stream and Materialize databases (in order, except
/// for unsorted property paths) and the in-memory build (as a multiset; by
/// count for LIMIT / OFFSET queries).
fn compare(stream: &mut Database, mat: &mut Database, mem: &mut Database, q: &str) -> Outcome {
    let s = run(stream, q);
    let m = run(mat, q);
    let o = run(mem, q);
    if nondeterministic(q) {
        assert_eq!(s == Outcome::Failed, m == Outcome::Failed, "{q}");
        return s;
    }
    let u = q.to_ascii_uppercase();
    let limited = u.contains("LIMIT") || u.contains("OFFSET");
    if unordered_paths(q) {
        // Which solutions a LIMIT keeps is not defined either.
        if limited {
            assert_eq!(
                row_count(&s),
                row_count(&m),
                "Stream vs Materialize (count): {q}"
            );
        } else {
            assert_eq!(
                multiset(&s),
                multiset(&m),
                "Stream vs Materialize (multiset): {q}"
            );
        }
    } else {
        assert_eq!(s, m, "Stream vs Materialize, in order: {q}");
    }
    if limited {
        assert_eq!(
            row_count(&s),
            row_count(&o),
            "Stream vs memory (count): {q}"
        );
    } else {
        let (a, b) = (multiset_unlabelled(&s), multiset_unlabelled(&o));
        if a != b {
            panic!("Stream vs memory (multiset): {q}\n{}", diff(&a, &b));
        }
    }
    s
}

/// The hand-written query list over LUBM and the named graphs.
fn extra_queries() -> Vec<String> {
    let dept0 = "<http://www.Department0.University0.edu>";
    let prof0 = "<http://www.Department0.University0.edu/FullProfessor0>";
    let student0 = "<http://www.Department0.University0.edu/UndergraduateStudent0>";
    let univ0 = "<http://www.University0.edu>";
    let mut qs: Vec<String> = [
        // LIMIT / OFFSET / Top-K.
        "SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 25".to_string(),
        "SELECT ?s ?p ?o WHERE { ?s ?p ?o } OFFSET 1000 LIMIT 13".to_string(),
        format!("{UB}SELECT ?x ?n WHERE {{ ?x ub:name ?n }} ORDER BY DESC(?n) LIMIT 7"),
        format!("{UB}SELECT ?x ?n WHERE {{ ?x ub:name ?n }} ORDER BY ?n ?x LIMIT 5 OFFSET 20"),
        format!("{UB}SELECT ?x WHERE {{ ?x rdf:type ub:FullProfessor }} OFFSET 3 LIMIT 4"),
        format!("{UB}SELECT DISTINCT ?t WHERE {{ ?x rdf:type ?t }} ORDER BY ?t"),
        format!("{UB}ASK {{ ?x ub:headOf {dept0} }}"),
        format!("{UB}ASK {{ {univ0} ub:headOf ?x }}"),
        // Property paths, both ends unbound included.
        format!("{UB}SELECT ?a ?b WHERE {{ ?a ub:subOrganizationOf+ ?b }} ORDER BY ?a ?b"),
        format!("{UB}SELECT ?a ?b WHERE {{ ?a ub:memberOf/ub:subOrganizationOf ?b }} LIMIT 50"),
        format!("{UB}SELECT ?a ?b WHERE {{ ?a ub:subOrganizationOf* ?b }} LIMIT 40"),
        format!("{UB}SELECT ?x WHERE {{ {student0} (ub:memberOf|ub:advisor)/ub:worksFor? ?x }}"),
        format!("{UB}SELECT ?x WHERE {{ ?x ^ub:headOf ?h }}"),
        // Named graphs.
        "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } } ORDER BY ?g ?s ?p ?o".to_string(),
        "SELECT ?s ?o WHERE { GRAPH <http://ex/g1> { ?s ?p ?o } }".to_string(),
        "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY ?g"
            .to_string(),
        format!(
            "{UB}SELECT ?g ?c ?t WHERE {{ GRAPH ?g {{ {prof0} <http://ex/rates> ?c }} \
             ?c rdf:type ?t }}"
        ),
        "SELECT ?s WHERE { ?s <http://ex/q> ?o }".to_string(),
        // Substring filters (a backed dictionary has no substring index).
        format!(
            "{UB}SELECT ?x ?n WHERE {{ ?x ub:name ?n FILTER(CONTAINS(?n, \"Course1\")) }} \
             ORDER BY ?n"
        ),
        format!("{UB}SELECT ?x WHERE {{ ?x ub:emailAddress ?e FILTER REGEX(?e, \"^Graduate.*5@\") }}"),
        format!(
            "{UB}SELECT ?x WHERE {{ ?x ub:name ?n FILTER(STRSTARTS(?n, \"FullProfessor\") \
             && STRENDS(?n, \"1\")) }}"
        ),
        // DESCRIBE / CONSTRUCT.
        format!("DESCRIBE {prof0}"),
        format!("{UB}CONSTRUCT {{ ?x ub:worksFor ?d }} WHERE {{ ?x ub:headOf ?d }}"),
        // Aggregates.
        format!(
            "{UB}SELECT ?d (COUNT(?x) AS ?n) WHERE {{ ?x ub:memberOf ?d }} GROUP BY ?d \
             ORDER BY DESC(?n) ?d LIMIT 5"
        ),
        format!("{UB}SELECT (COUNT(DISTINCT ?c) AS ?n) WHERE {{ ?x ub:takesCourse ?c }}"),
        format!("{UB}SELECT (MIN(?n) AS ?lo) (MAX(?n) AS ?hi) WHERE {{ ?x ub:name ?n }}"),
        format!(
            "{UB}SELECT ?d (SAMPLE(?x) AS ?s) (GROUP_CONCAT(?t) AS ?ts) WHERE {{ ?x ub:headOf ?d \
             . ?x ub:telephone ?t }} GROUP BY ?d HAVING (COUNT(?x) > 0) ORDER BY ?d"
        ),
        // OPTIONAL / MINUS / UNION / VALUES / BIND / sub-select / EXISTS.
        format!(
            "{UB}SELECT ?x ?a WHERE {{ ?x rdf:type ub:GraduateStudent OPTIONAL {{ ?x ub:advisor ?a }} }} \
             ORDER BY ?x LIMIT 30"
        ),
        format!(
            "{UB}SELECT ?x WHERE {{ ?x rdf:type ub:FullProfessor MINUS {{ ?x ub:headOf ?d }} }}"
        ),
        format!(
            "{UB}SELECT ?x WHERE {{ {{ ?x rdf:type ub:Lecturer }} UNION {{ ?x rdf:type ub:AssistantProfessor }} }}"
        ),
        format!(
            "{UB}SELECT ?x ?n WHERE {{ VALUES ?x {{ {prof0} {student0} }} ?x ub:name ?n }}"
        ),
        format!(
            "{UB}SELECT ?x ?l WHERE {{ ?x ub:name ?n BIND(STRLEN(?n) AS ?l) FILTER(?l > 20) }}"
        ),
        format!(
            "{UB}SELECT ?d ?m WHERE {{ {{ SELECT ?d (COUNT(?x) AS ?m) WHERE {{ ?x ub:worksFor ?d }} \
             GROUP BY ?d }} FILTER(?m > 5) }}"
        ),
        format!(
            "{UB}SELECT ?x WHERE {{ ?x rdf:type ub:FullProfessor FILTER NOT EXISTS {{ ?x ub:teacherOf ?c }} }}"
        ),
        // Graph functions over the CSR.
        format!(
            "{UB}SELECT (shortestPath({student0}, {univ0}, true, {{ub:memberOf, ub:subOrganizationOf}}) \
             AS ?y) WHERE {{}}"
        ),
        format!(
            "{UB}SELECT (kHopReachable({student0}, {univ0}, true, 3, \
             {{ub:memberOf, ub:subOrganizationOf}}) AS ?y) WHERE {{}}"
        ),
        "SELECT (WCC({}) AS ?y) WHERE {}".to_string(),
        "SELECT (PR(true, {}, 10, alpha=0.85, maxIter=20, tol=0.0001) AS ?y) WHERE {}".to_string(),
        // Unknown terms and errors.
        "SELECT ?o WHERE { <http://ex/nobody> ?p ?o }".to_string(),
        "SELECT ?x WHERE { ?x <http://ex/no-such-predicate> ?y }".to_string(),
        "SELECT ?x WHERE { ?x".to_string(),
    ]
    .into_iter()
    .collect();
    qs.extend(lubm_queries());
    qs
}

#[test]
fn stream_and_materialize_answer_lubm_and_named_graphs_identically() {
    let dir = tmp("lubm");
    let nq = dir.join("quads.nq");
    fs::write(&nq, QUADS).unwrap();
    let db_dir = dir.join("lubm.db");
    Database::build_disk(&db_dir, &[lubm_nt(), nq.clone()]).unwrap();
    let mut mem = Database::build_from_files("lubm", &[lubm_nt(), nq]).unwrap();

    // One process may open the directory twice (the lock is shared): the
    // two modes over the same files, the same fresh vstree.kv.
    let mut stream = open(&db_dir, DiskQueryMode::Stream);
    let mut mat = open(&db_dir, DiskQueryMode::Materialize);
    assert!(stream.stats().index_valid && mat.stats().index_valid);
    assert_eq!(stream.triple_num(), mem.triple_num());
    assert_eq!(stream.entity_num(), mem.entity_num());
    assert_eq!(stream.literal_num(), mem.literal_num());
    assert_eq!(stream.predicate_num(), mem.predicate_num());
    assert_eq!(stream.stats(), mat.stats());

    // Stream mode opened without reading the dictionary.
    assert!(stream.dict().is_disk_backed());
    assert_eq!(stream.dict().resident_string_count(), 0);

    let mut answered = 0;
    for q in extra_queries() {
        if row_count(&compare(&mut stream, &mut mat, &mut mem, &q)) > 0 {
            answered += 1;
        }
    }
    assert!(answered > 30, "most queries answer something ({answered})");

    // Substring filters ran without building the in-memory substring index
    // over the backed dictionary; the dictionary cache holds only the terms
    // the queries touched.
    assert!(stream.cached_string_index().is_none());
    assert!(stream.string_index().is_none());
    let resident = stream.dict().resident_string_count();
    assert!(
        resident < stream.entity_num() + stream.literal_num(),
        "{resident} resident terms"
    );
    // Statistics and schema agree.
    assert_eq!(stream.schema(), mem.schema());
    assert_eq!(stream.type_statistics(), mat.type_statistics());

    drop((stream, mat, mem));
    let _ = fs::remove_dir_all(&dir);
}

// ---- the SPARQL corpora ----------------------------------------------------

/// Every string literal of a Rust source file (`"…"` with escapes and line
/// continuations, `r"…"` and `r#"…"#`), skipping comments and char literals.
fn string_literals(src: &str) -> Vec<String> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    while i < c.len() {
        match c[i] {
            '/' if c.get(i + 1) == Some(&'/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '\'' => {
                // A char literal ('x', '\n', '\u{..}') or a lifetime.
                if c.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else if c.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while i < c.len() && c[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
            'r' if (i == 0 || !ident(c[i - 1]))
                && matches!(c.get(i + 1), Some('"') | Some('#')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while c.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if c.get(j) != Some(&'"') {
                    i += 1;
                    continue;
                }
                j += 1;
                let start = j;
                loop {
                    if j >= c.len() {
                        return out;
                    }
                    if c[j] == '"' && (0..hashes).all(|h| c.get(j + 1 + h) == Some(&'#')) {
                        break;
                    }
                    j += 1;
                }
                out.push(c[start..j].iter().collect());
                i = j + 1 + hashes;
            }
            '"' => {
                let mut s = String::new();
                let mut j = i + 1;
                while j < c.len() && c[j] != '"' {
                    if c[j] == '\\' {
                        j += 1;
                        match c.get(j) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some('0') => s.push('\0'),
                            Some('\\') => s.push('\\'),
                            Some('"') => s.push('"'),
                            Some('\'') => s.push('\''),
                            Some('u') => {
                                // \u{XXXX}
                                let close = (j..c.len()).find(|&k| c[k] == '}').unwrap_or(j);
                                let hex: String = c[j + 2..close].iter().collect();
                                if let Some(ch) =
                                    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                                {
                                    s.push(ch);
                                }
                                j = close;
                            }
                            Some('\n') => {
                                while c.get(j + 1).is_some_and(|ch| ch.is_whitespace()) {
                                    j += 1;
                                }
                            }
                            Some(other) => s.push(*other),
                            None => {}
                        }
                        j += 1;
                    } else {
                        s.push(c[j]);
                        j += 1;
                    }
                }
                out.push(s);
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// The data documents (Turtle that loads at least one triple) and the read
/// queries among a corpus file's string literals.
fn corpus(src: &str) -> (Vec<String>, Vec<String>) {
    let mut data = Vec::new();
    let mut queries = BTreeSet::new();
    for lit in string_literals(src) {
        if lit.trim().is_empty() {
            continue;
        }
        // A `format!` template without placeholders: un-double its braces.
        let lit = if lit.contains("{{") && Statement::parse(&lit).is_err() {
            let plain = lit.replace("{{", "{").replace("}}", "}");
            if Statement::parse(&plain).is_ok() {
                plain
            } else {
                lit
            }
        } else {
            lit
        };
        if let Ok(stmt) = Statement::parse(&lit) {
            if !stmt.is_update() {
                queries.insert(lit);
            }
            continue;
        }
        if Database::build_from_str("probe", &lit).is_ok_and(|db| db.triple_num() > 0) {
            data.push(lit);
        }
    }
    (data, queries.into_iter().collect())
}

#[test]
fn the_sparql_corpora_answer_identically_in_both_modes() {
    let corpora = [
        ("sparql11", include_str!("dt_sparql11.rs")),
        ("semantics", include_str!("dt_sparql_semantics.rs")),
        ("syntax", include_str!("dt_sparql_syntax.rs")),
    ];
    let mut total = 0;
    for (name, src) in corpora {
        let (data, queries) = corpus(src);
        assert!(
            !data.is_empty() && queries.len() > 10,
            "{name}: {} / {}",
            data.len(),
            queries.len()
        );
        let dir = tmp(&format!("corpus_{name}"));
        let files: Vec<PathBuf> = data
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let f = dir.join(format!("data{i}.ttl"));
                fs::write(&f, d).unwrap();
                f
            })
            .collect();
        let db_dir = dir.join("c.db");
        Database::build_disk(&db_dir, &files).unwrap();
        let mut mem = Database::build_from_files(name, &files).unwrap();
        let mut stream = open(&db_dir, DiskQueryMode::Stream);
        let mut mat = open(&db_dir, DiskQueryMode::Materialize);
        for q in &queries {
            compare(&mut stream, &mut mat, &mut mem, q);
        }
        total += queries.len();
        drop((stream, mat, mem));
        let _ = fs::remove_dir_all(&dir);
    }
    assert!(total >= 50, "{total} corpus queries");
    eprintln!("compared {total} corpus queries");
}

// ---- the seeded update workload ----------------------------------------------------

/// A small xorshift generator (deterministic per seed).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Every triple and quad of `db`, sorted (the comparable content).
type Rows = Vec<Vec<Option<String>>>;

fn content(db: &mut Database) -> (Rows, Rows) {
    let mut t = db
        .select("SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
        .unwrap()
        .rows;
    let mut q = db
        .select("SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }")
        .unwrap()
        .rows;
    t.sort();
    q.sort();
    (t, q)
}

const RULE: &str = r#"{
    "rulename": "grand",
    "description": "p0 twice",
    "isenable": 1,
    "type": 1,
    "logic": 1,
    "conditions": [{
        "patterns": [
            {"subject": "?x", "predicate": "<http://ex/p0>", "object": "?y"},
            {"subject": "?y", "predicate": "<http://ex/p0>", "object": "?z"}
        ],
        "filters": [],
        "count_info": {}
    }],
    "return": {"source": "?x", "target": "?z", "label": "grand"}
}"#;

/// One random update over the workload's vocabulary (`e0..e59`, `p0..p4`,
/// graphs `g0..g2`), sometimes with brand-new terms.
fn random_update(rng: &mut Rng, step: usize, load_nt: &Path) -> String {
    let e = |rng: &mut Rng| format!("<http://ex/e{}>", rng.below(60));
    let p = |rng: &mut Rng| format!("<http://ex/p{}>", rng.below(5));
    let g = |rng: &mut Rng| format!("<http://ex/g{}>", rng.below(3));
    let o = |rng: &mut Rng| {
        if rng.below(3) == 0 {
            format!("\"v{}\"", rng.below(40))
        } else {
            format!("<http://ex/e{}>", rng.below(60))
        }
    };
    match rng.below(15) {
        0..=3 => {
            let fresh = format!("<http://ex/new{step}>");
            let s = if rng.below(2) == 0 { fresh } else { e(rng) };
            format!(
                "INSERT DATA {{ {s} {} {} . {} {} \"lit{step}\" }}",
                p(rng),
                o(rng),
                e(rng),
                p(rng)
            )
        }
        4 => format!(
            "INSERT DATA {{ GRAPH {} {{ {} {} {} }} }}",
            g(rng),
            e(rng),
            p(rng),
            o(rng)
        ),
        5 | 6 => format!("DELETE DATA {{ {} {} {} }}", e(rng), p(rng), o(rng)),
        7 => format!("DELETE WHERE {{ {} ?p ?o }}", e(rng)),
        8 => format!(
            "DELETE {{ ?s {} ?o }} INSERT {{ ?s {} ?o }} WHERE {{ ?s {} ?o FILTER(isIRI(?o)) }}",
            p(rng),
            p(rng),
            p(rng)
        ),
        9 => format!(
            "DELETE DATA {{ GRAPH {} {{ {} {} {} }} }}",
            g(rng),
            e(rng),
            p(rng),
            o(rng)
        ),
        10 => match rng.below(4) {
            0 => format!("CLEAR SILENT GRAPH {}", g(rng)),
            1 => format!("DROP SILENT GRAPH {}", g(rng)),
            2 => "CLEAR DEFAULT".to_string(),
            _ => "DROP SILENT GRAPH <http://ex/never>".to_string(),
        },
        11 => match rng.below(3) {
            0 => format!("ADD SILENT {} TO {}", g(rng), g(rng)),
            1 => format!("COPY SILENT {} TO DEFAULT", g(rng)),
            _ => format!("MOVE SILENT DEFAULT TO {}", g(rng)),
        },
        12 => {
            let target = match rng.below(2) {
                0 => String::new(),
                _ => format!(" INTO GRAPH {}", g(rng)),
            };
            format!("LOAD <file://{}>{target}", load_nt.display())
        }
        13 => "#reason".to_string(),
        _ => format!(
            "INSERT {{ ?s <http://ex/p4> ?o }} WHERE {{ ?s {} ?o FILTER(isLiteral(?o)) }}",
            p(rng)
        ),
    }
}

/// The workload's starting data: 150 triples and 20 quads.
fn workload_data(rng: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..150 {
        let o = if rng.below(4) == 0 {
            format!("\"v{}\"", rng.below(40))
        } else {
            format!("<http://ex/e{}>", rng.below(60))
        };
        s.push_str(&format!(
            "<http://ex/e{}> <http://ex/p{}> {o} .\n",
            rng.below(60),
            rng.below(5)
        ));
    }
    for _ in 0..20 {
        s.push_str(&format!(
            "<http://ex/e{}> <http://ex/p{}> <http://ex/e{}> <http://ex/g{}> .\n",
            rng.below(60),
            rng.below(5),
            rng.below(60),
            rng.below(3)
        ));
    }
    s
}

fn apply(db: &mut Database, op: &str) {
    if op == "#reason" {
        db.reason_execute("grand").unwrap();
    } else {
        db.query(op).unwrap_or_else(|e| panic!("{op}: {e}"));
    }
}

#[test]
fn a_seeded_update_workload_matches_the_in_memory_oracle_across_reopens() {
    for seed in [7u64, 0x5eed_cafe] {
        let dir = tmp(&format!("workload_{seed}"));
        let mut rng = Rng(seed);
        let data = dir.join("start.nq");
        fs::write(&data, workload_data(&mut rng)).unwrap();
        let load_nt = dir.join("load.nq");
        fs::write(
            &load_nt,
            "<http://ex/loaded> <http://ex/p1> <http://ex/e3> .\n\
             <http://ex/e3> <http://ex/p0> <http://ex/loaded> <http://ex/g2> .\n",
        )
        .unwrap();
        let db_dir = dir.join("w.db");
        Database::build_disk(&db_dir, &[&data]).unwrap();
        let mut oracle = Database::build_from_files("w", &[&data]).unwrap();
        let rule = gstore::json::parse(RULE).unwrap();
        oracle.reason_add(&rule).unwrap();
        oracle.reason_compile("grand").unwrap();

        let modes = [
            DiskQueryMode::Stream,
            DiskQueryMode::Materialize,
            DiskQueryMode::Stream,
        ];
        let mut db = open(&db_dir, modes[0]);
        db.reason_add(&rule).unwrap();
        db.reason_compile("grand").unwrap();
        assert_eq!(content(&mut db), content(&mut oracle));
        for step in 0..90 {
            if step % 9 == 8 {
                drop(db);
                db = open(&db_dir, modes[(step / 9) % modes.len()]);
            }
            let op = random_update(&mut rng, step, &load_nt);
            let before = content(&mut db);
            let count = db.mutation_count();
            apply(&mut db, &op);
            apply(&mut oracle, &op);
            let after = content(&mut db);
            let advanced = db.mutation_count() > count;
            // A change always advances the count. Conversely, an operation
            // that only inserts or only deletes advances it only when it
            // changed something (a DELETE … INSERT or a COPY / MOVE may
            // delete and re-insert the same triples: applied changes, equal
            // content).
            if after != before {
                assert!(
                    advanced,
                    "seed {seed} step {step}: changed, count still: {op}"
                );
            }
            let pure = [
                "INSERT DATA",
                "DELETE DATA",
                "DELETE WHERE",
                "CLEAR",
                "DROP",
                "LOAD",
                "#",
            ]
            .iter()
            .any(|k| op.starts_with(k));
            if pure && after == before {
                assert!(
                    !advanced,
                    "seed {seed} step {step}: no change, count advanced: {op}"
                );
            }
            assert_eq!(after, content(&mut oracle), "seed {seed} step {step}: {op}");
        }
        // Durable: a reopen in the other mode sees the same.
        drop(db);
        for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
            let mut db = open(&db_dir, mode);
            assert_eq!(
                content(&mut db),
                content(&mut oracle),
                "seed {seed} reopened {mode}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

// ---- the VS-tree in Stream mode ------------------------------------------------

#[test]
fn the_vstree_is_used_only_while_fresh_and_never_rebuilt_at_open_by_default() {
    let dir = tmp("vstree");
    let nt = dir.join("d.nt");
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p> <http://ex/o{}> .\n",
            i % 17
        ));
    }
    fs::write(&nt, text).unwrap();
    let db_dir = dir.join("v.db");
    Database::build_disk(&db_dir, &[&nt]).unwrap();
    let q = "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o3> }";

    let mut db = open(&db_dir, DiskQueryMode::Stream);
    assert!(db.stats().index_valid, "a fresh vstree.kv is used");
    let want = db.select(q).unwrap().rows;
    assert_eq!(want.len(), 18);
    db.query("INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/o3> }")
        .unwrap();
    assert!(!db.stats().index_valid, "an insert makes it stale");
    assert_eq!(db.select(q).unwrap().rows.len(), 19);
    drop(db);

    // Stale on disk: not used, not rebuilt at open (by default) …
    let vs = db_dir.join("vstree.kv");
    let before = fs::metadata(&vs).unwrap().modified().unwrap();
    let mut db = open(&db_dir, DiskQueryMode::Stream);
    assert!(!db.stats().index_valid);
    assert_eq!(fs::metadata(&vs).unwrap().modified().unwrap(), before);
    assert_eq!(db.select(q).unwrap().rows.len(), 19);
    // … a rebuild inside a transaction waits for its end …
    db.begin().unwrap();
    db.rebuild_index();
    assert!(!db.stats().index_valid);
    db.query("DELETE DATA { <http://ex/s3> <http://ex/p> <http://ex/o3> }")
        .unwrap();
    db.commit().unwrap();
    assert!(db.stats().index_valid, "rebuilt when the transaction ended");
    assert_eq!(db.select(q).unwrap().rows.len(), 18);
    drop(db);
    // … and the rebuilt tree is fresh for the next open.
    let db = open(&db_dir, DiskQueryMode::Stream);
    assert!(db.stats().index_valid);
    drop(db);

    // `vstree_on_open` rebuilds a stale tree at open.
    let mut db = open(&db_dir, DiskQueryMode::Stream);
    db.query("INSERT DATA { <http://ex/new2> <http://ex/p> <http://ex/o3> }")
        .unwrap();
    drop(db);
    let options = OpenOptions::stream().with_vstree_on_open(true);
    let mut db = Database::open_with(&db_dir, &options).unwrap();
    assert!(db.stats().index_valid);
    assert_eq!(db.select(q).unwrap().rows.len(), 19);
    // A Materialize open of the same directory agrees.
    let mut mat = open(&db_dir, DiskQueryMode::Materialize);
    assert_eq!(mat.select(q).unwrap().rows, db.select(q).unwrap().rows);
    drop((db, mat));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stream_mode_interns_new_terms_in_the_on_disk_dictionary() {
    let dir = tmp("terms");
    let nt = dir.join("d.nt");
    fs::write(&nt, "<http://ex/a> <http://ex/p> \"x\" .\n").unwrap();
    let db_dir = dir.join("t.db");
    Database::build_disk(&db_dir, &[&nt]).unwrap();
    let mut db = open(&db_dir, DiskQueryMode::Stream);
    let (e0, l0, p0) = (db.entity_num(), db.literal_num(), db.predicate_num());
    db.query(
        "INSERT DATA { <http://ex/b> <http://ex/q> \"y\" . <http://ex/a> <http://ex/p> \"x\" }",
    )
    .unwrap();
    assert_eq!(
        (db.entity_num(), db.literal_num(), db.predicate_num()),
        (e0 + 1, l0 + 1, p0 + 1),
        "one new term per space; the existing triple adds nothing"
    );
    let b = db.dict().id_of(Space::Entity, "<http://ex/b>").unwrap();
    assert_eq!(b as usize, e0, "the next id");
    drop(db);
    // The same ids in Materialize mode and in a fresh Stream open.
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let db = open(&db_dir, mode);
        assert_eq!(db.dict().id_of(Space::Entity, "<http://ex/b>"), Some(b));
        assert_eq!(db.triple_num(), 2);
    }
    let _ = fs::remove_dir_all(&dir);
}
