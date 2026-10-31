//! JSON-LD parser (a common subset of JSON-LD 1.1).
//!
//! JSON-LD (<https://www.w3.org/TR/json-ld11/>) serializes an RDF graph as JSON.
//! To stay dependency-free (the whole crate hand-writes its parsers), this module
//! uses the crate's own JSON reader ([`crate::json`]) — no `serde`, no
//! `serde_json` — and a pragmatic expansion-to-triples pass.
//!
//! # Supported subset
//!
//! * `@context`: term → IRI mappings (string form), prefix expansion of compact
//!   IRIs (`prefix:local`), `@vocab`, `@base`, `@language`, and the expanded term
//!   definition form (`{"@id": …, "@type": …, "@container": "@list", "@language": …}`).
//!   Contexts may be a single object or an array of objects (processed in order).
//! * `@id`: node identifier (IRI resolved against `@base`, or a `_:` blank node).
//!   Absent → a fresh blank node is minted.
//! * `@type`: on a node object → one or more `rdf:type` triples (IRIs expanded
//!   against the active vocab/context).
//! * value objects `{"@value": …, "@type"/"@language": …}`: typed and
//!   language-tagged literals.
//! * native JSON scalars: strings → plain literals (or a term's coerced type),
//!   numbers → `xsd:integer` / `xsd:double`, booleans → `xsd:boolean`.
//! * term coercion via a term's `@type` (`@id`, `@vocab`, or a datatype IRI) and
//!   `@language`.
//! * arrays of values, nested node objects (each becomes its own subject), and
//!   node references (`{"@id": …}`).
//! * `@list` (both the explicit `{"@list": […]}` form and a term with
//!   `@container: "@list"`) expanded to an `rdf:first`/`rdf:rest`/`rdf:nil` chain.
//! * `@graph`: its member nodes are emitted.
//!
//! # Deliberately omitted
//!
//! Because the crate's [`Triple`] type carries no graph slot, **named graphs are
//! flattened into the default graph**: a node with both `@id` and `@graph` has its
//! own properties emitted normally, and every node inside its `@graph` is emitted
//! as an ordinary (default-graph) triple. Also unsupported: remote `@context`
//! references (a string context is ignored, not fetched), `@reverse`, `@index`,
//! `@nest`, `@container` values other than `@list`, `@json` typed values, framing,
//! and full IRI canonicalization/compaction. Numeric literals keep their JSON
//! lexical form rather than the JSON-LD canonical lexical form.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::json::Json;
use crate::model::{Term, Triple};
use crate::parser::sparql::ast::{xsd, RDF_TYPE};
use crate::parser::stream::BlankLabeler;

/// The `rdf:` namespace (for `rdf:first`/`rdf:rest`/`rdf:nil` list chains).
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// Parse a JSON-LD document into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    parse_str_with(content, &mut BlankLabeler::keep())
}

/// Parse a JSON-LD document, naming blank nodes with `labels`. The document
/// is parsed whole; the bulk loaders refuse one larger than
/// [`ImportOptions::max_document_bytes`](crate::parser::import::ImportOptions).
pub fn parse_str_with(content: &str, labels: &mut BlankLabeler) -> Result<Vec<Triple>> {
    let json = crate::json::parse(content).map_err(|e| GStoreError::RdfParse {
        line: e.line,
        msg: e.msg,
    })?;
    let mut proc = Processor {
        triples: Vec::new(),
        labels: std::mem::take(labels),
    };
    let done = proc.process_top(&json, &Context::default());
    *labels = proc.labels;
    done?;
    Ok(proc.triples)
}

/// Parse a JSON-LD file into triples.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let content = crate::parser::import::read_document(path.as_ref())?;
    parse_str(&content)
}

// The JSON reader is the crate-wide one in `crate::json`.

// ---- active context -------------------------------------------------------

/// A term (or prefix) definition from an `@context`.
#[derive(Debug, Clone, Default)]
struct TermDef {
    /// The IRI the term expands to.
    id: Option<String>,
    /// `@type` coercion: `"@id"`, `"@vocab"`, or a datatype IRI.
    type_: Option<String>,
    /// `@container`: only `"@list"` is acted upon.
    container: Option<String>,
    /// A per-term default `@language`.
    language: Option<String>,
}

