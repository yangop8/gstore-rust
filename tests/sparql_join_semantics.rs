//! SPARQL join operators in both executors: OPTIONAL and MINUS are hash
//! joins (review sparqlconf#2, query#10), `GRAPH ?g` joins with the inner
//! pattern's own binding of `?g` (query#3, sparqlconf#1), `EXISTS` honours
//! the current solution inside `VALUES` and sub-SELECTs (query#4), SELECT
//! expressions extend the solution left to right, each evaluated once
//! (query#6, sparqlconf#6), and a failing non-`SILENT` `SERVICE` is the
//! query's error (robust#6, tests-b#2, sec#17).
//!
//! Every query runs through the materializing executor
//! ([`TopKOptions::NONE`]) and the streaming one (a `LIMIT` larger than the
//! answer, with the bounded heap and with DP-B forced): both must give the
//! expected rows, in the same order (the streaming executor promises the
//! full evaluation's order). The spec-derived cases of the same operators
//! are W3C-style cases under `tests/w3c/{optional-minus,graph,exists,
//! projection,service}` (run by `tests/w3c_subset.rs`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstore::dict::Dictionary;
use gstore::model::{IdTriple, Term};
use gstore::parser::ntriples::parse_term;
use gstore::parser::sparql::parse;
use gstore::query::engine::{DpbMode, TopKOptions};
use gstore::query::{Evaluator, FunctionRegistry, QueryResult, Value};
use gstore::store::TripleStore;
use gstore::{Database, GStoreError};

const EX: &str = "http://ex/";
const PREFIX: &str = "PREFIX : <http://ex/> ";

/// The streaming executor with the bounded heap, DP-B off.
const HEAP_ONLY: TopKOptions = TopKOptions {
    early_termination: true,
    bounded_heap: true,
    dpb: DpbMode::Off,
};

/// The streaming executor with DP-B wherever the shape allows.
const FORCE_DPB: TopKOptions = TopKOptions {
    early_termination: true,
    bounded_heap: true,
    dpb: DpbMode::Always,
};

/// The `LIMIT` that sends a query without one through the streaming
/// executor (larger than every answer here).
const STREAM_LIMIT: &str = " LIMIT 100000000";

// ---- data ------------------------------------------------------------------

/// A term in the tests' short syntax: `:x` is `<http://ex/x>`, a bare
/// integer is an `xsd:integer`, anything else N-Triples.
fn term(s: &str) -> Term {
    if let Some(local) = s.strip_prefix(':') {
        Term::iri(format!("{EX}{local}"))
    } else if s.parse::<i64>().is_ok() {
        Term::typed_literal(s, "http://www.w3.org/2001/XMLSchema#integer")
    } else {
        parse_term(s).unwrap_or_else(|e| panic!("term {s}: {e}"))
    }
}

/// A dataset at the evaluator level (no query cache, so timings measure
/// the evaluation): a default graph and named graphs.
struct Data {
    dict: Dictionary,
    store: TripleStore,
    named: BTreeMap<u32, TripleStore>,
}

impl Data {
    /// Quads `(s, p, o, graph)` in [`term`] syntax; `graph` `""` is the
    /// default graph.
    fn new(quads: &[(String, String, String, String)]) -> Data {
        let mut dict = Dictionary::new();
        let mut default = Vec::new();
        let mut graphs: BTreeMap<u32, Vec<IdTriple>> = BTreeMap::new();
        for (s, p, o, g) in quads {
            let t = IdTriple::new(
                dict.intern_term(&term(s)),
                dict.intern_predicate(&term(p).dict_key()),
                dict.intern_term(&term(o)),
            );
            if g.is_empty() {
                default.push(t);
            } else {
                let gid = dict.intern_term(&term(g));
                graphs.entry(gid).or_default().push(t);
            }
        }
        let mut store = TripleStore::new();
        store.bulk_load(default);
        let named = graphs
            .into_iter()
            .map(|(g, ts)| {
                let mut gs = TripleStore::new();
                gs.bulk_load(ts);
                (g, gs)
            })
            .collect();
        Data { dict, store, named }
    }

