//! Executor differential guard (review query#17 / design#10, decision D3).
//!
//! Every SPARQL algebra operator exists twice: in the materializing
//! evaluator (`eval_pattern` in `src/query/engine.rs`) and in the streaming
//! executor of `LIMIT` / `ORDER BY … LIMIT` / `ASK` / `EXISTS`
//! (`stream_pattern` in `src/query/topk.rs`, with the bounded-heap Top-K and
//! DP-B on top). The two have drifted apart before — a fix landed in one copy
//! and not the other — and merging them is a refactor recorded in
//! `docs/REFACTOR_BACKLOG.md`. Until then this suite runs both on the same
//! batteries and requires the same answers:
//!
//! * every case of the hand-authored W3C-style subset (`tests/w3c/`, queries
//!   and updates; whatever its expected answer — an `xfail` case must still
//!   be answered alike by both executors),
//! * the LUBM queries (`testdata/lubm/`) over the LUBM data, as written and
//!   with `LIMIT` and `ORDER BY … LIMIT` added,
//! * a probe battery over the corners where the executors' copies differ
//!   most (named graphs and `GRAPH ?g` reuse, OPTIONAL / MINUS / EXISTS with
//!   maybe-unbound keys, property paths and negated property sets,
//!   sub-SELECTs in every position, aggregates, SELECT aliases, mixed
//!   predicate variables, ORDER BY over mixed types, the built-ins).
//!
//! on every storage layout (in memory; saved as a snapshot and opened; the
//! on-disk store in Materialize and in Stream mode; RocksDB with
//! `--features rocksdb`), under these executor settings, each compared with
//! the first:
//!
//! * `full` — [`TopKOptions::NONE`]: the materializing evaluator;
//! * `stream` — early termination only, every SELECT / CONSTRUCT streamed
//!   ([`force_streaming_on_this_thread`]: as if it had an unbounded `LIMIT`,
//!   sub-SELECTs included);
//! * `heap` — streaming plus the bounded-heap Top-K;
//! * `dpb` — streaming plus DP-B forced ([`DpbMode::Always`]);
//! * `auto` — the default options on the query as written (a `LIMIT`
//!   streams, anything else does not).
//!
//! Answers are compared as bags of rows (SELECT, variables in head order),
//! booleans (ASK), bags of triples (CONSTRUCT / DESCRIBE) or bags of quads
//! (the dataset an update leaves, read through the N-Quads export), with
//! every blank-node label erased (`BNODE()` and template blank nodes are
//! fresh per evaluation). A query that fails must fail under every setting.
//! Queries calling `RAND`, `UUID`, `STRUUID` or `NOW` are skipped (their
//! answers differ from one evaluation to the next).
//!
//! A divergence is a failure, unless the run is listed in [`KNOWN`] with
//! the review id that owns it — then it must still diverge (an entry whose
//! divergence is gone fails the run, so the fix removes it). `DIFF_FILTER=…`
//! runs only the queries whose name contains the substring; `-- --nocapture`
//! prints a line per query.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use gstore::query::engine::force_streaming_on_this_thread;
use gstore::query::{DpbMode, TopKOptions};
use gstore::zipio::{write_export, ExportFormat};
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult};

/// Runs that are known to diverge: `(name substring, layout or "*",
/// setting or "*", review id)`. Empty: every divergence the suite found
/// was fixed (see the round's changelog fragment).
const KNOWN: &[(&str, &str, &str, &str)] = &[];

// ---- temporary directories ------------------------------------------------

/// A directory under the system temp dir, removed when dropped — also when
/// the test fails.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let tag: String = tag
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let dir = std::env::temp_dir().join(format!(
            "gstore_xdiff_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create a temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- layouts and settings ------------------------------------------------

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

    fn name(self) -> &'static str {
        match self {
            Layout::Memory => "memory",
            Layout::Snapshot => "snapshot",
            Layout::DiskMaterialize => "disk/materialize",
            Layout::DiskStream => "disk/stream",
            #[cfg(feature = "rocksdb")]
            Layout::RocksDb => "rocksdb",
        }
    }
}

/// The database of `layout` built from `data` under `scratch` (built on
/// first use, opened again on later calls), its query cache off.
fn open_layout(layout: Layout, data: &Path, scratch: &Path) -> Result<Database, String> {
    let fail = |what: &str, e: gstore::GStoreError| format!("{what}: {e}");
    let mut db = match layout {
        Layout::Memory => {
            Database::build_from_files("xdiff", &[data]).map_err(|e| fail("memory build", e))?
        }
        Layout::Snapshot => {
            let dir = scratch.join("snapshot");
            if !dir.exists() {
                Database::build_from_files("xdiff", &[data])
                    .and_then(|db| db.save(&dir))
                    .map_err(|e| fail("snapshot build", e))?;
            }
            Database::open(&dir).map_err(|e| fail("snapshot open", e))?
        }
        Layout::DiskMaterialize | Layout::DiskStream => {
            let dir = scratch.join("disk");
            let mode = if layout == Layout::DiskStream {
                DiskQueryMode::Stream
            } else {
                DiskQueryMode::Materialize
            };
            if !Database::is_disk(&dir) {
                Database::build_disk(&dir, &[data]).map_err(|e| fail("disk build", e))?;
            }
            Database::open_with(&dir, &OpenOptions::new().with_disk_query_mode(mode))
                .map_err(|e| fail("disk open", e))?
        }
        #[cfg(feature = "rocksdb")]
        Layout::RocksDb => {
            let dir = scratch.join("rocks");
            if !Database::is_rocksdb(&dir) {
                drop(
                    Database::build_rocksdb_from_files("xdiff", &dir, &[data])
                        .map_err(|e| fail("rocksdb build", e))?,
                );
            }
            Database::open_rocksdb(&dir).map_err(|e| fail("rocksdb open", e))?
        }
    };
    // Keyed by the query text only: it would answer the second setting
    // with the first one's result.
    db.set_query_cache_size(0);
    Ok(db)
}

