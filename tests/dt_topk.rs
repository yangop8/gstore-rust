//! Data test: Top-K and early termination (docs/GAP_PLAN.md §3.4).
//!
//! The optimized evaluator (streaming LIMIT / ASK / EXISTS, bounded-heap
//! ORDER BY, DP-B) must return exactly what the full evaluation returns —
//! same rows, same order — for every query. The equivalence tests run many
//! randomized queries over many randomized small datasets three ways:
//! everything off ([`TopKOptions::NONE`]), the heap without DP-B, and
//! everything on. The other tests show the work actually shrinks, through
//! the evaluator's counters ([`EvalStats`]).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gstore::dict::Dictionary;
use gstore::model::{IdTriple, Term};
use gstore::parser::sparql::parse;
use gstore::query::engine::{EvalStats, TopKOptions};
use gstore::query::{Evaluator, FunctionRegistry, QueryResult, Value};
use gstore::store::TripleStore;
use gstore::GStoreError;

const EX: &str = "http://ex/";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const PREFIX: &str = "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

/// The heap without DP-B.
const HEAP_ONLY: TopKOptions = TopKOptions {
    early_termination: true,
    bounded_heap: true,
    dpb: false,
};

// ---- data ----------------------------------------------------------------

/// SplitMix64: a small deterministic PRNG (no dependencies).
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
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'x, T>(&mut self, xs: &'x [T]) -> &'x T {
        &xs[self.below(xs.len())]
    }
}

fn iri(local: &str) -> Term {
    Term::iri(format!("{EX}{local}"))
}

fn int(i: i64) -> Term {
    Term::typed_literal(i.to_string(), format!("{XSD}integer"))
}

/// A dataset: default graph plus named graphs, in id space.
struct Data {
    dict: Dictionary,
    store: TripleStore,
    named: BTreeMap<u32, TripleStore>,
}

impl Data {
    fn new(default: &[(Term, Term, Term)], named: &[(Term, Vec<(Term, Term, Term)>)]) -> Data {
        let mut dict = Dictionary::new();
        let ids = |dict: &mut Dictionary, triples: &[(Term, Term, Term)]| -> Vec<IdTriple> {
            triples
                .iter()
                .map(|(s, p, o)| {
                    IdTriple::new(
                        dict.intern_term(s),
                        dict.intern_predicate(&p.dict_key()),
                        dict.intern_term(o),
                    )
                })
                .collect()
        };
        let mut store = TripleStore::new();
        store.bulk_load(ids(&mut dict, default));
        let mut graphs = BTreeMap::new();
        for (g, triples) in named {
            let gid = dict.intern_term(g);
            let mut gs = TripleStore::new();
            gs.bulk_load(ids(&mut dict, triples));
            graphs.insert(gid, gs);
        }
        Data {
            dict,
            store,
            named: graphs,
        }
    }

    /// Evaluate `q` with `opts`; the result (errors as text) and the work done.
    fn run(&self, q: &str, opts: TopKOptions) -> (Result<QueryResult, String>, EvalStats) {
        let query = parse(q).unwrap_or_else(|e| panic!("parse error {e} in {q}"));
        let ev = Evaluator::new(&self.dict, &self.store)
            .with_named(&self.named)
            .with_topk_options(opts);
        let r = ev.evaluate(&query).map_err(|e| e.to_string());
        (r, ev.stats())
    }

    /// Run `q` fully, with the heap only and fully optimized; all three must
    /// agree. Returns the optimized run's counters.
    fn check(&self, q: &str) -> EvalStats {
        let (full, _) = self.run(q, TopKOptions::NONE);
        let (heap, _) = self.run(q, HEAP_ONLY);
        let (all, stats) = self.run(q, TopKOptions::ALL);
        assert_eq!(heap, full, "bounded heap / streaming differ from full evaluation:\n{q}");
        assert_eq!(all, full, "optimized evaluation differs from full evaluation:\n{q}");
        stats
    }
}

/// Which values the generator puts on `:val` / `:w`.
#[derive(Clone, Copy, PartialEq)]
enum Values {
    /// Small integers (many ties), as several integer datatypes/lexicals.
    Integers,
    /// Mostly integers, plus strings, decimals, doubles, booleans, IRIs,
    /// blank nodes, language-tagged strings and dateTimes.
    Mixed,
}

