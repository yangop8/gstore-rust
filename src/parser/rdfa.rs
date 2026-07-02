//! RDFa parser (RDFa 1.1 Core, a practical subset) over (X)HTML.
//!
//! RDFa (<https://www.w3.org/TR/rdfa-core/>) embeds RDF in host-language markup
//! via attributes. This module hand-rolls a small, lenient HTML/XHTML scanner
//! (no external crates — a minimal tag/attribute tokenizer with a stack-based
//! tree builder that auto-closes mismatched tags and understands void elements)
//! and then walks the resulting tree following the RDFa Core processing sequence.
//!
//! # Supported subset
//!
//! * attributes: `vocab`, `prefix`, `typeof`, `property`, `rel`, `rev`,
//!   `resource`, `href`, `src`, `about`, `content`, `datatype`, and
//!   `lang` / `xml:lang`.
//! * `xmlns:pfx="…"` prefix declarations (in addition to `@prefix`).
//! * CURIE / term / (safe-)CURIE / absolute-IRI resolution against the active
//!   `vocab`, prefix mappings, and document `base` (`<base href>` or `xml:base`).
//! * subject scoping down the tree: `about` / `resource` / `href` / `src` /
//!   `typeof` establish subjects (blank nodes are minted when needed) and are
//!   inherited by descendants.
//! * `typeof` → `rdf:type` triples; `rel` / `rev` → object-property triples,
//!   including the *incomplete-triple* chaining when no resource is given.
//! * `property` with `content`, a `datatype` (typed literal), `lang` (language
//!   literal), or element text content; `property` pointing at a resource
//!   (`resource`/`href`/`src`) yields an IRI object.
//! * a built-in initial context of common prefixes (`rdf`, `rdfs`, `xsd`, `owl`,
//!   `foaf`, `dc`, `dcterms`, `schema`, `skos`, `xhv`, `og`, `vcard`) and the
//!   XHTML `@rel`/`@rev` link terms (`license`, `next`, `prev`, …).
//!
//! # Deliberately omitted
//!
//! `@inlist`, `@datatype="rdf:XMLLiteral"`/`rdf:HTML` DOM serialization (the text
//! is taken verbatim), `@vocab` term-expansion caching, RDFa Lite-only shortcuts
//! beyond the above, `<head>`/`<body>` special-casing, role attributes, and
//! processor-graph/error triples. Full HTML5 tokenization quirks are not
//! reproduced; the scanner targets well-formed (X)HTML with common leniencies.

use std::collections::HashMap;
use std::path::Path;

use crate::error::Result;
use crate::model::{Term, Triple};
use crate::parser::sparql::ast::RDF_TYPE;

/// Parse an (X)HTML document containing RDFa into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    let tokens = Lexer::new(content).tokenize();
    let root = build_tree(tokens);
    let mut ex = Extractor::default();
    let base = find_base(&root).unwrap_or_default();
    let ctx = EvalContext::root(base);
    for child in root.element_children() {
        ex.process(child, &ctx, true);
    }
    Ok(ex.triples)
}

/// Parse an (X)HTML file containing RDFa into triples.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let content = std::fs::read_to_string(path)?;
    parse_str(&content)
}

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XHV_NS: &str = "http://www.w3.org/1999/xhtml/vocab#";

// ---- minimal HTML tokenizer + tree ---------------------------------------