/// One executor setting (see the module docs).
#[derive(Debug, Clone, Copy)]
struct Setting {
    name: &'static str,
    options: TopKOptions,
    /// Stream every SELECT / CONSTRUCT, not only those with a `LIMIT`.
    forced: bool,
}

const SETTINGS: [Setting; 5] = [
    Setting {
        name: "full",
        options: TopKOptions::NONE,
        forced: false,
    },
    Setting {
        name: "stream",
        options: TopKOptions {
            early_termination: true,
            bounded_heap: false,
            dpb: DpbMode::Off,
        },
        forced: true,
    },
    Setting {
        name: "heap",
        options: TopKOptions {
            early_termination: true,
            bounded_heap: true,
            dpb: DpbMode::Off,
        },
        forced: true,
    },
    Setting {
        name: "dpb",
        options: TopKOptions {
            early_termination: true,
            bounded_heap: true,
            dpb: DpbMode::Always,
        },
        forced: true,
    },
    Setting {
        name: "auto",
        options: TopKOptions::ALL,
        forced: false,
    },
];

/// Turns the streaming hook off again — also when a query panics.
struct Unforce;

impl Drop for Unforce {
    fn drop(&mut self) {
        force_streaming_on_this_thread(false);
    }
}

/// Run `text` on `db` under `setting`.
fn run(db: &mut Database, setting: Setting, text: &str) -> gstore::Result<QueryResult> {
    db.set_topk_options(setting.options);
    let _unforce = Unforce;
    force_streaming_on_this_thread(setting.forced);
    db.query(text)
}

// ---- answers ----------------------------------------------------------------

/// A cell with any blank-node label erased.
fn cell(c: &Option<String>) -> String {
    match c.as_deref() {
        None => "UNDEF".into(),
        Some(s) if s.starts_with("_:") => "_:".into(),
        Some(s) => s.to_string(),
    }
}

/// A term of a triple with any blank-node label erased.
fn term(t: &gstore::Term) -> String {
    match t {
        gstore::Term::Blank(_) => "_:".into(),
        other => other.to_string(),
    }
}

/// An answer in comparable form (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Rows(Vec<String>, BTreeMap<Vec<String>, usize>),
    Ask(bool),
    Triples(BTreeMap<String, usize>),
    Updated(BTreeMap<String, usize>),
    Error,
}

impl Answer {
    fn of(r: gstore::Result<QueryResult>) -> Answer {
        let bag = |items: Vec<String>| {
            let mut m = BTreeMap::new();
            for i in items {
                *m.entry(i).or_insert(0) += 1;
            }
            m
        };
        match r {
            Err(_) => Answer::Error,
            Ok(QueryResult::Select(rs)) => {
                let mut rows = BTreeMap::new();
                for r in &rs.rows {
                    *rows.entry(r.iter().map(cell).collect()).or_insert(0) += 1;
                }
                Answer::Rows(rs.vars.clone(), rows)
            }
            Ok(QueryResult::Ask(b)) => Answer::Ask(b),
            Ok(QueryResult::Construct(ts)) => Answer::Triples(bag(ts
                .iter()
                .map(|t| {
                    format!(
                        "{} {} {}",
                        term(&t.subject),
                        term(&t.predicate),
                        term(&t.object)
                    )
                })
                .collect())),
            Ok(QueryResult::Update { .. }) => Answer::Updated(BTreeMap::new()),
        }
    }

    /// A short description for a failure report.
    fn describe(&self) -> String {
        match self {
            Answer::Rows(vars, rows) => {
                let n: usize = rows.values().sum();
                let first: Vec<String> = rows.keys().take(3).map(|r| r.join(" ")).collect();
                format!("{n} row(s) over {vars:?}, first {first:?}")
            }
            Answer::Ask(b) => format!("ASK {b}"),
            Answer::Triples(t) | Answer::Updated(t) => {
                let n: usize = t.values().sum();
                let first: Vec<&String> = t.keys().take(3).collect();
                format!("{n} statement(s), first {first:?}")
            }
            Answer::Error => "an error".into(),
        }
    }
}

/// The dataset `db` holds, as a bag of N-Quads lines (blank labels erased).
fn dataset(db: &Database) -> Answer {
    let mut out = Vec::new();
    if write_export(db, ExportFormat::NQuads, &mut out).is_err() {
        return Answer::Error;
    }
    let text = String::from_utf8_lossy(&out);
    let mut m = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let erased: Vec<String> = line
            .split(' ')
            .map(|w| {
                if w.starts_with("_:") {
                    "_:".to_string()
                } else {
                    w.to_string()
                }
            })
            .collect();
        *m.entry(erased.join(" ")).or_insert(0) += 1;
    }
    Answer::Updated(m)
}

