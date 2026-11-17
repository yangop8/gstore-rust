//! Reasoning: upstream gStore's rule management (`reasonManage`), RDFS
//! entailment, and the crate's own textual rules.
//!
//! # Upstream rules (`reasonManage`)
//!
//! The JSON rules of upstream gStore's `src/Reason` (`ReasonHelper`) and its
//! `/api` operation `reasonManage` (types 1-8):
//!
//! * [`rules`] — the rule JSON model and its validation (no rule can inject
//!   SPARQL);
//! * [`compile`] — rule → upstream's `INSERT … WHERE`, `DELETE WHERE`,
//!   count and select statements, byte for byte;
//! * [`store`] — where rules live (`<db>/reason_rule_files/`, or memory);
//! * [`manage`] — the lifecycle (add, list, compile, execute, disable, show,
//!   delete, check), with exact retraction of inferred triples.
//!
//! The [`Database`](crate::Database) methods `reason_add` … `reason_check`
//! expose it to embedders; the `/api` server answers `reasonManage` with it
//! (`crate::gapi`).
//!
//! # RDFS entailment
//!
//! Given a store that mixes data and an
//! RDFS schema, [`materialize`] computes the RDFS closure and inserts every
//! entailed triple, iterating to a fixpoint. It implements the core
//! schema-driven rules (the ones that actually change query answers):
//!
//! | rule | premises | conclusion |
//! |------|----------|------------|
//! | subclass transitivity | `c ⊑ d`, `d ⊑ e` | `c ⊑ e` |
//! | type propagation       | `x a c`, `c ⊑ d` | `x a d` |
//! | subproperty transitivity | `p ⊑ₚ q`, `q ⊑ₚ r` | `p ⊑ₚ r` |
//! | subproperty propagation  | `p ⊑ₚ q`, `x p y` | `x q y` |
//! | domain | `p domain c`, `x p y` | `x a c` |
//! | range  | `p range c`,  `x p y` | `y a c` (only for an entity `y`: a literal is never typed) |
//!
//! Note the id-space subtlety: a property used as the *subject* of a schema
//! triple (`p rdfs:subPropertyOf q`) is interned as an **entity**, while the same
//! property in data (`x p y`) is a **predicate** — two different id spaces sharing
//! the dictionary key. `ent_to_pred` bridges them through the dictionary. Only
//! an IRI is ever a property: a literal or blank node in a `subPropertyOf`,
//! `domain` or `range` subject or object position (legal RDF, e.g. OWL's
//! `:p rdfs:subPropertyOf [ owl:inverseOf :q ]`) never becomes a predicate,
//! though `subPropertyOf` stays transitive through it.
//!
//! RDFS materialization is a library API
//! ([`Database::materialize_rdfs`](crate::Database::materialize_rdfs)); no
//! server operation or command-line tool runs it.
//!
//! # Textual rules
//!
//! [`RuleSet`] is the crate's own forward-chaining rule format (below),
//! reached through [`Database::add_rule`](crate::Database::add_rule) …
//! [`run_rules`](crate::Database::run_rules). It is a library API: rules are
//! **not persisted** (they live as long as the `Database` value; save, backup
//! and open do not carry them), and no server operation uses them — the
//! server's rules are `reasonManage`'s.
//!
//! # Graphs
//!
//! Every reasoner here — RDFS materialization, textual rules and
//! `reasonManage`'s rules — reads and writes the **default graph** only: a
//! named graph's schema or data is never consulted, and nothing is inferred
//! into a named graph (review ext#15).

pub mod compile;
pub mod manage;
pub mod rules;
pub mod store;

pub use compile::CompiledRule;
pub use manage::{ReasonError, ReasonErrorKind};

use std::collections::{HashMap, HashSet};

use crate::dict::{Dictionary, TermStr};
use crate::error::{GStoreError, Result};
use crate::model::id::{is_entity_id, PredId};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::sparql::ast::RDF_TYPE;
use crate::store::{MutableStore, TripleSource};

const RDFS_SUBCLASS: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUBPROP: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

/// Materialize the RDFS closure into `store`, returning the triples that were
/// added (so a caller can record them for transaction rollback). Idempotent:
/// re-running on a closed store adds nothing.
pub fn materialize<S: TripleSource + MutableStore>(
    dict: &mut Dictionary,
    store: &mut S,
) -> Vec<IdTriple> {
    materialize_with(dict, store, |_, store: &mut S, t| store.insert(t))
}

/// [`materialize`], inserting every inferred triple through `insert` (given the
/// dictionary as it is at that moment) instead of a plain `store.insert` —
/// e.g. to persist the terms a triple uses before the triple itself. `insert`
/// returns whether the triple was newly added.
pub fn materialize_with<S, F>(dict: &mut Dictionary, store: &mut S, mut insert: F) -> Vec<IdTriple>
where
    S: TripleSource + MutableStore,
    F: FnMut(&Dictionary, &mut S, IdTriple) -> bool,
{
    let key = |iri: &str| Term::iri(iri).dict_key();
    let sco_p = dict.predicate_id(&key(RDFS_SUBCLASS));
    let spo_p = dict.predicate_id(&key(RDFS_SUBPROP));
    let dom_p = dict.predicate_id(&key(RDFS_DOMAIN));
    let rng_p = dict.predicate_id(&key(RDFS_RANGE));

    // No RDFS schema vocabulary ⇒ nothing to infer.
    if sco_p.is_none() && spo_p.is_none() && dom_p.is_none() && rng_p.is_none() {
        return Vec::new();
    }
    // Subclass/domain/range rules assert `rdf:type` triples, so the type
    // predicate must exist (intern it if the data never used it directly).
    let needs_type = sco_p.is_some() || dom_p.is_some() || rng_p.is_some();
    // (An exhausted predicate id space leaves `rdf:type` unresolved: the
    // type-asserting rules then simply infer nothing, rather than panicking.)
    let type_p = if needs_type {
        dict.try_intern_predicate(&key(RDF_TYPE)).ok()
    } else {
        dict.predicate_id(&key(RDF_TYPE))
    };

    // A subproperty target may only ever appear as a schema object (e.g.
    // `:mother rdfs:subPropertyOf :parent` with no `:parent` data triple), so it
    // has no predicate id yet. Intern a predicate id for every property named in
    // a subPropertyOf statement so [`ent_to_pred`] can resolve the target —
    // only IRIs: a literal or blank node there (`:p rdfs:subPropertyOf "x"`,
    // OWL's `:p rdfs:subPropertyOf [ owl:inverseOf :q ]`) is legal data but
    // no predicate, and became one (`<a> "x" <b>`, which no reader loads
    // back; review ext#3).
    if let Some(spo) = spo_p {
        let prop_ents: Vec<u32> = store
            .so_by_p(spo)
            .iter()
            .flat_map(|&(p, q)| [p, q])
            .collect();
        let keys: Vec<String> = prop_ents
            .iter()
            .filter_map(|&e| dict.term(e).map(TermStr::into_string))
            .filter(|k| is_iri_key(k))
            .collect();
        for k in keys {
            // Best effort: a property that cannot get a predicate id (id space
            // exhausted) is just never resolved as a subproperty target.
            let _ = dict.try_intern_predicate(&k);
        }
    }

    let mut added_all: Vec<IdTriple> = Vec::new();
    loop {
        let mut new: Vec<IdTriple> = Vec::new();
        gather(dict, store, type_p, sco_p, spo_p, dom_p, rng_p, &mut new);
        let mut added = 0usize;
        for t in new {
            if insert(dict, store, t) {
                added_all.push(t);
                added += 1;
            }
        }
        if added == 0 {
            break; // fixpoint reached
        }
    }
    added_all
}

