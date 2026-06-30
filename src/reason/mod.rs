//! RDFS entailment by forward-chaining materialization.
//!
//! Corresponds to gStore's `src/Reason`. Given a store that mixes data and an
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
//! | range  | `p range c`,  `x p y` | `y a c` |
//!
//! Note the id-space subtlety: a property used as the *subject* of a schema
//! triple (`p rdfs:subPropertyOf q`) is interned as an **entity**, while the same
//! property in data (`x p y`) is a **predicate** — two different id spaces sharing
//! the dictionary key. [`ent_to_pred`] bridges them through the dictionary.

use std::collections::HashMap;

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::PredId;
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
    let type_p = if needs_type {
        Some(dict.intern_predicate(&key(RDF_TYPE)))
    } else {
        dict.predicate_id(&key(RDF_TYPE))
    };

    // A subproperty target may only ever appear as a schema object (e.g.
    // `:mother rdfs:subPropertyOf :parent` with no `:parent` data triple), so it
    // has no predicate id yet. Intern a predicate id for every property named in
    // a subPropertyOf statement so [`ent_to_pred`] can resolve the target.
    if let Some(spo) = spo_p {
        let prop_ents: Vec<u32> = store
            .so_by_p(spo)
            .iter()
            .flat_map(|&(p, q)| [p, q])
            .collect();
        let keys: Vec<String> = prop_ents
            .iter()
            .filter_map(|&e| dict.id_to_string(e).map(str::to_owned))
            .collect();
        for k in keys {
            dict.intern_predicate(&k);
        }
    }

    let mut added_all: Vec<IdTriple> = Vec::new();
    loop {
        let mut new: Vec<IdTriple> = Vec::new();
        gather(dict, store, type_p, sco_p, spo_p, dom_p, rng_p, &mut new);
        let mut added = 0usize;
        for t in new {
            if store.insert(t) {
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
            let (Some(p_pred), Some(q_pred)) =
                (ent_to_pred(dict, p_ent), ent_to_pred(dict, q_ent))
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
    if let (Some(rng), Some(tp)) = (rng_p, type_p) {
        for (p_ent, c) in store.so_by_p(rng) {
            if let Some(p_pred) = ent_to_pred(dict, p_ent) {
                for (_x, y) in store.so_by_p(p_pred) {
                    new.push(IdTriple::new(y, tp, c));
                }
            }
        }
    }
}

/// Map an entity id (a property used as a schema subject/object) to its
/// predicate id, via the shared dictionary key. `None` if that IRI is never used
/// as a predicate in the data.
fn ent_to_pred(dict: &Dictionary, ent: u32) -> Option<PredId> {
    let key = dict.id_to_string(ent)?;
    dict.predicate_id(key)
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

impl PatternTerm {
    /// The required surface string this position imposes under a binding:
    /// `Some(s)` ⇒ must equal `s`; `None` ⇒ a free (unbound) variable.
    fn required<'a>(&'a self, binding: &'a HashMap<String, String>) -> Option<String> {
        match self {
            PatternTerm::Const(t) => Some(t.dict_key()),
            PatternTerm::Var(name) => binding.get(name).cloned(),
        }
    }
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
    /// per satisfying binding, returning the concrete head triples (deduplicated
    /// by the store on insert). Pure read — interning/insertion is the caller's.
    fn derive<S: TripleSource>(&self, dict: &Dictionary, store: &S) -> Vec<Triple> {
        // Conjunctive nested-loop join: start with the empty binding and extend
        // it pattern by pattern.
        let mut bindings: Vec<HashMap<String, String>> = vec![HashMap::new()];
        for pat in &self.body {
            // The predicate must be a constant IRI resolvable to a predicate id;
            // an unknown predicate matches nothing.
            let PatternTerm::Const(pred_term) = &pat.predicate else {
                return Vec::new();
            };
            let Some(pred_id) = dict.predicate_id(&pred_term.dict_key()) else {
                return Vec::new();
            };
            let pairs = store.so_by_p(pred_id);
            let mut next: Vec<HashMap<String, String>> = Vec::new();
            for b in &bindings {
                let req_s = pat.subject.required(b);
                let req_o = pat.object.required(b);
                for &(s_id, o_id) in &pairs {
                    let (Some(s_str), Some(o_str)) =
                        (dict.id_to_string(s_id), dict.id_to_string(o_id))
                    else {
                        continue;
                    };
                    if req_s.as_deref().is_some_and(|r| r != s_str) {
                        continue;
                    }
                    if req_o.as_deref().is_some_and(|r| r != o_str) {
                        continue;
                    }
                    let mut nb = b.clone();
                    let mut ok = true;
                    if let PatternTerm::Var(name) = &pat.subject {
                        bind_var(&mut nb, name, s_str, &mut ok);
                    }
                    if ok {
                        if let PatternTerm::Var(name) = &pat.object {
                            bind_var(&mut nb, name, o_str, &mut ok);
                        }
                    }
                    if ok {
                        next.push(nb);
                    }
                }
            }
            bindings = next;
            if bindings.is_empty() {
                break;
            }
        }

        let mut out = Vec::new();
        for b in &bindings {
            if let Some(t) = instantiate(&self.head, b) {
                out.push(t);
            }
        }
        out
    }
}

/// Bind variable `name` to `value` in `binding`, requiring consistency if it is
/// already bound (so a variable repeated within or across patterns must take the
/// same value). Clears `ok` on a contradiction.
fn bind_var(binding: &mut HashMap<String, String>, name: &str, value: &str, ok: &mut bool) {
    match binding.get(name) {
        Some(existing) if existing != value => *ok = false,
        Some(_) => {}
        None => {
            binding.insert(name.to_owned(), value.to_owned());
        }
    }
}

/// Instantiate a pattern into a concrete triple under a binding, or `None` if a
/// variable is unbound or the resulting subject/predicate is not a valid term
/// (a literal subject, or a non-IRI predicate).
fn instantiate(pat: &TriplePattern, binding: &HashMap<String, String>) -> Option<Triple> {
    let subject = resolve_term(&pat.subject, binding)?;
    let predicate = resolve_term(&pat.predicate, binding)?;
    let object = resolve_term(&pat.object, binding)?;
    // A subject cannot be a literal; a predicate must be an IRI.
    if subject.is_literal() || !matches!(predicate, Term::Iri(_)) {
        return None;
    }
    Some(Triple::new(subject, predicate, object))
}

/// Resolve a pattern position to a concrete term under a binding.
fn resolve_term(pt: &PatternTerm, binding: &HashMap<String, String>) -> Option<Term> {
    match pt {
        PatternTerm::Const(t) => Some(t.clone()),
        PatternTerm::Var(name) => parse_term(binding.get(name)?),
    }
}

/// A named collection of forward-chaining rules (gStore's `reason_rule_files/`).
/// Rules keep insertion order; names are unique.
#[derive(Debug, Default, Clone)]
pub struct RuleSet {
    rules: Vec<Rule>,
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
        for r in &mut self.rules {
            r.effect_count = 0;
        }
        let mut added_all: Vec<IdTriple> = Vec::new();
        loop {
            let mut round_added = 0usize;
            for i in 0..self.rules.len() {
                if !self.rules[i].enabled {
                    continue;
                }
                // Read phase: derive concrete head triples against the current
                // store (immutable borrow of dict + store).
                let derived = self.rules[i].derive(dict, store);
                // Write phase: intern + insert; count only genuinely-new triples.
                for t in derived {
                    let id = IdTriple::new(
                        dict.intern_entity(&t.subject.dict_key()),
                        dict.intern_predicate(&t.predicate.dict_key()),
                        dict.intern_term(&t.object),
                    );
                    if store.insert(id) {
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
    for chunk in toks.chunks_exact(3) {
        patterns.push(TriplePattern {
            subject: chunk[0].clone(),
            predicate: chunk[1].clone(),
            object: chunk[2].clone(),
        });
    }
    Ok(patterns)
}

/// Tokenize a rule side into [`PatternTerm`]s, skipping `.` triple separators.
fn tokenize(s: &str) -> Result<Vec<PatternTerm>> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'.' {
            // A standalone triple separator (IRIs/literals keep their dots inside
            // the bracketed/quoted token below, so a bare '.' is punctuation).
            i += 1;
            continue;
        }
        let start = i;
        if c == b'<' {
            // <iri>
            i += 1;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            if i >= bytes.len() {
                return Err(GStoreError::Database("unterminated <iri> in rule".into()));
            }
            i += 1; // consume '>'
            let raw = &s[start..i];
            out.push(PatternTerm::Const(parse_term(raw).ok_or_else(|| {
                GStoreError::Database(format!("invalid IRI term '{raw}'"))
            })?));
        } else if c == b'"' {
            // "literal" with optional ^^<dt> or @lang suffix.
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2, // skip escaped char
                    b'"' => break,
                    _ => i += 1,
                }
            }
            if i >= bytes.len() {
                return Err(GStoreError::Database("unterminated literal in rule".into()));
            }
            i += 1; // consume closing '"'
                    // optional @lang or ^^<dt>
            if i < bytes.len() && bytes[i] == b'@' {
                i += 1;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'.' {
                    i += 1;
                }
            } else if i + 1 < bytes.len() && bytes[i] == b'^' && bytes[i + 1] == b'^' {
                i += 2;
                if i < bytes.len() && bytes[i] == b'<' {
                    while i < bytes.len() && bytes[i] != b'>' {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1; // consume '>'
                    }
                }
            }
            let raw = &s[start..i];
            out.push(PatternTerm::Const(parse_term(raw).ok_or_else(|| {
                GStoreError::Database(format!("invalid literal term '{raw}'"))
            })?));
        } else if c == b'?' {
            // ?variable
            i += 1;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'.' {
                i += 1;
            }
            let name = &s[start + 1..i];
            if name.is_empty() {
                return Err(GStoreError::Database("empty variable name '?'".into()));
            }
            out.push(PatternTerm::Var(name.to_owned()));
        } else if c == b'_' && i + 1 < bytes.len() && bytes[i + 1] == b':' {
            // _:blank
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'.' {
                i += 1;
            }
            let raw = &s[start..i];
            out.push(PatternTerm::Const(parse_term(raw).ok_or_else(|| {
                GStoreError::Database(format!("invalid blank node '{raw}'"))
            })?));
        } else {
            return Err(GStoreError::Database(format!(
                "unexpected token starting at '{}' (use <iri>, ?var, \"literal\", or _:blank)",
                &s[start..(start + 1).min(s.len())]
            )));
        }
    }
    Ok(out)
}

/// Parse one term from its N-Triples surface form (the inverse of
/// [`Term::dict_key`]): `<iri>`, `_:blank`, or a literal `"v"`/`"v"@lang`/
/// `"v"^^<dt>`. Returns `None` on malformed input.
fn parse_term(s: &str) -> Option<Term> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        return Some(Term::iri(inner));
    }
    if let Some(label) = s.strip_prefix("_:") {
        if label.is_empty() {
            return None;
        }
        return Some(Term::blank(label));
    }
    if s.starts_with('"') {
        // Find the closing quote, honoring backslash escapes.
        let bytes = s.as_bytes();
        let mut i = 1usize;
        let mut value = String::new();
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    i += 1;
                    let e = *bytes.get(i)?;
                    value.push(match e {
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'"' => '"',
                        b'\\' => '\\',
                        other => other as char,
                    });
                    i += 1;
                }
                b'"' => break,
                _ => {
                    // Push this UTF-8 char whole.
                    let ch = s[i..].chars().next()?;
                    value.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            return None;
        }
        i += 1; // past closing quote
        let rest = &s[i..];
        if let Some(lang) = rest.strip_prefix('@') {
            return Some(Term::Literal {
                value,
                datatype: None,
                lang: Some(lang.to_owned()),
            });
        }
        if let Some(dt) = rest.strip_prefix("^^") {
            let dt = dt.strip_prefix('<').and_then(|r| r.strip_suffix('>'))?;
            return Some(Term::typed_literal(value, dt));
        }
        if rest.is_empty() {
            return Some(Term::plain_literal(value));
        }
        return None;
    }
    None
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
            rs.add_rule("c", "?x <p> ?y => ?x <q> ?y . ?a <r> ?b").is_err(),
            "multi-pattern head"
        );
        assert!(
            rs.add_rule("d", "?x <p> => ?x <q> ?y").is_err(),
            "incomplete pattern"
        );
    }
}
