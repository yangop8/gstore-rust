//! Exactness of [`VsPolicy::Auto`]: on a VS-tree consistent with the store it
//! computes, for every BGP, the very candidate sets [`VsPolicy::Tree`]
//! computes (so the plans and the order of solutions are the same), and on an
//! out-of-core tree it never reads a page of the tree. After deletes (the
//! tree then holds stale bits) an in-memory tree still gives equal sets and an
//! out-of-core one gives subsets of `Tree`'s that still hold every answer.
//!
//! A child module of the engine, so it can look at the candidate sets
//! `prepare_bgp` hands the planner.

use super::*;
use crate::dict::Dictionary;
use crate::model::IdTriple;
use crate::parser::sparql::ast::{GraphPattern, PatternTerm, TriplePattern};
use crate::query::vs_policy::{reset_vs_stats, vs_stats, VsPolicy, VsStats};
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::TripleStore;

/// xorshift64*: deterministic test data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const ENTITIES: usize = 70;
const LITERALS: usize = 15;
/// Enough predicates that some share a signature bit: the encoding gives
/// ids below 100 distinct bits per direction, `p` and `p + 100` the same.
const PREDICATES: usize = 130;

fn entity(i: usize) -> Term {
    Term::iri(format!("http://ex/e{i}"))
}
fn literal(i: usize) -> Term {
    Term::plain_literal(format!("l{i}"))
}
fn predicate(i: usize) -> Term {
    Term::iri(format!("http://ex/p{i}"))
}

/// A random graph: skewed predicates (low ids common), a quarter literal
/// objects, a few entities and predicates without any triple.
fn graph(seed: u64) -> (Dictionary, TripleStore) {
    let mut rng = Rng(seed | 1);
    let mut d = Dictionary::new();
    let es: Vec<u32> = (0..ENTITIES).map(|i| d.intern_term(&entity(i))).collect();
    let ls: Vec<u32> = (0..LITERALS).map(|i| d.intern_term(&literal(i))).collect();
    let ps: Vec<u32> = (0..PREDICATES)
        .map(|i| d.intern_predicate(&predicate(i).dict_key()))
        .collect();
    let mut triples = Vec::new();
    for _ in 0..900 {
        // The last 5 entities and the last 2 predicates stay unused.
        let s = es[rng.below(ENTITIES - 5)];
        let p = if rng.chance(50) {
            ps[rng.below(PREDICATES - 2)]
        } else {
            ps[rng.below(12)]
        };
        let o = if rng.chance(25) {
            ls[rng.below(LITERALS)]
        } else {
            es[rng.below(ENTITIES - 5)]
        };
        triples.push(IdTriple::new(s, p, o));
    }
    let mut store = TripleStore::new();
    store.bulk_load(triples);
    (d, store)
}

/// The VS-tree entries of `store`, as the database signs them.
fn entries(store: &TripleStore) -> Vec<(u32, Signature)> {
    let mut out = Vec::new();
    store.for_each_entity_adjacency(|e, outgoing, incoming| {
        let mut sig = Signature::new();
        for &(p, o) in outgoing {
            sig.encode_edge(p, o, EdgeDir::Out);
        }
        for &(p, s) in incoming {
            sig.encode_edge(p, s, EdgeDir::In);
        }
        out.push((e, sig));
    });
    out
}

/// A temporary directory, removed on drop (also when a test panics).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("gstore_vs_policy_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A random BGP over the variables `?a`–`?d` (predicate variables are
/// separate): constants are drawn from the whole dictionary, unused terms
/// included, and now and then a term the dictionary lacks.
fn bgp(rng: &mut Rng) -> Vec<TriplePattern> {
    let node_var = |rng: &mut Rng| PatternTerm::Var(["a", "b", "c", "d"][rng.below(4)].into());
    let n = 1 + rng.below(4);
    (0..n)
        .map(|_| {
            let subject = if rng.chance(75) {
                node_var(rng)
            } else {
                PatternTerm::Term(entity(rng.below(ENTITIES)))
            };
            let predicate = if rng.chance(80) {
                PatternTerm::Term(predicate(rng.below(PREDICATES)))
            } else if rng.chance(3) {
                PatternTerm::Term(predicate(PREDICATES + 1)) // not in the dictionary
            } else {
                PatternTerm::Var(["p", "q"][rng.below(2)].into())
            };
            let object = match rng.below(20) {
                0..=9 => node_var(rng),
                10..=16 => PatternTerm::Term(entity(rng.below(ENTITIES))),
                _ => PatternTerm::Term(literal(rng.below(LITERALS))),
            };
            TriplePattern {
                subject,
                predicate,
                object,
            }
        })
        .collect()
}