    /// Parse `text`: one quad per line, `s p o [g] .`, in [`term`] syntax
    /// (a literal must not hold a space).
    fn parse(text: &str) -> Data {
        let quads: Vec<(String, String, String, String)> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| {
                let parts: Vec<&str> = l.trim_end_matches('.').split_whitespace().collect();
                let g = parts.get(3).copied().unwrap_or("");
                (
                    parts[0].to_string(),
                    parts[1].to_string(),
                    parts[2].to_string(),
                    g.to_string(),
                )
            })
            .collect();
        Data::new(&quads)
    }

    fn evaluator(&self, opts: TopKOptions) -> Evaluator<'_, TripleStore> {
        Evaluator::new(&self.dict, &self.store)
            .with_named(&self.named)
            .with_topk_options(opts)
    }

    fn run(&self, q: &str, opts: TopKOptions) -> Result<QueryResult, GStoreError> {
        let query = parse(&format!("{PREFIX}{q}")).unwrap_or_else(|e| panic!("{e}: {q}"));
        self.evaluator(opts).evaluate(&query)
    }

    /// The rows of SELECT `q` through `opts`.
    fn rows(&self, q: &str, opts: TopKOptions) -> Vec<Vec<Option<String>>> {
        match self.run(q, opts) {
            Ok(QueryResult::Select(rs)) => rs.rows,
            other => panic!("{q} ({opts:?}): {other:?}"),
        }
    }

    /// The rows of SELECT `q` (no top-level LIMIT) through every executor:
    /// all of them must agree, row for row; returns them.
    fn rows_everywhere(&self, q: &str) -> Vec<Vec<Option<String>>> {
        let full = self.rows(q, TopKOptions::NONE);
        let limited = format!("{q}{STREAM_LIMIT}");
        for opts in [HEAP_ONLY, FORCE_DPB, TopKOptions::ALL] {
            assert_eq!(
                self.rows(&limited, opts),
                full,
                "streaming ({opts:?}) differs from materializing:\n{q}"
            );
        }
        // The materializing executor with the LIMIT agrees too.
        assert_eq!(self.rows(&limited, TopKOptions::NONE), full, "{q}");
        full
    }

    /// The answer of ASK `q` through every executor (which must agree).
    fn ask_everywhere(&self, q: &str) -> bool {
        let mut answers = Vec::new();
        for opts in [TopKOptions::NONE, HEAP_ONLY, TopKOptions::ALL] {
            match self.run(q, opts) {
                Ok(QueryResult::Ask(b)) => answers.push(b),
                other => panic!("{q}: {other:?}"),
            }
        }
        assert!(answers.windows(2).all(|w| w[0] == w[1]), "{q}: {answers:?}");
        answers[0]
    }
}

fn iri(local: &str) -> Option<String> {
    Some(format!("<{EX}{local}>"))
}

fn lit(s: &str) -> Option<String> {
    Some(format!("\"{s}\""))
}

fn int(i: i64) -> Option<String> {
    Some(format!(
        "\"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    ))
}

fn sorted(mut rows: Vec<Vec<Option<String>>>) -> Vec<Vec<Option<String>>> {
    rows.sort();
    rows
}

// ---- OPTIONAL / MINUS / joins over unbound variables: hash joins -----------

/// `n` subjects with `:p`, every second one with `:q`, every subject but
/// the last with a `:name`, and a `:nick` per name.
fn join_data(n: usize) -> Data {
    let mut quads = Vec::new();
    let q = |s: String, p: &str, o: String| (s, p.to_string(), o, String::new());
    for i in 0..n {
        let s = format!(":s{i}");
        quads.push(q(s.clone(), ":p", format!(":o{i}")));
        if i % 2 == 0 {
            quads.push(q(s.clone(), ":q", format!(":x{i}")));
        }
        if i + 1 < n {
            quads.push(q(s.clone(), ":name", format!("\"n{i}\"")));
            quads.push(q(format!(":k{i}"), ":nick", format!("\"n{i}\"")));
        }
    }
    Data::new(&quads)
}

/// The fastest of `runs` evaluations of `q` (an aggregate, so it never
/// streams; or with a LIMIT, so it does), checking the count.
fn best_time(data: &Data, q: &str, opts: TopKOptions, runs: usize) -> Duration {
    let query = parse(&format!("{PREFIX}{q}")).unwrap();
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = data.evaluator(opts).evaluate(&query);
            let took = t.elapsed();
            assert!(r.is_ok(), "{q}: {r:?}");
            took
        })
        .min()
        .unwrap()
}