fn value(rng: &mut Rng, kind: Values) -> Term {
    let i = rng.below(9) as i64 - 4;
    if kind == Values::Integers || rng.chance(60) {
        return match rng.below(6) {
            0 => Term::typed_literal(format!("0{}", i.abs()), format!("{XSD}integer")),
            1 => Term::typed_literal(i.to_string(), format!("{XSD}int")),
            _ => int(i),
        };
    }
    match rng.below(9) {
        0 => Term::plain_literal(format!("s{}", rng.below(4))),
        1 => Term::typed_literal(format!("{i}.5"), format!("{XSD}decimal")),
        2 => Term::typed_literal(format!("{i}.25e0"), format!("{XSD}double")),
        3 => Term::typed_literal(if rng.chance(50) { "true" } else { "false" }, format!("{XSD}boolean")),
        4 => iri(&format!("v{}", rng.below(3))),
        5 => Term::blank(format!("b{}", rng.below(3))),
        6 => Term::Literal {
            value: format!("t{}", rng.below(3)),
            datatype: None,
            lang: Some("en".into()),
        },
        7 => Term::typed_literal(
            format!("2020-01-0{}T00:00:00Z", 1 + rng.below(3)),
            format!("{XSD}dateTime"),
        ),
        _ => Term::typed_literal("x", format!("{EX}custom")),
    }
}

/// A random graph over `n` entities: types, values, and `:p0`..`:p2` links
/// (self-loops included); every predicate occurs at least once.
fn random_triples(rng: &mut Rng, n: usize, kind: Values) -> Vec<(Term, Term, Term)> {
    let e = |i: usize| iri(&format!("e{i}"));
    let mut t = Vec::new();
    for i in 0..n {
        if rng.chance(70) {
            t.push((e(i), iri("type"), iri(&format!("T{}", rng.below(2)))));
        }
        for _ in 0..rng.below(3) {
            t.push((e(i), iri("val"), value(rng, kind)));
        }
        if rng.chance(40) {
            t.push((e(i), iri("w"), value(rng, kind)));
        }
    }
    for _ in 0..(2 * n) {
        let p = format!("p{}", rng.below(3));
        t.push((e(rng.below(n)), iri(&p), e(rng.below(n))));
    }
    for p in ["p0", "p1", "p2", "type"] {
        t.push((e(0), iri(p), e(1 % n)));
    }
    t.push((e(1 % n), iri("val"), int(1)));
    t.push((e(0), iri("w"), int(2)));
    t.push((e(n - 1), iri("type"), iri("T0")));
    t.push((e(n / 2), iri("type"), iri("T1")));
    t
}

fn random_data(rng: &mut Rng, n: usize, kind: Values) -> Data {
    let default = random_triples(rng, n, kind);
    let named = vec![
        (iri("g1"), random_triples(rng, n / 2 + 1, kind)),
        (iri("g2"), random_triples(rng, n / 2 + 1, kind)),
    ];
    Data::new(&default, &named)
}

/// ` LIMIT n` with an optional ` OFFSET m` (sometimes OFFSET first).
fn slice(rng: &mut Rng) -> String {
    let limit = *rng.pick(&[0usize, 1, 1, 2, 3, 3, 5, 8, 20, 1000]);
    match rng.below(4) {
        0 => format!(" LIMIT {limit}"),
        1 => format!(" LIMIT {limit} OFFSET {}", rng.pick(&[0usize, 1, 2, 4, 50])),
        2 => format!(" OFFSET {} LIMIT {limit}", rng.pick(&[1usize, 3])),
        _ => format!(" LIMIT {limit}"),
    }
}

fn dir(rng: &mut Rng, key: &str) -> String {
    match rng.below(3) {
        0 => format!("DESC({key})"),
        1 => format!("ASC({key})"),
        _ => key.to_string(),
    }
}