/// Whether `q` calls a function whose answer differs per evaluation.
fn nondeterministic(q: &str) -> bool {
    let upper: String = q
        .to_ascii_uppercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    ["RAND(", "UUID(", "NOW("].iter().any(|f| upper.contains(f))
}

// ---- the batteries ------------------------------------------------------------

/// One query (or update) of a battery, over the data file of its group.
#[derive(Debug, Clone)]
struct Probe {
    name: String,
    text: String,
    update: bool,
}

/// A battery: queries over one dataset.
struct Battery {
    name: String,
    /// The data file (Turtle, TriG or N-Triples), or `None`: empty.
    data: Option<PathBuf>,
    probes: Vec<Probe>,
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every case of `tests/w3c/`, one battery each (each has its own data).
fn w3c_batteries() -> Vec<Battery> {
    let root = repo().join("tests").join("w3c");
    let mut out = Vec::new();
    let mut areas: Vec<PathBuf> = fs::read_dir(&root)
        .expect("tests/w3c")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    for area in areas {
        let mut cases: Vec<PathBuf> = fs::read_dir(&area)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        cases.sort();
        for case in cases {
            let name = format!(
                "w3c/{}/{}",
                area.file_name().unwrap().to_string_lossy(),
                case.file_name().unwrap().to_string_lossy()
            );
            let data = ["data.ttl", "data.trig"]
                .iter()
                .map(|f| case.join(f))
                .find(|p| p.is_file());
            let (text, update) = match (
                fs::read_to_string(case.join("query.rq")),
                fs::read_to_string(case.join("update.ru")),
            ) {
                (Ok(q), _) => (q, false),
                (_, Ok(u)) => (u, true),
                _ => panic!("{name}: no query.rq or update.ru"),
            };
            out.push(Battery {
                name: name.clone(),
                data,
                probes: vec![Probe { name, text, update }],
            });
        }
    }
    out
}

/// The LUBM queries over the LUBM data: each as written, with `LIMIT 7`
/// and with `ORDER BY ?<first projected variable> LIMIT 7` added.
fn lubm_battery() -> Battery {
    let dir = repo().join("testdata").join("lubm");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("testdata/lubm")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rq"))
        .collect();
    files.sort();
    let mut probes = Vec::new();
    for f in files {
        let stem = f.file_stem().unwrap().to_string_lossy().to_string();
        let q = fs::read_to_string(&f).unwrap();
        let q = q.trim_end().to_string();
        let first_var = q
            .split("SELECT")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().find(|w| w.starts_with('?')))
            .map(str::to_string);
        probes.push(Probe {
            name: format!("lubm/{stem}"),
            text: q.clone(),
            update: false,
        });
        probes.push(Probe {
            name: format!("lubm/{stem}+limit"),
            text: format!("{q}\nLIMIT 7"),
            update: false,
        });
        if let Some(v) = first_var {
            probes.push(Probe {
                name: format!("lubm/{stem}+order-limit"),
                text: format!("{q}\nORDER BY DESC({v}) LIMIT 7"),
                update: false,
            });
        }
    }
    Battery {
        name: "lubm".into(),
        data: Some(dir.join("lubm.nt")),
        probes,
    }
}

/// The probe battery's dataset: entities with names, ages, e-mails, typed
/// and language-tagged literals, blank nodes, chains and cycles for paths,
/// and named graphs whose names also occur as nodes.
const PROBE_DATA: &str = r#"@prefix : <http://ex/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
:alice :name "Alice" ; :age 30 ; :email "alice@example.org" ; :knows :bob , :carol ;
    :score "7.5"^^xsd:decimal ; :born "1990-05-01"^^xsd:date ; :label "chat"@fr , "cat"@en .
:bob :name "Bob" ; :age 25 ; :knows :carol ; :score 9 ; :label "dog"@EN-us .
:carol :name "Carol"^^xsd:string ; :age "41"^^xsd:integer ; :knows :alice ; :score 2.5e0 .
:dave :name "dave" ; :knows _:x ; :score "n/a" .
_:x :name "Xavier" ; :knows :alice .
:a :p :b . :b :p :c . :c :p :a . :c :p :d . :d :q :a .
:a :q :b . :b :q :b .
:n1 :val 3 . :n2 :val 1 . :n3 :val 2 . :n4 :val 1 . :n5 :val -4 .
:n1 :next :n2 . :n2 :next :n3 . :n3 :next :n4 . :n4 :next :n5 .
:n1 :mixed :alice . :n2 :mixed "str" . :n3 :mixed 7 . :n4 :mixed _:x . :n5 :mixed :p .
:p :inverseOf :q .
:g1 :created "2020" ; :owner :alice .
:g2 :created "2021" .
GRAPH :g1 { :g1 :created "2020" . :alice :member :g1 . :bob :p :g1 . :s1 :p :o1 . }
GRAPH :g2 { :s2 :p :o2 . :g2 :created "2022" . :alice :knows :dave . }
GRAPH :g3 { :alice :name "Alicia" . :x1 :p :x2 . :x2 :p :x3 . }
"#;