/// One forward-chaining round: append all directly-entailed triples to `new`.
/// Generic over the [`TripleSource`] so it reasons over any backend (the
/// `so_by_p` accessor returns owned `Vec`s, which we iterate by reference).
#[allow(clippy::too_many_arguments)]
fn gather<S: TripleSource>(
    dict: &Dictionary,
    store: &S,
    type_p: Option<PredId>,
    sco_p: Option<PredId>,
    spo_p: Option<PredId>,
    dom_p: Option<PredId>,
    rng_p: Option<PredId>,
    new: &mut Vec<IdTriple>,
) {
    // --- subClassOf transitivity + rdf:type propagation ---
    if let Some(sco) = sco_p {
        let pairs = store.so_by_p(sco); // (c, d) meaning c ⊑ d
        let mut sub: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(c, d) in &pairs {
            sub.entry(c).or_default().push(d);
        }
        for &(c, d) in &pairs {
            if let Some(es) = sub.get(&d) {
                for &e in es {
                    if c != e {
                        new.push(IdTriple::new(c, sco, e));
                    }
                }
            }
        }
        if let Some(tp) = type_p {
            for (x, c) in store.so_by_p(tp) {
                if let Some(ds) = sub.get(&c) {
                    for &d in ds {
                        new.push(IdTriple::new(x, tp, d));
                    }
                }
            }
        }
    }

    // --- subPropertyOf transitivity + data propagation ---
    if let Some(spo) = spo_p {
        let pairs = store.so_by_p(spo); // (p, q) meaning p ⊑ₚ q (as entities)
        let mut sub: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(p, q) in &pairs {
            sub.entry(p).or_default().push(q);
        }
        for &(p, q) in &pairs {
            if let Some(rs) = sub.get(&q) {
                for &r in rs {
                    if p != r {
                        new.push(IdTriple::new(p, spo, r));
                    }
                }
            }
        }
        for &(p_ent, q_ent) in &pairs {
            let (Some(p_pred), Some(q_pred)) = (ent_to_pred(dict, p_ent), ent_to_pred(dict, q_ent))
            else {
                continue;
            };
            if p_pred == q_pred {
                continue;
            }
            for (x, y) in store.so_by_p(p_pred) {
                new.push(IdTriple::new(x, q_pred, y));
            }
        }
    }

    // --- domain: (p domain c), (x p y) ⇒ (x a c) ---
    if let (Some(dom), Some(tp)) = (dom_p, type_p) {
        for (p_ent, c) in store.so_by_p(dom) {
            if let Some(p_pred) = ent_to_pred(dict, p_ent) {
                for (x, _y) in store.so_by_p(p_pred) {
                    new.push(IdTriple::new(x, tp, c));
                }
            }
        }
    }

    // --- range: (p range c), (x p y) ⇒ (y a c) ---
    // Only for an entity `y`: a literal object (`x p "lit"`) cannot be the
    // subject of a triple, so `"lit" a c` is never inferred (RDFS leaves that
    // entailment to a generalized-RDF reasoner; an RDF store has no such
    // triple).
    if let (Some(rng), Some(tp)) = (rng_p, type_p) {
        for (p_ent, c) in store.so_by_p(rng) {
            if let Some(p_pred) = ent_to_pred(dict, p_ent) {
                for (_x, y) in store.so_by_p(p_pred) {
                    if is_entity_id(y) {
                        new.push(IdTriple::new(y, tp, c));
                    }
                }
            }
        }
    }
}

/// Map an entity id (a property used as a schema subject/object) to its
/// predicate id, via the shared dictionary key. `None` if that IRI is never used
/// as a predicate in the data, or the term is not an IRI (a literal or blank
/// node is never a predicate, even where an older version interned one as
/// such; review ext#3).
fn ent_to_pred(dict: &Dictionary, ent: u32) -> Option<PredId> {
    let key = dict.term(ent)?;
    if !is_iri_key(&key) {
        return None;
    }
    dict.predicate_id(&key)
}

/// Whether dictionary key `key` names an IRI (`<…>`), the only term that
/// may be a predicate.
fn is_iri_key(key: &str) -> bool {
    key.starts_with('<') && key.ends_with('>')
}

// ===========================================================================
// User-defined reasoning rules
//
// Corresponds to gStore's `src/Reason` (`ReasonHelper`): rules are *defined*,
// *enabled/disabled*, *compiled*, *materialized*, and their *effect* (how many
// triples each one inferred) is tracked. gStore stores each rule as a JSON file
// of antecedent SPARQL patterns + a consequent template; here we use a small
// hand-parsed textual rule format (no `serde_json`) and forward-chain it
// directly over the triple store to a fixpoint.
//
// Rule format (one rule):
//
// ```text
// ?x <ancestor> ?y . ?y <ancestor> ?z => ?x <ancestor> ?z
// ```
//
// * `=>` separates the antecedent (a conjunction of triple patterns) from the
//   single consequent pattern.
// * A position is either a variable (`?name`), an IRI constant (`<iri>`), a
//   blank node (`_:b`), or a literal (`"v"`, `"v"@lang`, `"v"^^<dt>`).
// * `.` between antecedent patterns is optional punctuation (patterns are
//   grouped three tokens at a time).
// * The **predicate** position must be a constant IRI in every pattern (the
//   triple store indexes by predicate id; variable predicates are rejected).
// * Every variable in the consequent must be bound by the antecedent, and the
//   consequent subject must resolve to an entity (a literal cannot be a subject).
// ===========================================================================