#[derive(Debug)]
enum Token {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    End(String),
    Text(String),
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn new(s: &str) -> Lexer {
        Lexer {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    fn starts_with(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(i, c)| self.peek_at(i) == Some(c))
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn tokenize(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some(c) = self.peek() {
            if c == '<' {
                if self.starts_with("<!--") {
                    self.skip_until("-->", 4, 3);
                } else if self.starts_with("<![CDATA[") {
                    self.pos += 9;
                    let mut text = String::new();
                    while self.peek().is_some() && !self.starts_with("]]>") {
                        text.push(self.bump().unwrap());
                    }
                    if self.starts_with("]]>") {
                        self.pos += 3;
                    }
                    if !text.is_empty() {
                        tokens.push(Token::Text(text));
                    }
                } else if self.starts_with("<!") || self.starts_with("<?") {
                    // DOCTYPE / declaration / processing instruction: skip to '>'.
                    self.skip_until(">", 2, 1);
                } else if self.starts_with("</") {
                    self.pos += 2;
                    let name = self.read_name().to_ascii_lowercase();
                    self.skip_to_gt();
                    tokens.push(Token::End(name));
                } else {
                    // Start tag.
                    self.bump(); // '<'
                    let name = self.read_name().to_ascii_lowercase();
                    if name.is_empty() {
                        // Not a tag after all; treat '<' as text.
                        tokens.push(Token::Text("<".to_string()));
                        continue;
                    }
                    let attrs = self.read_attrs();
                    self.skip_ws();
                    let self_closing = self.peek() == Some('/');
                    self.skip_to_gt();
                    let is_raw = matches!(name.as_str(), "script" | "style");
                    tokens.push(Token::Start {
                        name: name.clone(),
                        attrs,
                        self_closing,
                    });
                    if is_raw && !self_closing {
                        // Consume raw text until the matching close tag.
                        let close = format!("</{name}");
                        let mut raw = String::new();
                        while self.peek().is_some() && !self.starts_with_ci(&close) {
                            raw.push(self.bump().unwrap());
                        }
                        if self.starts_with_ci(&close) {
                            self.pos += close.chars().count();
                            self.skip_to_gt();
                        }
                        let _ = raw; // raw content is not RDFa data
                        tokens.push(Token::End(name));
                    }
                }
            } else {
                let mut text = String::new();
                while let Some(c) = self.peek() {
                    if c == '<' {
                        break;
                    }
                    if c == '&' {
                        text.push(self.read_entity());
                    } else {
                        text.push(c);
                        self.bump();
                    }
                }
                tokens.push(Token::Text(text));
            }
        }
        tokens
    }

    fn starts_with_ci(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(i, c)| {
            self.peek_at(i)
                .map(|d| d.eq_ignore_ascii_case(&c))
                .unwrap_or(false)
        })
    }

    /// Skip a construct that starts `open_len` chars in and ends with `end`.
    fn skip_until(&mut self, end: &str, open_len: usize, close_len: usize) {
        self.pos += open_len;
        while self.peek().is_some() && !self.starts_with(end) {
            self.bump();
        }
        if self.starts_with(end) {
            self.pos += close_len;
        }
    }

    fn skip_to_gt(&mut self) {
        while let Some(c) = self.peek() {
            self.bump();
            if c == '>' {
                break;
            }
        }
    }

    fn read_name(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || matches!(c, '>' | '/' | '=') {
                break;
            }
            s.push(c);
            self.bump();
        }
        s
    }

    fn read_attrs(&mut self) -> Vec<(String, String)> {
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some('>') | Some('/') | None => break,
                _ => {}
            }
            let name = self.read_name().to_ascii_lowercase();
            if name.is_empty() {
                // Stray character (e.g. '='); consume to avoid an infinite loop.
                self.bump();
                continue;
            }
            self.skip_ws();
            let value = if self.peek() == Some('=') {
                self.bump();
                self.skip_ws();
                self.read_attr_value()
            } else {
                String::new()
            };
            attrs.push((name, value));
        }
        attrs
    }

    fn read_attr_value(&mut self) -> String {
        let mut value = String::new();
        match self.peek() {
            Some(q @ ('"' | '\'')) => {
                self.bump();
                while let Some(c) = self.peek() {
                    if c == q {
                        self.bump();
                        break;
                    }
                    if c == '&' {
                        value.push(self.read_entity());
                    } else {
                        value.push(c);
                        self.bump();
                    }
                }
            }
            _ => {
                // Unquoted value.
                while let Some(c) = self.peek() {
                    if c.is_whitespace() || matches!(c, '>' | '/') {
                        break;
                    }
                    if c == '&' {
                        value.push(self.read_entity());
                    } else {
                        value.push(c);
                        self.bump();
                    }
                }
            }
        }
        value
    }

    /// Decode one entity reference starting at `&`; on anything unrecognized,
    /// emit the literal `&` and leave the rest as text (HTML-lenient).
    fn read_entity(&mut self) -> char {
        let start = self.pos;
        self.bump(); // '&'
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c == ';' {
                self.bump();
                break;
            }
            if c.is_whitespace() || c == '&' || c == '<' || name.len() > 32 {
                // Not a well-formed entity; back off and treat '&' as literal.
                self.pos = start + 1;
                return '&';
            }
            name.push(c);
            self.bump();
        }
        match name.as_str() {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{00A0}',
            _ => {
                if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or('\u{FFFD}')
                } else if let Some(dec) = name.strip_prefix('#') {
                    dec.parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or('\u{FFFD}')
                } else {
                    // Unknown named entity: keep the literal '&' and rewind.
                    self.pos = start + 1;
                    '&'
                }
            }
        }
    }
}

#[derive(Debug)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Child>,
}

#[derive(Debug)]
enum Child {
    Element(Node),
    Text(String),
}

impl Node {
    fn element_children(&self) -> impl Iterator<Item = &Node> {
        self.children.iter().filter_map(|c| match c {
            Child::Element(e) => Some(e),
            Child::Text(_) => None,
        })
    }

    /// Concatenated text of the whole subtree.
    fn text_content(&self) -> String {
        let mut s = String::new();
        self.collect_text(&mut s);
        s
    }

