//! A sharded / distributed-query core: an **in-process** scatter-gather
//! [`TripleSource`] over `N` partitioned [`TripleStore`]s.
//!
//! Corresponds, in spirit, to gStore's distributed deployment (gStore + the
//! `gStore`-on-cluster work): the triple set is **partitioned** across shards
//! and a query is answered by *scatter-gather* — fan the access out to every
//! shard, then **merge** the per-shard answers into one. Because the query
//! engine ([`crate::query::engine::Evaluator`]) is generic over
//! [`TripleSource`], the *exact same* optimizer + executor that runs over a
//! single [`TripleStore`] runs, unmodified, over a [`ShardedStore`].
//!
//! ## Partitioning
//!
//! Triples are partitioned by `hash(subject) % N` so that **a subject's whole
//! adjacency co-locates on one shard**. That makes every subject-rooted access
//! (`s??`, `sp?`, `s?o`, `spo`) a single-shard lookup, while object- and
//! predicate-rooted accesses (`??o`, `?po`, `?p?`) must touch all shards (the
//! same object/predicate can occur on many shards, reached from different
//! subjects).
//!
//! ## Merge contract (the load-bearing part)
//!
//! The engine relies on the precise return-type contract of each
//! [`TripleSource`] method (sorted, de-duplicated `Vec`s; `(pred,sub)` /
//! `(sub,obj)` orderings; global-distinct counts). After concatenating the
//! per-shard answers this module **re-establishes that contract** — sort +
//! dedup pair/key lists, dedup-then-count the distinct statistics, and rebuild
//! `iter_all` in predicate-major `(pred, sub, obj)` order — so that a
//! [`ShardedStore`] is **observationally identical** to a single
//! [`TripleStore`] holding the same triples. Get this wrong and the join
//! engine silently returns wrong answers; the unit tests below assert byte-for-
//! byte parity across every access pattern and a multi-pattern BGP.
//!
//! ## Scope (deferred)
//!
//! This is the *distributed-query MERGE core*, in-process. A real cluster also
//! needs a **network transport** (e.g. gRPC) to ship sub-queries to remote
//! shard servers and stream partial results back, plus **membership /
//! rebalancing** (node join/leave, partition reassignment, replication, fault
//! tolerance). Those are deliberately out of scope here — the merge semantics
//! are the part that must be correct first, and they are transport-agnostic.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::dict::Dictionary;
use crate::error::Result;
use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::parser::sparql;
use crate::query::{Evaluator, QueryResult};
use crate::store::{TripleSource, TripleStore};

/// A triple set partitioned across `N` [`TripleStore`] shards by
/// `hash(subject) % N`, queried by scatter-gather. Implements [`TripleSource`]
/// so the generic [`Evaluator`] runs over it unchanged.
///
/// Correctness rests on the by-construction invariant that every triple of a
/// subject lives on exactly one shard (`shard_of(subject)`); `triple_count` and
/// `pred_card` sum across shards on that basis. If a routed-`insert` API is ever
/// added it MUST place triples via `shard_of(sub)`, or those sums silently break.
#[derive(Debug, Clone)]
pub struct ShardedStore {
    shards: Vec<TripleStore>,
}

/// The shard index a subject hashes to. Stable within a process run (all that
/// scatter-gather needs); `DefaultHasher` keeps us on `std` with no new deps.
fn shard_of(sub: EntityLiteralId, num_shards: usize) -> usize {
    let mut h = DefaultHasher::new();
    sub.hash(&mut h);
    (h.finish() % num_shards as u64) as usize
}

/// Sort + dedup a gathered list back into the trait's `Vec` contract.
fn sort_dedup<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort_unstable();
    v.dedup();
    v
}

impl ShardedStore {
    /// Create an empty store with `num_shards` shards (`num_shards` is clamped
    /// to at least 1).
    pub fn new(num_shards: usize) -> ShardedStore {
        let n = num_shards.max(1);
        ShardedStore {
            shards: (0..n).map(|_| TripleStore::new()).collect(),
        }
    }

    /// Build from id-triples, routing each triple to `hash(subject) % N` and
    /// bulk-loading each shard once (so every shard's indexes are sorted +
    /// de-duplicated, exactly as a single [`TripleStore::bulk_load`]).
    pub fn from_triples(
        num_shards: usize,
        triples: impl IntoIterator<Item = IdTriple>,
    ) -> ShardedStore {
        let n = num_shards.max(1);
        let mut buckets: Vec<Vec<IdTriple>> = vec![Vec::new(); n];
        for t in triples {
            buckets[shard_of(t.sub, n)].push(t);
        }
        let shards = buckets
            .into_iter()
            .map(|b| {
                let mut s = TripleStore::new();
                s.bulk_load(b);
                s
            })
            .collect();
        ShardedStore { shards }
    }