/// The candidate sets `prepare_bgp` hands the planner under `policy` (`None`
/// when the BGP is empty or trivial), with the decisions it counted.
fn candidates_under(
    d: &Dictionary,
    store: &TripleStore,
    tree: &VsTree,
    tps: &[TriplePattern],
    policy: VsPolicy,
) -> (Option<Candidates>, VsStats) {
    let layout = VarLayout::build(&GraphPattern::Bgp(tps.to_vec()));
    let ev = Evaluator::with_vstree(d, store, tree).with_vs_policy(policy);
    reset_vs_stats();
    let cands = match ev.prepare_bgp(tps, &layout, &HashMap::new()) {
        PreparedBgp::Plan(p) => Some(p.cands),
        _ => None,
    };
    (cands, vs_stats())
}

/// The rows of `SELECT * WHERE { tps }` under `policy`, in solution order.
fn rows_under(
    d: &Dictionary,
    store: &TripleStore,
    tree: &VsTree,
    tps: &[TriplePattern],
    policy: VsPolicy,
) -> Vec<Vec<Option<String>>> {
    let q = crate::parser::sparql::parse(&select_text(tps)).unwrap();
    let ev = Evaluator::with_vstree(d, store, tree).with_vs_policy(policy);
    match ev.evaluate(&q).unwrap() {
        QueryResult::Select(rs) => rs.rows,
        other => panic!("not a SELECT result: {other:?}"),
    }
}

fn select_text(tps: &[TriplePattern]) -> String {
    let term = |t: &PatternTerm| match t {
        PatternTerm::Var(v) => format!("?{v}"),
        PatternTerm::Term(t) => t.to_string(),
    };
    let body: Vec<String> = tps
        .iter()
        .map(|tp| {
            format!(
                "{} {} {} .",
                term(&tp.subject),
                term(&tp.predicate),
                term(&tp.object)
            )
        })
        .collect();
    format!("SELECT * WHERE {{ {} }}", body.join(" "))
}

fn sorted(mut rows: Vec<Vec<Option<String>>>) -> Vec<Vec<Option<String>>> {
    rows.sort();
    rows
}

#[test]
fn the_data_has_predicates_sharing_a_bit() {
    // Otherwise the bit classes below would all be single predicates.
    let (d, _) = graph(1);
    let ids: Vec<u32> = (0..PREDICATES)
        .map(|i| d.predicate_id(&predicate(i).dict_key()).unwrap())
        .collect();
    let shared = ids.iter().any(|&p| {
        ids.iter().any(|&q| {
            q != p
                && Signature::predicate_bit(p, EdgeDir::Out)
                    == Signature::predicate_bit(q, EdgeDir::Out)
        })
    });
    assert!(shared);
}