/// The active JSON-LD context threaded through processing.
#[derive(Debug, Clone, Default)]
struct Context {
    base: Option<String>,
    vocab: Option<String>,
    language: Option<String>,
    terms: HashMap<String, TermDef>,
}

// ---- triple extraction ----------------------------------------------------

#[derive(Default)]
struct Processor {
    triples: Vec<Triple>,
    /// Names author-written `_:` labels and minted nodes (see
    /// [`BlankLabeler`]: a minted label never equals an author label).
    labels: BlankLabeler,
}

impl Processor {
    /// Mint a fresh blank node.
    fn fresh_blank(&mut self) -> Term {
        Term::Blank(self.labels.fresh())
    }

    /// Turn an expanded string into a [`Term`] (a blank node when it begins
    /// `_:`, named through the labeler).
    fn term(&self, expanded: &str) -> Term {
        match to_term(expanded) {
            Term::Blank(l) => Term::Blank(self.labels.explicit(&l)),
            other => other,
        }
    }

    fn push(&mut self, s: Term, p: Term, o: Term) {
        self.triples.push(Triple::new(s, p, o));
    }

    /// Entry point: a document is a node object, an array of them, or an object
    /// carrying `@graph`.
    fn process_top(&mut self, json: &Json, base: &Context) -> Result<()> {
        match json {
            Json::Arr(items) => {
                for it in items {
                    self.process_top(it, base)?;
                }
                Ok(())
            }
            Json::Obj(pairs) => {
                let ctx = self.active_context(base, pairs)?;
                self.process_node(pairs, &ctx)?;
                Ok(())
            }
            _ => Err(rdf_err("JSON-LD document must be an object or array")),
        }
    }

    /// Build the active context by layering an object's `@context` on `parent`.
    fn active_context(&self, parent: &Context, pairs: &[(String, Json)]) -> Result<Context> {
        match get(pairs, "@context") {
            Some(c) => self.build_context(parent, c),
            None => Ok(parent.clone()),
        }
    }