    /// Build from an existing [`TripleStore`] by re-partitioning its triples.
    pub fn from_store(num_shards: usize, store: &TripleStore) -> ShardedStore {
        ShardedStore::from_triples(num_shards, store.iter_all())
    }

    /// Number of shards.
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Borrow shard `i` (for inspection / per-shard stats).
    pub fn shard(&self, i: usize) -> &TripleStore {
        &self.shards[i]
    }

    /// The shard index a subject lives on.
    pub fn shard_index(&self, sub: EntityLiteralId) -> usize {
        shard_of(sub, self.shards.len())
    }

    /// Per-shard triple counts (for diagnostics / asserting spread).
    pub fn shard_sizes(&self) -> Vec<u64> {
        self.shards.iter().map(TripleStore::triple_count).collect()
    }

    /// Run a read query (SELECT / ASK / CONSTRUCT / DESCRIBE) over this sharded
    /// store using the generic [`Evaluator`] — the scatter-gather is transparent
    /// to the engine. Convenience wrapper over `Evaluator::new(dict, self)`.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql_text)?;
        Evaluator::new(dict, self).evaluate(&q)
    }
}

impl TripleSource for ShardedStore {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        // A subject co-locates on one shard, but OR-ing across all is equally
        // correct and robust to how the store was built.
        self.shards.iter().any(|s| s.exists(sub, pred, obj))
    }

    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        // Subject-rooted: lives on one shard. Still merge defensively so the
        // result is sorted-unique by (pred, obj), matching the single store.
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend_from_slice(s.po_by_s(sub));
        }
        sort_dedup(out)
    }

    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.o_by_sp(sub, pred));
        }
        sort_dedup(out)
    }

    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.p_by_so(sub, obj));
        }
        sort_dedup(out)
    }

    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        // Object-rooted: the same object is reached from subjects on different
        // shards — must gather all shards and re-sort/dedup by (pred, sub).
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend_from_slice(s.ps_by_o(obj));
        }
        sort_dedup(out)
    }

    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.s_by_po(pred, obj));
        }
        sort_dedup(out)
    }

    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        // Predicate-rooted: a predicate spans shards — gather + re-sort/dedup.
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend_from_slice(s.so_by_p(pred));
        }
        sort_dedup(out)
    }

    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.subs_by_p(pred));
        }
        sort_dedup(out)
    }

    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.objs_by_p(pred));
        }
        sort_dedup(out)
    }

    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.subject_keys());
        }
        sort_dedup(out)
    }

    fn object_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.object_keys());
        }
        sort_dedup(out)
    }

    fn triple_count(&self) -> u64 {
        // Each triple lives on exactly one shard → the sum is the global count.
        self.shards.iter().map(TripleStore::triple_count).sum()
    }

    fn distinct_subjects(&self) -> usize {
        // Subjects co-locate (disjoint across shards) but dedup anyway so the
        // count is global-distinct, never a naive sum.
        self.subject_keys().len()
    }

    fn distinct_objects(&self) -> usize {
        // Objects DO repeat across shards → must dedup, not sum.
        self.object_keys().len()
    }

    fn num_predicates(&self) -> usize {
        // Predicates repeat across shards → dedup.
        let mut preds: HashSet<PredId> = HashSet::new();
        for s in &self.shards {
            preds.extend(s.predicates());
        }
        preds.len()
    }

    fn pred_card(&self, pred: PredId) -> usize {
        // Triple counts partition cleanly → sum is correct.
        self.shards.iter().map(|s| s.pred_card(pred)).sum()
    }

    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        // Global-distinct subjects of the predicate (dedup across shards).
        self.subs_by_p(pred).len()
    }

    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        // Global-distinct objects of the predicate (dedup across shards).
        self.objs_by_p(pred).len()
    }

    fn iter_all(&self) -> Vec<IdTriple> {
        // A single TripleStore yields triples in predicate-major (pred, sub,
        // obj) order (its p2so index drives iteration). Reproduce that exactly:
        // for each distinct predicate ascending, emit the merged (sub, obj)
        // pairs (already sorted-unique from `so_by_p`).
        let mut preds: Vec<PredId> = Vec::new();
        for s in &self.shards {
            preds.extend(s.predicates());
        }
        let preds = sort_dedup(preds);
        let mut out = Vec::new();
        for pred in preds {
            for (sub, obj) in self.so_by_p(pred) {
                out.push(IdTriple::new(sub, pred, obj));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::Term;
    use crate::query::{QueryResult, ResultSet};

    /// Build a connected graph with enough distinct subjects to spread across
    /// several shards, plus shared objects (a common `Person` class and a hub
    /// node) so object-/predicate-rooted accesses genuinely span shards.
    /// Returns the dictionary, the id-triples, and a few resolved ids.
    fn fixture() -> (Dictionary, Vec<IdTriple>) {
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let typ = d.intern_predicate(&Term::iri("http://ex/type").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());

        let person = d.intern_term(&Term::iri("http://ex/Person"));
        let hub = d.intern_term(&Term::iri("http://ex/hub"));

        let names = [
            "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy",
        ];
        let people: Vec<EntityLiteralId> = names
            .iter()
            .map(|n| d.intern_term(&Term::iri(format!("http://ex/{n}"))))
            .collect();

        let mut triples = Vec::new();
        for (i, &p) in people.iter().enumerate() {
            // every person is a Person (shared object across many shards)
            triples.push(IdTriple::new(p, typ, person));
            // every person knows the hub (shared object across many shards)
            triples.push(IdTriple::new(p, knows, hub));
            // ring of acquaintances (knows-edges spread over subjects)
            let next = people[(i + 1) % people.len()];
            triples.push(IdTriple::new(p, knows, next));
            // a literal name
            let lit = d.intern_term(&Term::plain_literal(*names.get(i).unwrap()));
            triples.push(IdTriple::new(p, name, lit));
        }
        // give the hub some outgoing edges too, so it is both subject and object
        triples.push(IdTriple::new(hub, typ, person));
        triples.push(IdTriple::new(hub, knows, people[0]));

        (d, triples)
    }

    fn single(triples: &[IdTriple]) -> TripleStore {
        let mut s = TripleStore::new();
        s.bulk_load(triples.iter().copied());
        s
    }

    fn rows_sorted(rs: &ResultSet) -> Vec<Vec<Option<String>>> {
        let mut r = rs.rows.clone();
        r.sort();
        r
    }

    #[test]
    fn shards_spread_triples_across_more_than_one_shard() {
        let (_d, triples) = fixture();
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());
        let nonempty = sharded.shard_sizes().iter().filter(|&&c| c > 0).count();
        assert!(
            nonempty > 1,
            "expected triples spread across >1 shard, got sizes {:?}",
            sharded.shard_sizes()
        );
        // total triple count is conserved across the partition
        assert_eq!(sharded.triple_count(), single(&triples).triple_count());
    }

    #[test]
    fn from_store_repartitions_identically() {
        let (_d, triples) = fixture();
        let base = single(&triples);
        let a = ShardedStore::from_triples(4, triples.iter().copied());
        let b = ShardedStore::from_store(4, &base);
        assert_eq!(a.iter_all(), b.iter_all());
        assert_eq!(a.shard_sizes(), b.shard_sizes());
    }

    /// The heart of the task: every access-pattern method must return results
    /// byte-for-byte identical to a single TripleStore over the same triples,
    /// including ordering and de-duplication.
    #[test]
    fn every_access_pattern_matches_single_store() {
        let (_d, triples) = fixture();
        let base = single(&triples);
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());

        // Global statistics / scans.
        assert_eq!(
            TripleSource::triple_count(&sharded),
            base.triple_count(),
            "triple_count"
        );
        assert_eq!(
            sharded.distinct_subjects(),
            base.distinct_subjects(),
            "distinct_subjects"
        );
        assert_eq!(
            sharded.distinct_objects(),
            base.distinct_objects(),
            "distinct_objects"
        );
        assert_eq!(
            sharded.num_predicates(),
            base.num_predicates(),
            "num_predicates"
        );
        assert_eq!(
            sharded.subject_keys(),
            base.subject_keys().collect::<Vec<_>>(),
            "subject_keys"
        );
        assert_eq!(
            sharded.object_keys(),
            base.object_keys().collect::<Vec<_>>(),
            "object_keys"
        );
        assert_eq!(sharded.iter_all(), base.iter_all().collect::<Vec<_>>(), "iter_all");

        // Subject-rooted accesses over every id that appears anywhere.
        let mut ids: Vec<EntityLiteralId> = base.subject_keys().collect();
        ids.extend(base.object_keys());
        ids = sort_dedup(ids);
        for &s in &ids {
            assert_eq!(
                TripleSource::po_by_s(&sharded, s),
                base.po_by_s(s).to_vec(),
                "po_by_s({s})"
            );
            assert_eq!(
                TripleSource::ps_by_o(&sharded, s),
                base.ps_by_o(s).to_vec(),
                "ps_by_o({s})"
            );
        }

        // Predicate-rooted accesses + per-predicate statistics.
        let preds: Vec<PredId> = base.predicates().collect();
        for &p in &preds {
            assert_eq!(
                TripleSource::so_by_p(&sharded, p),
                base.so_by_p(p).to_vec(),
                "so_by_p({p})"
            );
            assert_eq!(sharded.subs_by_p(p), base.subs_by_p(p), "subs_by_p({p})");
            assert_eq!(sharded.objs_by_p(p), base.objs_by_p(p), "objs_by_p({p})");
            assert_eq!(sharded.pred_card(p), base.pred_card(p), "pred_card({p})");
            assert_eq!(
                sharded.pred_distinct_subj(p),
                base.pred_distinct_subj(p),
                "pred_distinct_subj({p})"
            );
            assert_eq!(
                sharded.pred_distinct_obj(p),
                base.pred_distinct_obj(p),
                "pred_distinct_obj({p})"
            );
            // Two-constant accesses across the full id × id space touched here.
            for &s in &ids {
                assert_eq!(
                    TripleSource::o_by_sp(&sharded, s, p),
                    base.o_by_sp(s, p),
                    "o_by_sp({s},{p})"
                );
                assert_eq!(
                    TripleSource::s_by_po(&sharded, p, s),
                    base.s_by_po(p, s),
                    "s_by_po({p},{s})"
                );
            }
        }

        // s?o and exact existence over a representative id cross-product.
        for &s in &ids {
            for &o in &ids {
                assert_eq!(
                    TripleSource::p_by_so(&sharded, s, o),
                    base.p_by_so(s, o),
                    "p_by_so({s},{o})"
                );
                for &p in &preds {
                    assert_eq!(
                        TripleSource::exists(&sharded, s, p, o),
                        base.exists(s, p, o),
                        "exists({s},{p},{o})"
                    );
                }
            }
        }
    }

    /// A multi-pattern BGP join, evaluated by the *same* generic Evaluator over
    /// the single store and the sharded store, must yield identical rows.
    #[test]
    fn multi_pattern_bgp_select_matches_single_store() {
        let (d, triples) = fixture();
        let base = single(&triples);
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());

        let q = "SELECT ?x ?y WHERE { \
                 ?x <http://ex/knows> ?y . \
                 ?y <http://ex/type> <http://ex/Person> . \
                 ?x <http://ex/name> ?n }";

        let parsed = sparql::parse(q).unwrap();
        let base_rs = match Evaluator::new(&d, &base).evaluate(&parsed).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        let shard_rs = match sharded.query(&d, q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };

        assert_eq!(shard_rs.vars, base_rs.vars);
        assert!(!base_rs.rows.is_empty(), "fixture should produce some rows");
        assert_eq!(rows_sorted(&shard_rs), rows_sorted(&base_rs));
    }

    /// ASK over the sharded store via the generic evaluator.
    #[test]
    fn ask_query_matches_single_store() {
        let (d, triples) = fixture();
        let base = single(&triples);
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());

        for q in [
            "ASK { ?p <http://ex/type> <http://ex/Person> }",
            "ASK { <http://ex/alice> <http://ex/knows> <http://ex/missing> }",
        ] {
            let parsed = sparql::parse(q).unwrap();
            let base_ans = match Evaluator::new(&d, &base).evaluate(&parsed).unwrap() {
                QueryResult::Ask(a) => a,
                other => panic!("expected ASK, got {other:?}"),
            };
            let shard_ans = match sharded.query(&d, q).unwrap() {
                QueryResult::Ask(a) => a,
                other => panic!("expected ASK, got {other:?}"),
            };
            assert_eq!(shard_ans, base_ans, "ASK mismatch for {q}");
        }
    }

    #[test]
    fn empty_and_single_shard_are_well_formed() {
        let empty = ShardedStore::new(0); // clamped to 1
        assert_eq!(empty.num_shards(), 1);
        assert_eq!(TripleSource::triple_count(&empty), 0);
        assert!(empty.iter_all().is_empty());

        let (_d, triples) = fixture();
        let one = ShardedStore::from_triples(1, triples.iter().copied());
        let base = single(&triples);
        assert_eq!(one.iter_all(), base.iter_all().collect::<Vec<_>>());
    }
}
