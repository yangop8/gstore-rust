//! The read-query result cache of a [`Database`](super::Database).
//!
//! Upstream gStore's `QueryCache` (`src/Query/QueryCache.cpp`) memoizes the
//! id tables of small basic graph patterns, bounded by memory: at most
//! `TOTAL_MEMORY_LIMIT` (10⁸) cached ids in all, no entry above
//! `ITEM_MEMORY_LIMIT` (10⁶) ids, least-recently-used entries evicted first,
//! and the whole cache cleared on every update. Those limits are compile-time
//! constants upstream; gStore 1.4's `conf.ini` has no key for them.
//!
//! This cache keeps whole read-query results keyed by the SPARQL text, with
//! the same policy:
//!
//! * **Bounded by memory, not entry count.** The budget is an approximate
//!   byte count (the result's strings plus per-row/per-cell overhead and the
//!   key), [`DEFAULT_QUERY_CACHE_BYTES`] by default and set with
//!   [`Database::set_query_cache_size`](super::Database::set_query_cache_size)
//!   (0 disables caching). The proposed `conf.ini` key is
//!   `query_cache_size` (bytes) in the `[system]` section. A single result
//!   larger than an eighth of the budget is not cached (upstream: 1 % of its
//!   total), so one huge answer cannot flush everything else. Least-recently
//!   used entries are evicted first.
//! * **Invalidated by every mutation** — insert, delete, `CLEAR`/`DROP`,
//!   `LOAD`, inference, rollback (the database's
//!   [`mutation_count`](super::Database::mutation_count) advances) — and by
//!   anything else that can change answers (registering a function, the
//!   integer-division mode).
//! * **Only deterministic queries are cached** ([`is_cacheable`]): never a
//!   query calling `NOW()`, `RAND()`, `UUID()`, `STRUUID()` or `BNODE()`,
//!   using `SERVICE` (the remote data may change), calling a user-defined
//!   function (registered closures, `PFN`) — those may be non-deterministic —
//!   or a CONSTRUCT whose template has a `[ … ]` / `( … )` node (a fresh
//!   blank node on every run).
//!   SPARQL built-ins, XSD casts, the legacy `rs:` functions and gStore's
//!   advanced graph functions (deterministic for given data) are cacheable.

use std::collections::{BTreeMap, HashMap};

use crate::model::Term;
use crate::parser::sparql::ast::{
    is_anon_var, Expr, GraphPattern, PatternTerm, Projection, Query, SelectItem, SelectQuery,
};
use crate::parser::sparql::{builtins, rs_function_name};
use crate::query::QueryResult;

/// The default query-cache budget: 64 MiB.
pub const DEFAULT_QUERY_CACHE_BYTES: usize = 64 << 20;

/// An entry may take at most `budget / MAX_ENTRY_FRACTION` bytes.
const MAX_ENTRY_FRACTION: usize = 8;

/// Fixed per-entry bookkeeping (map slot, LRU slot, result header).
const ENTRY_OVERHEAD: usize = 128;

/// SPARQL built-ins whose value changes from call to call.
const NON_DETERMINISTIC: [&str; 5] = ["NOW", "RAND", "UUID", "STRUUID", "BNODE"];

/// The XSD constructor casts the engine evaluates (deterministic).
const XSD_CASTS: [&str; 8] = [
    "string", "integer", "decimal", "double", "float", "boolean", "dateTime", "date",
];

#[derive(Debug)]
struct Entry {
    result: QueryResult,
    tick: u64,
    bytes: usize,
}

/// A byte-bounded LRU cache of read-query results (see the module docs).
#[derive(Debug)]
pub(crate) struct QueryCache {
    map: HashMap<String, Entry>,
    /// Recency order: access tick → key (the smallest tick is the LRU entry).
    lru: BTreeMap<u64, String>,
    budget: usize,
    used: usize,
    tick: u64,
    /// The database's [`mutation_count`](super::Database::mutation_count)
    /// the cached results were computed at; a lookup or insert at another
    /// count empties the cache first (a backstop to the explicit clearing
    /// on every mutation).
    generation: u64,
}

impl QueryCache {
    pub(crate) fn new(budget: usize) -> QueryCache {
        QueryCache {
            map: HashMap::new(),
            lru: BTreeMap::new(),
            budget,
            used: 0,
            tick: 0,
            generation: 0,
        }
    }

    /// Forget every result computed at another data generation.
    fn sync(&mut self, generation: u64) {
        if generation != self.generation {
            self.clear();
            self.generation = generation;
        }
    }

    /// Look up `key` among the results computed at data generation
    /// `generation`, refreshing its recency and returning a clone on hit.
    pub(crate) fn get(&mut self, key: &str, generation: u64) -> Option<QueryResult> {
        self.sync(generation);
        self.tick += 1;
        let tick = self.tick;
        let entry = self.map.get_mut(key)?;
        self.lru.remove(&entry.tick);
        entry.tick = tick;
        self.lru.insert(tick, key.to_string());
        Some(entry.result.clone())
    }