    fn build_context(&self, parent: &Context, ctx_json: &Json) -> Result<Context> {
        let mut ctx = parent.clone();
        match ctx_json {
            // Remote contexts are not fetched (documented limitation).
            Json::Str(_) => {}
            // A null context resets everything.
            Json::Null => ctx = Context::default(),
            Json::Arr(items) => {
                for it in items {
                    ctx = self.build_context(&ctx, it)?;
                }
            }
            Json::Obj(defs) => {
                for (k, v) in defs {
                    match k.as_str() {
                        "@base" => ctx.base = as_opt_string(v),
                        "@vocab" => ctx.vocab = as_opt_string(v),
                        "@language" => ctx.language = as_opt_string(v),
                        _ if k.starts_with('@') => {}
                        // `"term": null` explicitly removes the mapping (JSON-LD
                        // context term deletion) rather than defining a dead term.
                        _ if matches!(v, Json::Null) => {
                            ctx.terms.remove(k);
                        }
                        _ => {
                            ctx.terms.insert(k.clone(), self.build_term_def(&ctx, v));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(ctx)
    }

    fn build_term_def(&self, ctx: &Context, v: &Json) -> TermDef {
        let mut def = TermDef::default();
        match v {
            Json::Str(s) => def.id = Some(expand_iri(ctx, s, true, false)),
            Json::Obj(pairs) => {
                if let Some(Json::Str(id)) = get(pairs, "@id") {
                    def.id = Some(expand_iri(ctx, id, true, false));
                }
                if let Some(Json::Str(t)) = get(pairs, "@type") {
                    def.type_ = Some(if t == "@id" || t == "@vocab" {
                        t.clone()
                    } else {
                        expand_iri(ctx, t, true, false)
                    });
                }
                if let Some(Json::Str(c)) = get(pairs, "@container") {
                    def.container = Some(c.clone());
                }
                if let Some(Json::Str(l)) = get(pairs, "@language") {
                    def.language = Some(l.clone());
                }
            }
            _ => {}
        }
        def
    }

    /// Process a node object, returning its subject term.
    fn process_node(&mut self, pairs: &[(String, Json)], parent_ctx: &Context) -> Result<Term> {
        let ctx = self.active_context(parent_ctx, pairs)?;

        // Subject identity.
        let subject = match get(pairs, "@id") {
            Some(Json::Str(s)) => self.term(&expand_iri(&ctx, s, false, true)),
            _ => self.fresh_blank(),
        };

        // @type → rdf:type triples.
        if let Some(t) = get(pairs, "@type") {
            for tv in item_refs(t) {
                if let Json::Str(s) = tv {
                    let iri = expand_iri(&ctx, s, true, false);
                    if is_absolute_iri(&iri) || iri.starts_with("_:") {
                        let ty = self.term(&iri);
                        self.push(subject.clone(), rdf_type(), ty);
                    }
                }
            }
        }

        // Remaining properties.
        for (key, val) in pairs {
            if key.starts_with('@') {
                if key == "@graph" {
                    self.process_graph(val, &ctx)?;
                }
                continue;
            }
            let pred_iri = expand_iri(&ctx, key, true, false);
            if !is_absolute_iri(&pred_iri) {
                continue; // drop keys that do not expand to an absolute IRI
            }
            let predicate = Term::Iri(pred_iri);
            let def = ctx.terms.get(key).cloned().unwrap_or_default();
            self.emit_values(&subject, &predicate, val, &ctx, &def)?;
        }
        Ok(subject)
    }

    fn process_graph(&mut self, g: &Json, ctx: &Context) -> Result<()> {
        match g {
            Json::Arr(items) => {
                for it in items {
                    if let Json::Obj(p) = it {
                        self.process_node(p, ctx)?;
                    }
                }
            }
            Json::Obj(p) => {
                self.process_node(p, ctx)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn emit_values(
        &mut self,
        subj: &Term,
        pred: &Term,
        val: &Json,
        ctx: &Context,
        def: &TermDef,
    ) -> Result<()> {
        // A term with @container:@list turns an array/scalar into an RDF list.
        if def.container.as_deref() == Some("@list") {
            let already_list = matches!(val, Json::Obj(p) if get(p, "@list").is_some());
            if !already_list {
                let items = item_refs(val);
                let head = self.build_list(&items, ctx, def)?;
                self.push(subj.clone(), pred.clone(), head);
                return Ok(());
            }
        }
        match val {
            Json::Arr(items) => {
                for it in items {
                    self.emit_single(subj, pred, it, ctx, def)?;
                }
            }
            other => self.emit_single(subj, pred, other, ctx, def)?,
        }
        Ok(())
    }

    fn emit_single(
        &mut self,
        subj: &Term,
        pred: &Term,
        val: &Json,
        ctx: &Context,
        def: &TermDef,
    ) -> Result<()> {
        match val {
            Json::Obj(pairs) => {
                if let Some(v) = get(pairs, "@value") {
                    let lit = self.value_object_to_literal(pairs, v, ctx, def)?;
                    self.push(subj.clone(), pred.clone(), lit);
                } else if let Some(list) = get(pairs, "@list") {
                    let items = item_refs(list);
                    let head = self.build_list(&items, ctx, def)?;
                    self.push(subj.clone(), pred.clone(), head);
                } else {
                    let obj = self.process_node(pairs, ctx)?;
                    self.push(subj.clone(), pred.clone(), obj);
                }
            }
            Json::Str(s) => {
                let obj = self.string_to_term(s, ctx, def);
                self.push(subj.clone(), pred.clone(), obj);
            }
            Json::Num(n) => self.push(subj.clone(), pred.clone(), number_to_literal(n)),
            Json::Bool(b) => self.push(subj.clone(), pred.clone(), bool_to_literal(*b)),
            Json::Null => {}
            Json::Arr(_) => self.emit_values(subj, pred, val, ctx, def)?,
        }
        Ok(())
    }

    /// Expand a JSON array into an `rdf:first`/`rdf:rest`/`rdf:nil` chain,
    /// returning the head (`rdf:nil` for an empty list).
    fn build_list(&mut self, items: &[&Json], ctx: &Context, def: &TermDef) -> Result<Term> {
        let nil = Term::iri(format!("{RDF_NS}nil"));
        if items.is_empty() {
            return Ok(nil);
        }
        let first = Term::iri(format!("{RDF_NS}first"));
        let rest = Term::iri(format!("{RDF_NS}rest"));
        let head = self.fresh_blank();
        let mut current = head.clone();
        let n = items.len();
        for (i, item) in items.iter().enumerate() {
            let obj = self.list_item_term(item, ctx, def)?;
            self.push(current.clone(), first.clone(), obj);
            let next = if i + 1 < n {
                self.fresh_blank()
            } else {
                nil.clone()
            };
            self.push(current.clone(), rest.clone(), next.clone());
            current = next;
        }
        Ok(head)
    }

    fn list_item_term(&mut self, val: &Json, ctx: &Context, def: &TermDef) -> Result<Term> {
        match val {
            Json::Obj(pairs) => {
                if let Some(v) = get(pairs, "@value") {
                    self.value_object_to_literal(pairs, v, ctx, def)
                } else if let Some(list) = get(pairs, "@list") {
                    let items = item_refs(list);
                    self.build_list(&items, ctx, def)
                } else {
                    self.process_node(pairs, ctx)
                }
            }
            Json::Str(s) => Ok(self.string_to_term(s, ctx, def)),
            Json::Num(n) => Ok(number_to_literal(n)),
            Json::Bool(b) => Ok(bool_to_literal(*b)),
            Json::Null => Ok(Term::iri(format!("{RDF_NS}nil"))),
            Json::Arr(_) => Err(rdf_err("nested arrays inside @list are unsupported")),
        }
    }

    /// A bare JSON string, honoring the term's `@type`/`@language` coercion.
    fn string_to_term(&self, s: &str, ctx: &Context, def: &TermDef) -> Term {
        match def.type_.as_deref() {
            Some("@id") => self.term(&expand_iri(ctx, s, false, true)),
            Some("@vocab") => self.term(&expand_iri(ctx, s, true, false)),
            Some(dt) => Term::typed_literal(s, dt),
            None => {
                let lang = def.language.clone().or_else(|| ctx.language.clone());
                make_plain(s, lang)
            }
        }
    }

    fn value_object_to_literal(
        &self,
        pairs: &[(String, Json)],
        v: &Json,
        ctx: &Context,
        def: &TermDef,
    ) -> Result<Term> {
        let value = match v {
            Json::Str(s) => s.clone(),
            Json::Num(n) => n.clone(),
            Json::Bool(b) => bool_lexical(*b).to_string(),
            Json::Null => String::new(),
            _ => return Err(rdf_err("@value must be a scalar")),
        };
        if let Some(Json::Str(t)) = get(pairs, "@type") {
            let dt = expand_iri(ctx, t, true, false);
            return Ok(Term::typed_literal(value, dt));
        }
        if let Some(Json::Str(l)) = get(pairs, "@language") {
            return Ok(Term::Literal {
                value,
                datatype: None,
                lang: Some(l.clone()),
            });
        }
        // No explicit @type/@language: native scalars keep their xsd type.
        match v {
            Json::Num(n) => return Ok(number_to_literal(n)),
            Json::Bool(b) => return Ok(bool_to_literal(*b)),
            _ => {}
        }
        let lang = def.language.clone().or_else(|| ctx.language.clone());
        Ok(make_plain(&value, lang))
    }
}

fn rdf_type() -> Term {
    Term::iri(RDF_TYPE)
}

/// Expand a value to an IRI. `vocab` enables term lookup + `@vocab` prefixing
/// (predicate/type position); `document` enables `@base` resolution (`@id` position).
fn expand_iri(ctx: &Context, value: &str, vocab: bool, document: bool) -> String {
    if value.is_empty() {
        return match &ctx.base {
            Some(b) if document => b.clone(),
            _ => value.to_string(),
        };
    }
    if value.starts_with('@') || value.starts_with("_:") {
        return value.to_string();
    }
    // Term lookup only applies in vocab position.
    if vocab {
        if let Some(def) = ctx.terms.get(value) {
            if let Some(id) = &def.id {
                return id.clone();
            }
        }
    }
    // Compact IRI `prefix:local`.
    if let Some((prefix, suffix)) = value.split_once(':') {
        if suffix.starts_with("//") || prefix == "_" {
            return value.to_string();
        }
        if let Some(def) = ctx.terms.get(prefix) {
            if let Some(id) = &def.id {
                return format!("{id}{suffix}");
            }
        }
        // Already an absolute IRI with some other scheme.
        return value.to_string();
    }
    if vocab {
        if let Some(v) = &ctx.vocab {
            return format!("{v}{value}");
        }
    }
    if document {
        if let Some(b) = &ctx.base {
            return resolve_against_base(b, value);
        }
    }
    value.to_string()
}

/// Turn an expanded string into a [`Term`] (blank node when it begins `_:`).
fn to_term(expanded: &str) -> Term {
    match expanded.strip_prefix("_:") {
        Some(label) => Term::Blank(label.to_string()),
        None => Term::Iri(expanded.to_string()),
    }
}

fn number_to_literal(n: &str) -> Term {
    if n.contains('.') || n.contains('e') || n.contains('E') {
        Term::typed_literal(n, xsd::DOUBLE)
    } else {
        Term::typed_literal(n, xsd::INTEGER)
    }
}

fn bool_to_literal(b: bool) -> Term {
    Term::typed_literal(bool_lexical(b), xsd::BOOLEAN)
}

fn bool_lexical(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

fn make_plain(value: &str, lang: Option<String>) -> Term {
    match lang {
        Some(l) => Term::Literal {
            value: value.to_string(),
            datatype: None,
            lang: Some(l),
        },
        None => Term::plain_literal(value),
    }
}

fn get<'a>(pairs: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A value as a list of item references: array elements (nulls dropped), or the
/// value itself as a single item.
fn item_refs(v: &Json) -> Vec<&Json> {
    match v {
        Json::Arr(a) => a.iter().filter(|x| !matches!(x, Json::Null)).collect(),
        Json::Null => Vec::new(),
        other => vec![other],
    }
}

fn as_opt_string(v: &Json) -> Option<String> {
    match v {
        Json::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn rdf_err(msg: impl Into<String>) -> GStoreError {
    GStoreError::RdfParse {
        line: 0,
        msg: msg.into(),
    }
}

/// Does `s` carry an absolute-IRI scheme (`scheme:` where scheme starts alpha)?
fn is_absolute_iri(s: &str) -> bool {
    match s.find(':') {
        Some(i) => {
            let scheme = &s[..i];
            !scheme.is_empty()
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

/// A practical relative-reference resolver (absolute pass-through; `#frag` and
/// relative paths joined onto the base). Copied minimally to keep this module
/// self-contained.
fn resolve_against_base(base: &str, reference: &str) -> String {
    if reference.is_empty() {
        return base.to_string();
    }
    if is_absolute_iri(reference) {
        return reference.to_string();
    }
    if base.is_empty() {
        return reference.to_string();
    }
    if let Some(frag) = reference.strip_prefix('#') {
        let stem = base.split('#').next().unwrap_or(base);
        return format!("{stem}#{frag}");
    }
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..=i], reference),
        None => reference.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(ts: &'a [Triple], p: &str) -> Vec<&'a Triple> {
        ts.iter().filter(|t| t.predicate == Term::iri(p)).collect()
    }

    #[test]
    fn context_prefix_and_id_and_type() {
        let doc = r#"{
            "@context": {"ex": "http://ex/", "name": "http://ex/name"},
            "@id": "ex:alice",
            "@type": "ex:Person",
            "name": "Alice"
        }"#;
        let ts = parse_str(doc).unwrap();
        let types = find(&ts, RDF_TYPE);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].subject, Term::iri("http://ex/alice"));
        assert_eq!(types[0].object, Term::iri("http://ex/Person"));
        let names = find(&ts, "http://ex/name");
        assert_eq!(names[0].object, Term::plain_literal("Alice"));
    }

    #[test]
    fn deep_nesting_is_rejected_not_overflowed() {
        // Pathological nesting is refused with an error, not a stack overflow
        // (guards the recursive reader / node processor / Json drop) — #4.
        let doc = "[".repeat(5000);
        assert!(parse_str(&doc).is_err());
    }

    #[test]
    fn invalid_number_literals_are_rejected() {
        // #11: malformed numbers must not be stored as xsd:integer/double literals.
        for bad in [
            r#"{"@id":"http://ex/a","http://ex/v": 1-2}"#,
            r#"{"@id":"http://ex/a","http://ex/v": -}"#,
            r#"{"@id":"http://ex/a","http://ex/v": 1.2.3}"#,
        ] {
            assert!(parse_str(bad).is_err(), "should reject: {bad}");
        }
        // a well-formed number still parses
        assert!(parse_str(r#"{"@id":"http://ex/a","http://ex/v": 42}"#).is_ok());
    }

    #[test]
    fn vocab_expands_bare_terms() {
        let doc = r#"{
            "@context": {"@vocab": "http://schema.org/"},
            "@id": "http://ex/a",
            "name": "Bob"
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://schema.org/name")[0].object, Term::plain_literal("Bob"));
    }

    #[test]
    fn value_object_typed_and_lang() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/a",
            "salary": {"@value": "2500", "@type": "http://www.w3.org/2001/XMLSchema#integer"},
            "label": {"@value": "chat", "@language": "fr"}
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://ex/salary")[0].object,
            Term::typed_literal("2500", "http://www.w3.org/2001/XMLSchema#integer")
        );
        assert_eq!(
            find(&ts, "http://ex/label")[0].object,
            Term::Literal {
                value: "chat".into(),
                datatype: None,
                lang: Some("fr".into())
            }
        );
    }

    #[test]
    fn native_scalars_get_xsd_types() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/a",
            "age": 42,
            "height": 1.75,
            "active": true
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/age")[0].object, Term::typed_literal("42", xsd::INTEGER));
        assert_eq!(find(&ts, "http://ex/height")[0].object, Term::typed_literal("1.75", xsd::DOUBLE));
        assert_eq!(find(&ts, "http://ex/active")[0].object, Term::typed_literal("true", xsd::BOOLEAN));
    }

    #[test]
    fn id_coercion_makes_iri_object() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/", "knows": {"@id": "http://ex/knows", "@type": "@id"}},
            "@id": "http://ex/a",
            "knows": "http://ex/b"
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/knows")[0].object, Term::iri("http://ex/b"));
    }

    #[test]
    fn nested_node_object_becomes_subject() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/a",
            "knows": {"@id": "http://ex/b", "name": "Bob"}
        }"#;
        let ts = parse_str(doc).unwrap();
        let knows = find(&ts, "http://ex/knows");
        assert_eq!(knows[0].object, Term::iri("http://ex/b"));
        let name = find(&ts, "http://ex/name");
        assert_eq!(name[0].subject, Term::iri("http://ex/b"));
        assert_eq!(name[0].object, Term::plain_literal("Bob"));
    }

    #[test]
    fn explicit_list_expands_to_rdf_chain() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/s",
            "nums": {"@list": [1, 2, 3]}
        }"#;
        let ts = parse_str(doc).unwrap();
        let first = Term::iri(format!("{RDF_NS}first"));
        let rest = Term::iri(format!("{RDF_NS}rest"));
        let nil = Term::iri(format!("{RDF_NS}nil"));
        // 3 first + 3 rest + 1 outer = 7
        assert_eq!(ts.len(), 7);
        assert_eq!(ts.iter().filter(|t| t.predicate == first).count(), 3);
        assert_eq!(ts.iter().filter(|t| t.predicate == rest).count(), 3);
        assert!(ts.iter().any(|t| t.object == nil));
        let firsts: Vec<&Term> = ts
            .iter()
            .filter(|t| t.predicate == first)
            .map(|t| &t.object)
            .collect();
        assert!(firsts.contains(&&Term::typed_literal("1", xsd::INTEGER)));
        assert!(firsts.contains(&&Term::typed_literal("3", xsd::INTEGER)));
        // the head of the chain is the object of ex:nums
        let outer = find(&ts, "http://ex/nums");
        assert_eq!(outer.len(), 1);
        assert!(matches!(outer[0].object, Term::Blank(_)));
    }

    #[test]
    fn container_list_coercion() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/", "items": {"@id": "http://ex/items", "@container": "@list"}},
            "@id": "http://ex/s",
            "items": ["a", "b"]
        }"#;
        let ts = parse_str(doc).unwrap();
        let first = Term::iri(format!("{RDF_NS}first"));
        let firsts: Vec<&Term> = ts
            .iter()
            .filter(|t| t.predicate == first)
            .map(|t| &t.object)
            .collect();
        assert_eq!(firsts.len(), 2);
        assert!(firsts.contains(&&Term::plain_literal("a")));
        assert!(firsts.contains(&&Term::plain_literal("b")));
    }

    #[test]
    fn context_term_null_removes_mapping() {
        // #3 (LOW): `"term": null` in a layered context deletes the mapping, so the
        // term no longer resolves to its former IRI and falls back to @vocab.
        let doc = r#"{
            "@context": [
                {"@vocab": "http://ex/", "keep": "http://other/keep"},
                {"keep": null}
            ],
            "@id": "http://ex/s",
            "keep": "v"
        }"#;
        let ts = parse_str(doc).unwrap();
        assert!(find(&ts, "http://other/keep").is_empty(), "nulled term mapping must be removed");
        assert_eq!(find(&ts, "http://ex/keep").len(), 1, "term falls back to @vocab expansion");
    }

    #[test]
    fn empty_list_is_nil() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/s",
            "nums": {"@list": []}
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].object, Term::iri(format!("{RDF_NS}nil")));
    }

    #[test]
    fn array_of_values_emits_multiple_triples() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/a",
            "tag": ["x", "y", "z"]
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/tag").len(), 3);
    }

    #[test]
    fn graph_members_are_emitted() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@graph": [
                {"@id": "http://ex/a", "name": "A"},
                {"@id": "http://ex/b", "name": "B"}
            ]
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/name").len(), 2);
    }

    #[test]
    fn top_level_array_of_nodes() {
        let doc = r#"[
            {"@context": {"@vocab": "http://ex/"}, "@id": "http://ex/a", "name": "A"},
            {"@context": {"@vocab": "http://ex/"}, "@id": "http://ex/b", "name": "B"}
        ]"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/name").len(), 2);
    }

    #[test]
    fn blank_node_id_and_generated_blank() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "_:b0",
            "name": "anon",
            "child": {"name": "kid"}
        }"#;
        let ts = parse_str(doc).unwrap();
        let name = find(&ts, "http://ex/name");
        assert!(name.iter().any(|t| t.subject == Term::blank("b0")));
        // the nested node without @id gets a generated blank subject
        assert!(name.iter().any(|t| matches!(&t.subject, Term::Blank(l) if l != "b0")));
    }

    #[test]
    fn string_escapes_and_unicode() {
        let doc = r#"{"@context":{"@vocab":"http://ex/"},"@id":"http://ex/a","v":"line1\nline2 é"}"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/v")[0].object, Term::plain_literal("line1\nline2 é"));
    }

    #[test]
    fn context_default_language_applies_to_strings() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/", "@language": "en"},
            "@id": "http://ex/a",
            "name": "Alice"
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://ex/name")[0].object,
            Term::Literal { value: "Alice".into(), datatype: None, lang: Some("en".into()) }
        );
    }

    #[test]
    fn multiple_types_in_array() {
        let doc = r#"{
            "@context": {"@vocab": "http://ex/"},
            "@id": "http://ex/a",
            "@type": ["T1", "T2"]
        }"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, RDF_TYPE).len(), 2);
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_str("{ not json").is_err());
        assert!(parse_str("").is_err());
    }
}