    fn collect_text(&self, out: &mut String) {
        for c in &self.children {
            match c {
                Child::Text(t) => out.push_str(t),
                Child::Element(e) => e.collect_text(out),
            }
        }
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Maximum element nesting depth retained when building the tree. Deeper elements
/// are flattened (attached childless to the current parent) so the recursive tree
/// walks and drop cannot overflow the stack on adversarial input.
const MAX_TREE_DEPTH: usize = 256;

/// Build a tree from the flat token stream, auto-closing mismatched/omitted end
/// tags and treating void/self-closing elements as childless.
fn build_tree(tokens: Vec<Token>) -> Node {
    let mut stack: Vec<Node> = vec![Node {
        name: String::new(),
        attrs: Vec::new(),
        children: Vec::new(),
    }];
    for tok in tokens {
        match tok {
            Token::Start {
                name,
                attrs,
                self_closing,
            } => {
                let node = Node {
                    name: name.clone(),
                    attrs,
                    children: Vec::new(),
                };
                if self_closing || is_void_element(&name) {
                    stack.last_mut().unwrap().children.push(Child::Element(node));
                } else if stack.len() >= MAX_TREE_DEPTH {
                    // Bound nesting: past the cap, attach the element (childless) to
                    // the current parent instead of descending, so pathological
                    // input (e.g. a megabyte of `<div>`) cannot grow an unbounded
                    // tree that overflows the recursive walks (find_base/process/
                    // collect_text) or the recursive `Node` drop.
                    stack.last_mut().unwrap().children.push(Child::Element(node));
                } else {
                    stack.push(node);
                }
            }
            Token::End(name) => {
                if let Some(idx) = stack.iter().rposition(|n| n.name == name) {
                    if idx == 0 {
                        continue; // never pop the synthetic root
                    }
                    while stack.len() > idx {
                        let n = stack.pop().unwrap();
                        stack.last_mut().unwrap().children.push(Child::Element(n));
                    }
                }
                // Unmatched end tag: ignore.
            }
            Token::Text(t) => {
                stack.last_mut().unwrap().children.push(Child::Text(t));
            }
        }
    }
    while stack.len() > 1 {
        let n = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(Child::Element(n));
    }
    stack.pop().unwrap()
}

fn find_base(node: &Node) -> Option<String> {
    if node.name == "base" {
        if let Some(h) = node.attr("href") {
            return Some(h.to_string());
        }
    }
    for c in node.element_children() {
        if let Some(b) = find_base(c) {
            return Some(b);
        }
    }
    None
}

// ---- RDFa evaluation ------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Forward,
    Reverse,
}

#[derive(Clone)]
struct EvalContext {
    base: String,
    parent_subject: Term,
    parent_object: Option<Term>,
    vocab: Option<String>,
    prefixes: HashMap<String, String>,
    lang: Option<String>,
    /// Incomplete triples awaiting a child subject: `(predicate, direction)`.
    incomplete: Vec<(Term, Dir)>,
}

impl EvalContext {
    fn root(base: String) -> EvalContext {
        EvalContext {
            parent_subject: Term::Iri(base.clone()),
            base,
            parent_object: None,
            vocab: None,
            prefixes: initial_prefixes(),
            lang: None,
            incomplete: Vec::new(),
        }
    }
}

#[derive(Default)]
struct Extractor {
    triples: Vec<Triple>,
    bnode: usize,
}

impl Extractor {
    /// Mint a fresh, document-unique blank node for a `@typeof`/`@rel` chain.
    /// The `~` cannot appear in a parsed `_:` CURIE reference (an NCName), so a
    /// minted label can never collide with an author-written `_:` blank node —
    /// mirrors the Turtle (`g~`) and JSON-LD (`jld~`) parsers.
    fn fresh_blank(&mut self) -> Term {
        let t = Term::Blank(format!("rdfa~{}", self.bnode));
        self.bnode += 1;
        t
    }

    fn push(&mut self, s: Term, p: Term, o: Term) {
        self.triples.push(Triple::new(s, p, o));
    }