/// One position in a triple pattern: a variable to bind, or a constant term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternTerm {
    /// A query variable (the `?name`, without the leading `?`).
    Var(String),
    /// A fixed RDF term.
    Const(Term),
}

/// A triple pattern: subject/predicate/object, each a variable or a constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriplePattern {
    pub subject: PatternTerm,
    pub predicate: PatternTerm,
    pub object: PatternTerm,
}

/// A forward-chaining rule: `body ⟹ head`. `body` is a conjunction of triple
/// patterns; `head` is a single pattern instantiated per satisfying binding.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Unique rule name (gStore's rule filename).
    pub name: String,
    /// Antecedent: conjunction of triple patterns.
    pub body: Vec<TriplePattern>,
    /// Consequent: a single triple pattern.
    pub head: TriplePattern,
    /// Whether the rule participates in materialization.
    pub enabled: bool,
    /// Triples this rule inferred during the most recent [`RuleSet::apply`]
    /// (gStore's `effectNum`).
    pub effect_count: usize,
}

impl Rule {
    /// Parse a rule from `name` + the textual `body => head` definition.
    pub fn parse(name: impl Into<String>, text: &str) -> Result<Rule> {
        let (body_str, head_str) = text.split_once("=>").ok_or_else(|| {
            GStoreError::Database("rule must contain '=>' separating body and head".into())
        })?;
        let body = parse_patterns(body_str)?;
        if body.is_empty() {
            return Err(GStoreError::Database("rule body is empty".into()));
        }
        let mut head = parse_patterns(head_str)?;
        if head.len() != 1 {
            return Err(GStoreError::Database(format!(
                "rule head must be exactly one triple pattern, got {}",
                head.len()
            )));
        }
        let head = head.pop().unwrap();
        // The predicate position must be a constant IRI everywhere (the store
        // indexes by predicate id; variable predicates are unsupported).
        for pat in body.iter().chain(std::iter::once(&head)) {
            if !matches!(pat.predicate, PatternTerm::Const(Term::Iri(_))) {
                return Err(GStoreError::Database(
                    "rule predicate must be a constant IRI (<...>)".into(),
                ));
            }
        }
        Ok(Rule {
            name: name.into(),
            body,
            head,
            enabled: true,
            effect_count: 0,
        })
    }

    /// Evaluate the body against the current store and instantiate the head once
    /// per satisfying binding, returning the concrete head triples (each
    /// once). Pure read — interning/insertion is the caller's. `work` counts
    /// the stored pairs read and the bindings built (the join's cost).
    ///
    /// The body is a left-deep **hash join** over dictionary ids, pattern by
    /// pattern: the pattern's pairs (`so_by_p`, its constants applied) are
    /// indexed by the positions an earlier pattern bound, and every binding
    /// probes that index — linear in the pairs plus the bindings, where the
    /// nested loop it replaced compared every binding with every pair and
    /// looked both terms of each pair up (review ext#7: a two-hop rule over
    /// a 2000-edge chain took 150 ms, four times as long per doubling).
    fn derive<S: TripleSource>(&self, dict: &Dictionary, store: &S, work: &mut u64) -> Vec<Triple> {
        /// Where a pattern position's value comes from.
        #[derive(Clone, Copy, PartialEq)]
        enum Pos {
            /// A constant (its pairs are already filtered).
            Fixed,
            /// A variable an earlier pattern bound: binding column `i`.
            Bound(usize),
            /// A variable first bound here: a new binding column.
            Free,
        }
        // The variables in the order they are bound; a binding is one row of
        // their values, every row binding the same ones.
        let mut vars: Vec<&str> = Vec::new();
        let mut rows: Vec<Vec<u32>> = vec![Vec::new()];
        for pat in &self.body {
            // The predicate is a constant IRI (`Rule::parse`); an unknown one,
            // like an unknown constant term, matches nothing.
            let PatternTerm::Const(pred_term) = &pat.predicate else {
                return Vec::new();
            };
            let Some(pred_id) = dict.predicate_id(&pred_term.dict_key()) else {
                return Vec::new();
            };
            let constant = |pt: &PatternTerm| -> Option<Option<u32>> {
                match pt {
                    PatternTerm::Const(t) => dict.term_id(t).map(Some),
                    PatternTerm::Var(_) => Some(None),
                }
            };
            let (Some(s_const), Some(o_const)) = (constant(&pat.subject), constant(&pat.object))
            else {
                return Vec::new();
            };
            let position = |pt: &PatternTerm, vars: &[&str]| match pt {
                PatternTerm::Const(_) => Pos::Fixed,
                PatternTerm::Var(name) => match vars.iter().position(|v| *v == name.as_str()) {
                    Some(i) => Pos::Bound(i),
                    None => Pos::Free,
                },
            };
            let s_pos = position(&pat.subject, &vars);
            let o_pos = position(&pat.object, &vars);
            // `?x <p> ?x` with `?x` new: subject and object must be equal.
            let same_free = s_pos == Pos::Free && o_pos == Pos::Free && pat.subject == pat.object;
            let all = store.so_by_p(pred_id);
            *work += all.len() as u64;
            let pairs: Vec<(u32, u32)> = all
                .iter()
                .copied()
                .filter(|&(s, o)| {
                    s_const.is_none_or(|c| c == s)
                        && o_const.is_none_or(|c| c == o)
                        && (!same_free || s == o)
                })
                .collect();
            let mut next: Vec<Vec<u32>> = Vec::new();
            let extend = |row: &[u32], add: &[u32], next: &mut Vec<Vec<u32>>| {
                let mut r = Vec::with_capacity(row.len() + add.len());
                r.extend_from_slice(row);
                r.extend_from_slice(add);
                next.push(r);
            };
            match (s_pos, o_pos) {
                (Pos::Bound(i), Pos::Bound(j)) => {
                    let set: HashSet<(u32, u32)> = pairs.into_iter().collect();
                    next = rows
                        .into_iter()
                        .filter(|r| set.contains(&(r[i], r[j])))
                        .collect();
                }
                (Pos::Bound(i), Pos::Fixed) => {
                    let set: HashSet<u32> = pairs.into_iter().map(|(s, _)| s).collect();
                    next = rows.into_iter().filter(|r| set.contains(&r[i])).collect();
                }
                (Pos::Fixed, Pos::Bound(j)) => {
                    let set: HashSet<u32> = pairs.into_iter().map(|(_, o)| o).collect();
                    next = rows.into_iter().filter(|r| set.contains(&r[j])).collect();
                }
                (Pos::Bound(i), Pos::Free) | (Pos::Free, Pos::Bound(i)) => {
                    let subject_bound = matches!(s_pos, Pos::Bound(_));
                    let mut index: HashMap<u32, Vec<u32>> = HashMap::new();
                    for (s, o) in pairs {
                        let (key, value) = if subject_bound { (s, o) } else { (o, s) };
                        index.entry(key).or_default().push(value);
                    }
                    for r in &rows {
                        for &v in index.get(&r[i]).map_or(&[][..], Vec::as_slice) {
                            extend(r, &[v], &mut next);
                        }
                    }
                }
                (Pos::Fixed, Pos::Fixed) => {
                    if !pairs.is_empty() {
                        next = rows;
                    }
                }
                (Pos::Fixed, Pos::Free) | (Pos::Free, Pos::Fixed) => {
                    let subject_free = s_pos == Pos::Free;
                    for r in &rows {
                        for &(s, o) in &pairs {
                            extend(r, &[if subject_free { s } else { o }], &mut next);
                        }
                    }
                }
                (Pos::Free, Pos::Free) => {
                    for r in &rows {
                        for &(s, o) in &pairs {
                            if same_free {
                                extend(r, &[s], &mut next);
                            } else {
                                extend(r, &[s, o], &mut next);
                            }
                        }
                    }
                }
            }
            // The columns this pattern added, in the order `extend` wrote them.
            for (pt, pos) in [(&pat.subject, s_pos), (&pat.object, o_pos)] {
                if let (PatternTerm::Var(name), Pos::Free) = (pt, pos) {
                    if !vars.contains(&name.as_str()) {
                        vars.push(name.as_str());
                    }
                }
            }
            *work += next.len() as u64;
            rows = next;
            if rows.is_empty() {
                return Vec::new();
            }
        }

        // The head, once per distinct (subject, object) binding.
        let column = |pt: &PatternTerm| match pt {
            PatternTerm::Var(name) => vars.iter().position(|v| *v == name.as_str()).map(Some),
            PatternTerm::Const(_) => Some(None),
        };
        let (Some(s_col), Some(o_col)) = (column(&self.head.subject), column(&self.head.object))
        else {
            // A head variable the body never binds.
            return Vec::new();
        };
        let term = |pt: &PatternTerm, col: Option<usize>, row: &[u32]| -> Option<Term> {
            match (pt, col) {
                (PatternTerm::Const(t), _) => Some(t.clone()),
                (PatternTerm::Var(_), Some(c)) => Term::from_dict_key(&dict.term(row[c])?),
                (PatternTerm::Var(_), None) => None,
            }
        };
        let mut seen: HashSet<(Option<u32>, Option<u32>)> = HashSet::new();
        let mut out = Vec::new();
        for row in &rows {
            if !seen.insert((s_col.map(|c| row[c]), o_col.map(|c| row[c]))) {
                continue;
            }
            let (Some(subject), Some(object)) = (
                term(&self.head.subject, s_col, row),
                term(&self.head.object, o_col, row),
            ) else {
                continue;
            };
            let PatternTerm::Const(predicate) = &self.head.predicate else {
                continue;
            };
            // A subject cannot be a literal; a predicate must be an IRI.
            if subject.is_literal() || !matches!(predicate, Term::Iri(_)) {
                continue;
            }
            out.push(Triple::new(subject, predicate.clone(), object));
        }
        out
    }
}

