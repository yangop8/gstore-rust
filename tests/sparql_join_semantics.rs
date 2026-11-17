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
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
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

/// The CPU time this thread has used: what an evaluation costs, whatever
/// else the machine runs (wall time also counts the time the thread waits
/// for a CPU, which on a busy machine hits a long run far more than a short
/// one; round-3 review). Wall time where no per-thread clock is at hand.
fn thread_cpu_time() -> Duration {
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Timespec {
            tv_sec: std::ffi::c_long,
            tv_nsec: std::ffi::c_long,
        }
        extern "C" {
            fn clock_gettime(clock: i32, tp: *mut Timespec) -> i32;
        }
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec.
        if unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// The CPU time ([`thread_cpu_time`]) of one evaluation of `q` (an
/// aggregate, so it never streams; or with a LIMIT, so it does), checking
/// it succeeds.
fn cpu_time(
    data: &Data,
    query: &gstore::parser::sparql::ast::Query,
    opts: TopKOptions,
) -> Duration {
    let ev = data.evaluator(opts);
    let t = thread_cpu_time();
    let r = ev.evaluate(query);
    let took = thread_cpu_time().saturating_sub(t);
    assert!(r.is_ok(), "{r:?}");
    took
}

/// The rows the hashed operands of OPTIONAL, MINUS and joins looked at in
/// one evaluation of `q` (`EvalStats::join_probe_rows`), checking it
/// succeeds.
fn probe_rows(data: &Data, q: &str, opts: TopKOptions) -> u64 {
    let query = parse(&format!("{PREFIX}{q}")).unwrap();
    let ev = data.evaluator(opts);
    let r = ev.evaluate(&query);
    assert!(r.is_ok(), "{q}: {r:?}");
    ev.stats().join_probe_rows
}

/// OPTIONAL, MINUS and a join after an OPTIONAL (whose shared variable is
/// unbound in one row) grow linearly with the data in both executors, and
/// the full path costs about what the streaming LIMIT path costs — they
/// used to compare every left solution with every right one (review
/// sparqlconf#2: 12-16 s for 20,000 x 10,000 rows against 0.06 s with
/// LIMIT; the streaming MINUS scanned too; query#10: the cross product
/// after one unbound row, and the streaming `Probe`'s scan of an operand
/// whose key some row leaves unbound).
///
/// Round-3 review: this test timed the evaluations by wall clock, small
/// size and large size one after the other, and failed under CPU
/// contention (a ~2 ms run fits in one scheduler slice, a ~25 ms one is
/// preempted: 3 of 11 runs of the binary failed with four busy loops on
/// four CPUs). The work is now the primary measure, deterministic: the
/// rows the operators' hashed operands looked at
/// (`EvalStats::join_probe_rows`) must be at least the matches (the
/// operators go through the index) and at most a few per data row, 8x for
/// 8x the data, in both executors. The time is still checked, as the CPU
/// time of the evaluating thread, small and large measured alternately,
/// with the same generous ratios over 8x the data (linear ~8x, quadratic
/// ~64x), and a miss is measured again before it fails.
#[test]
fn optional_minus_and_unbound_joins_scale_linearly() {
    let (small, large) = (2_500, 20_000);
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
    // The work, in both executors: deterministic.
    for (what, full, streamed) in queries {
        for (exec, q, opts) in [
            ("full", full, TopKOptions::NONE),
            ("streamed", streamed, TopKOptions::ALL),
        ] {
            let at_small = probe_rows(&datasets[0], q, opts);
            let at_large = probe_rows(&datasets[1], q, opts);
            eprintln!(
                "{what} ({exec}): {at_small} rows looked at for {small}, {at_large} for {large}"
            );
            // At least the matches: every :q row (OPTIONAL, MINUS) or
            // every name (the join) is found through the index.
            assert!(
                at_small >= small as u64 / 2 && at_large >= large as u64 / 2,
                "{what} ({exec}): the operator did not go through its index \
                 ({at_small}, {at_large} rows looked at)"
            );
            // Linear: a few rows per data row (the join after OPTIONAL
            // looks at about 3n: n - 1 names, n - 1 nicks for the named
            // rows and n - 1 for the unnamed one), 8x for 8x the data —
            // comparing every left solution with every right one looks at
            // n²/2 and more.
            assert!(
                at_large <= 4 * large as u64 && at_large <= at_small * 9,
                "{what} ({exec}): not linear: {at_small} rows looked at for {small}, \
                 {at_large} for {large}"
            );
        }
    }
    // The time (CPU time of this thread), as a second measure.
    let mut quadratic = Vec::new();
    for (what, full, streamed) in queries {
        let (full, streamed) = (
            parse(&format!("{PREFIX}{full}")).unwrap(),
            parse(&format!("{PREFIX}{streamed}")).unwrap(),
        );
        for attempt in 0..3 {
            let runs = 1 + attempt;
            // Small and large alternate, so a change of the machine's state
            // affects both.
            let mut t = [Duration::MAX; 4];
            for _ in 0..runs {
                t[0] = t[0].min(cpu_time(&datasets[0], &full, TopKOptions::NONE));
                t[1] = t[1].min(cpu_time(&datasets[1], &full, TopKOptions::NONE));
                t[2] = t[2].min(cpu_time(&datasets[0], &streamed, TopKOptions::ALL));
                t[3] = t[3].min(cpu_time(&datasets[1], &streamed, TopKOptions::ALL));
            }
            let [t_small, t_large, s_small, s_large] = t;
            // Linear: ~8x for 8x the data (quadratic: ~64x), in both
            // executors (review: the streaming MINUS and the streaming
            // Probe were quadratic too). The constant covers timer noise on
            // the small runs.
            let linear = |at_small: Duration, at_large: Duration| {
                at_large <= at_small * 20 + Duration::from_millis(30)
            };
            let scales = linear(t_small, t_large) && linear(s_small, s_large);
            // The full path costs about what the streaming path costs.
            let like_stream = t_large <= s_large * 10 + Duration::from_millis(100);
            let verdict = format!(
                "{what}: full {small} rows {t_small:?}, {large} rows {t_large:?}; \
                 streamed {s_small:?}, {s_large:?} (attempt {attempt})"
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

// ---- GRAPH ?g --------------------------------------------------------------

fn graph_data() -> Data {
    Data::parse(
        ":g1 :created \"2020\" :g1 .
         :doc7 :created \"1999\" :g1 .
         :doc8 :created \"2001\" :g2 .
         :s1 :p :g2 :g1 .
         :a :g3 :b :g3 .
         :a :q :c :g3 .
         :d :p :e .",
    )
}

/// `GRAPH ?g { P }` joins the graph name with P's solutions: a solution of
/// P that binds ?g to another term is not compatible (SPARQL 1.1 §18.5
/// Graph(var, P); review query#3 / sparqlconf#1 — the binding was
/// overwritten). Through both executors.
#[test]
fn a_graph_variable_joins_with_its_binding_inside_the_graph() {
    let data = graph_data();
    let rows = data.rows_everywhere("SELECT ?g ?t WHERE { GRAPH ?g { ?g :created ?t } }");
    assert_eq!(rows, vec![vec![iri("g1"), lit("2020")]]);
    // ?g in the object position: :s1 :p :g2 is in :g1, so no solution.
    assert!(!data.ask_everywhere("ASK { GRAPH ?g { ?s :p ?g } }"));
    let rows = data.rows_everywhere("SELECT ?s WHERE { GRAPH ?g { ?s :p ?g } }");
    assert!(rows.is_empty(), "{rows:?}");
    // ?g in the predicate position (a variable bound in two id spaces):
    // only the triple whose predicate is the graph's own name.
    let rows = data.rows_everywhere("SELECT ?g ?s ?o WHERE { GRAPH ?g { ?s ?g ?o } }");
    assert_eq!(rows, vec![vec![iri("g3"), iri("a"), iri("b")]]);
    // A plain GRAPH ?g still ranges over every named graph.
    let rows = data.rows_everywhere("SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }");
    assert_eq!(
        sorted(rows),
        vec![vec![iri("g1")], vec![iri("g2")], vec![iri("g3")]]
    );
}

/// Inside EXISTS, a graph variable the current solution binds to a literal
/// names no graph: no solution (review sparqlconf#1 — the variable was left
/// free, so EXISTS ranged over every graph).
#[test]
fn exists_with_a_graph_variable_bound_to_a_literal_finds_nothing() {
    let data = graph_data();
    let rows = data.rows_everywhere(
        "SELECT ?x WHERE { BIND(\"lit\" AS ?g) FILTER EXISTS { GRAPH ?g { ?s ?p ?o } } BIND(1 AS ?x) }",
    );
    assert!(rows.is_empty(), "{rows:?}");
    let rows = data.rows_everywhere(
        "SELECT ?x WHERE { BIND(\"lit\" AS ?g) FILTER NOT EXISTS { GRAPH ?g { ?s ?p ?o } } BIND(1 AS ?x) }",
    );
    assert_eq!(rows, vec![vec![int(1)]]);
    // Bound to a graph's IRI, EXISTS looks in that graph only.
    let rows = data.rows_everywhere(
        "SELECT ?g WHERE { VALUES ?g { :g1 :g2 } FILTER EXISTS { GRAPH ?g { :doc8 ?p ?o } } }",
    );
    assert_eq!(rows, vec![vec![iri("g2")]]);
}

/// `INSERT { ?g :meta ?t } WHERE { GRAPH ?g { ?g :created ?t } }` writes the
/// one compatible solution, durably — not one triple per subject of the
/// graph (review sparqlconf#1). Through both executors' settings.
#[test]
fn insert_where_graph_variable_writes_only_compatible_solutions() {
    for opts in [TopKOptions::NONE, TopKOptions::ALL] {
        let mut db = Database::new("graphvar_insert");
        db.set_topk_options(opts);
        db.query(
            "PREFIX : <http://ex/> INSERT DATA { \
               GRAPH :g1 { :g1 :created \"2020\" . :doc7 :created \"1999\" } \
               GRAPH :g2 { :doc8 :created \"2001\" } }",
        )
        .unwrap();
        let r = db
            .query(
                "PREFIX : <http://ex/> INSERT { ?g :meta ?t } WHERE { GRAPH ?g { ?g :created ?t } }",
            )
            .unwrap();
        assert!(
            matches!(r, QueryResult::Update { changed: 1 }),
            "{opts:?}: {r:?}"
        );
        let rs = db
            .select("PREFIX : <http://ex/> SELECT ?s ?t WHERE { ?s :meta ?t }")
            .unwrap();
        assert_eq!(rs.rows, vec![vec![iri("g1"), lit("2020")]], "{opts:?}");
    }
}

// ---- EXISTS ----------------------------------------------------------------

fn exists_data() -> Data {
    Data::parse(
        ":a :p :o1 .
         :x :p :o2 .
         :x :p :o3 .",
    )
}

/// EXISTS / NOT EXISTS test the pattern against the current solution also
/// where substitution does not reach: a VALUES block's rows and a
/// sub-SELECT's projected variables must be compatible with it (SPARQL 1.1
/// §8.1, §18.6; review query#4 — they ignored the outer binding).
#[test]
fn exists_honours_outer_bindings_in_values_and_sub_selects() {
    let data = exists_data();
    let rows =
        data.rows_everywhere("SELECT ?s WHERE { ?s :p ?o FILTER EXISTS { VALUES ?s { :a } } }");
    assert_eq!(rows, vec![vec![iri("a")]]);
    let rows =
        data.rows_everywhere("SELECT ?s WHERE { ?s :p ?o FILTER NOT EXISTS { VALUES ?s { :a } } }");
    assert_eq!(rows, vec![vec![iri("x")], vec![iri("x")]]);
    // A VALUES row with UNDEF is compatible with anything.
    let rows = data.rows_everywhere(
        "SELECT ?s WHERE { ?s :p ?o FILTER EXISTS { VALUES (?s ?z) { (:nobody 1) (UNDEF 2) } } }",
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    let rows = data.rows_everywhere(
        "SELECT ?s WHERE { ?s :p ?o FILTER EXISTS { \
           { SELECT ?s WHERE { ?s :p ?z } GROUP BY ?s HAVING (COUNT(*) > 1) } } }",
    );
    assert_eq!(rows, vec![vec![iri("x")], vec![iri("x")]]);
    let rows = data.rows_everywhere(
        "SELECT ?s WHERE { ?s :p ?o FILTER NOT EXISTS { \
           { SELECT ?s WHERE { ?s :p ?z } GROUP BY ?s HAVING (COUNT(*) > 1) } } }",
    );
    assert_eq!(rows, vec![vec![iri("a")]]);
    // A variable the sub-SELECT does not project is its own: the outer ?s
    // does not constrain it.
    let rows = data.rows_everywhere(
        "SELECT ?s WHERE { ?s :p ?o FILTER EXISTS { { SELECT ?z WHERE { ?s :p ?z } } } }",
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    // VALUES joined with a triple pattern inside EXISTS.
    let rows = data.rows_everywhere(
        "SELECT ?s ?o WHERE { ?s :p ?o FILTER EXISTS { VALUES ?o { :o1 :o3 } ?s :p ?o } }",
    );
    assert_eq!(
        rows,
        vec![vec![iri("a"), iri("o1")], vec![iri("x"), iri("o3")]]
    );
    // As a SELECT expression and in ASK.
    let rows =
        data.rows_everywhere("SELECT ?s (EXISTS { VALUES ?s { :a } } AS ?e) WHERE { ?s :p ?o }");
    let flag = |b: bool| {
        Some(format!(
            "\"{b}\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ))
    };
    assert_eq!(
        rows,
        vec![
            vec![iri("a"), flag(true)],
            vec![iri("x"), flag(false)],
            vec![iri("x"), flag(false)]
        ]
    );
    assert!(!data.ask_everywhere("ASK { :x :p ?o FILTER EXISTS { VALUES ?o { :o1 } } }"));
}

// ---- SELECT expressions ----------------------------------------------------

/// SELECT expressions extend the solution left to right (SPARQL 1.1
/// §18.2.4.4 / §18.2.5): each sees the aliases before it, and only those —
/// with or without ORDER BY, with or without LIMIT, in both executors
/// (review query#6 / sparqlconf#6: without ORDER BY a later expression did
/// not see an earlier alias; with it an earlier one saw later aliases).
#[test]
fn select_expressions_see_earlier_aliases_on_every_path() {
    let data = Data::parse(
        ":a :name \"Alice\" .
         :b :name \"Bob\" .
         :c :name \"Carol\" .
         :d :name \"Dave\" .",
    );
    for q in [
        "SELECT (1 AS ?a) (?a + 1 AS ?b) WHERE {}",
        "SELECT (1 AS ?a) (?a + 1 AS ?b) WHERE {} ORDER BY ?a",
        "SELECT (1 AS ?a) (?a + 1 AS ?b) WHERE {} ORDER BY ?b",
    ] {
        assert_eq!(data.rows_everywhere(q), vec![vec![int(1), int(2)]], "{q}");
    }
    // A later alias is not in scope for an earlier expression.
    for q in [
        "SELECT (?b + 1 AS ?a) (1 AS ?b) WHERE {}",
        "SELECT (?b + 1 AS ?a) (1 AS ?b) WHERE {} ORDER BY ?a",
        "SELECT (?b + 1 AS ?a) (1 AS ?b) WHERE {} ORDER BY ?b",
    ] {
        assert_eq!(data.rows_everywhere(q), vec![vec![None, int(1)]], "{q}");
    }
    let want = vec![
        vec![iri("a"), int(5), int(50)],
        vec![iri("b"), int(3), int(30)],
        vec![iri("c"), int(5), int(50)],
        vec![iri("d"), int(4), int(40)],
    ];
    let chain = "SELECT ?s (STRLEN(?n) AS ?l) (?l * 10 AS ?m) WHERE { ?s :name ?n }";
    assert_eq!(sorted(data.rows_everywhere(chain)), want);
    assert_eq!(data.rows_everywhere(&format!("{chain} ORDER BY ?s")), want);
    assert_eq!(
        data.rows_everywhere(&format!("{chain} ORDER BY DESC(?m) ?s")),
        vec![
            want[0].clone(),
            want[2].clone(),
            want[3].clone(),
            want[1].clone()
        ]
    );
    for opts in [TopKOptions::NONE, HEAP_ONLY, FORCE_DPB, TopKOptions::ALL] {
        let two = data.rows(&format!("{chain} LIMIT 2"), opts);
        assert_eq!(two.len(), 2);
        for row in &two {
            assert!(want.contains(row), "{row:?} ({opts:?})");
        }
        let two = data.rows(&format!("{chain} ORDER BY ?m ?s LIMIT 2"), opts);
        assert_eq!(two, vec![want[1].clone(), want[3].clone()], "{opts:?}");
    }
    // DISTINCT over an alias chain.
    let rows = data.rows_everywhere(
        "SELECT DISTINCT (STRLEN(?n) AS ?l) (?l * 10 AS ?m) WHERE { ?s :name ?n } ORDER BY ?l",
    );
    assert_eq!(
        rows,
        vec![
            vec![int(3), int(30)],
            vec![int(4), int(40)],
            vec![int(5), int(50)]
        ]
    );
}

/// Each SELECT expression is evaluated once per solution, so the projected
/// value is the one ORDER BY sorted on (review query#6: with ORDER BY the
/// projection evaluated every expression a second time — `RAND()` came out
/// unsorted, and a function ran twice per row).
#[test]
fn select_expressions_are_evaluated_once_per_solution() {
    let data = Data::parse(
        ":a :name \"Alice\" .
         :b :name \"Bob\" .
         :c :name \"Carol\" .
         :d :name \"Dave\" .",
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let counting = || {
        let calls = Arc::clone(&calls);
        let mut reg = FunctionRegistry::new();
        reg.register("TICK", move |_| {
            Some(Value::Int(calls.fetch_add(1, Ordering::Relaxed) as i64))
        });
        reg
    };
    for (q, solutions) in [
        ("SELECT ?s (TICK() AS ?t) WHERE { ?s :name ?n }", 4),
        (
            "SELECT ?s (TICK() AS ?t) WHERE { ?s :name ?n } ORDER BY ?s",
            4,
        ),
        (
            "SELECT ?s (TICK() AS ?t) WHERE { ?s :name ?n } ORDER BY DESC(?t)",
            4,
        ),
        (
            "SELECT ?s (TICK() AS ?t) WHERE { ?s :name ?n } ORDER BY ?s LIMIT 2",
            4,
        ),
        ("SELECT ?s (TICK() AS ?t) WHERE { ?s :name ?n } LIMIT 2", 2),
    ] {
        for opts in [TopKOptions::NONE, HEAP_ONLY, TopKOptions::ALL] {
            calls.store(0, Ordering::Relaxed);
            let query = parse(&format!("{PREFIX}{q}")).unwrap();
            let r = data
                .evaluator(opts)
                .with_functions(counting())
                .evaluate(&query);
            let Ok(QueryResult::Select(rs)) = r else {
                panic!("{q}: {r:?}")
            };
            let ran = calls.load(Ordering::Relaxed);
            let expect =
                if opts == TopKOptions::NONE && q.contains("LIMIT 2") && !q.contains("ORDER") {
                    4 // the materializing executor evaluates every solution
                } else {
                    solutions
                };
            assert_eq!(ran, expect, "{q} ({opts:?}): {:?}", rs.rows);
            // ORDER BY DESC(?t): the projected ?t is in descending order.
            if q.contains("DESC(?t)") {
                let ts: Vec<String> = rs.rows.iter().map(|r| r[1].clone().unwrap()).collect();
                let mut want = ts.clone();
                want.sort_by(|a, b| b.cmp(a));
                assert_eq!(ts, want, "{q} ({opts:?})");
            }
        }
    }
    // RAND(): the projected values are the sorted ones.
    for opts in [TopKOptions::NONE, HEAP_ONLY, TopKOptions::ALL] {
        for q in [
            "SELECT ?s (RAND() AS ?r) WHERE { ?s :name ?n } ORDER BY ?r",
            "SELECT ?s (RAND() AS ?r) WHERE { ?s :name ?n } ORDER BY ?r LIMIT 3",
        ] {
            let rows = data.rows(q, opts);
            let rs: Vec<f64> = rows
                .iter()
                .map(|r| {
                    let v = r[1].as_deref().unwrap();
                    v[1..v.find("\"^^").unwrap()].parse().unwrap()
                })
                .collect();
            assert!(
                rs.windows(2).all(|w| w[0] <= w[1]),
                "{q} ({opts:?}): {rs:?}"
            );
        }
    }
}

// ---- SERVICE ---------------------------------------------------------------

/// A one-shot fake SPARQL endpoint: answers every request with `reply`,
/// counting them; stops (and is joined) when dropped.
struct FakeEndpoint {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl FakeEndpoint {
    fn start(reply: &'static str) -> FakeEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let handle = {
            let (stop, hits) = (Arc::clone(&stop), Arc::clone(&hits));
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut conn) = conn else { continue };
                    hits.fetch_add(1, Ordering::SeqCst);
                    // Read the head and the body, then answer and close.
                    let mut reader = BufReader::new(conn.try_clone().unwrap());
                    let mut len = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(v) = lower.strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0u8; len];
                    let _ = reader.read_exact(&mut body);
                    let _ = conn.write_all(reply.as_bytes());
                }
            })
        };
        FakeEndpoint {
            addr,
            stop,
            hits,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/sparql", self.addr)
    }
}

impl Drop for FakeEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// An address nothing listens on (bound, then released).
fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}/sparql")
}

fn service_db() -> Database {
    Database::build_from_str(
        "service_errors",
        "@prefix : <http://ex/> . :alice :knows :bob . :alice :knows :carol .",
    )
    .unwrap()
}

/// The SELECT rows of `q` with `opts`, or the error's text.
fn run_db(
    db: &mut Database,
    q: &str,
    opts: TopKOptions,
) -> Result<Vec<Vec<Option<String>>>, String> {
    db.set_topk_options(opts);
    db.select(q).map(|rs| rs.rows).map_err(|e| e.to_string())
}

/// A non-`SILENT` SERVICE that fails — an HTTP error, a reply that is not
/// SPARQL results JSON, no endpoint listening, a variable endpoint, an
/// inner pattern that cannot be sent — fails the query with an error
/// naming the endpoint and the cause, in a join, under OPTIONAL and in
/// both executors (SPARQL 1.1 Federated Query §4; review robust#6,
/// tests-b#2, sec#17: the query answered as if the endpoint had no data).
#[test]
fn a_failed_service_is_a_query_error_naming_its_cause() {
    let http500 =
        FakeEndpoint::start("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom");
    let not_json = FakeEndpoint::start(
        "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\n\
         Content-Length: 20\r\n\r\n<html>not json</html",
    );
    let dead = dead_url();
    let mut db = service_db();
    let cases: Vec<(String, &str)> = vec![
        (http500.url(), "HTTP 500"),
        (not_json.url(), "JSON"),
        (dead.clone(), ""),
    ];
    for (url, cause) in &cases {
        for shape in [
            "SELECT ?f ?n WHERE { :alice :knows ?f . SERVICE <URL> { ?f :name ?n } }",
            "SELECT ?f ?n WHERE { :alice :knows ?f OPTIONAL { SERVICE <URL> { ?f :name ?n } } }",
            "SELECT * WHERE { SERVICE <URL> { ?s ?p ?o } }",
        ] {
            let q = format!("{PREFIX}{}", shape.replace("URL", url));
            for (opts, q) in [
                (TopKOptions::NONE, q.clone()),
                (TopKOptions::ALL, format!("{q} LIMIT 10")),
            ] {
                let r = run_db(&mut db, &q, opts);
                let Err(e) = r else {
                    panic!("a failed SERVICE answered {r:?}: {q} ({opts:?})")
                };
                assert!(
                    e.contains("SERVICE") && e.contains(url.as_str()) && e.contains(cause),
                    "{e} ({q})"
                );
            }
        }
    }
    assert!(http500.hits.load(Ordering::SeqCst) > 0);
    assert!(not_json.hits.load(Ordering::SeqCst) > 0);
    // A variable endpoint (unsupported) and an inner pattern the request
    // cannot carry: errors too, before any connection.
    let silent_free = [
        (
            format!(
                "{PREFIX}SELECT * WHERE {{ :alice :knows ?f . SERVICE ?ep {{ ?f :name ?n }} }}"
            ),
            "variable",
        ),
        (
            format!(
                "{PREFIX}SELECT * WHERE {{ SERVICE <{}> {{ ?s ?p ?o FILTER(?o != 1) }} }}",
                http500.url()
            ),
            "cannot",
        ),
    ];
    let before = http500.hits.load(Ordering::SeqCst);
    for (q, cause) in &silent_free {
        for opts in [TopKOptions::NONE, TopKOptions::ALL] {
            let r = run_db(&mut db, q, opts);
            let Err(e) = r else {
                panic!("answered {r:?}: {q}")
            };
            assert!(e.contains("SERVICE") && e.contains(cause), "{e} ({q})");
        }
    }
    assert_eq!(http500.hits.load(Ordering::SeqCst), before);
}

/// Once a non-`SILENT` SERVICE has failed the query, no further request is
/// sent: an `EXISTS` evaluated per solution — here in the projection, where
/// no FILTER loop polls the failure — contacts the endpoint once per query,
/// not once per solution (review robust#6 / sec#17: the failure is final,
/// so later requests only repeat it, each up to the client's timeouts).
#[test]
fn a_failed_service_is_contacted_once_per_query() {
    let http500 =
        FakeEndpoint::start("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom");
    let mut db = Database::build_from_str(
        "service_once",
        "@prefix : <http://ex/> . :a :p 1 . :b :p 2 . :c :p 3 . :d :p 4 . :e :p 5 .",
    )
    .unwrap();
    let q = format!(
        "{PREFIX}SELECT ?s (EXISTS {{ SERVICE <{}> {{ ?s :q ?x }} }} AS ?e) WHERE {{ ?s :p ?o }}",
        http500.url()
    );
    for (opts, q) in [
        (TopKOptions::NONE, q.clone()),
        (TopKOptions::ALL, format!("{q} LIMIT 10")),
    ] {
        let before = http500.hits.load(Ordering::SeqCst);
        let r = run_db(&mut db, &q, opts);
        let Err(e) = r else {
            panic!("a failed SERVICE answered {r:?}: {q} ({opts:?})")
        };
        assert!(e.contains("HTTP 500"), "{e} ({q})");
        let sent = http500.hits.load(Ordering::SeqCst) - before;
        assert_eq!(sent, 1, "requests for one query ({opts:?})");
    }
}

/// With `SILENT`, the same failures are the join identity: the outer
/// solutions survive with the service's variables unbound (SPARQL 1.1
/// Federated Query §4 / Query §18.5).
#[test]
fn a_failed_silent_service_is_the_identity() {
    let http500 =
        FakeEndpoint::start("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom");
    let dead = dead_url();
    let mut db = service_db();
    let both = vec![vec![iri("bob"), None], vec![iri("carol"), None]];
    for target in [
        format!("<{}>", http500.url()),
        format!("<{dead}>"),
        "?ep".to_string(),
    ] {
        let q = format!(
            "{PREFIX}SELECT ?f ?n WHERE {{ :alice :knows ?f . SERVICE SILENT {target} {{ ?f :name ?n }} }}"
        );
        for (opts, q) in [
            (TopKOptions::NONE, q.clone()),
            (TopKOptions::ALL, format!("{q} LIMIT 10")),
        ] {
            let rows = run_db(&mut db, &q, opts).unwrap_or_else(|e| panic!("{e}: {q}"));
            assert_eq!(sorted(rows), both, "{q} ({opts:?})");
        }
    }
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