/// A random SELECT / ASK / CONSTRUCT over the generator's vocabulary.
fn random_query(rng: &mut Rng) -> String {
    let wheres = [
        "{ ?a :p0 ?b . ?b :p1 ?c }",
        "{ ?a :p0 ?b . ?a :val ?x . ?b :val ?y }",
        "{ ?a :type ?t . OPTIONAL { ?a :val ?x } }",
        "{ ?a :type ?t . OPTIONAL { ?a :val ?x FILTER(?x != 0) } OPTIONAL { ?a :w ?y } }",
        "{ { ?a :p0 ?b } UNION { ?a :p1 ?b } ?b :val ?x }",
        "{ { ?a :p0 ?b } UNION { ?b :p2 ?a . ?a :val ?x } }",
        "{ ?a :val ?x . FILTER(?x > 0) }",
        "{ ?a :p0 ?b . ?b :val ?x . FILTER(?a != ?b) }",
        "{ ?a :val ?x . BIND(?x * 2 AS ?y) }",
        "{ ?a :type ?t . BIND(STR(?a) AS ?y) ?a :p0 ?b }",
        "{ VALUES ?t { :T0 :T1 } ?a :type ?t . ?a :val ?x }",
        "{ ?a :p0 ?b . VALUES (?b ?y) { (:e1 1) (:e2 UNDEF) (:e3 3) } }",
        "{ ?a :p0 ?b . MINUS { ?a :type :T1 } }",
        "{ ?a :val ?x . MINUS { ?a :p1 ?z } }",
        "{ ?a :p0 ?b . FILTER EXISTS { ?b :val ?z } }",
        "{ ?a :type ?t . FILTER NOT EXISTS { ?a :p2 ?z } }",
        "{ { SELECT ?a ?x WHERE { ?a :val ?x } ORDER BY DESC(?x) ?a LIMIT 4 } ?a :p0 ?b }",
        "{ { SELECT DISTINCT ?a WHERE { ?a :p0 ?z } LIMIT 3 } OPTIONAL { ?a :val ?x } }",
        "{ ?a :p0+ ?b . ?b :val ?x }",
        "{ ?a :p0/:p1 ?b }",
        "{ GRAPH ?g { ?a :p0 ?b . ?b :val ?x } }",
        "{ GRAPH :g1 { ?a :val ?x } ?a :type ?t }",
        "{ ?a :val ?x . ?a :w ?y }",
        "{ ?a ?p ?b . ?b :val ?x }",
        "{ ?a :p0 ?b . ?b :p1 ?c . ?c :p2 ?a }",
        "{ ?a :val ?x . OPTIONAL { ?a :p0 ?b . ?b :val ?y } FILTER(!BOUND(?y) || ?y >= ?x) }",
        "{ ?a :p0 ?b . ?c :p1 ?d }",
        "{ }",
        "{ ?a :nope ?b }",
    ];
    let w = rng.pick(&wheres);
    let mut vars: Vec<&str> = ["?a", "?b", "?c", "?t", "?x", "?y", "?g", "?p"]
        .into_iter()
        .filter(|v| w.contains(v))
        .collect();
    if vars.is_empty() {
        vars.push("?a");
    }
    if w.contains('+') || w.contains('/') {
        // Property paths are evaluated through hash sets, so the order of
        // their solutions is not deterministic (two full evaluations can
        // differ): only a total ORDER BY makes such a query comparable.
        if rng.chance(20) {
            return format!("{PREFIX}ASK {w}");
        }
        let distinct = if rng.chance(30) { "DISTINCT " } else { "" };
        let keys: Vec<String> = vars.iter().map(|v| dir(rng, v)).collect();
        return format!(
            "{PREFIX}SELECT {distinct}* WHERE {w} ORDER BY {}{}",
            keys.join(" "),
            slice(rng)
        );
    }
    match rng.below(20) {
        0 => return format!("{PREFIX}ASK {w}"),
        1 => {
            let o = if rng.chance(50) {
                format!(" ORDER BY {}", dir(rng, vars.first().unwrap_or(&"?a")))
            } else {
                String::new()
            };
            return format!(
                "{PREFIX}CONSTRUCT {{ ?a :q ?x . ?a :r ?b }} WHERE {w}{o}{}",
                slice(rng)
            );
        }
        _ => {}
    }
    let distinct = if rng.chance(30) { "DISTINCT " } else { "" };
    let proj = match rng.below(4) {
        0 => "*".to_string(),
        1 if vars.len() >= 2 => format!("{} {}", vars[0], vars[vars.len() - 1]),
        2 if vars.contains(&"?x") => "?a (?x + 1 AS ?s) (STR(?a) AS ?n)".to_string(),
        _ => vars.first().unwrap_or(&"?a").to_string(),
    };
    let order = match rng.below(5) {
        0 => String::new(),
        1 => {
            let v = rng.pick(&vars).to_string();
            format!(" ORDER BY {}", dir(rng, &v))
        }
        2 => {
            let mut keys = Vec::new();
            for _ in 0..1 + rng.below(3) {
                let v = rng.pick(&vars).to_string();
                keys.push(dir(rng, &v));
            }
            format!(" ORDER BY {}", keys.join(" "))
        }
        3 if proj.contains("?s)") => format!(" ORDER BY {} ?a", dir(rng, "?s")),
        _ => format!(" ORDER BY {} {}", dir(rng, "STR(?a)"), dir(rng, "?x")),
    };
    let values = if rng.chance(10) {
        " VALUES ?t { :T0 }"
    } else {
        ""
    };
    format!("{PREFIX}SELECT {distinct}{proj} WHERE {w}{order}{}{values}", slice(rng))
}