/// OPTIONAL, MINUS and a join after an OPTIONAL (whose shared variable is
/// unbound in one row) cost about what the streaming LIMIT path costs and
/// grow linearly with the data — they used to compare every left solution
/// with every right one (review sparqlconf#2: 12-16 s for 20,000 x 10,000
/// rows against 0.06 s with LIMIT; query#10: the cross product after one
/// unbound row). Elapsed time is the measure (the operators have no work
/// counter; the unit tests of `src/query/probe.rs` count probe visits);
/// the bounds are generous ratios over 4x the data, and a miss is measured
/// again before it fails, so a busy machine does not fail the test.
#[test]
fn optional_minus_and_unbound_joins_scale_linearly() {
    let (small, large) = (5_000, 20_000);
    let queries = [
        (
            "OPTIONAL",
            "SELECT (COUNT(*) AS ?c) WHERE { ?s :p ?o OPTIONAL { ?s :q ?x } }",
            "SELECT * WHERE { ?s :p ?o OPTIONAL { ?s :q ?x } } LIMIT 100000000",
        ),
        (
            "MINUS",
            "SELECT (COUNT(*) AS ?c) WHERE { ?s :p ?o MINUS { ?s :q ?x } }",
            "SELECT * WHERE { ?s :p ?o MINUS { ?s :q ?x } } LIMIT 100000000",
        ),
        (
            "join after OPTIONAL",
            "SELECT (COUNT(*) AS ?c) WHERE { { ?s :p ?o OPTIONAL { ?s :name ?n } } { ?k :nick ?n } }",
            "SELECT * WHERE { { ?s :p ?o OPTIONAL { ?s :name ?n } } { ?k :nick ?n } } LIMIT 100000000",
        ),
    ];
    let datasets = [join_data(small), join_data(large)];
    // The answers first (correct before and after the fix).
    let count = |data: &Data, q: &str| match data.run(q, TopKOptions::NONE) {
        Ok(QueryResult::Select(rs)) => rs.rows[0][0].clone(),
        other => panic!("{q}: {other:?}"),
    };
    for (n, data) in [small, large].iter().zip(&datasets) {
        let n = *n as i64;
        assert_eq!(
            count(data, queries[0].1),
            int(n),
            "OPTIONAL keeps every left row"
        );
        assert_eq!(
            count(data, queries[1].1),
            int(n / 2),
            "MINUS drops the :q half"
        );
        // n - 1 named rows each meet their nick; the unnamed row meets all
        // n - 1 nicks.
        assert_eq!(count(data, queries[2].1), int(2 * (n - 1)));
    }
    let mut quadratic = Vec::new();
    for (what, full, streamed) in queries {
        for attempt in 0..3 {
            let runs = 1 + attempt;
            let t_small = best_time(&datasets[0], full, TopKOptions::NONE, runs);
            let t_large = best_time(&datasets[1], full, TopKOptions::NONE, runs);
            let t_stream = best_time(&datasets[1], streamed, TopKOptions::ALL, runs);
            // Linear: ~4x for 4x the data (quadratic: ~16x). The constant
            // covers timer noise on the small run.
            let scales = t_large <= t_small * 9 + Duration::from_millis(30);
            // The full path costs about what the streaming path costs.
            let like_stream = t_large <= t_stream * 10 + Duration::from_millis(100);
            let verdict = format!(
                "{what}: {small} rows {t_small:?}, {large} rows {t_large:?}, \
                 streamed {t_stream:?} (attempt {attempt})"
            );
            eprintln!("{verdict}");
            if scales && like_stream {
                break;
            }
            if attempt == 2 {
                quadratic.push(verdict);
            }
        }
    }
    assert!(quadratic.is_empty(), "not linear: {quadratic:#?}");
}