    /// The RDFa Core processing sequence for one element (subset).
    fn process(&mut self, el: &Node, ctx: &EvalContext, is_root: bool) {
        let mut skip = false;
        let new_subject: Option<Term>;
        let mut current_object_resource: Option<Term> = None;
        let mut typed_resource: Option<Term> = None;

        // Local mutable context copies.
        let mut prefixes = ctx.prefixes.clone();
        let mut vocab = ctx.vocab.clone();
        let mut lang = ctx.lang.clone();
        // `xml:base` (rare in RDFa) overrides the inherited base for this subtree.
        let base = match el.attr("xml:base") {
            Some(b) => b.to_string(),
            None => ctx.base.clone(),
        };

        // Step 2: @vocab.
        if let Some(v) = el.attr("vocab") {
            vocab = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }

        // Step 3: prefix mappings from @prefix and xmlns:*.
        if let Some(p) = el.attr("prefix") {
            parse_prefix_attr(p, &mut prefixes);
        }
        for (k, v) in &el.attrs {
            if let Some(pfx) = k.strip_prefix("xmlns:") {
                prefixes.insert(pfx.to_string(), v.clone());
            }
        }

        // Step 4: language.
        if let Some(l) = el.attr("xml:lang").or_else(|| el.attr("lang")) {
            lang = if l.is_empty() {
                None
            } else {
                Some(l.to_string())
            };
        }

        let local = LocalCtx {
            base: &base,
            vocab: &vocab,
            prefixes: &prefixes,
        };

        let has_rel = el.attr("rel").is_some();
        let has_rev = el.attr("rev").is_some();
        let has_property = el.attr("property").is_some();
        let has_content = el.attr("content").is_some();
        let has_datatype = el.attr("datatype").is_some();
        let has_typeof = el.attr("typeof").is_some();

        if !has_rel && !has_rev {
            if has_property && !has_content && !has_datatype {
                // Step 5.1: @property only (no rel/rev, no content/datatype).
                new_subject = if let Some(a) = el.attr("about") {
                    resolve_resource(a, &local)
                } else if is_root {
                    Some(Term::Iri(base.clone()))
                } else {
                    ctx.parent_object.clone().or(Some(ctx.parent_subject.clone()))
                };
                if has_typeof {
                    typed_resource = if let Some(a) = el.attr("about") {
                        resolve_resource(a, &local)
                    } else if let Some(r) = el.attr("resource") {
                        resolve_resource(r, &local)
                    } else if let Some(h) = el.attr("href") {
                        Some(Term::Iri(resolve_iri(&base, h)))
                    } else if let Some(s) = el.attr("src") {
                        Some(Term::Iri(resolve_iri(&base, s)))
                    } else {
                        Some(self.fresh_blank())
                    };
                    current_object_resource = typed_resource.clone();
                }
            } else {
                // Step 5.2.
                new_subject = if let Some(a) = el.attr("about") {
                    resolve_resource(a, &local)
                } else if let Some(r) = el.attr("resource") {
                    resolve_resource(r, &local)
                } else if let Some(h) = el.attr("href") {
                    Some(Term::Iri(resolve_iri(&base, h)))
                } else if let Some(s) = el.attr("src") {
                    Some(Term::Iri(resolve_iri(&base, s)))
                } else if is_root {
                    Some(Term::Iri(base.clone()))
                } else if has_typeof {
                    Some(self.fresh_blank())
                } else {
                    if !has_property {
                        skip = true;
                    }
                    ctx.parent_object.clone().or(Some(ctx.parent_subject.clone()))
                };
                if has_typeof {
                    typed_resource = new_subject.clone();
                }
            }
        } else {
            // Step 6: @rel and/or @rev present.
            new_subject = if let Some(a) = el.attr("about") {
                resolve_resource(a, &local)
            } else if is_root {
                Some(Term::Iri(base.clone()))
            } else {
                ctx.parent_object.clone().or(Some(ctx.parent_subject.clone()))
            };
            // The current object resource comes from @resource/@href/@src.
            current_object_resource = el
                .attr("resource")
                .and_then(|r| resolve_resource(r, &local))
                .or_else(|| el.attr("href").map(|h| Term::Iri(resolve_iri(&base, h))))
                .or_else(|| el.attr("src").map(|s| Term::Iri(resolve_iri(&base, s))));
            if has_typeof {
                // Per RDFa Core 1.1 step 6 the typed resource IS the current
                // object resource: with @about it is new_subject; otherwise it is
                // the @resource/@href/@src target — minting ONE fresh bnode into
                // BOTH when none is given, so the rdf:type triple and the rel/rev
                // triple share that node instead of typing a disconnected bnode
                // (and losing the rel triple to a never-completed incomplete list).
                typed_resource = if el.attr("about").is_some() {
                    new_subject.clone()
                } else {
                    if current_object_resource.is_none() {
                        current_object_resource = Some(self.fresh_blank());
                    }
                    current_object_resource.clone()
                };
            }
        }

        // Step 7: emit rdf:type for @typeof.
        if let Some(tr) = &typed_resource {
            if let Some(t) = el.attr("typeof") {
                for tok in t.split_whitespace() {
                    if let Some(ty) = resolve_term_or_curie(tok, &local) {
                        self.push(tr.clone(), Term::iri(RDF_TYPE), ty);
                    }
                }
            }
        }

        // Steps 8/9: rel/rev triples (complete) or incomplete-triple setup.
        let mut local_incomplete: Vec<(Term, Dir)> = Vec::new();
        if has_rel || has_rev {
            let ns = new_subject.clone();
            if let Some(cor) = current_object_resource.clone() {
                if let (Some(subj), Some(rel)) = (ns.clone(), el.attr("rel")) {
                    for tok in rel.split_whitespace() {
                        if let Some(p) = resolve_term_or_curie(tok, &local) {
                            self.push(subj.clone(), p, cor.clone());
                        }
                    }
                }
                if let (Some(subj), Some(rev)) = (ns.clone(), el.attr("rev")) {
                    for tok in rev.split_whitespace() {
                        if let Some(p) = resolve_term_or_curie(tok, &local) {
                            self.push(cor.clone(), p, subj.clone());
                        }
                    }
                }
            } else {
                // No object yet: mint a bnode and record incomplete triples.
                let b = self.fresh_blank();
                current_object_resource = Some(b);
                if let Some(rel) = el.attr("rel") {
                    for tok in rel.split_whitespace() {
                        if let Some(p) = resolve_term_or_curie(tok, &local) {
                            local_incomplete.push((p, Dir::Forward));
                        }
                    }
                }
                if let Some(rev) = el.attr("rev") {
                    for tok in rev.split_whitespace() {
                        if let Some(p) = resolve_term_or_curie(tok, &local) {
                            local_incomplete.push((p, Dir::Reverse));
                        }
                    }
                }
            }
        }

        // Step 10: complete the parent's incomplete triples with new_subject.
        if !skip {
            if let Some(subj) = &new_subject {
                for (p, dir) in &ctx.incomplete {
                    match dir {
                        Dir::Forward => {
                            self.push(ctx.parent_subject.clone(), p.clone(), subj.clone())
                        }
                        Dir::Reverse => {
                            self.push(subj.clone(), p.clone(), ctx.parent_subject.clone())
                        }
                    }
                }
            }
        }

        // Step 11: @property literals / resources.
        if has_property {
            if let Some(subj) = &new_subject {
                let obj = self.property_object(el, &local, &lang, &typed_resource, has_rel || has_rev);
                if let (Some(obj), Some(prop)) = (obj, el.attr("property")) {
                    for tok in prop.split_whitespace() {
                        if let Some(p) = resolve_term_or_curie(tok, &local) {
                            self.push(subj.clone(), p, obj.clone());
                        }
                    }
                }
            }
        }

        // Step 12: recurse into children.
        let child_ctx = if skip {
            EvalContext {
                base,
                parent_subject: ctx.parent_subject.clone(),
                parent_object: ctx.parent_object.clone(),
                vocab,
                prefixes,
                lang,
                incomplete: ctx.incomplete.clone(),
            }
        } else {
            let ps = new_subject
                .clone()
                .unwrap_or_else(|| ctx.parent_subject.clone());
            let po = current_object_resource
                .clone()
                .or_else(|| new_subject.clone())
                .unwrap_or_else(|| ps.clone());
            EvalContext {
                base,
                parent_subject: ps,
                parent_object: Some(po),
                vocab,
                prefixes,
                lang,
                incomplete: local_incomplete,
            }
        };
        for child in el.element_children() {
            self.process(child, &child_ctx, false);
        }
    }