/// A named collection of forward-chaining rules in the textual format above
/// (the library's own rules, [`Database::add_rule`](crate::Database::add_rule)
/// … [`run_rules`](crate::Database::run_rules)). Rules keep insertion order;
/// names are unique.
///
/// **Not persisted.** A rule set lives as long as the
/// [`Database`](crate::Database) value: saving, backing up or reopening a
/// database does not carry it (the triples it inferred are ordinary data
/// and are kept). Nothing outside the library uses it: the server's
/// `reasonManage`, which keeps its rules in `reason_rule_files/`, is the
/// upstream JSON rule system ([`manage`]). Like every reasoner here it reads
/// and writes the default graph only.
#[derive(Debug, Default, Clone)]
pub struct RuleSet {
    rules: Vec<Rule>,
    /// The join work of the last [`apply`](Self::apply) (see `Rule::derive`).
    work: u64,
}

impl RuleSet {
    pub fn new() -> RuleSet {
        RuleSet::default()
    }

    /// Define a rule from textual form. Errors if the name already exists or the
    /// text does not parse.
    pub fn add_rule(&mut self, name: impl Into<String>, text: &str) -> Result<()> {
        let name = name.into();
        if self.rules.iter().any(|r| r.name == name) {
            return Err(GStoreError::Database(format!(
                "a rule named '{name}' already exists"
            )));
        }
        self.rules.push(Rule::parse(name, text)?);
        Ok(())
    }