/// The probe battery (see the module docs).
const PROBES: &[(&str, &str)] = &[
    // GRAPH ?g reusing ?g inside (review sparqlconf#1 / query#3)
    ("graph-var-reused-as-subject", "SELECT ?g ?t WHERE { GRAPH ?g { ?g <http://ex/created> ?t } }"),
    ("graph-var-reused-as-object", "ASK { GRAPH ?g { ?s <http://ex/p> ?g } }"),
    ("graph-all", "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }"),
    ("graph-join-default", "SELECT ?g ?s ?n WHERE { ?s <http://ex/name> ?n GRAPH ?g { ?s ?p ?o } }"),
    ("graph-const", "SELECT ?s ?o WHERE { GRAPH <http://ex/g3> { ?s <http://ex/p> ?o } }"),
    ("graph-bound-outside", "SELECT ?g ?o WHERE { ?g <http://ex/created> ?c GRAPH ?g { ?g <http://ex/created> ?o } }"),
    // OPTIONAL / MINUS / EXISTS with maybe-unbound keys
    ("optional-basic", "SELECT ?s ?n ?e WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/email> ?e } }"),
    ("optional-filter", "SELECT ?s ?a WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/age> ?a FILTER(?a > 26) } }"),
    ("optional-nested", "SELECT ?s ?k ?kn WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/knows> ?k OPTIONAL { ?k <http://ex/name> ?kn } } }"),
    ("optional-chain-unbound-key", "SELECT ?s ?e ?x WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/email> ?e } OPTIONAL { ?e <http://ex/p> ?x } }"),
    ("join-after-optional", "SELECT ?s ?e ?a WHERE { { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/email> ?e } } { ?s <http://ex/age> ?a } }"),
    ("minus-shared", "SELECT ?s WHERE { ?s <http://ex/name> ?n MINUS { ?s <http://ex/email> ?e } }"),
    ("minus-unshared", "SELECT ?s WHERE { ?s <http://ex/name> ?n MINUS { ?x <http://ex/q> ?y } }"),
    ("minus-optional-unbound", "SELECT ?s ?e WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/email> ?e } MINUS { ?s <http://ex/age> 25 } }"),
    ("not-exists", "SELECT ?s WHERE { ?s <http://ex/name> ?n FILTER NOT EXISTS { ?s <http://ex/knows> <http://ex/carol> } }"),
    ("exists-outer-var", "SELECT ?s ?k WHERE { ?s <http://ex/knows> ?k FILTER EXISTS { ?k <http://ex/knows> ?s } }"),
    ("exists-optional-var", "SELECT ?s ?e WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/email> ?e } FILTER(!BOUND(?e) || EXISTS { ?s <http://ex/age> ?a }) }"),
    // property paths and negated property sets (review sparqlconf#3, #4, #9, #13)
    ("path-plus", "SELECT ?x ?y WHERE { ?x <http://ex/p>+ ?y }"),
    ("path-star-const", "SELECT ?y WHERE { <http://ex/a> <http://ex/p>* ?y }"),
    ("path-question", "SELECT ?x ?y WHERE { ?x <http://ex/p>? ?y FILTER(?x = <http://ex/a>) }"),
    ("path-seq-multiplicity", "SELECT ?x ?y WHERE { ?x <http://ex/p>/<http://ex/p> ?y }"),
    ("path-alt-multiplicity", "SELECT ?x ?y WHERE { ?x <http://ex/p>|<http://ex/q> ?y }"),
    ("path-alt-same", "SELECT ?y WHERE { <http://ex/a> (<http://ex/q>|<http://ex/q>)/<http://ex/q> ?y }"),
    ("path-inverse", "SELECT ?x WHERE { <http://ex/a> ^<http://ex/p> ?x }"),
    ("path-bound-end-by-pattern", "SELECT ?s ?y WHERE { ?s <http://ex/q> ?b . ?b <http://ex/p>+ ?y }"),
    ("path-zero-unknown-const", "SELECT ?x WHERE { <http://ex/nowhere> <http://ex/p>* ?x }"),
    ("nps-forward", "SELECT ?x ?y WHERE { ?x !<http://ex/p> ?y FILTER(isIRI(?y)) }"),
    ("nps-inverse", "SELECT ?x ?y WHERE { ?x !^<http://ex/p> ?y FILTER(isIRI(?x) && isIRI(?y)) }"),
    ("nps-mixed", "SELECT ?x ?y WHERE { ?x !(<http://ex/p>|^<http://ex/q>) ?y FILTER(?x = <http://ex/a>) }"),
    ("path-in-optional", "SELECT ?s ?r WHERE { ?s <http://ex/name> ?n OPTIONAL { ?s <http://ex/knows>+ ?r } }"),
    // sub-SELECT in every position (review sparqlconf#5)
    ("subselect-whole-where", "SELECT ?s ?n WHERE { { SELECT ?s ?n WHERE { ?s <http://ex/name> ?n } ORDER BY ?n LIMIT 3 } }"),
    ("subselect-optional", "SELECT ?s ?c WHERE { ?s <http://ex/name> ?n OPTIONAL { SELECT ?s (COUNT(?k) AS ?c) WHERE { ?s <http://ex/knows> ?k } GROUP BY ?s } }"),
    ("subselect-minus", "SELECT ?s WHERE { ?s <http://ex/name> ?n MINUS { SELECT ?s WHERE { ?s <http://ex/age> ?a FILTER(?a < 30) } } }"),
    ("subselect-union", "SELECT ?x WHERE { { SELECT ?x WHERE { ?x <http://ex/p> ?y } LIMIT 2 } UNION { SELECT ?x WHERE { ?x <http://ex/q> ?y } } }"),
    ("subselect-exists", "SELECT ?s WHERE { ?s <http://ex/name> ?n FILTER EXISTS { SELECT ?s WHERE { ?s <http://ex/knows> ?k } } }"),
    ("subselect-graph", "SELECT ?g ?s WHERE { GRAPH ?g { SELECT ?s WHERE { ?s <http://ex/p> ?o } } }"),
    ("subselect-limit-offset", "SELECT ?v WHERE { { SELECT ?v WHERE { ?n <http://ex/val> ?v } ORDER BY ?v OFFSET 1 LIMIT 2 } }"),
    // SELECT aliases and BIND (review sparqlconf#6 / query#6)
    ("alias-chain", "SELECT ?s (?a + 1 AS ?b) (?b * 2 AS ?c) WHERE { ?s <http://ex/age> ?a }"),
    ("alias-chain-order", "SELECT ?s (?a + 1 AS ?b) (?b * 2 AS ?c) WHERE { ?s <http://ex/age> ?a } ORDER BY DESC(?c)"),
    ("bind-chain", "SELECT ?s ?d WHERE { ?s <http://ex/age> ?a BIND(?a * 2 AS ?b) BIND(?b - 1 AS ?d) FILTER(?d > 50) }"),
    ("values-inline", "SELECT ?s ?n WHERE { VALUES ?s { <http://ex/alice> <http://ex/bob> <http://ex/zed> } ?s <http://ex/name> ?n }"),
    ("values-trailing-undef", "SELECT ?s ?a WHERE { ?s <http://ex/age> ?a } VALUES (?s ?a) { (<http://ex/alice> UNDEF) (UNDEF 25) }"),
    // aggregates (review sparqlconf#7, #10, #15)
    ("min-max-mixed", "SELECT (MIN(?o) AS ?min) (MAX(?o) AS ?max) WHERE { ?s <http://ex/mixed> ?o }"),
    ("min-max-score", "SELECT (MIN(?v) AS ?min) (MAX(?v) AS ?max) (SUM(?v) AS ?sum) WHERE { ?s <http://ex/score> ?v FILTER(isNumeric(?v)) }"),
    ("count-distinct", "SELECT (COUNT(DISTINCT ?k) AS ?c) (COUNT(?k) AS ?all) WHERE { ?s <http://ex/knows> ?k }"),
    ("group-concat-distinct", "SELECT ?s (GROUP_CONCAT(DISTINCT ?l; separator=\"|\") AS ?ls) WHERE { ?s <http://ex/label> ?l } GROUP BY ?s"),
    ("avg-group-having", "SELECT ?k (AVG(?a) AS ?avg) WHERE { ?s <http://ex/knows> ?k . ?s <http://ex/age> ?a } GROUP BY ?k HAVING (COUNT(?s) >= 1)"),
    ("sample-group", "SELECT ?s (SAMPLE(?n) AS ?any) WHERE { ?s <http://ex/name> ?n } GROUP BY ?s"),
    ("non-grouped-projection-error", "SELECT ?s ?o WHERE { ?s <http://ex/knows> ?o } GROUP BY ?s"),
    ("aggregate-order-limit", "SELECT ?k (COUNT(*) AS ?c) WHERE { ?s <http://ex/knows> ?k } GROUP BY ?k ORDER BY DESC(?c) LIMIT 2"),
    // ORDER BY / LIMIT / DISTINCT shapes (Top-K, DP-B)
    ("order-mixed-types", "SELECT ?o WHERE { ?s <http://ex/mixed> ?o } ORDER BY ?o"),
    ("order-desc-limit", "SELECT ?n ?v WHERE { ?n <http://ex/val> ?v } ORDER BY DESC(?v) LIMIT 3"),
    ("order-expr-limit", "SELECT ?n ?v WHERE { ?n <http://ex/val> ?v } ORDER BY (?v * 2 + 1) LIMIT 2"),
    ("order-ties-limit", "SELECT ?n WHERE { ?n <http://ex/val> ?v } ORDER BY ?v LIMIT 2"),
    ("order-offset", "SELECT ?n ?v WHERE { ?n <http://ex/val> ?v } ORDER BY ?v ?n OFFSET 2"),
    ("dpb-tree", "SELECT ?a ?b ?v WHERE { ?a <http://ex/next> ?b . ?b <http://ex/val> ?v } ORDER BY ?v LIMIT 2"),
    ("dpb-two-keys", "SELECT ?a ?v ?w WHERE { ?a <http://ex/next> ?b . ?a <http://ex/val> ?v . ?b <http://ex/val> ?w } ORDER BY DESC(?v + ?w) LIMIT 3"),
    ("distinct-limit", "SELECT DISTINCT ?k WHERE { ?s <http://ex/knows> ?k } LIMIT 2"),
    ("distinct-order-limit", "SELECT DISTINCT ?v WHERE { ?n <http://ex/val> ?v } ORDER BY ?v LIMIT 2"),
    ("limit-zero", "SELECT ?s WHERE { ?s ?p ?o } LIMIT 0"),
    ("offset-beyond", "SELECT ?s WHERE { ?s <http://ex/name> ?n } OFFSET 100"),
    ("cross-product-limit", "SELECT ?a ?b WHERE { ?a <http://ex/val> ?x . ?b <http://ex/val> ?y } LIMIT 5"),
    // mixed predicate variables
    ("mixed-pred-var", "SELECT ?x ?o ?y WHERE { ?x ?p ?o . ?s ?x ?y }"),
    ("pred-var-join", "SELECT ?p ?q WHERE { <http://ex/a> ?p ?o . ?o ?q <http://ex/c> }"),
    ("pred-var-is-subject", "SELECT ?p ?v WHERE { ?s ?p ?o . ?p <http://ex/inverseOf> ?v }"),
    // built-ins, literals, errors (review sparqlconf#11, #14, #17, query#14)
    ("simple-vs-xsd-string", "SELECT ?s WHERE { ?s <http://ex/name> \"Carol\" }"),
    ("lang-case", "SELECT ?s WHERE { ?s <http://ex/label> \"dog\"@en-US }"),
    ("sameterm", "SELECT ?s ?n WHERE { ?s <http://ex/name> ?n FILTER(sameTerm(?n, \"Carol\")) }"),
    ("regex-flags", "SELECT ?n WHERE { ?s <http://ex/name> ?n FILTER(REGEX(?n, \"^a.i\", \"i\")) }"),
    ("replace", "SELECT (REPLACE(?n, \"[aeiou]\", \"_\") AS ?r) WHERE { ?s <http://ex/name> ?n }"),
    ("string-args", "SELECT ?s (STRLEN(?v) AS ?l) (UCASE(?v) AS ?u) WHERE { ?s <http://ex/score> ?v }"),
    ("strlang-strdt-iri", "SELECT (STRLANG(?n, \"en\") AS ?l) (STRDT(?n, <http://ex/dt>) AS ?d) (IRI(CONCAT(\"http://ex/\", ?n)) AS ?i) WHERE { ?s <http://ex/name> ?n }"),
    ("bound-unknown", "SELECT ?s (BOUND(?zz) AS ?b) WHERE { ?s <http://ex/age> ?a FILTER(!BOUND(?zz)) }"),
    ("in-not-in", "SELECT ?s WHERE { ?s <http://ex/age> ?a FILTER(?a IN (25, 41) || ?a NOT IN (30)) }"),
    ("if-coalesce-error", "SELECT ?s (IF(?v > 5, \"hi\", \"lo\") AS ?h) (COALESCE(?v / 0, -1) AS ?c) WHERE { ?s <http://ex/score> ?v }"),
    ("numeric-arith", "SELECT ?s ((?a / 7) AS ?q) (ABS(-?a) AS ?abs) (ROUND(?a / 4) AS ?r) WHERE { ?s <http://ex/age> ?a }"),
    ("date-functions", "SELECT (YEAR(?d) AS ?y) (MONTH(?d) AS ?m) (DATATYPE(?d) AS ?dt) WHERE { ?s <http://ex/born> ?d }"),
    ("lang-langmatches", "SELECT ?l (LANG(?l) AS ?t) WHERE { ?s <http://ex/label> ?l FILTER(LANGMATCHES(LANG(?l), \"en\")) }"),
    ("str-of-numbers", "SELECT ?s WHERE { ?s <http://ex/val> ?v FILTER(CONTAINS(STR(?v), \"1\")) }"),
    ("contains-prefilter", "SELECT ?s ?n WHERE { ?s <http://ex/name> ?n FILTER(CONTAINS(?n, \"aro\") || STRSTARTS(?n, \"X\")) }"),
    ("union-different-vars", "SELECT ?a ?b WHERE { { ?a <http://ex/p> <http://ex/b> } UNION { <http://ex/b> <http://ex/q> ?b } }"),
    ("blank-node-results", "SELECT ?x ?n WHERE { ?x <http://ex/name> ?n FILTER(isBlank(?x)) }"),
    // other query forms
    ("ask-true", "ASK { <http://ex/a> <http://ex/p>+ <http://ex/a> }"),
    ("ask-false", "ASK { <http://ex/d> <http://ex/p> ?x }"),
    ("construct", "CONSTRUCT { ?s <http://ex/friendOf> ?k . ?k <http://ex/seenBy> [ <http://ex/who> ?s ] } WHERE { ?s <http://ex/knows> ?k }"),
    ("construct-limit", "CONSTRUCT { ?s <http://ex/v> ?v } WHERE { ?s <http://ex/val> ?v } ORDER BY ?v LIMIT 2"),
    ("construct-where", "CONSTRUCT WHERE { ?s <http://ex/q> ?o }"),
    ("describe", "DESCRIBE <http://ex/alice>"),
    ("describe-var", "DESCRIBE ?s WHERE { ?s <http://ex/age> 25 }"),
    // updates
    ("update-insert-where", "INSERT { ?k <http://ex/knownBy> ?s } WHERE { ?s <http://ex/knows> ?k }"),
    ("update-delete-insert-limit-sub", "DELETE { ?n <http://ex/val> ?v } INSERT { ?n <http://ex/val2> ?v } WHERE { { SELECT ?n ?v WHERE { ?n <http://ex/val> ?v } ORDER BY ?v LIMIT 2 } }"),
    ("update-graph", "INSERT { GRAPH <http://ex/g4> { ?s <http://ex/copy> ?o } } WHERE { GRAPH ?g { ?s <http://ex/p> ?o } }"),
];