    /// Compute the object of an `@property` (step 11 object rules). When the
    /// element also carries `@rel`/`@rev`, its `@resource`/`@href`/`@src` (and its
    /// `@typeof` chaining resource) belong to the rel/rev triple — per RDFa Core
    /// 1.1 step 11 they are NOT the property value — so `@property` falls through
    /// to the element's `@content`/text as a literal instead of duplicating the
    /// link target.
    fn property_object(
        &self,
        el: &Node,
        local: &LocalCtx,
        lang: &Option<String>,
        typed_resource: &Option<Term>,
        has_rel_or_rev: bool,
    ) -> Option<Term> {
        let datatype = el
            .attr("datatype")
            .map(|d| datatype_iri(d, local))
            .unwrap_or(None);
        let has_datatype = el.attr("datatype").is_some();

        if let Some(content) = el.attr("content") {
            return Some(make_literal(content, datatype, lang));
        }
        if has_datatype {
            // datatype present (possibly empty → plain literal).
            let text = el.text_content();
            return Some(match datatype {
                Some(dt) if !dt.is_empty() => Term::typed_literal(text, dt),
                _ => make_literal(&text, None, lang),
            });
        }
        // No content, no datatype: a pointed-to resource, else a text literal.
        // The resource/href/src and typeof-chaining arms apply only when @rel/@rev
        // are absent (otherwise that target is the rel/rev object, not the value).
        if !has_rel_or_rev {
            if let Some(r) = el.attr("resource") {
                return resolve_resource(r, local);
            }
            if let Some(h) = el.attr("href") {
                return Some(Term::Iri(resolve_iri(local.base, h)));
            }
            if let Some(s) = el.attr("src") {
                return Some(Term::Iri(resolve_iri(local.base, s)));
            }
            if el.attr("typeof").is_some() && el.attr("about").is_none() {
                return typed_resource.clone();
            }
        }
        Some(make_literal(&el.text_content(), None, lang))
    }
}

/// A read-only bundle of the resolution inputs for the current element.
struct LocalCtx<'a> {
    base: &'a str,
    vocab: &'a Option<String>,
    prefixes: &'a HashMap<String, String>,
}