    /// Cache `key → value`, computed at data generation `generation`,
    /// evicting least-recently-used entries until it fits the budget. A
    /// result above the per-entry limit (or any result, with a zero budget)
    /// is not cached.
    pub(crate) fn put(&mut self, key: String, value: QueryResult, generation: u64) {
        self.sync(generation);
        let bytes = key.len() + result_bytes(&value) + ENTRY_OVERHEAD;
        if self.budget == 0 || bytes > self.budget / MAX_ENTRY_FRACTION {
            return;
        }
        self.remove(&key);
        while self.used + bytes > self.budget {
            let Some((_, lru)) = self.lru.pop_first() else {
                break;
            };
            if let Some(e) = self.map.remove(&lru) {
                self.used -= e.bytes;
            }
        }
        self.tick += 1;
        self.lru.insert(self.tick, key.clone());
        self.used += bytes;
        self.map.insert(
            key,
            Entry {
                result: value,
                tick: self.tick,
                bytes,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        if let Some(e) = self.map.remove(key) {
            self.lru.remove(&e.tick);
            self.used -= e.bytes;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
        self.used = 0;
    }

    /// The byte budget.
    pub(crate) fn budget(&self) -> usize {
        self.budget
    }

    /// Number of cached results.
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Approximate bytes currently held.
    pub(crate) fn bytes(&self) -> usize {
        self.used
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
}

/// Approximate heap + inline size of a result.
fn result_bytes(r: &QueryResult) -> usize {
    const CELL: usize = std::mem::size_of::<Option<String>>();
    const ROW: usize = std::mem::size_of::<Vec<Option<String>>>();
    match r {
        QueryResult::Select(rs) => {
            rs.vars.iter().map(|v| v.len() + CELL).sum::<usize>()
                + rs.rows
                    .iter()
                    .map(|row| {
                        ROW + row
                            .iter()
                            .map(|c| CELL + c.as_ref().map_or(0, String::len))
                            .sum::<usize>()
                    })
                    .sum::<usize>()
        }
        QueryResult::Construct(ts) => ts
            .iter()
            .map(|t| term_bytes(&t.subject) + term_bytes(&t.predicate) + term_bytes(&t.object))
            .sum(),
        QueryResult::Ask(_) | QueryResult::Update { .. } => 0,
    }
}

fn term_bytes(t: &Term) -> usize {
    std::mem::size_of::<Term>()
        + match t {
            Term::Iri(s) | Term::Blank(s) => s.len(),
            Term::Literal {
                value,
                datatype,
                lang,
            } => {
                value.len()
                    + datatype.as_ref().map_or(0, String::len)
                    + lang.as_ref().map_or(0, String::len)
            }
        }
}

/// Whether `q`'s result may be cached: a read query that is deterministic
/// for a given database state (see the module docs).
pub(crate) fn is_cacheable(q: &Query) -> bool {
    match q {
        Query::Select(s) => select_ok(s),
        Query::Ask(a) => pattern_ok(&a.pattern),
        // A `[ … ]` / `( … )` node in the template is a fresh blank node per
        // solution and per run, like BNODE().
        Query::Construct(c) => {
            pattern_ok(&c.pattern)
                && c.order_by.iter().all(|o| expr_ok(&o.expr))
                && !c.template.iter().any(|tp| {
                    [&tp.subject, &tp.predicate, &tp.object]
                        .into_iter()
                        .any(|t| matches!(t, PatternTerm::Var(v) if is_anon_var(v)))
                })
        }
        Query::Describe(d) => d.pattern.as_ref().is_none_or(pattern_ok),
        Query::Update(_) => false,
    }
}

fn select_ok(s: &SelectQuery) -> bool {
    let items_ok = match &s.projection {
        Projection::All => true,
        Projection::Items(items) => items.iter().all(|it| match it {
            SelectItem::Var(_) => true,
            SelectItem::Expr(e, _) => expr_ok(e),
        }),
    };
    items_ok
        && pattern_ok(&s.pattern)
        && s.group_by.iter().chain(&s.having).all(expr_ok)
        && s.order_by.iter().all(|o| expr_ok(&o.expr))
}

fn pattern_ok(gp: &GraphPattern) -> bool {
    match gp {
        GraphPattern::Empty
        | GraphPattern::Bgp(_)
        | GraphPattern::Path(_)
        | GraphPattern::Values(_, _) => true,
        GraphPattern::Join(a, b) | GraphPattern::Union(a, b) | GraphPattern::Minus(a, b) => {
            pattern_ok(a) && pattern_ok(b)
        }
        GraphPattern::LeftJoin(a, b, fs) => {
            pattern_ok(a) && pattern_ok(b) && fs.iter().all(expr_ok)
        }
        GraphPattern::Filter(fs, inner) => fs.iter().all(expr_ok) && pattern_ok(inner),
        GraphPattern::Extend(inner, _, e) => pattern_ok(inner) && expr_ok(e),
        GraphPattern::SubSelect(sq) => select_ok(sq),
        GraphPattern::Graph(_, inner) => pattern_ok(inner),
        // Remote data may change at any time.
        GraphPattern::Service { .. } => false,
    }
}

fn expr_ok(e: &Expr) -> bool {
    match e {
        Expr::Var(_) | Expr::Const(_) => true,
        Expr::Or(a, b) | Expr::And(a, b) | Expr::Compare(_, a, b) | Expr::Arith(_, a, b) => {
            expr_ok(a) && expr_ok(b)
        }
        Expr::Not(a) | Expr::Unary(_, a) => expr_ok(a),
        Expr::In { expr, list, .. } => expr_ok(expr) && list.iter().all(expr_ok),
        // A name outside the built-in table is a user-defined function.
        Expr::Builtin(name, args) => {
            builtins::is_builtin(name)
                && !NON_DETERMINISTIC.contains(&name.as_str())
                && args.iter().all(expr_ok)
        }
        Expr::IriCall { iri, args } => {
            let known = match rs_function_name(iri) {
                Some(name) => !NON_DETERMINISTIC.contains(&name),
                None => iri
                    .strip_prefix("http://www.w3.org/2001/XMLSchema#")
                    .is_some_and(|local| XSD_CASTS.contains(&local)),
            };
            known && args.iter().all(expr_ok)
        }
        Expr::Exists(_, p) => pattern_ok(p),
        Expr::Aggregate { arg, .. } => arg.as_deref().is_none_or(expr_ok),
        // Upstream's graph functions are deterministic for given data.
        Expr::GraphFn { .. } => true,
        // User code.
        Expr::Pfn { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::sparql::parse;
    use crate::query::ResultSet;

    fn rows(n: usize) -> QueryResult {
        QueryResult::Select(ResultSet {
            vars: vec!["x".into()],
            rows: (0..n)
                .map(|i| vec![Some(format!("<http://ex/{i}>"))])
                .collect(),
        })
    }

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        let one = "q1".len() + result_bytes(&rows(10)) + ENTRY_OVERHEAD;
        // Room for exactly eight such entries (each at the per-entry limit).
        let mut c = QueryCache::new(8 * one);
        for k in ["q1", "q2", "q3", "q4", "q5", "q6", "q7", "q8"] {
            c.put(k.into(), rows(10), 0);
        }
        assert_eq!(c.len(), 8);
        assert_eq!(c.bytes(), 8 * one);
        c.get("q1", 0); // q2 is now the least recently used
        c.put("q9".into(), rows(10), 0);
        assert_eq!(c.len(), 8);
        assert!(c.contains("q1") && c.contains("q9") && !c.contains("q2"));
        assert!(c.bytes() <= c.budget());
        // Re-putting a key replaces it without double counting.
        c.put("q9".into(), rows(10), 0);
        assert_eq!(c.bytes(), 8 * one);
        // Results of another data generation are never served.
        assert!(c.get("q9", 1).is_none());
        assert_eq!((c.len(), c.bytes()), (0, 0));
        c.put("q1".into(), rows(10), 1);
        assert!(c.get("q1", 1).is_some());
        c.clear();
        assert_eq!((c.len(), c.bytes()), (0, 0));
    }

    #[test]
    fn oversized_results_and_zero_budget_are_not_cached() {
        let mut c = QueryCache::new(8 * 1024);
        c.put("big".into(), rows(1000), 0);
        assert_eq!(c.len(), 0);
        c.put("small".into(), rows(1), 0);
        assert_eq!(c.len(), 1);
        let mut off = QueryCache::new(0);
        off.put("small".into(), rows(1), 0);
        assert_eq!(off.len(), 0);
    }

    #[test]
    fn only_deterministic_queries_are_cacheable() {
        let ok = |q: &str| is_cacheable(&parse(q).unwrap());
        assert!(ok("SELECT * WHERE { ?s ?p ?o }"));
        assert!(ok(
            "SELECT (STR(?o) AS ?x) WHERE { ?s ?p ?o FILTER(?o > 1) } ORDER BY ?x"
        ));
        assert!(ok("PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
             SELECT (xsd:integer(?o) AS ?x) WHERE { ?s ?p ?o }"));
        assert!(ok("ASK { ?s ?p ?o }"));
        for q in [
            "SELECT (NOW() AS ?t) WHERE {}",
            "SELECT ?s WHERE { ?s ?p ?o } ORDER BY (RAND())",
            "SELECT (UUID() AS ?u) WHERE {}",
            "SELECT (STRUUID() AS ?u) WHERE {}",
            "CONSTRUCT { ?s <p> ?o } WHERE { ?s ?p ?o BIND(BNODE() AS ?b) }",
            "ASK { ?s ?p ?o FILTER(?o < RAND()) }",
            "SELECT * WHERE { SERVICE <http://ex/sparql> { ?s ?p ?o } }",
            "SELECT (<http://ex/fn#f>(1) AS ?x) WHERE {}",
            "SELECT ?s WHERE { ?s ?p ?o FILTER EXISTS { ?s ?q ?r FILTER(RAND() < 0.5) } }",
            "SELECT * WHERE { { SELECT (COUNT(RAND()) AS ?c) WHERE {} } }",
            "INSERT DATA { <a> <b> <c> }",
        ] {
            assert!(!ok(q), "{q} must not be cached");
        }
    }
}