fn probe_battery(scratch: &Path) -> Battery {
    let data = scratch.join("probes.trig");
    fs::write(&data, PROBE_DATA).unwrap();
    Battery {
        name: "probes".into(),
        data: Some(data),
        probes: PROBES
            .iter()
            .map(|(name, text)| Probe {
                name: format!("probes/{name}"),
                text: text.to_string(),
                update: name.starts_with("update-"),
            })
            .collect(),
    }
}

// ---- running and comparing ------------------------------------------------------

/// One run whose answer differs from the `full` setting's.
#[derive(Debug, Clone)]
struct Divergence {
    probe: String,
    layout: &'static str,
    setting: &'static str,
    full: String,
    got: String,
}

impl Divergence {
    fn known(&self) -> Option<&'static str> {
        KNOWN.iter().find_map(|(name, layout, setting, id)| {
            (self.probe.contains(name)
                && (*layout == "*" || *layout == self.layout)
                && (*setting == "*" || *setting == self.setting))
                .then_some(*id)
        })
    }
}

/// Run `battery` on every layout and setting; its divergences, and how
/// many runs it made.
fn run_battery(battery: &Battery, filter: &str) -> (Vec<Divergence>, usize) {
    let probes: Vec<&Probe> = battery
        .probes
        .iter()
        .filter(|p| p.name.contains(filter) && !nondeterministic(&p.text))
        .collect();
    if probes.is_empty() {
        return (Vec::new(), 0);
    }
    let scratch = TempDir::new(&battery.name);
    let data = battery.data.clone().unwrap_or_else(|| {
        let p = scratch.path().join("empty.ttl");
        fs::write(&p, "").unwrap();
        p
    });
    let mut out = Vec::new();
    let mut runs = 0;
    for layout in Layout::all() {
        let mut db = match open_layout(layout, &data, scratch.path()) {
            Ok(db) => db,
            Err(e) => panic!("{} on {}: {e}", battery.name, layout.name()),
        };
        for probe in &probes {
            let mut answers: Vec<(&'static str, Answer)> = Vec::new();
            for setting in SETTINGS {
                runs += 1;
                let answer = if probe.update {
                    // A fresh database per setting: the update changes it.
                    let fresh = TempDir::new(&format!("{}_u", battery.name));
                    let mut db = match open_layout(layout, &data, fresh.path()) {
                        Ok(db) => db,
                        Err(e) => panic!("{} on {}: {e}", probe.name, layout.name()),
                    };
                    match run(&mut db, setting, &probe.text) {
                        Ok(_) => dataset(&db),
                        Err(_) => Answer::Error,
                    }
                } else {
                    Answer::of(run(&mut db, setting, &probe.text))
                };
                answers.push((setting.name, answer));
            }
            let (_, full) = &answers[0];
            for (setting, a) in &answers[1..] {
                if a != full {
                    out.push(Divergence {
                        probe: probe.name.clone(),
                        layout: layout.name(),
                        setting,
                        full: full.describe(),
                        got: a.describe(),
                    });
                }
            }
        }
    }
    (out, runs)
}

/// Run `batteries` on a few threads with large stacks (some W3C cases nest
/// deeply); every divergence, and the number of runs.
fn run_all(batteries: &[Battery], filter: &str) -> (Vec<Divergence>, usize) {
    let next = AtomicUsize::new(0);
    let found: Mutex<(Vec<Divergence>, usize)> = Mutex::new((Vec::new(), 0));
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 4);
    std::thread::scope(|s| {
        for _ in 0..workers {
            std::thread::Builder::new()
                .stack_size(256 << 20)
                .spawn_scoped(s, || loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(b) = batteries.get(i) else {
                        break;
                    };
                    let (d, n) = run_battery(b, filter);
                    let mut f = found.lock().unwrap();
                    f.0.extend(d);
                    f.1 += n;
                })
                .expect("spawn a worker");
        }
    });
    found.into_inner().unwrap()
}