fn make_literal(value: &str, datatype: Option<String>, lang: &Option<String>) -> Term {
    match datatype {
        Some(dt) if !dt.is_empty() => Term::typed_literal(value, dt),
        _ => match lang {
            Some(l) => Term::Literal {
                value: value.to_string(),
                datatype: None,
                lang: Some(l.clone()),
            },
            None => Term::plain_literal(value),
        },
    }
}

/// Parse a `@prefix` attribute (`"a: http://x b: http://y"`) into `out`.
fn parse_prefix_attr(s: &str, out: &mut HashMap<String, String>) {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let mut i = 0;
    while i + 1 < toks.len() {
        if let Some(prefix) = toks[i].strip_suffix(':') {
            out.insert(prefix.to_string(), toks[i + 1].to_string());
            i += 2;
        } else {
            i += 1;
        }
    }
}

/// Resolve `about`/`resource` (URIorSafeCURIEorCURIE).
fn resolve_resource(value: &str, local: &LocalCtx) -> Option<Term> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(inner) = v.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        return resolve_curie(inner, local);
    }
    if let Some((prefix, suffix)) = v.split_once(':') {
        if prefix == "_" {
            return Some(Term::Blank(suffix.to_string()));
        }
        if suffix.starts_with("//") {
            return Some(Term::Iri(v.to_string()));
        }
        if let Some(ns) = local.prefixes.get(prefix) {
            return Some(Term::Iri(format!("{ns}{suffix}")));
        }
        if is_absolute_iri(v) {
            return Some(Term::Iri(v.to_string()));
        }
        return Some(Term::Iri(resolve_iri(local.base, v)));
    }
    Some(Term::Iri(resolve_iri(local.base, v)))
}

/// Resolve a CURIE or term inside a safe-CURIE `[ … ]`.
fn resolve_curie(inner: &str, local: &LocalCtx) -> Option<Term> {
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }
    if let Some((prefix, suffix)) = inner.split_once(':') {
        if prefix == "_" {
            return Some(Term::Blank(suffix.to_string()));
        }
        if let Some(ns) = local.prefixes.get(prefix) {
            return Some(Term::Iri(format!("{ns}{suffix}")));
        }
        if is_absolute_iri(inner) {
            return Some(Term::Iri(inner.to_string()));
        }
        return None;
    }
    local
        .vocab
        .as_ref()
        .map(|vc| Term::Iri(format!("{vc}{inner}")))
}

/// Resolve `rel`/`rev`/`property`/`typeof` tokens (TERMorCURIEorAbsIRI).
fn resolve_term_or_curie(value: &str, local: &LocalCtx) -> Option<Term> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(inner) = v.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        return resolve_curie(inner, local);
    }
    if let Some((prefix, suffix)) = v.split_once(':') {
        if suffix.starts_with("//") {
            return Some(Term::Iri(v.to_string()));
        }
        if let Some(ns) = local.prefixes.get(prefix) {
            return Some(Term::Iri(format!("{ns}{suffix}")));
        }
        if is_absolute_iri(v) {
            return Some(Term::Iri(v.to_string()));
        }
        return None;
    }
    // Bare term: expand against @vocab, else a known XHTML link term.
    if let Some(vc) = local.vocab {
        return Some(Term::Iri(format!("{vc}{v}")));
    }
    default_term(v).map(Term::Iri)
}

/// Resolve a `@datatype` value to an IRI string (empty stays empty).
fn datatype_iri(value: &str, local: &LocalCtx) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return Some(String::new());
    }
    resolve_term_or_curie(v, local).and_then(|t| match t {
        Term::Iri(s) => Some(s),
        _ => None,
    })
}

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

/// Resolve a (possibly relative) IRI reference against `base` (practical subset).
fn resolve_iri(base: &str, reference: &str) -> String {
    let reference = reference.trim();
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
    if reference.starts_with('/') {
        // Replace path from the authority onward (best-effort).
        if let Some(scheme_end) = base.find("://") {
            let after = &base[scheme_end + 3..];
            let auth_end = after.find('/').map(|i| scheme_end + 3 + i).unwrap_or(base.len());
            return format!("{}{}", &base[..auth_end], reference);
        }
        return reference.to_string();
    }
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..=i], reference),
        None => reference.to_string(),
    }
}