    /// Remove a rule by name; `true` if it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.name != name);
        self.rules.len() != before
    }

    /// Enable a rule by name; `true` if it existed.
    pub fn enable(&mut self, name: &str) -> bool {
        self.set_enabled(name, true)
    }

    /// Disable a rule by name; `true` if it existed.
    pub fn disable(&mut self, name: &str) -> bool {
        self.set_enabled(name, false)
    }

    fn set_enabled(&mut self, name: &str, on: bool) -> bool {
        if let Some(r) = self.rules.iter_mut().find(|r| r.name == name) {
            r.enabled = on;
            true
        } else {
            false
        }
    }

    /// Rules in definition order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// `(name, enabled, effect_count)` per rule, in definition order.
    pub fn list(&self) -> Vec<(String, bool, usize)> {
        self.rules
            .iter()
            .map(|r| (r.name.clone(), r.enabled, r.effect_count))
            .collect()
    }

    /// The join work (stored pairs read, bindings built) of the last
    /// [`apply`](Self::apply) (tests).
    #[cfg(test)]
    pub(crate) fn last_work(&self) -> u64 {
        self.work
    }

    /// The effect count of a named rule, if it exists.
    pub fn effect_count(&self, name: &str) -> Option<usize> {
        self.rules
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.effect_count)
    }

    /// Materialize the closure of every **enabled** rule into `store`, iterating
    /// all rules to a fixpoint. Each rule's [`effect_count`](Rule::effect_count)
    /// is reset and then set to the number of triples it newly inferred. Returns
    /// every triple added (so a caller can record them for rollback).
    pub fn apply<S: TripleSource + MutableStore>(
        &mut self,
        dict: &mut Dictionary,
        store: &mut S,
    ) -> Vec<IdTriple> {
        self.apply_with(dict, store, |_, store: &mut S, t| store.insert(t))
    }

    /// [`apply`](Self::apply), inserting every inferred triple through `insert`
    /// (given the dictionary after the triple's terms were interned) instead of
    /// a plain `store.insert` — e.g. to persist those terms before the triple.
    /// `insert` returns whether the triple was newly added.
    pub fn apply_with<S, F>(
        &mut self,
        dict: &mut Dictionary,
        store: &mut S,
        mut insert: F,
    ) -> Vec<IdTriple>
    where
        S: TripleSource + MutableStore,
        F: FnMut(&Dictionary, &mut S, IdTriple) -> bool,
    {
        for r in &mut self.rules {
            r.effect_count = 0;
        }
        self.work = 0;
        let mut added_all: Vec<IdTriple> = Vec::new();
        loop {
            let mut round_added = 0usize;
            for i in 0..self.rules.len() {
                if !self.rules[i].enabled {
                    continue;
                }
                // Read phase: derive concrete head triples against the current
                // store (immutable borrow of dict + store).
                let derived = self.rules[i].derive(dict, store, &mut self.work);
                // Write phase: intern + insert; count only genuinely-new triples.
                for t in derived {
                    // A head triple whose terms cannot be interned (a
                    // dictionary id space is exhausted) is not materialized.
                    let (Ok(s), Ok(p), Ok(o)) = (
                        dict.try_intern_entity(&t.subject.dict_key()),
                        dict.try_intern_predicate(&t.predicate.dict_key()),
                        dict.try_intern_term(&t.object),
                    ) else {
                        continue;
                    };
                    let id = IdTriple::new(s, p, o);
                    if insert(dict, store, id) {
                        added_all.push(id);
                        self.rules[i].effect_count += 1;
                        round_added += 1;
                    }
                }
            }
            if round_added == 0 {
                break; // fixpoint reached
            }
        }
        added_all
    }
}

/// Parse a side of a rule (a `.`-separated conjunction) into triple patterns.
fn parse_patterns(s: &str) -> Result<Vec<TriplePattern>> {
    let toks = tokenize(s)?;
    if toks.len() % 3 != 0 {
        return Err(GStoreError::Database(format!(
            "expected a multiple of 3 terms (whole triple patterns), got {}",
            toks.len()
        )));
    }
    let mut patterns = Vec::with_capacity(toks.len() / 3);
    for chunk in toks.as_chunks::<3>().0 {
        patterns.push(TriplePattern {
            subject: chunk[0].clone(),
            predicate: chunk[1].clone(),
            object: chunk[2].clone(),
        });
    }
    Ok(patterns)
}