/// Fail on every divergence not in [`KNOWN`], and on every [`KNOWN`] entry
/// that no longer diverges (when its battery ran unfiltered).
fn verdict(what: &str, divergences: Vec<Divergence>, runs: usize, filter: &str) {
    let mut report = Vec::new();
    for d in &divergences {
        match d.known() {
            Some(id) => println!(
                "KNOWN {} [{} {}] ({id}): full {} / {} {}",
                d.probe, d.layout, d.setting, d.full, d.setting, d.got
            ),
            None => report.push(format!(
                "DIVERGES {} [{} {}]\n    full:   {}\n    {}: {}",
                d.probe, d.layout, d.setting, d.full, d.setting, d.got
            )),
        }
    }
    if filter.is_empty() {
        for (name, layout, setting, id) in KNOWN {
            let in_battery = divergences.iter().any(|d| {
                d.probe.contains(name)
                    && (*layout == "*" || *layout == d.layout)
                    && (*setting == "*" || *setting == d.setting)
            });
            let ours = name.starts_with(what);
            if ours && !in_battery {
                report.push(format!(
                    "XPASS {name} [{layout} {setting}] ({id}): no longer diverges; \
                     remove it from KNOWN"
                ));
            }
        }
    }
    println!("{what}: {runs} run(s), {} divergence(s)", divergences.len());
    assert!(report.is_empty(), "{}", report.join("\n"));
    assert!(runs > 0 || !filter.is_empty(), "{what}: nothing ran");
}