/// The OPTIONAL loop polls cancellation between left solutions: a FILTER
/// inside the OPTIONAL that raises the cancel flag (`STOP()`, counting its
/// calls) runs once, not once per matching left solution, and the query
/// reports the cancel (review sparqlconf#2: no cancel check inside the loop).
#[test]
fn a_cancel_stops_an_optional_between_left_solutions() {
    let data = join_data(2_000);
    let flag = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut reg = FunctionRegistry::new();
    {
        let (flag, calls) = (Arc::clone(&flag), Arc::clone(&calls));
        reg.register("STOP", move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            flag.store(true, Ordering::Relaxed);
            Some(Value::Bool(true))
        });
    }
    let q = parse(&format!(
        "{PREFIX}SELECT (COUNT(*) AS ?c) WHERE {{ ?s :p ?o OPTIONAL {{ ?s :q ?x FILTER(STOP()) }} }}"
    ))
    .unwrap();
    let r = Evaluator::new(&data.dict, &data.store)
        .with_functions(reg)
        .with_cancel(Arc::clone(&flag))
        .with_topk_options(TopKOptions::NONE)
        .evaluate(&q);
    assert!(matches!(r, Err(GStoreError::Cancelled)), "{r:?}");
    let calls = calls.load(Ordering::Relaxed);
    assert!(
        calls <= 2,
        "the OPTIONAL ran on after the cancel: {calls} calls"
    );
}

/// Semantics of the hashed operators where the shared variables are
/// unbound in some solutions — on either side — through both executors.
#[test]
fn hashed_operators_keep_compatible_merge_semantics() {
    let data = Data::parse(
        ":s1 :p :o1 .
         :s2 :p :o2 .
         :s3 :p :o3 .
         :s1 :name \"a\" .
         :s2 :name \"b\" .
         :k1 :nick \"a\" .
         :k2 :nick \"c\" .
         :s1 :q :x1 .
         :y1 :r :x9 .",
    );
    // A join after an OPTIONAL: the row without ?n meets every nick.
    let rows = data.rows_everywhere(
        "SELECT ?s ?k WHERE { { ?s :p ?o OPTIONAL { ?s :name ?n } } { ?k :nick ?n } }",
    );
    assert_eq!(
        sorted(rows),
        vec![
            vec![iri("s1"), iri("k1")],
            vec![iri("s3"), iri("k1")],
            vec![iri("s3"), iri("k2")],
        ]
    );
    // MINUS: a left row whose shared variable is unbound shares no
    // variable with the right side, so nothing removes it.
    let rows = data.rows_everywhere(
        "SELECT ?s WHERE { ?s :p ?o OPTIONAL { ?s :name ?n } MINUS { ?k :nick ?n } }",
    );
    assert_eq!(sorted(rows), vec![vec![iri("s2")], vec![iri("s3")]]);
    // OPTIONAL whose right side has rows without the shared variable:
    // those are compatible with every left row.
    let rows = data.rows_everywhere(
        "SELECT ?s ?x WHERE { ?s :p ?o OPTIONAL { { ?s :q ?x } UNION { ?y :r ?x } } }",
    );
    assert_eq!(
        sorted(rows),
        vec![
            vec![iri("s1"), iri("x1")],
            vec![iri("s1"), iri("x9")],
            vec![iri("s2"), iri("x9")],
            vec![iri("s3"), iri("x9")],
        ]
    );
    // MINUS whose right side has rows binding other variables only.
    let rows = data
        .rows_everywhere("SELECT ?s WHERE { ?s :p ?o MINUS { { ?s :q ?x } UNION { ?y :r ?x } } }");
    assert_eq!(sorted(rows), vec![vec![iri("s2")], vec![iri("s3")]]);
    // The OPTIONAL's FILTER sees the merged solution.
    let rows = data.rows_everywhere(
        "SELECT ?s ?n WHERE { ?s :p ?o OPTIONAL { ?s :name ?n FILTER(?n != \"b\") } }",
    );
    assert_eq!(
        sorted(rows),
        vec![
            vec![iri("s1"), lit("a")],
            vec![iri("s2"), None],
            vec![iri("s3"), None]
        ]
    );
}