// ---- equivalence ---------------------------------------------------------

/// `TOPK_SCALE=n` runs the randomized tests `n` times longer.
fn scale() -> usize {
    std::env::var("TOPK_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

/// `TOPK_SEED=n` changes the randomized tests' data and queries.
fn seed(base: u64) -> u64 {
    base ^ std::env::var("TOPK_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn randomized_queries_match_full_evaluation() {
    let mut rng = Rng(seed(7));
    let mut total = EvalStats::default();
    for round in 0..40 * scale() {
        let kind = if round % 3 == 0 {
            Values::Integers
        } else {
            Values::Mixed
        };
        let n = 6 + rng.below(14);
        let data = random_data(&mut rng, n, kind);
        for _ in 0..40 {
            let q = random_query(&mut rng);
            let s = data.check(&q);
            total.streamed += s.streamed;
            total.heap_topk += s.heap_topk;
            total.dpb_topk += s.dpb_topk;
        }
    }
    // Every path ran many times.
    assert!(total.streamed > 200, "{total:?}");
    assert!(total.heap_topk > 200, "{total:?}");
    assert!(total.dpb_topk > 5, "{total:?}");
}

/// Shapes DP-B handles over integer data: each query must run through DP-B
/// and agree with the full evaluation, ties and offsets included.
const DPB_SHAPES: &[&str] = &[
    // star around ?c
    "?c :p0 ?a . ?c :p1 ?b . ?a :val ?x . ?b :val ?y",
    // chain, with the edge directions mixed
    "?a :p0 ?b . ?c :p1 ?b . ?a :val ?x . ?c :val ?y",
    // a constant restriction and a reversed edge
    "?a :type :T0 . ?b :p0 ?a . ?a :val ?x . ?b :val ?y",
    // two patterns on one tree edge
    "?a :p0 ?b . ?a :p2 ?b . ?a :val ?x . ?b :val ?y",
    // a predicate variable on a tree edge
    "?a ?p ?b . ?a :val ?x . ?b :val ?y",
    // a self-loop restriction
    "?a :p1 ?a . ?a :val ?x . ?a :p0 ?b . ?b :val ?y",
    // one scored variable (the other is a plain node)
    "?a :p0 ?b . ?b :w ?y . ?a :val ?x",
];

const DPB_KEYS: &[&str] = &[
    "?x + ?y",
    "?x + 2*?y",
    "2*?x - 3*?y + 5",
    "(?x - ?y) * 2",
    "-?x",
    "?y",
    "?x * -1 + 0*?y",
    "+?x - -?y",
];

fn dpb_query(rng: &mut Rng, shape: &str, key: &str) -> String {
    let filter = match rng.below(4) {
        0 => " FILTER(?x != 1)",
        1 => " FILTER(?a != ?b)",
        _ => "",
    };
    let (distinct, proj) = match rng.below(5) {
        0 => ("DISTINCT ", "?a".to_string()),
        1 => ("DISTINCT ", "?x ?y".to_string()),
        2 => ("", format!("?a ?b (({key}) AS ?score)")),
        _ => ("", "*".to_string()),
    };
    let mut order = dir(rng, &format!("({key})"));
    if rng.chance(40) {
        let extra = *rng.pick(&["?a", "?b", "STR(?x)"]);
        order += &format!(" {}", dir(rng, extra));
    }
    format!(
        "{PREFIX}SELECT {distinct}{proj} WHERE {{ {shape}{filter} }} ORDER BY {order}{}",
        slice(rng)
    )
}

#[test]
fn dpb_matches_full_evaluation_on_tree_shaped_bgps() {
    let mut rng = Rng(seed(11));
    let mut used = 0;
    let mut runs = 0;
    for _ in 0..30 * scale() {
        let n = 5 + rng.below(12);
        let data = random_data(&mut rng, n, Values::Integers);
        for _ in 0..25 {
            let shape = *rng.pick(DPB_SHAPES);
            let key = *rng.pick(DPB_KEYS);
            let q = dpb_query(&mut rng, shape, key);
            let s = data.check(&q);
            runs += 1;
            // LIMIT 0 is answered before choosing a strategy.
            if !q.contains("LIMIT 0") {
                assert_eq!(s.dpb_topk, 1, "DP-B was not used for:\n{q}");
                used += 1;
            }
        }
    }
    assert!(used > runs / 2, "{used} of {runs}");
}

#[test]
fn dpb_falls_back_when_values_are_not_integers() {
    // The same shapes over mixed values: whenever a non-integer value can
    // reach the key, the heap answers instead — with the same rows.
    let mut rng = Rng(seed(13));
    let (mut dpb, mut heap) = (0, 0);
    for _ in 0..25 * scale() {
        let n = 5 + rng.below(12);
        let data = random_data(&mut rng, n, Values::Mixed);
        for _ in 0..20 {
            let shape = *rng.pick(DPB_SHAPES);
            let key = *rng.pick(DPB_KEYS);
            let s = data.check(&dpb_query(&mut rng, shape, key));
            dpb += s.dpb_topk;
            heap += s.heap_topk;
        }
    }
    assert!(heap > 50, "heap fallback used {heap} times");
    assert!(dpb > 0, "DP-B still used where the reachable values are integers");
}

#[test]
fn dpb_declines_shapes_it_does_not_handle() {
    let mut rng = Rng(17);
    let data = random_data(&mut rng, 12, Values::Integers);
    for q in [
        // a cycle
        "SELECT * WHERE { ?a :p0 ?b . ?b :p1 ?c . ?c :p2 ?a . ?a :val ?x . ?c :val ?y } ORDER BY (?x + ?y) LIMIT 3",
        // disconnected
        "SELECT * WHERE { ?a :val ?x . ?b :val ?y } ORDER BY (?x + ?y) LIMIT 3",
        // a non-linear key
        "SELECT * WHERE { ?a :p0 ?b . ?a :val ?x . ?b :val ?y } ORDER BY (?x * ?y) LIMIT 3",
        // a decimal coefficient
        "SELECT * WHERE { ?a :p0 ?b . ?a :val ?x . ?b :val ?y } ORDER BY (?x + 0.5 * ?y) LIMIT 3",
        // a key variable outside the BGP
        "SELECT * WHERE { ?a :val ?x . BIND(?x AS ?y) } ORDER BY (?x + ?y) LIMIT 3",
        // a predicate variable used twice
        "SELECT * WHERE { ?a ?p ?b . ?b ?p ?c . ?a :val ?x } ORDER BY ?x LIMIT 3",
        // OPTIONAL is not a BGP
        "SELECT * WHERE { ?a :val ?x OPTIONAL { ?a :w ?y } } ORDER BY ?x LIMIT 3",
        // a constant key
        "SELECT * WHERE { ?a :val ?x } ORDER BY (0 * ?x) LIMIT 3",
    ] {
        let q = format!("{PREFIX}{q}");
        let s = data.check(&q);
        assert_eq!(s.dpb_topk, 0, "{q}");
        assert_eq!(s.heap_topk, 1, "{q}");
    }
}

#[test]
fn order_by_ties_unbound_and_mixed_types_keep_the_full_sort_order() {
    // Every kind of term on one key, with duplicates: SPARQL's order
    // (unbound < blank < IRI < literals) and the stable order of ties.
    let mut t = Vec::new();
    let vals = [
        int(3),
        int(-1),
        int(3),
        Term::plain_literal("a"),
        Term::blank("z"),
        iri("v"),
        Term::typed_literal("2.5", format!("{XSD}decimal")),
        Term::typed_literal("NaN", format!("{XSD}double")),
        Term::typed_literal("true", format!("{XSD}boolean")),
        Term::plain_literal("a"),
        int(3),
    ];
    for (i, v) in vals.iter().enumerate() {
        t.push((iri(&format!("e{i}")), iri("val"), v.clone()));
        t.push((iri(&format!("e{i}")), iri("type"), iri("T")));
    }
    t.push((iri("e20"), iri("type"), iri("T"))); // no :val → unbound key
    t.push((iri("e21"), iri("type"), iri("T")));
    let data = Data::new(&t, &[]);
    for order in ["?v", "DESC(?v)", "?v DESC(?e)", "DESC(?v) ?e", "STR(?v)", "(?v + 1)"] {
        for slice in ["LIMIT 1", "LIMIT 3", "LIMIT 4 OFFSET 2", "LIMIT 100", "OFFSET 11 LIMIT 5"] {
            for distinct in ["", "DISTINCT "] {
                data.check(&format!(
                    "{PREFIX}SELECT {distinct}?v WHERE {{ ?e :type :T OPTIONAL {{ ?e :val ?v }} }} \
                     ORDER BY {order} {slice}"
                ));
                data.check(&format!(
                    "{PREFIX}SELECT {distinct}?e ?v WHERE {{ ?e :type :T OPTIONAL {{ ?e :val ?v }} }} \
                     ORDER BY {order} {slice}"
                ));
            }
        }
    }
}

// ---- early termination ---------------------------------------------------

/// `n` chains `:sI :p :oI . :oI :q vJ` (each `:oI` with `fan` values).
fn chains(n: usize, fan: usize) -> Data {
    let mut t = Vec::new();
    for i in 0..n {
        t.push((iri(&format!("s{i}")), iri("p"), iri(&format!("o{i}"))));
        t.push((iri(&format!("s{i}")), iri("type"), iri("S")));
        for j in 0..fan {
            t.push((iri(&format!("o{i}")), iri("q"), int((i * fan + j) as i64)));
        }
    }
    Data::new(&t, &[])
}

/// Run `q` fully and optimized; check they agree; return both counters.
fn compare(data: &Data, q: &str) -> (EvalStats, EvalStats) {
    let q = format!("{PREFIX}{q}");
    let (full, full_stats) = data.run(&q, TopKOptions::NONE);
    let (opt, opt_stats) = data.run(&q, TopKOptions::ALL);
    assert_eq!(opt, full, "{q}");
    (full_stats, opt_stats)
}

#[test]
fn limit_stops_the_join_early() {
    let data = chains(5000, 2);
    let (full, opt) = compare(&data, "SELECT * WHERE { ?s :p ?o . ?o :q ?v } LIMIT 3");
    assert!(full.bgp_bindings >= 10_000, "{full:?}");
    assert!(opt.bgp_bindings <= 10, "{opt:?}");
    assert_eq!(opt.streamed, 1);

    let (full, opt) = compare(&data, "SELECT * WHERE { ?s :p ?o . ?o :q ?v } LIMIT 2 OFFSET 7");
    assert!(full.bgp_bindings >= 10_000);
    assert!(opt.bgp_bindings <= 30, "{opt:?}");

    // DISTINCT counts distinct rows: one ?s per two solutions.
    let (_, opt) = compare(&data, "SELECT DISTINCT ?s WHERE { ?s :p ?o . ?o :q ?v } LIMIT 4");
    assert!(opt.bgp_bindings <= 20, "{opt:?}");
}

#[test]
fn ask_stops_at_the_first_solution() {
    let data = chains(5000, 2);
    let (full, opt) = compare(&data, "ASK { ?s :p ?o . ?o :q ?v . FILTER(?v >= 0) }");
    assert!(full.bgp_bindings >= 10_000);
    assert!(opt.bgp_bindings <= 4, "{opt:?}");
    let (_, opt) = compare(&data, "ASK { ?s :p ?o . ?o :q ?v . FILTER(?v < 0) }");
    assert!(opt.bgp_bindings >= 10_000, "no solution: everything is tried");
}

#[test]
fn exists_stops_at_the_first_solution() {
    let data = chains(300, 40);
    let (full, opt) =
        compare(&data, "SELECT ?s WHERE { ?s :p ?o FILTER EXISTS { ?o :q ?v } }");
    // Full: every EXISTS enumerates all 40 values; optimized: one each.
    assert!(full.bgp_bindings >= 300 * 40, "{full:?}");
    assert!(opt.bgp_bindings <= 300 * 3, "{opt:?}");
}

#[test]
fn filter_bind_union_optional_minus_stream() {
    let data = chains(4000, 2);
    for (q, max) in [
        ("SELECT * WHERE { ?s :p ?o . ?o :q ?v FILTER(?v > 100) } LIMIT 2", 400),
        ("SELECT * WHERE { ?s :p ?o . ?o :q ?v BIND(?v + 1 AS ?w) } LIMIT 2", 10),
        ("SELECT * WHERE { { ?s :p ?o } UNION { ?o :q ?v } } LIMIT 3", 10),
        ("SELECT * WHERE { ?o :q ?v } LIMIT 2 OFFSET 2", 10),
        ("SELECT ?s WHERE { GRAPH ?g { ?s :p ?o } } LIMIT 1", 10),
        // the OPTIONAL side is evaluated once, in full (8000 values)
        ("SELECT * WHERE { ?s :p ?o OPTIONAL { ?o :q ?v } } LIMIT 2", 8000 + 10),
        // the MINUS side too (4000 types)
        ("SELECT * WHERE { ?s :p ?o MINUS { ?s :type :Nothing } } LIMIT 2", 10),
        ("SELECT * WHERE { ?s :p ?o MINUS { ?o :q 5 } } LIMIT 2", 10),
    ] {
        let (full, opt) = compare(&data, q);
        assert!(opt.bgp_bindings <= max, "{q}: {opt:?} (full: {full:?})");
        assert!(full.bgp_bindings > opt.bgp_bindings || full.bgp_bindings <= 10, "{q}");
    }
}

#[test]
fn construct_with_limit_streams_and_ranks() {
    let data = chains(3000, 2);
    let (full, opt) = compare(&data, "CONSTRUCT { ?s :r ?v } WHERE { ?s :p ?o . ?o :q ?v } LIMIT 3");
    assert!(full.bgp_bindings >= 6000);
    assert!(opt.bgp_bindings <= 10, "{opt:?}");
    let (_, opt) = compare(
        &data,
        "CONSTRUCT { ?s :r ?v } WHERE { ?s :p ?o . ?o :q ?v } ORDER BY DESC(?v) LIMIT 3 OFFSET 1",
    );
    assert_eq!(opt.heap_topk, 1);
}

#[test]
fn graph_function_limit_pushdown_is_kept() {
    // Upstream's single-BGP LIMIT pushdown for graph functions: the function
    // sees only the first LIMIT solutions, now without computing the rest.
    let data = chains(2000, 1);
    let (full, opt) = compare(
        &data,
        "SELECT ?s (kHopReachable(?s, :o5, true, 1, {}) AS ?r) WHERE { ?s :p ?o } LIMIT 3",
    );
    assert!(full.bgp_bindings >= 2000);
    assert!(opt.bgp_bindings <= 5, "{opt:?}");
}

// ---- top-k -----------------------------------------------------------------

#[test]
fn order_by_limit_uses_a_bounded_heap() {
    let data = chains(3000, 2);
    for q in [
        "SELECT ?s ?v WHERE { ?s :p ?o . ?o :q ?v } ORDER BY DESC(?v) LIMIT 5",
        "SELECT ?s ?v WHERE { ?s :p ?o . ?o :q ?v } ORDER BY ?s DESC(?v) LIMIT 5 OFFSET 10",
        "SELECT DISTINCT ?s WHERE { ?s :p ?o . ?o :q ?v } ORDER BY DESC(?v) LIMIT 5",
        "SELECT ?s ((?v * 2) AS ?d) WHERE { ?s :p ?o . ?o :q ?v FILTER(?v > 10) } ORDER BY ?d LIMIT 5",
    ] {
        let (_, opt) = compare(&data, q);
        assert_eq!(opt.heap_topk + opt.dpb_topk, 1, "{q}");
    }
}

/// `centers` hubs, each with `fan` `:a`, `:b` and `:c` neighbours carrying
/// integer `:val`s: `centers · fan³` matches of the star query.
fn stars(centers: usize, fan: usize) -> Data {
    let mut t = Vec::new();
    let mut k = 0i64;
    for c in 0..centers {
        let hub = iri(&format!("h{c}"));
        t.push((hub.clone(), iri("type"), iri("Hub")));
        for arm in ["a", "b", "c"] {
            for j in 0..fan {
                let leaf = iri(&format!("{arm}{c}_{j}"));
                t.push((hub.clone(), iri(arm), leaf.clone()));
                k = (k * 7919 + 13) % 1000;
                t.push((leaf, iri("val"), int(k)));
            }
        }
    }
    Data::new(&t, &[])
}

#[test]
fn dpb_enumerates_only_the_best_matches() {
    let data = stars(20, 12); // 20 · 12³ = 34560 matches
    let q = "SELECT ?h ?x ?y ?z WHERE { ?h :type :Hub . ?h :a ?a . ?h :b ?b . ?h :c ?c . \
             ?a :val ?x . ?b :val ?y . ?c :val ?z } ORDER BY DESC(?x + 2*?y - ?z) ?h LIMIT 10";
    let (full, opt) = compare(&data, q);
    assert!(full.bgp_bindings >= 34_560, "{full:?}");
    assert_eq!(opt.dpb_topk, 1, "{opt:?}");
    assert_eq!(opt.bgp_bindings, 0, "DP-B does not run the joins: {opt:?}");
    assert!(opt.dpb_matches <= 40, "{opt:?}");
}

// ---- cancellation ----------------------------------------------------------

/// A function `STOP()` that raises the evaluator's cancel flag and is true.
fn cancelling(data: &Data, q: &str, opts: TopKOptions) -> Result<QueryResult, GStoreError> {
    let flag = Arc::new(AtomicBool::new(false));
    let raise = Arc::clone(&flag);
    let mut reg = FunctionRegistry::new();
    reg.register("STOP", move |_| {
        raise.store(true, Ordering::Relaxed);
        Some(Value::Bool(true))
    });
    let q = parse(&format!("{PREFIX}{q}")).unwrap();
    Evaluator::new(&data.dict, &data.store)
        .with_functions(reg)
        .with_cancel(flag)
        .with_topk_options(opts)
        .evaluate(&q)
}

#[test]
fn cancellation_is_reported_on_every_path() {
    let data = stars(4, 4);
    for q in [
        "SELECT * WHERE { ?h :a ?a . ?a :val ?x FILTER(STOP()) } LIMIT 5",
        "SELECT * WHERE { ?h :a ?a . ?a :val ?x FILTER(STOP()) } ORDER BY ?a LIMIT 5",
        "SELECT * WHERE { ?h :a ?a . ?a :val ?x FILTER(STOP()) } ORDER BY DESC(?x) LIMIT 5",
        "SELECT DISTINCT ?h WHERE { ?h :a ?a . ?a :val ?x FILTER(STOP()) } ORDER BY ?x LIMIT 5",
        "ASK { ?h :a ?a . ?a :val ?x FILTER(STOP() && ?x < 0) }",
        "CONSTRUCT { ?h :r ?x } WHERE { ?h :a ?a . ?a :val ?x FILTER(STOP()) } LIMIT 2",
    ] {
        for opts in [TopKOptions::ALL, HEAP_ONLY, TopKOptions::NONE] {
            let r = cancelling(&data, q, opts);
            assert!(matches!(r, Err(GStoreError::Cancelled)), "{q}: {r:?}");
        }
    }
}
