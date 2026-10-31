//! Micro benchmark: LIMIT / ORDER BY … LIMIT / ASK with and without the
//! Top-K and early-termination optimizations (docs/GAP_PLAN.md §3.4).
//!
//! ```text
//! cargo run --release --example topk_bench [scale]
//! ```
//!
//! `scale` (default 1) multiplies the dataset sizes. Each query runs with
//! every optimization off (full evaluation, then sort and cut), with the
//! streaming executor and bounded heap but no DP-B, with the default options
//! (DP-B when the statistics favour it) and with DP-B forced; the rows are
//! checked to be identical and the times and the evaluator's work counters
//! (default options) are printed.

use std::time::{Duration, Instant};

use gstore::dict::Dictionary;
use gstore::model::{IdTriple, Term};
use gstore::parser::sparql::parse;
use gstore::query::engine::{DpbMode, EvalStats, TopKOptions};
use gstore::query::{Evaluator, QueryResult};
use gstore::store::TripleStore;

const EX: &str = "http://ex/";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

fn iri(local: &str) -> Term {
    Term::iri(format!("{EX}{local}"))
}

fn int(i: u64) -> Term {
    Term::typed_literal(i.to_string(), XSD_INTEGER)
}

struct Data {
    dict: Dictionary,
    store: TripleStore,
}

fn build(triples: Vec<(Term, Term, Term)>) -> Data {
    let mut dict = Dictionary::new();
    let ids: Vec<IdTriple> = triples
        .iter()
        .map(|(s, p, o)| {
            IdTriple::new(
                dict.intern_term(s),
                dict.intern_predicate(&p.dict_key()),
                dict.intern_term(o),
            )
        })
        .collect();
    let mut store = TripleStore::new();
    store.bulk_load(ids);
    Data { dict, store }
}

/// `n` people, each `:knows` a friend with an integer `:score`.
fn people(n: usize) -> Data {
    let mut t = Vec::with_capacity(3 * n);
    let mut k = 1u64;
    for i in 0..n {
        let p = iri(&format!("person{i}"));
        let f = iri(&format!("friend{i}"));
        k = (k * 7919 + 104_729) % 1_000_003;
        t.push((p.clone(), iri("type"), iri("Person")));
        t.push((p, iri("knows"), f.clone()));
        t.push((f, iri("score"), int(k)));
    }
    build(t)
}

/// `hubs` hubs with `fan` neighbours on each of three arms, each neighbour
/// with an integer `:val`: `hubs · fan³` star matches.
fn stars(hubs: usize, fan: usize) -> Data {
    let mut t = Vec::new();
    let mut k = 7u64;
    for h in 0..hubs {
        let hub = iri(&format!("hub{h}"));
        t.push((hub.clone(), iri("type"), iri("Hub")));
        for arm in ["a", "b", "c"] {
            for j in 0..fan {
                let leaf = iri(&format!("{arm}{h}_{j}"));
                k = (k * 7919 + 104_729) % 100_003;
                t.push((hub.clone(), iri(arm), leaf.clone()));
                t.push((leaf, iri("val"), int(k)));
            }
        }
    }
    build(t)
}

fn run(data: &Data, q: &str, opts: TopKOptions) -> (QueryResult, EvalStats, Duration) {
    let query = parse(q).expect("query parses");
    let ev = Evaluator::new(&data.dict, &data.store).with_topk_options(opts);
    let start = Instant::now();
    let r = ev.evaluate(&query).expect("query runs");
    (r, ev.stats(), start.elapsed())
}

fn bench(name: &str, data: &Data, q: &str) {
    let heap_only = TopKOptions {
        dpb: DpbMode::Off,
        ..TopKOptions::ALL
    };
    let forced = TopKOptions {
        dpb: DpbMode::Always,
        ..TopKOptions::ALL
    };
    let (full, full_stats, full_t) = run(data, q, TopKOptions::NONE);
    let (heap, _, heap_t) = run(data, q, heap_only);
    let (all, all_stats, all_t) = run(data, q, TopKOptions::ALL);
    let (dpb, _, dpb_t) = run(data, q, forced);
    assert_eq!(heap, full, "{name}: heap/streaming result differs");
    assert_eq!(all, full, "{name}: optimized result differs");
    assert_eq!(dpb, full, "{name}: forced DP-B result differs");
    println!(
        "{name:<28} full {full_t:>9.2?}  stream/heap {heap_t:>9.2?}  default {all_t:>9.2?}  \
         DP-B forced {dpb_t:>9.2?}   bindings {:>7} -> {:<4} DP-B matches {}",
        full_stats.bgp_bindings, all_stats.bgp_bindings, all_stats.dpb_matches
    );
}

fn main() {
    let scale: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let p = "PREFIX : <http://ex/> ";

    let data = people(100_000 * scale);
    bench(
        "LIMIT 10",
        &data,
        &format!("{p}SELECT ?p ?s WHERE {{ ?p :knows ?f . ?f :score ?s }} LIMIT 10"),
    );
    bench(
        "DISTINCT … LIMIT 10 OFFSET 5",
        &data,
        &format!(
            "{p}SELECT DISTINCT ?p WHERE {{ ?p :type :Person . ?p :knows ?f }} LIMIT 10 OFFSET 5"
        ),
    );
    bench(
        "ASK",
        &data,
        &format!("{p}ASK {{ ?p :knows ?f . ?f :score ?s FILTER(?s > 10) }}"),
    );
    bench(
        "ORDER BY DESC LIMIT 10",
        &data,
        &format!(
            "{p}SELECT ?p ?s WHERE {{ ?p :knows ?f . ?f :score ?s }} ORDER BY DESC(?s) LIMIT 10"
        ),
    );

    let data = stars(20 * scale, 20);
    bench(
        "DP-B star, LIMIT 10",
        &data,
        &format!(
            "{p}SELECT ?h ?x ?y ?z WHERE {{ ?h :type :Hub . ?h :a ?a . ?h :b ?b . ?h :c ?c . \
             ?a :val ?x . ?b :val ?y . ?c :val ?z }} ORDER BY DESC(?x + 2*?y - ?z) LIMIT 10"
        ),
    );

    // Fan-out 5: about four matches per value DP-B reads, the default
    // mode's threshold.
    let data = stars(400 * scale, 5);
    bench(
        "DP-B star fan-out 5",
        &data,
        &format!(
            "{p}SELECT ?h ?x ?y ?z WHERE {{ ?h :type :Hub . ?h :a ?a . ?h :b ?b . ?h :c ?c . \
             ?a :val ?x . ?b :val ?y . ?c :val ?z }} ORDER BY (?x + ?y + ?z) LIMIT 10"
        ),
    );
}