fn filter() -> String {
    std::env::var("DIFF_FILTER").unwrap_or_default()
}

#[test]
fn the_w3c_subset_is_answered_alike_by_both_executors() {
    let filter = filter();
    let batteries = w3c_batteries();
    assert!(
        batteries.len() > 150,
        "found only {} W3C cases",
        batteries.len()
    );
    let (d, n) = run_all(&batteries, &filter);
    verdict("w3c/", d, n, &filter);
}

#[test]
fn the_lubm_queries_are_answered_alike_by_both_executors() {
    let filter = filter();
    let (d, n) = run_all(&[lubm_battery()], &filter);
    verdict("lubm/", d, n, &filter);
}

#[test]
fn the_probe_battery_is_answered_alike_by_both_executors() {
    let filter = filter();
    let scratch = TempDir::new("probe_data");
    let (d, n) = run_all(&[probe_battery(scratch.path())], &filter);
    verdict("probes/", d, n, &filter);
}

/// The suite must notice a divergence: an answer that differs from the
/// full setting's is reported (a self-check of the comparison).
#[test]
fn a_differing_answer_is_a_divergence() {
    let rows = |cells: &[&str]| {
        let mut m = BTreeMap::new();
        for c in cells {
            *m.entry(vec![c.to_string()]).or_insert(0) += 1;
        }
        Answer::Rows(vec!["x".into()], m)
    };
    assert_ne!(rows(&["<a>", "<b>"]), rows(&["<a>"]));
    assert_ne!(rows(&["<a>", "<a>"]), rows(&["<a>"]), "bags, not sets");
    assert_eq!(
        rows(&["<a>", "<b>"]),
        rows(&["<b>", "<a>"]),
        "order is free"
    );
    assert_ne!(Answer::Error, rows(&[]));
    // Blank-node labels are erased, nothing else.
    assert_eq!(cell(&Some("_:b1".into())), cell(&Some("_:other".into())));
    assert_ne!(cell(&Some("<_:x>".into())), "_:");
    assert!(nondeterministic("SELECT (rand () AS ?r) {}"));
    assert!(!nondeterministic("SELECT ?brand {}"));
}