/// The subset of RDFa 1.1 initial-context prefixes we predefine.
fn initial_prefixes() -> HashMap<String, String> {
    let pairs = [
        ("rdf", RDF_NS),
        ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
        ("xsd", "http://www.w3.org/2001/XMLSchema#"),
        ("owl", "http://www.w3.org/2002/07/owl#"),
        ("dc", "http://purl.org/dc/elements/1.1/"),
        ("dcterms", "http://purl.org/dc/terms/"),
        ("foaf", "http://xmlns.com/foaf/0.1/"),
        ("skos", "http://www.w3.org/2004/02/skos/core#"),
        ("schema", "http://schema.org/"),
        ("xhv", XHV_NS),
        ("og", "http://ogp.me/ns#"),
        ("vcard", "http://www.w3.org/2006/vcard/ns#"),
    ];
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The XHTML `@rel`/`@rev` link terms from the RDFa initial context.
fn default_term(term: &str) -> Option<String> {
    const TERMS: &[&str] = &[
        "alternate",
        "appendix",
        "bookmark",
        "chapter",
        "contents",
        "copyright",
        "first",
        "glossary",
        "help",
        "icon",
        "index",
        "last",
        "license",
        "meta",
        "next",
        "prev",
        "previous",
        "section",
        "start",
        "stylesheet",
        "subsection",
        "top",
        "up",
        "describedby",
        "role",
    ];
    if TERMS.contains(&term) {
        Some(format!("{XHV_NS}{term}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(ts: &'a [Triple], p: &str) -> Vec<&'a Triple> {
        ts.iter().filter(|t| t.predicate == Term::iri(p)).collect()
    }

    #[test]
    fn typeof_and_property_with_vocab() {
        let doc = r#"
        <div vocab="http://schema.org/" about="http://ex/alice" typeof="Person">
          <span property="name">Alice</span>
          <span property="age" content="30" datatype="http://www.w3.org/2001/XMLSchema#integer"></span>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let types = find(&ts, RDF_TYPE);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].subject, Term::iri("http://ex/alice"));
        assert_eq!(types[0].object, Term::iri("http://schema.org/Person"));
        let name = find(&ts, "http://schema.org/name");
        assert_eq!(name[0].subject, Term::iri("http://ex/alice"));
        assert_eq!(name[0].object, Term::plain_literal("Alice"));
        let age = find(&ts, "http://schema.org/age");
        assert_eq!(
            age[0].object,
            Term::typed_literal("30", "http://www.w3.org/2001/XMLSchema#integer")
        );
    }

    #[test]
    fn rel_with_href_makes_iri_object() {
        let doc = r#"
        <div about="http://ex/a">
          <a rel="http://xmlns.com/foaf/0.1/knows" href="http://ex/b">Bob</a>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let knows = find(&ts, "http://xmlns.com/foaf/0.1/knows");
        assert_eq!(knows.len(), 1);
        assert_eq!(knows[0].subject, Term::iri("http://ex/a"));
        assert_eq!(knows[0].object, Term::iri("http://ex/b"));
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        // #5: pathological nesting is flattened past the depth cap, so the
        // recursive walks/drop don't overflow the stack — parsing simply returns.
        let doc = "<div>".repeat(3000);
        let _ = parse_str(&doc).unwrap();
    }

    #[test]
    fn rel_typeof_without_about_types_the_object() {
        // #6: with @rel + @typeof and no @about, the type must land on the object
        // (the @href target), not on a disconnected fresh bnode.
        let doc = r#"
        <div about="http://ex/a">
          <a rel="http://ex/knows" href="http://ex/b" typeof="http://ex/Person">x</a>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let knows = find(&ts, "http://ex/knows");
        assert_eq!(knows.len(), 1);
        assert_eq!(knows[0].object, Term::iri("http://ex/b"));
        let types = find(&ts, RDF_TYPE);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].subject, Term::iri("http://ex/b"), "type must be on the rel object");
        assert_eq!(types[0].object, Term::iri("http://ex/Person"));
    }

    #[test]
    fn minted_blank_never_collides_with_author_blank() {
        // #1 (LOW): an author-written `_:rdfa0` must stay distinct from a blank the
        // parser mints. The minted label carries a `~` (not a valid `_:` CURIE
        // char), so the two can never be unified into one node.
        let doc = r#"
        <div>
          <span about="_:rdfa0" property="http://ex/p">authored</span>
          <span typeof="http://ex/T"></span>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let authored = find(&ts, "http://ex/p");
        assert_eq!(authored[0].subject, Term::Blank("rdfa0".into()), "author label preserved");
        let types = find(&ts, RDF_TYPE);
        assert_eq!(types.len(), 1);
        let Term::Blank(minted) = &types[0].subject else {
            panic!("bare @typeof must mint a blank subject");
        };
        assert!(minted.starts_with("rdfa~"), "minted label must use the ~ separator: {minted}");
        assert_ne!(&types[0].subject, &authored[0].subject, "minted must not equal author blank");
    }

    #[test]
    fn property_with_rel_uses_text_not_href() {
        // #13: @property alongside @rel must take the element's text as a literal,
        // not reuse the @href (which is the rel triple's object).
        let doc = r#"<a about="http://ex/a" rel="http://ex/next" property="http://ex/ref" href="http://ex/y">text</a>"#;
        let ts = parse_str(doc).unwrap();
        let next = find(&ts, "http://ex/next");
        assert_eq!(next[0].object, Term::iri("http://ex/y"), "rel object is the href target");
        let refs = find(&ts, "http://ex/ref");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].object, Term::plain_literal("text"), "property value is the text literal");
    }

    #[test]
    fn prefix_curie_resolution() {
        let doc = r#"
        <div prefix="foaf: http://xmlns.com/foaf/0.1/" about="http://ex/a">
          <span property="foaf:name">Alice</span>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://xmlns.com/foaf/0.1/name")[0].object,
            Term::plain_literal("Alice")
        );
    }

    #[test]
    fn builtin_prefix_available_without_declaration() {
        let doc = r#"<div about="http://ex/a"><span property="dc:title">T</span></div>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://purl.org/dc/elements/1.1/title")[0].object,
            Term::plain_literal("T")
        );
    }

    #[test]
    fn lang_literal_from_xml_lang() {
        let doc = r#"
        <div vocab="http://ex/" about="http://ex/a" xml:lang="fr">
          <span property="label">chat</span>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://ex/label")[0].object,
            Term::Literal { value: "chat".into(), datatype: None, lang: Some("fr".into()) }
        );
    }

    #[test]
    fn resource_scopes_child_subject() {
        let doc = r#"
        <div vocab="http://ex/" about="http://ex/a">
          <div resource="http://ex/b">
            <span property="name">Bob</span>
          </div>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let name = find(&ts, "http://ex/name");
        assert_eq!(name[0].subject, Term::iri("http://ex/b"));
    }

    #[test]
    fn typeof_without_about_mints_blank_node() {
        // The typeof element is nested (not the document root), so — lacking
        // @about/@resource — it mints a fresh blank node subject. (A root typeof
        // element would instead take the document base as its subject.)
        let doc = r#"
        <div>
          <div vocab="http://schema.org/" typeof="Person">
            <span property="name">Anon</span>
          </div>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let types = find(&ts, RDF_TYPE);
        assert_eq!(types.len(), 1);
        assert!(matches!(types[0].subject, Term::Blank(_)));
        let name = find(&ts, "http://schema.org/name");
        assert_eq!(name[0].subject, types[0].subject);
    }

    #[test]
    fn meta_content_property() {
        let doc = r#"
        <html prefix="og: http://ogp.me/ns#">
          <head about="http://ex/page">
            <meta property="og:title" content="Hello"/>
          </head>
        </html>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://ogp.me/ns#title")[0].object,
            Term::plain_literal("Hello")
        );
    }

    #[test]
    fn property_pointing_at_resource_is_iri_object() {
        let doc = r#"
        <div vocab="http://ex/" about="http://ex/a">
          <a property="homepage" href="http://ex/home">home</a>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            find(&ts, "http://ex/homepage")[0].object,
            Term::iri("http://ex/home")
        );
    }

    #[test]
    fn rel_chaining_via_incomplete_triples() {
        // <div> has rel but no resource; the nested subject completes it.
        let doc = r#"
        <div vocab="http://ex/" about="http://ex/a" rel="knows">
          <div about="http://ex/b"><span property="name">Bob</span></div>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let knows = find(&ts, "http://ex/knows");
        assert_eq!(knows.len(), 1);
        assert_eq!(knows[0].subject, Term::iri("http://ex/a"));
        assert_eq!(knows[0].object, Term::iri("http://ex/b"));
    }

    #[test]
    fn base_element_resolves_relative_iris() {
        let doc = r#"
        <html>
          <head><base href="http://ex/"/></head>
          <body>
            <div about="alice"><span property="http://ex/name">A</span></div>
          </body>
        </html>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/name")[0].subject, Term::iri("http://ex/alice"));
    }

    #[test]
    fn entities_in_text() {
        let doc = r#"<div about="http://ex/a"><span property="http://ex/n">a &lt; b &amp; c</span></div>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(find(&ts, "http://ex/n")[0].object, Term::plain_literal("a < b & c"));
    }

    #[test]
    fn xhtml_link_term_without_vocab() {
        let doc = r#"<div about="http://ex/a"><link rel="license" href="http://ex/lic"/></div>"#;
        let ts = parse_str(doc).unwrap();
        let lic = find(&ts, "http://www.w3.org/1999/xhtml/vocab#license");
        assert_eq!(lic.len(), 1);
        assert_eq!(lic[0].object, Term::iri("http://ex/lic"));
    }

    #[test]
    fn script_content_is_ignored() {
        let doc = r#"
        <div vocab="http://ex/" about="http://ex/a">
          <script>var x = "<span property='name'>nope</span>";</script>
          <span property="name">Real</span>
        </div>"#;
        let ts = parse_str(doc).unwrap();
        let name = find(&ts, "http://ex/name");
        assert_eq!(name.len(), 1);
        assert_eq!(name[0].object, Term::plain_literal("Real"));
    }
}