// ---- the executors agree ---------------------------------------------------

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A random dataset: a few subjects, predicates :p :q :r :name, values,
/// some triples in named graphs :g0 / :g1 (whose names also occur as
/// subjects and objects).
fn random_data(rng: &mut Rng) -> Data {
    let nodes = ["a", "b", "c", "d", "g0", "g1"];
    let preds = [":p", ":q", ":r", ":name"];
    let mut quads = Vec::new();
    for _ in 0..(8 + rng.below(25)) {
        let s = format!(":{}", nodes[rng.below(nodes.len())]);
        let p = preds[rng.below(preds.len())];
        let o = if p == ":name" {
            format!("{}", rng.below(4))
        } else {
            format!(":{}", nodes[rng.below(nodes.len())])
        };
        let g = match rng.below(3) {
            0 => String::new(),
            1 => ":g0".to_string(),
            _ => ":g1".to_string(),
        };
        quads.push((s, p.to_string(), o, g));
    }
    Data::new(&quads)
}

/// Every operator fixed here, shaped so shared variables are unbound in
/// some solutions; the materializing and streaming executors must return
/// the same rows in the same order (a D3 guard until the executors are
/// merged; review query#17 / design#10).
#[test]
fn join_operators_agree_across_executors() {
    let queries = [
        "SELECT * WHERE { ?s :p ?o OPTIONAL { ?o :q ?x } }",
        "SELECT * WHERE { ?s :p ?o OPTIONAL { ?o :q ?x FILTER(?x != :a) } }",
        "SELECT * WHERE { ?s :p ?o OPTIONAL { ?s :name ?n } ?k :name ?n }",
        "SELECT * WHERE { { ?s :p ?o OPTIONAL { ?s :name ?n } } { ?k :r ?o OPTIONAL { ?k :name ?n } } }",
        "SELECT * WHERE { ?s :p ?o OPTIONAL { { ?o :q ?x } UNION { ?y :r ?x } } }",
        "SELECT * WHERE { ?s :p ?o MINUS { ?s :q ?x } }",
        "SELECT * WHERE { ?s :p ?o OPTIONAL { ?s :name ?n } MINUS { ?k :name ?n } }",
        "SELECT * WHERE { ?s :p ?o MINUS { { ?o :q ?x } UNION { ?y :r ?x } } }",
        "SELECT * WHERE { ?s :p ?o OPTIONAL { ?o :p ?x OPTIONAL { ?x :name ?n } } }",
        "SELECT * WHERE { GRAPH ?g { ?g :p ?o } }",
        "SELECT * WHERE { GRAPH ?g { ?s :p ?g } }",
        "SELECT * WHERE { GRAPH ?g { ?s ?p ?o OPTIONAL { ?o :q ?g } } }",
        "SELECT * WHERE { ?s :r ?g GRAPH ?g { ?s ?p ?o } }",
        "SELECT * WHERE { ?s :p ?o FILTER EXISTS { VALUES ?s { :a :c } } }",
        "SELECT * WHERE { ?s :p ?o FILTER NOT EXISTS { { SELECT ?o WHERE { ?o :q ?z } } } }",
        "SELECT * WHERE { ?s :p ?o FILTER EXISTS { GRAPH ?o { ?s ?p2 ?x } } }",
        "SELECT ?s (STR(?o) AS ?t) (CONCAT(?t, \"!\") AS ?u) WHERE { ?s :p ?o }",
        "SELECT ?s (STR(?o) AS ?t) (CONCAT(?t, \"!\") AS ?u) WHERE { ?s :p ?o } ORDER BY ?u ?s",
    ];
    let mut rng = Rng(0x5eed);
    for _ in 0..40 {
        let data = random_data(&mut rng);
        for q in queries {
            data.rows_everywhere(q);
        }
    }
}