/// Tokenize a rule side into [`PatternTerm`]s, skipping `.` triple separators.
/// It walks characters, never bytes, so no input — non-ASCII text, an
/// escape before a multi-byte character, a truncated literal — can make it
/// slice inside a character (it panicked on `é <p> ?y …` and on `"\\é"`;
/// review ext#4). Every term is checked with the crate's one term validator
/// ([`crate::model::term`]) and decoded with [`Term::from_dict_key`], the
/// inverse of the stored form a binding takes (review ext#7: the format had
/// a parser of its own with different escape rules).
fn tokenize(s: &str) -> Result<Vec<PatternTerm>> {
    use crate::model::term::{is_valid_blank_label, is_valid_iriref, is_valid_lang_tag};
    let err = |m: String| GStoreError::Database(m);
    let ends_token = |c: char| c.is_whitespace() || c == '.';
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() || c == '.' {
            // A standalone triple separator (IRIs/literals keep their dots
            // inside the bracketed/quoted token below, so a bare '.' is
            // punctuation).
            chars.next();
            continue;
        }
        match c {
            '<' => {
                // <iri>
                let Some(len) = s[start..].find('>') else {
                    return Err(err("unterminated <iri> in rule".into()));
                };
                let end = start + len + 1;
                let raw = &s[start..end];
                let iri = &raw[1..raw.len() - 1];
                if !is_valid_iriref(iri) {
                    return Err(err(format!(
                        "invalid IRI term '{raw}' (it holds a space, a control character or \
                         one of <>\"{{}}|^`\\)"
                    )));
                }
                out.push(PatternTerm::Const(Term::iri(iri)));
                while chars.peek().is_some_and(|&(i, _)| i < end) {
                    chars.next();
                }
            }
            '"' => {
                // "literal" with an optional @lang or ^^<dt> suffix.
                chars.next();
                let mut closed = None;
                while let Some((i, ch)) = chars.next() {
                    match ch {
                        '\\' => {
                            if chars.next().is_none() {
                                break;
                            }
                        }
                        '"' => {
                            closed = Some(i + 1);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(mut end) = closed else {
                    return Err(err("unterminated literal in rule".into()));
                };
                let rest = &s[end..];
                if let Some(tag) = rest.strip_prefix('@') {
                    let len = tag.find(ends_token).unwrap_or(tag.len());
                    if !is_valid_lang_tag(&tag[..len]) {
                        return Err(err(format!(
                            "invalid language tag '@{}' in rule",
                            &tag[..len]
                        )));
                    }
                    end += 1 + len;
                } else if let Some(dt) = rest.strip_prefix("^^<") {
                    let Some(len) = dt.find('>') else {
                        return Err(err("unterminated datatype IRI in rule".into()));
                    };
                    if !is_valid_iriref(&dt[..len]) {
                        return Err(err(format!("invalid datatype IRI '<{}>'", &dt[..len])));
                    }
                    end += 3 + len + 1;
                }
                let raw = &s[start..end];
                let term = Term::from_dict_key(raw).ok_or_else(|| {
                    err(format!(
                        "invalid literal term '{raw}' (escapes: \\\\ \\\" \\n \\r \\t)"
                    ))
                })?;
                out.push(PatternTerm::Const(term));
                while chars.peek().is_some_and(|&(i, _)| i < end) {
                    chars.next();
                }
                if chars.peek().is_some_and(|&(_, c)| !ends_token(c)) {
                    return Err(err(format!("unexpected text after the literal '{raw}'")));
                }
            }
            '?' => {
                // ?variable
                chars.next();
                let from = start + 1;
                let mut end = from;
                while let Some(&(i, ch)) = chars.peek() {
                    if ends_token(ch) {
                        break;
                    }
                    end = i + ch.len_utf8();
                    chars.next();
                }
                let name = &s[from..end];
                if name.is_empty() {
                    return Err(err("empty variable name '?'".into()));
                }
                if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return Err(err(format!(
                        "invalid variable name '?{name}' (letters, digits and '_')"
                    )));
                }
                out.push(PatternTerm::Var(name.to_owned()));
            }
            '_' if s[start..].starts_with("_:") => {
                // _:blank
                let from = start + 2;
                let mut end = from;
                chars.next();
                chars.next();
                while let Some(&(i, ch)) = chars.peek() {
                    if ends_token(ch) {
                        break;
                    }
                    end = i + ch.len_utf8();
                    chars.next();
                }
                let label = &s[from..end];
                if !is_valid_blank_label(label) {
                    return Err(err(format!("invalid blank node '_:{label}'")));
                }
                out.push(PatternTerm::Const(Term::blank(label)));
            }
            other => {
                return Err(err(format!(
                    "unexpected token starting at '{other}' (use <iri>, ?var, \"literal\", or \
                     _:blank)"
                )));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TripleStore;

    /// Intern an entity by IRI.
    fn ent(d: &mut Dictionary, iri: &str) -> u32 {
        d.intern_entity(&Term::iri(iri).dict_key())
    }
    /// Intern a predicate by IRI.
    fn pred(d: &mut Dictionary, iri: &str) -> u32 {
        d.intern_predicate(&Term::iri(iri).dict_key())
    }

    #[test]
    fn type_propagates_up_the_subclass_chain() {
        let mut d = Dictionary::new();
        let typ = pred(&mut d, RDF_TYPE);
        let sco = pred(&mut d, RDFS_SUBCLASS);
        let (grad, student, person) = (
            ent(&mut d, "GradStudent"),
            ent(&mut d, "Student"),
            ent(&mut d, "Person"),
        );
        let alice = ent(&mut d, "alice");
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(grad, sco, student));
        s.insert(IdTriple::new(student, sco, person));
        s.insert(IdTriple::new(alice, typ, grad));

        let added = materialize(&mut d, &mut s);
        assert!(!added.is_empty());
        // alice is a Student and a Person; GradStudent ⊑ Person (transitivity).
        assert!(s.exists(alice, typ, student));
        assert!(s.exists(alice, typ, person));
        assert!(s.exists(grad, sco, person));
        // idempotent
        assert!(materialize(&mut d, &mut s).is_empty());
    }

    #[test]
    fn subproperty_propagates_data() {
        let mut d = Dictionary::new();
        let spo = pred(&mut d, RDFS_SUBPROP);
        // p and q exist both as entities (schema subjects) and predicates (data).
        let mother_p = pred(&mut d, "mother");
        let parent_p = pred(&mut d, "parent");
        let mother_e = ent(&mut d, "mother");
        let parent_e = ent(&mut d, "parent");
        let (alice, bob) = (ent(&mut d, "alice"), ent(&mut d, "bob"));
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(mother_e, spo, parent_e));
        s.insert(IdTriple::new(alice, mother_p, bob));

        let added = materialize(&mut d, &mut s);
        assert!(!added.is_empty());
        assert!(s.exists(alice, parent_p, bob)); // mother ⊑ₚ parent
    }

    /// Regression: `p range c` with `x p "lit"` used to infer `"lit" a c` — a
    /// triple whose subject is a literal. Only entity objects are typed.
    #[test]
    fn range_never_types_a_literal_object() {
        let mut d = Dictionary::new();
        let typ = pred(&mut d, RDF_TYPE);
        let rng = pred(&mut d, RDFS_RANGE);
        let name_p = pred(&mut d, "name");
        let name_e = ent(&mut d, "name");
        let label = ent(&mut d, "Label");
        let alice = ent(&mut d, "alice");
        let bob = ent(&mut d, "bob");
        let lit = d.intern_literal(&Term::plain_literal("Alice").dict_key());
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(name_e, rng, label));
        s.insert(IdTriple::new(alice, name_p, lit));
        s.insert(IdTriple::new(alice, name_p, bob));

        let added = materialize(&mut d, &mut s);
        assert!(!s.exists(lit, typ, label), "a literal was typed");
        assert!(
            added.iter().all(|t| is_entity_id(t.sub)),
            "inferred a literal subject: {added:?}"
        );
        // The entity object is still typed.
        assert!(s.exists(bob, typ, label));
        assert_eq!(added.len(), 1);
    }

    #[test]
    fn domain_and_range_assign_types() {
        let mut d = Dictionary::new();
        let typ = pred(&mut d, RDF_TYPE);
        let dom = pred(&mut d, RDFS_DOMAIN);
        let rng = pred(&mut d, RDFS_RANGE);
        let works_p = pred(&mut d, "worksAt");
        let works_e = ent(&mut d, "worksAt");
        let (person, org) = (ent(&mut d, "Person"), ent(&mut d, "Org"));
        let (alice, acme) = (ent(&mut d, "alice"), ent(&mut d, "acme"));
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(works_e, dom, person));
        s.insert(IdTriple::new(works_e, rng, org));
        s.insert(IdTriple::new(alice, works_p, acme));

        materialize(&mut d, &mut s);
        assert!(s.exists(alice, typ, person)); // domain
        assert!(s.exists(acme, typ, org)); // range
    }

    #[test]
    fn no_schema_means_no_inference() {
        let mut d = Dictionary::new();
        let p = pred(&mut d, "knows");
        let (a, b) = (ent(&mut d, "a"), ent(&mut d, "b"));
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(a, p, b));
        assert!(materialize(&mut d, &mut s).is_empty());
        assert_eq!(s.triple_count(), 1);
    }

    // ---- user-defined rules ----------------------------------------------

    /// Seed a chain `a → b → c → d` under the `<ancestor>` predicate, returning
    /// the dictionary, store, and the four entity ids.
    fn ancestor_chain() -> (Dictionary, TripleStore, [u32; 4], u32) {
        let mut d = Dictionary::new();
        let mut s = TripleStore::new();
        let anc = pred(&mut d, "ancestor");
        let a = ent(&mut d, "a");
        let b = ent(&mut d, "b");
        let c = ent(&mut d, "c");
        let dd = ent(&mut d, "d");
        s.insert(IdTriple::new(a, anc, b));
        s.insert(IdTriple::new(b, anc, c));
        s.insert(IdTriple::new(c, anc, dd));
        (d, s, [a, b, c, dd], anc)
    }

    #[test]
    fn parse_rule_roundtrips_terms() {
        let r = Rule::parse(
            "anc",
            "?x <ancestor> ?y . ?y <ancestor> ?z => ?x <ancestor> ?z",
        )
        .unwrap();
        assert_eq!(r.body.len(), 2);
        assert_eq!(r.head.subject, PatternTerm::Var("x".into()));
        assert_eq!(r.head.predicate, PatternTerm::Const(Term::iri("ancestor")));
        assert_eq!(r.head.object, PatternTerm::Var("z".into()));
        assert!(r.enabled);
    }

    #[test]
    fn custom_transitive_rule_infers_closure_with_effect_count() {
        let (mut d, mut s, [a, b, c, dd], anc) = ancestor_chain();
        let mut rs = RuleSet::new();
        rs.add_rule(
            "anc",
            "?x <ancestor> ?y . ?y <ancestor> ?z => ?x <ancestor> ?z",
        )
        .unwrap();

        let added = rs.apply(&mut d, &mut s);
        // Transitive closure of a→b→c→d adds exactly a→c, a→d, b→d.
        assert_eq!(added.len(), 3);
        assert!(s.exists(a, anc, c));
        assert!(s.exists(a, anc, dd));
        assert!(s.exists(b, anc, dd));
        assert_eq!(rs.effect_count("anc"), Some(3));

        // Idempotent: a second pass infers nothing and the effect count resets.
        assert!(rs.apply(&mut d, &mut s).is_empty());
        assert_eq!(rs.effect_count("anc"), Some(0));
    }

    #[test]
    fn disabling_a_rule_stops_inference() {
        let (mut d, mut s, _ids, _anc) = ancestor_chain();
        let mut rs = RuleSet::new();
        rs.add_rule(
            "anc",
            "?x <ancestor> ?y . ?y <ancestor> ?z => ?x <ancestor> ?z",
        )
        .unwrap();

        assert!(rs.disable("anc"));
        let added = rs.apply(&mut d, &mut s);
        assert!(added.is_empty(), "a disabled rule must infer nothing");
        assert_eq!(rs.effect_count("anc"), Some(0));
        assert_eq!(s.triple_count(), 3, "store unchanged while disabled");

        // Re-enabling restores inference.
        assert!(rs.enable("anc"));
        assert_eq!(rs.apply(&mut d, &mut s).len(), 3);
        assert_eq!(rs.effect_count("anc"), Some(3));
    }

    #[test]
    fn rule_with_literal_head_and_listing() {
        let mut d = Dictionary::new();
        let mut s = TripleStore::new();
        let works = pred(&mut d, "worksAt");
        let alice = ent(&mut d, "alice");
        let bob = ent(&mut d, "bob");
        let acme = ent(&mut d, "acme");
        s.insert(IdTriple::new(alice, works, acme));
        s.insert(IdTriple::new(bob, works, acme));

        let mut rs = RuleSet::new();
        rs.add_rule("emp", "?x <worksAt> ?c => ?x <status> \"employed\"")
            .unwrap();
        let added = rs.apply(&mut d, &mut s);
        assert_eq!(added.len(), 2, "one status triple per worker");

        let status = d.predicate_id(&Term::iri("status").dict_key()).unwrap();
        let emp = d.term_id(&Term::plain_literal("employed")).unwrap();
        assert!(s.exists(alice, status, emp));
        assert!(s.exists(bob, status, emp));

        let listed = rs.list();
        assert_eq!(listed, vec![("emp".to_string(), true, 2)]);
    }

    #[test]
    fn duplicate_rule_name_and_remove() {
        let mut rs = RuleSet::new();
        rs.add_rule("r", "?x <p> ?y => ?x <q> ?y").unwrap();
        assert!(rs.add_rule("r", "?x <p> ?y => ?x <q> ?y").is_err());
        assert!(rs.remove("r"));
        assert!(!rs.remove("r"));
        // Now re-adding the name is fine.
        assert!(rs.add_rule("r", "?x <p> ?y => ?x <q> ?y").is_ok());
    }

    #[test]
    fn malformed_rules_are_rejected() {
        let mut rs = RuleSet::new();
        assert!(rs.add_rule("a", "?x <p> ?y").is_err(), "missing =>");
        assert!(
            rs.add_rule("b", "?x ?p ?y => ?x <q> ?y").is_err(),
            "variable predicate"
        );
        assert!(
            rs.add_rule("c", "?x <p> ?y => ?x <q> ?y . ?a <r> ?b")
                .is_err(),
            "multi-pattern head"
        );
        assert!(
            rs.add_rule("d", "?x <p> => ?x <q> ?y").is_err(),
            "incomplete pattern"
        );
    }

    /// Review ext#3: a literal or blank node in a subPropertyOf position
    /// (legal data — OWL writes `:p rdfs:subPropertyOf [ owl:inverseOf :q ]`)
    /// was interned as a predicate, and data was copied under it
    /// (`<a> "lit" <b>`, which no reader loads back). Only IRIs are
    /// properties; subPropertyOf stays transitive through a blank node.
    #[test]
    fn subproperty_never_makes_a_literal_or_blank_node_a_predicate() {
        let mut d = Dictionary::new();
        let spo = pred(&mut d, RDFS_SUBPROP);
        let p_pred = pred(&mut d, "http://ex/p");
        let p_ent = ent(&mut d, "http://ex/p");
        let p2_pred = pred(&mut d, "http://ex/p2");
        let p2_ent = ent(&mut d, "http://ex/p2");
        let q_ent = ent(&mut d, "http://ex/q");
        let lit_key = Term::plain_literal("lit").dict_key();
        let lit = d.intern_literal(&lit_key);
        let blank_key = Term::blank("b").dict_key();
        let blank = d.intern_entity(&blank_key);
        let (a, b, c) = (
            ent(&mut d, "http://ex/a"),
            ent(&mut d, "http://ex/b"),
            ent(&mut d, "http://ex/c"),
        );
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(p_ent, spo, lit));
        s.insert(IdTriple::new(p2_ent, spo, blank));
        s.insert(IdTriple::new(blank, spo, q_ent));
        s.insert(IdTriple::new(a, p_pred, b));
        s.insert(IdTriple::new(a, p2_pred, c));

        let added = materialize(&mut d, &mut s);
        assert!(
            d.predicate_id(&lit_key).is_none(),
            "a literal became a predicate"
        );
        assert!(
            d.predicate_id(&blank_key).is_none(),
            "a blank node became a predicate"
        );
        for t in &added {
            let key = d.pred(t.pred).unwrap();
            assert!(
                is_iri_key(&key),
                "inferred under the predicate {key}: {added:?}"
            );
        }
        // p2 ⊑ _:b ⊑ q: p2 ⊑ q, and a p2 c gives a q c.
        assert!(s.exists(p2_ent, spo, q_ent));
        let q_pred = d
            .predicate_id(&Term::iri("http://ex/q").dict_key())
            .unwrap();
        assert!(s.exists(a, q_pred, c));
        assert_eq!(added.len(), 2, "{added:?}");
        // A literal interned as a predicate by an older version is not used.
        let mut d2 = d.clone();
        d2.intern_predicate(&lit_key);
        let mut s2 = s.clone();
        assert!(materialize(&mut d2, &mut s2).is_empty());
    }

    /// Review ext#4: the textual rule parser sliced its input by bytes and
    /// panicked on a multi-byte character where a token was expected or
    /// after a `\` (`é <p> ?y …`, `"\é"`). Every malformed text is an error;
    /// well-formed non-ASCII rules work.
    #[test]
    fn non_ascii_rule_text_is_an_error_not_a_panic() {
        let mut rs = RuleSet::new();
        for bad in [
            "é <p> ?y => ?x <q> ?y",
            "?x <p> \"\\é\" => ?x <q> ?x",
            "?x <p> \"\\é => ?x <q> ?x",
            "?x <p> \"é => ?x <q> ?x",
            "?x <p> ?y => ?x <q> \"x\"@é",
            "?x <p> ?y => ?x <q> \"x\"é",
            "?x <p> ?y => ?x <q> ?y é",
            "?x <p> ?é! => ?x <q> ?x",
            "?x <p> ?y => ?x <q> _:é!",
            "?x <a b> ?y => ?x <q> ?y",
            "?x <p> \"x\\u00e9\" => ?x <q> ?x",
            "?x <p> \"x\"^^<a b> => ?x <q> ?x",
            "?x <p> \"x\" ?y => ?x <q> ?x ✓",
        ] {
            assert!(rs.add_rule("bad", bad).is_err(), "{bad}");
        }
        assert!(rs.rules().is_empty());

        let mut d = Dictionary::new();
        let mut s = TripleStore::new();
        let p = pred(&mut d, "http://ex/é");
        let x = ent(&mut d, "http://ex/x");
        let v = d.intern_literal(&Term::plain_literal("é\"").dict_key());
        s.insert(IdTriple::new(x, p, v));
        rs.add_rule(
            "ok",
            "?x <http://ex/é> \"é\\\"\" . => ?x <http://ex/q> \"ü\\n\"@de-CH",
        )
        .unwrap();
        assert_eq!(rs.apply(&mut d, &mut s).len(), 1);
        let q = d
            .predicate_id(&Term::iri("http://ex/q").dict_key())
            .unwrap();
        let lit = Term::Literal {
            value: "ü\n".into(),
            datatype: None,
            lang: Some("de-CH".into()),
        };
        assert!(s.exists(x, q, d.term_id(&lit).unwrap()));
    }

    /// Review ext#7: the textual rules' join compared every binding with
    /// every stored pair, so a two-hop rule's cost grew with the square of
    /// the data. The hash join's work (pairs read plus bindings built) grows
    /// with the data.
    #[test]
    fn rule_joins_scale_linearly() {
        let work = |n: usize| {
            let mut d = Dictionary::new();
            let mut s = TripleStore::new();
            let p = pred(&mut d, "p");
            let ids: Vec<u32> = (0..=n).map(|i| ent(&mut d, &format!("n{i}"))).collect();
            for w in ids.windows(2) {
                s.insert(IdTriple::new(w[0], p, w[1]));
            }
            let mut rs = RuleSet::new();
            rs.add_rule("two", "?x <p> ?y . ?y <p> ?z => ?x <q> ?z")
                .unwrap();
            assert_eq!(rs.apply(&mut d, &mut s).len(), n - 1);
            rs.last_work()
        };
        let (small, large) = (work(500), work(4000));
        assert!(small > 0);
        assert!(large <= 9 * small, "8x the data: {small} -> {large}");
    }

    /// Joins of every shape: a constant subject, a variable bound twice in
    /// one pattern, a pattern binding nothing new, a cross product.
    #[test]
    fn rule_joins_of_every_shape() {
        let mut d = Dictionary::new();
        let mut s = TripleStore::new();
        let p = pred(&mut d, "p");
        let r = pred(&mut d, "r");
        let (a, b, c) = (ent(&mut d, "a"), ent(&mut d, "b"), ent(&mut d, "c"));
        s.insert(IdTriple::new(a, p, b));
        s.insert(IdTriple::new(b, p, b));
        s.insert(IdTriple::new(c, r, a));
        let run = |rule: &str, d: &mut Dictionary, s: &mut TripleStore| {
            let mut st = s.clone();
            let mut rs = RuleSet::new();
            rs.add_rule("x", rule).unwrap();
            let n = rs.apply(d, &mut st).len();
            (n, st)
        };
        // A self loop.
        assert_eq!(run("?x <p> ?x => ?x <self> ?x", &mut d, &mut s).0, 1);
        // A constant subject.
        assert_eq!(run("<a> <p> ?y => ?y <q> <a>", &mut d, &mut s).0, 1);
        // Both ends bound by earlier patterns.
        assert_eq!(
            run(
                "?x <p> ?y . ?c <r> ?x . ?x <p> ?y => ?c <q> ?y",
                &mut d,
                &mut s
            )
            .0,
            1
        );
        // A cross product (no shared variable).
        assert_eq!(
            run("?x <p> ?y . ?c <r> ?z => ?c <q> ?y", &mut d, &mut s).0,
            1
        );
        // A constant pattern that holds, and one that does not.
        assert_eq!(
            run("<c> <r> <a> . ?x <p> ?y => ?x <q> ?y", &mut d, &mut s).0,
            2
        );
        assert_eq!(
            run("<c> <r> <b> . ?x <p> ?y => ?x <q> ?y", &mut d, &mut s).0,
            0
        );
        // An unknown constant matches nothing.
        assert_eq!(run("<zz> <p> ?y => ?y <q> <a>", &mut d, &mut s).0, 0);
        // A head variable the body never binds infers nothing.
        assert_eq!(run("?x <p> ?y => ?x <q> ?w", &mut d, &mut s).0, 0);
    }
}