#[test]
fn auto_computes_the_trees_sets_and_never_reads_a_disk_tree() {
    for seed in 1..=4u64 {
        let (d, store) = graph(seed);
        let tmp = TempDir::new("exact");
        let mem = VsTree::build(entries(&store));
        let disk = VsTree::build_disk(tmp.0.join("vstree.kv"), entries(&store)).unwrap();
        let mut rng = Rng(seed * 7919 + 13);
        let mut decisions = VsStats::default();
        for i in 0..150 {
            let tps = bgp(&mut rng);
            for (kind, tree) in [("memory", &mem), ("disk", &disk)] {
                let (want, _) = candidates_under(&d, &store, tree, &tps, VsPolicy::Tree);
                // Both ways of testing a bit class give the same sets.
                for forced in [Some(true), Some(false)] {
                    FORCE_LISTS.with(|f| f.set(forced));
                    let (got, _) = candidates_under(&d, &store, tree, &tps, VsPolicy::Auto);
                    FORCE_LISTS.with(|f| f.set(None));
                    assert_eq!(got, want, "seed {seed} bgp {i} ({kind}, lists {forced:?})");
                }
                let pages = tree.disk_pages_read();
                let (got, st) = candidates_under(&d, &store, tree, &tps, VsPolicy::Auto);
                assert_eq!(
                    got,
                    want,
                    "seed {seed} bgp {i} ({kind}): {}",
                    select_text(&tps)
                );
                if kind == "disk" {
                    assert_eq!(tree.disk_pages_read(), pages, "Auto read the disk tree");
                    assert_eq!(st.tree_reads(), 0);
                    decisions.skipped += st.skipped;
                    decisions.store_checked += st.store_checked;
                    decisions.listed += st.listed;
                }
                // Deterministic: the same decisions again.
                let (again, st2) = candidates_under(&d, &store, tree, &tps, VsPolicy::Auto);
                assert_eq!((again, st2), (got, st));
            }
            if i % 5 == 0 {
                // Same plans, so the same solutions in the same order; and
                // the same answers without the tree.
                for tree in [&mem, &disk] {
                    let want = rows_under(&d, &store, tree, &tps, VsPolicy::Tree);
                    assert_eq!(rows_under(&d, &store, tree, &tps, VsPolicy::Auto), want);
                    assert_eq!(
                        sorted(rows_under(&d, &store, tree, &tps, VsPolicy::Off)),
                        sorted(want)
                    );
                }
            }
        }
        // Every kind of decision was exercised.
        assert!(decisions.skipped > 0 && decisions.store_checked > 0 && decisions.listed > 0);
    }
}

#[test]
fn after_deletes_auto_stays_sound_and_never_looser_than_the_tree() {
    for seed in 5..=7u64 {
        let (d, mut store) = graph(seed);
        let tmp = TempDir::new("stale");
        // Trees of the data before the deletes: a superset of every entity's
        // signature afterwards (as a database keeps its tree on a delete).
        let mem = VsTree::build(entries(&store));
        let disk = VsTree::build_disk(tmp.0.join("vstree.kv"), entries(&store)).unwrap();
        let mut rng = Rng(seed * 104_729 + 3);
        let all: Vec<IdTriple> = TripleSource::iter_all(&store);
        for t in all {
            if rng.chance(30) {
                store.remove(t);
            }
        }
        for i in 0..150 {
            let tps = bgp(&mut rng);
            // In memory, Auto only skips exact refinements: equal sets.
            let (want, _) = candidates_under(&d, &store, &mem, &tps, VsPolicy::Tree);
            let (got, _) = candidates_under(&d, &store, &mem, &tps, VsPolicy::Auto);
            assert_eq!(got, want, "seed {seed} bgp {i} (memory)");
            // On disk, Auto tests the current store: the walk's sets minus
            // the stale entries.
            let (want, _) = candidates_under(&d, &store, &disk, &tps, VsPolicy::Tree);
            let (got, _) = candidates_under(&d, &store, &disk, &tps, VsPolicy::Auto);
            match (&got, &want) {
                (Some(got), Some(want)) => {
                    // A variable the walk seeds is seeded by Auto too.
                    assert_eq!(
                        got.keys().collect::<BTreeSet<_>>(),
                        want.keys().collect::<BTreeSet<_>>()
                    );
                    for (v, set) in got {
                        let tree_set = &want[v];
                        assert!(
                            set.iter().all(|x| tree_set.binary_search(x).is_ok()),
                            "seed {seed} bgp {i}: var {v} not a subset"
                        );
                    }
                }
                (a, b) => assert_eq!(a.is_none(), b.is_none()),
            }
            // Same answers under every policy.
            let want = sorted(rows_under(&d, &store, &disk, &tps, VsPolicy::Off));
            for p in [VsPolicy::Tree, VsPolicy::Auto] {
                assert_eq!(sorted(rows_under(&d, &store, &disk, &tps, p)), want);
                assert_eq!(sorted(rows_under(&d, &store, &mem, &tps, p)), want);
            }
        }
    }
}

#[test]
fn an_empty_tree_filters_nothing_under_any_policy() {
    let (d, store) = graph(9);
    let tmp = TempDir::new("empty");
    let mem = VsTree::new();
    let disk = VsTree::build_disk(tmp.0.join("vstree.kv"), Vec::new()).unwrap();
    let mut rng = Rng(77);
    for _ in 0..60 {
        let tps = bgp(&mut rng);
        for tree in [&mem, &disk] {
            let (off, _) = candidates_under(&d, &store, tree, &tps, VsPolicy::Off);
            let (auto, _) = candidates_under(&d, &store, tree, &tps, VsPolicy::Auto);
            let (walk, _) = candidates_under(&d, &store, tree, &tps, VsPolicy::Tree);
            assert_eq!(auto, off);
            assert_eq!(walk, off);
        }
    }
}