/// The settings really reach the executors they name: `full` never
/// streams, the forced settings stream a SELECT / CONSTRUCT that has no
/// `LIMIT` (and a sub-SELECT), `heap` keeps an `ORDER BY` in the bounded
/// heap, `dpb` answers it with DP-B (a self-check of the hook).
#[test]
fn each_setting_takes_the_executor_it_names() {
    let db = Database::build_from_str(
        "xdiff_stats",
        "@prefix : <http://ex/> .\n:a :p 1 . :b :p 2 . :c :p 3 . :a :q :b .\n",
    )
    .unwrap();
    let stats = |setting: Setting, q: &str| {
        let query = gstore::parser::sparql::parse(q).unwrap();
        let ev =
            gstore::query::Evaluator::new(db.dict(), db.store()).with_topk_options(setting.options);
        let _unforce = Unforce;
        force_streaming_on_this_thread(setting.forced);
        ev.evaluate(&query).unwrap();
        ev.stats()
    };
    let [full, stream, heap, dpb, auto] = SETTINGS;
    let plain = "PREFIX : <http://ex/> SELECT ?s WHERE { ?s :p ?o }";
    let sub = "PREFIX : <http://ex/> SELECT ?s WHERE { ?s :q ?t { SELECT ?t WHERE { ?t :p ?o } } }";
    let ordered = "PREFIX : <http://ex/> SELECT ?s WHERE { ?s :p ?o } ORDER BY DESC(?o)";
    let construct = "PREFIX : <http://ex/> CONSTRUCT { ?s :r ?o } WHERE { ?s :p ?o }";
    for q in [plain, sub, ordered, construct] {
        let s = stats(full, q);
        assert_eq!(
            (s.streamed, s.heap_topk, s.dpb_topk),
            (0, 0, 0),
            "{q}: {s:?}"
        );
        // Without a LIMIT the default options evaluate in full as well.
        assert_eq!(stats(auto, q).streamed, 0, "{q}");
    }
    for q in [plain, sub, construct] {
        for setting in [stream, heap, dpb] {
            assert!(stats(setting, q).streamed > 0, "{q} ({})", setting.name);
        }
    }
    assert!(stats(heap, ordered).heap_topk > 0);
    assert!(stats(dpb, ordered).dpb_topk > 0);
    // The hook is off again after each run.
    assert_eq!(stats(auto, plain).streamed, 0);
}