#[test]
fn retain_sorted_equals_a_binary_search_filter() {
    let mut rng = Rng(4242);
    for _ in 0..300 {
        let mut members: Vec<u32> = (0..rng.below(400))
            .map(|_| rng.below(1000) as u32)
            .collect();
        members.sort_unstable();
        members.dedup();
        let mut set: Vec<u32> = (0..rng.below(400))
            .map(|_| rng.below(1000) as u32)
            .collect();
        set.sort_unstable();
        set.dedup();
        let want: Vec<u32> = set
            .iter()
            .copied()
            .filter(|x| members.binary_search(x).is_ok())
            .collect();
        retain_sorted(&mut set, &members);
        assert_eq!(set, want);
    }
}

/// A star of constant edges on `?a` (`?a p o` / `s p ?a`), its constants
/// taken from triples of the store so that it often has solutions.
fn star(rng: &mut Rng, triples: &[IdTriple], d: &Dictionary) -> Vec<TriplePattern> {
    let term = |id: u32| d.term(id).map(|t| t.to_string()).unwrap();
    let parse_term = |s: String| crate::parser::ntriples::parse_term(&s).unwrap();
    let pred = |id: u32| d.pred(id).map(|t| t.to_string()).unwrap();
    let n = 1 + rng.below(3);
    let anchor = triples[rng.below(triples.len())];
    (0..n)
        .map(|_| {
            // Mostly edges of the anchor's subject (non-empty stars).
            let t = if rng.chance(70) {
                let own: Vec<&IdTriple> = triples.iter().filter(|t| t.sub == anchor.sub).collect();
                *own[rng.below(own.len())]
            } else {
                triples[rng.below(triples.len())]
            };
            let p = PatternTerm::Term(Term::iri(pred(t.pred).trim_matches(['<', '>']).to_string()));
            if rng.chance(75) {
                TriplePattern {
                    subject: PatternTerm::Var("a".into()),
                    predicate: p,
                    object: PatternTerm::Term(parse_term(term(t.obj))),
                }
            } else {
                TriplePattern {
                    subject: PatternTerm::Term(parse_term(term(t.sub))),
                    predicate: p,
                    object: PatternTerm::Var("a".into()),
                }
            }
        })
        .collect()
}

#[test]
fn shortcuts_give_the_plain_plans_rows_in_order() {
    for seed in 11..=14u64 {
        let (d, store) = graph(seed);
        let triples: Vec<IdTriple> = TripleSource::iter_all(&store);
        let tmp = TempDir::new("plain");
        let mem = VsTree::build(entries(&store));
        let disk = VsTree::build_disk(tmp.0.join("vstree.kv"), entries(&store)).unwrap();
        let mut rng = Rng(seed * 31 + 7);
        let mut stars = 0;
        for i in 0..200 {
            let tps = if i % 2 == 0 {
                star(&mut rng, &triples, &d)
            } else {
                bgp(&mut rng)
            };
            for tree in [&mem, &disk] {
                for policy in [VsPolicy::Auto, VsPolicy::Tree, VsPolicy::Off] {
                    PLAIN_PLANS.with(|f| f.set(true));
                    let want = rows_under(&d, &store, tree, &tps, policy);
                    PLAIN_PLANS.with(|f| f.set(false));
                    let got = rows_under(&d, &store, tree, &tps, policy);
                    assert_eq!(got, want, "seed {seed} query {i}: {}", select_text(&tps));
                    // Candidate lookups wherever a candidate set exists.
                    ALWAYS_LOOK_UP.with(|f| f.set(true));
                    let looked_up = rows_under(&d, &store, tree, &tps, policy);
                    ALWAYS_LOOK_UP.with(|f| f.set(false));
                    assert_eq!(looked_up, want, "seed {seed} query {i} (lookups)");
                    if i % 2 == 0 && !got.is_empty() {
                        stars += 1;
                    }
                }
            }
        }
        assert!(stars > 50, "too few stars with solutions: {stars}");
    }
}
