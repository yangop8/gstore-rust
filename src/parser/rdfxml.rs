//! RDF/XML parser (a practical subset).
//!
//! RDF/XML (<https://www.w3.org/TR/rdf-syntax-grammar/>) serializes a graph as
//! XML. This reader hand-rolls a minimal XML scanner (no external crates) and
//! supports the slice of the grammar that real datasets use:
//!
//! * the `rdf:RDF` document element with `xmlns`/`xmlns:prefix` declarations
//! * `rdf:Description` nodes and *typed* node elements (`<ex:Person …>` ⇒ an
//!   `rdf:type` triple)
//! * `rdf:about`, `rdf:ID` (resolved against `xml:base`), `rdf:nodeID`
//! * property elements with `rdf:resource`, nested node elements (striped
//!   syntax), or text content (with `rdf:datatype` / `xml:lang`)
//! * property *attributes* (shorthand for literal-valued properties)
//! * `rdf:parseType="Resource"` (anonymous blank node) and `="Literal"`
//! * `rdf:li` membership properties (lowered to `rdf:_1`, `rdf:_2`, …) and the
//!   `rdf:Seq`/`rdf:Bag`/`rdf:Alt` container classes
//!
//! It is deliberately lenient and does not validate; constructs outside the
//! supported subset are either ignored or surfaced as a parse error.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::{Term, Triple};

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Parse an RDF/XML document into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    let root = XmlParser::new(content).parse_document()?;
    let mut ex = Extractor::default();
    ex.run(&root)?;
    Ok(ex.triples)
}

/// Parse an RDF/XML file into triples.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let content = std::fs::read_to_string(path)?;
    parse_str(&content)
}

// ---- minimal XML DOM ------------------------------------------------------

#[derive(Debug)]
struct XmlElement {
    /// The raw qualified name, e.g. `rdf:Description`.
    name: String,
    /// Raw qualified attribute name → decoded value.
    attrs: Vec<(String, String)>,
    children: Vec<XmlChild>,
}

#[derive(Debug)]
enum XmlChild {
    Element(XmlElement),
    Text(String),
}

impl XmlElement {
    fn elements(&self) -> impl Iterator<Item = &XmlElement> {
        self.children.iter().filter_map(|c| match c {
            XmlChild::Element(e) => Some(e),
            XmlChild::Text(_) => None,
        })
    }

    fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            if let XmlChild::Text(t) = c {
                s.push_str(t);
            }
        }
        s
    }

    fn has_element_children(&self) -> bool {
        self.children
            .iter()
            .any(|c| matches!(c, XmlChild::Element(_)))
    }
}

struct XmlParser {
    chars: Vec<char>,
    pos: usize,
}

impl XmlParser {
    fn new(s: &str) -> XmlParser {
        XmlParser {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn err(&self, msg: impl Into<String>) -> GStoreError {
        let line = self.chars[..self.pos.min(self.chars.len())]
            .iter()
            .filter(|&&c| c == '\n')
            .count()
            + 1;
        GStoreError::RdfParse {
            line,
            msg: msg.into(),
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
        s.chars()
            .enumerate()
            .all(|(i, c)| self.peek_at(i) == Some(c))
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// Skip XML prolog noise: `<?…?>`, `<!-- … -->`, and `<!DOCTYPE …>`.
    fn skip_misc(&mut self) -> Result<()> {
        loop {
            self.skip_ws();
            if self.starts_with("<?") {
                while !self.starts_with("?>") && self.peek().is_some() {
                    self.bump();
                }
                if self.starts_with("?>") {
                    self.pos += 2;
                } else {
                    return Err(self.err("unterminated processing instruction"));
                }
            } else if self.starts_with("<!--") {
                self.skip_comment()?;
            } else if self.starts_with("<!DOCTYPE") {
                self.skip_doctype()?;
            } else {
                return Ok(());
            }
        }
    }

    fn skip_comment(&mut self) -> Result<()> {
        self.pos += 4; // "<!--"
        while !self.starts_with("-->") {
            if self.bump().is_none() {
                return Err(self.err("unterminated comment"));
            }
        }
        self.pos += 3;
        Ok(())
    }

    fn skip_doctype(&mut self) -> Result<()> {
        self.pos += 9; // "<!DOCTYPE"
        let mut depth = 0i32;
        while let Some(c) = self.peek() {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                '>' if depth <= 0 => {
                    self.bump();
                    return Ok(());
                }
                _ => {}
            }
            self.bump();
        }
        Err(self.err("unterminated DOCTYPE"))
    }

    fn parse_document(&mut self) -> Result<XmlElement> {
        self.skip_misc()?;
        if self.peek() != Some('<') {
            return Err(self.err("expected an XML root element"));
        }
        self.parse_element()
    }

    /// Parse an element starting at `<`.
    fn parse_element(&mut self) -> Result<XmlElement> {
        if self.bump() != Some('<') {
            return Err(self.err("expected '<'"));
        }
        let name = self.read_name();
        if name.is_empty() {
            return Err(self.err("empty element name"));
        }
        let attrs = self.read_attrs()?;
        self.skip_ws();
        if self.starts_with("/>") {
            self.pos += 2;
            return Ok(XmlElement {
                name,
                attrs,
                children: Vec::new(),
            });
        }
        if self.bump() != Some('>') {
            return Err(self.err(format!("expected '>' to close start tag <{name}>")));
        }
        let children = self.parse_content(&name)?;
        Ok(XmlElement {
            name,
            attrs,
            children,
        })
    }

    /// Parse element content until the matching end tag `</name>`.
    fn parse_content(&mut self, name: &str) -> Result<Vec<XmlChild>> {
        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err(format!("unexpected EOF inside <{name}>"))),
                Some('<') => {
                    if !text.is_empty() {
                        children.push(XmlChild::Text(std::mem::take(&mut text)));
                    }
                    if self.starts_with("</") {
                        self.pos += 2;
                        let close = self.read_name();
                        self.skip_ws();
                        if self.bump() != Some('>') {
                            return Err(self.err(format!("malformed end tag </{close}>")));
                        }
                        if close != name {
                            return Err(self.err(format!(
                                "mismatched end tag: <{name}> closed by </{close}>"
                            )));
                        }
                        return Ok(children);
                    } else if self.starts_with("<!--") {
                        self.skip_comment()?;
                    } else if self.starts_with("<![CDATA[") {
                        self.pos += 9;
                        let mut cdata = String::new();
                        while !self.starts_with("]]>") {
                            match self.bump() {
                                Some(c) => cdata.push(c),
                                None => return Err(self.err("unterminated CDATA")),
                            }
                        }
                        self.pos += 3;
                        children.push(XmlChild::Text(cdata));
                    } else {
                        children.push(XmlChild::Element(self.parse_element()?));
                    }
                }
                Some('&') => {
                    text.push(self.read_entity()?);
                }
                Some(c) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
    }

    fn read_name(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '>' || c == '/' || c == '=' {
                break;
            }
            s.push(c);
            self.bump();
        }
        s
    }

    fn read_attrs(&mut self) -> Result<Vec<(String, String)>> {
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some('>') | Some('/') | None => break,
                _ => {}
            }
            let name = self.read_name();
            if name.is_empty() {
                return Err(self.err("malformed attribute"));
            }
            self.skip_ws();
            if self.bump() != Some('=') {
                return Err(self.err(format!("expected '=' after attribute '{name}'")));
            }
            self.skip_ws();
            let quote = match self.bump() {
                Some(q @ ('"' | '\'')) => q,
                _ => return Err(self.err(format!("attribute '{name}' value must be quoted"))),
            };
            let mut value = String::new();
            loop {
                match self.peek() {
                    None => return Err(self.err("unterminated attribute value")),
                    Some('&') => value.push(self.read_entity()?),
                    Some(c) if c == quote => {
                        self.bump();
                        break;
                    }
                    Some(c) => {
                        value.push(c);
                        self.bump();
                    }
                }
            }
            attrs.push((name, value));
        }
        Ok(attrs)
    }

    /// Decode one XML entity reference starting at `&`.
    fn read_entity(&mut self) -> Result<char> {
        debug_assert_eq!(self.peek(), Some('&'));
        self.bump();
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c == ';' {
                self.bump();
                break;
            }
            name.push(c);
            self.bump();
            if name.len() > 32 {
                return Err(self.err("unterminated entity reference"));
            }
        }
        let ch = match name.as_str() {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    let code = u32::from_str_radix(hex, 16)
                        .map_err(|_| self.err(format!("bad character reference '&{name};'")))?;
                    char::from_u32(code)
                        .ok_or_else(|| self.err(format!("invalid code point in '&{name};'")))?
                } else if let Some(dec) = name.strip_prefix('#') {
                    let code = dec
                        .parse::<u32>()
                        .map_err(|_| self.err(format!("bad character reference '&{name};'")))?;
                    char::from_u32(code)
                        .ok_or_else(|| self.err(format!("invalid code point in '&{name};'")))?
                } else {
                    return Err(self.err(format!("unknown entity '&{name};'")));
                }
            }
        };
        Ok(ch)
    }
}

// ---- triple extraction ----------------------------------------------------

#[derive(Default)]
struct Extractor {
    triples: Vec<Triple>,
    bnode: usize,
}

/// The inherited XML context threaded through the walk.
#[derive(Clone)]
struct Ctx {
    ns: HashMap<String, String>,
    lang: Option<String>,
    base: String,
}

impl Ctx {
    fn root() -> Ctx {
        let mut ns = HashMap::new();
        ns.insert("xml".to_string(), XML_NS.to_string());
        Ctx {
            ns,
            lang: None,
            base: String::new(),
        }
    }
}

impl Extractor {
    fn fresh_blank(&mut self) -> Term {
        let t = Term::Blank(format!("rx{}", self.bnode));
        self.bnode += 1;
        t
    }

    /// Walk the document root: either `rdf:RDF` (iterate its node children) or a
    /// single node element.
    fn run(&mut self, root: &XmlElement) -> Result<()> {
        let ctx = self.child_ctx(&Ctx::root(), root);
        let (ns, local) = expand_name(&root.name, &ctx.ns)?;
        if ns == RDF_NS && local == "RDF" {
            for child in root.elements() {
                self.node(child, &ctx)?;
            }
        } else {
            self.node(root, &ctx)?;
        }
        Ok(())
    }

    /// Build the child context: merge `xmlns` declarations, `xml:lang`, `xml:base`.
    fn child_ctx(&self, parent: &Ctx, el: &XmlElement) -> Ctx {
        let mut ctx = parent.clone();
        for (k, v) in &el.attrs {
            if k == "xmlns" {
                ctx.ns.insert(String::new(), v.clone());
            } else if let Some(prefix) = k.strip_prefix("xmlns:") {
                ctx.ns.insert(prefix.to_string(), v.clone());
            } else if k == "xml:base" {
                ctx.base = v.clone();
            } else if k == "xml:lang" {
                ctx.lang = if v.is_empty() { None } else { Some(v.clone()) };
            }
        }
        ctx
    }

    /// Process a node element, returning its subject term.
    fn node(&mut self, el: &XmlElement, parent: &Ctx) -> Result<Term> {
        let ctx = self.child_ctx(parent, el);
        let (ns, local) = expand_name(&el.name, &ctx.ns)?;

        // Subject identity.
        let subject = if let Some(about) = attr_value(el, RDF_NS, "about", &ctx.ns)? {
            Term::Iri(resolve(&ctx.base, &about))
        } else if let Some(id) = attr_value(el, RDF_NS, "ID", &ctx.ns)? {
            Term::Iri(resolve(&ctx.base, &format!("#{id}")))
        } else if let Some(nid) = attr_value(el, RDF_NS, "nodeID", &ctx.ns)? {
            Term::Blank(nid)
        } else {
            self.fresh_blank()
        };

        // Typed node: a non-`rdf:Description` element name is an rdf:type.
        if !(ns == RDF_NS && local == "Description") {
            self.push(subject.clone(), iri_type(), Term::Iri(format!("{ns}{local}")));
        }

        // Property attributes (shorthand literal-valued properties) and rdf:type.
        for (raw, value) in &el.attrs {
            let (a_ns, a_local) = expand_attr_name(raw, &ctx.ns)?;
            if is_syntax_attr(raw, &a_ns, &a_local) {
                if a_ns == RDF_NS && a_local == "type" {
                    self.push(
                        subject.clone(),
                        iri_type(),
                        Term::Iri(resolve(&ctx.base, value)),
                    );
                }
                continue;
            }
            self.push(
                subject.clone(),
                Term::Iri(format!("{a_ns}{a_local}")),
                Term::Literal {
                    value: value.clone(),
                    datatype: None,
                    lang: ctx.lang.clone(),
                },
            );
        }

        // Property elements.
        let mut li = 1usize;
        for pe in el.elements() {
            self.property(&subject, pe, &ctx, &mut li)?;
        }
        Ok(subject)
    }

    /// Process a property element of `subject`.
    fn property(
        &mut self,
        subject: &Term,
        pe: &XmlElement,
        parent: &Ctx,
        li: &mut usize,
    ) -> Result<()> {
        let ctx = self.child_ctx(parent, pe);
        let (ns, local) = expand_name(&pe.name, &ctx.ns)?;
        let predicate = if ns == RDF_NS && local == "li" {
            let n = *li;
            *li += 1;
            Term::Iri(format!("{RDF_NS}_{n}"))
        } else {
            Term::Iri(format!("{ns}{local}"))
        };

        // rdf:resource ⇒ IRI object.
        if let Some(res) = attr_value(pe, RDF_NS, "resource", &ctx.ns)? {
            self.push(
                subject.clone(),
                predicate,
                Term::Iri(resolve(&ctx.base, &res)),
            );
            return Ok(());
        }
        // rdf:nodeID ⇒ blank-node object.
        if let Some(nid) = attr_value(pe, RDF_NS, "nodeID", &ctx.ns)? {
            self.push(subject.clone(), predicate, Term::Blank(nid));
            return Ok(());
        }

        // parseType handling.
        if let Some(pt) = attr_value(pe, RDF_NS, "parseType", &ctx.ns)? {
            match pt.as_str() {
                "Resource" => {
                    let obj = self.fresh_blank();
                    self.push(subject.clone(), predicate, obj.clone());
                    let mut inner_li = 1usize;
                    // Property attributes on the property element apply to `obj`.
                    for (raw, value) in &pe.attrs {
                        let (a_ns, a_local) = expand_attr_name(raw, &ctx.ns)?;
                        if is_syntax_attr(raw, &a_ns, &a_local) {
                            continue;
                        }
                        self.push(
                            obj.clone(),
                            Term::Iri(format!("{a_ns}{a_local}")),
                            Term::Literal {
                                value: value.clone(),
                                datatype: None,
                                lang: ctx.lang.clone(),
                            },
                        );
                    }
                    for inner in pe.elements() {
                        self.property(&obj, inner, &ctx, &mut inner_li)?;
                    }
                    return Ok(());
                }
                "Literal" => {
                    self.push(
                        subject.clone(),
                        predicate,
                        Term::Literal {
                            value: pe.text(),
                            datatype: Some(format!("{RDF_NS}XMLLiteral")),
                            lang: None,
                        },
                    );
                    return Ok(());
                }
                _ => { /* Collection / other: fall through to generic handling. */ }
            }
        }

        // Striped syntax: a nested node element is the object.
        if pe.has_element_children() {
            for child in pe.elements() {
                let obj = self.node(child, &ctx)?;
                self.push(subject.clone(), predicate.clone(), obj);
            }
            return Ok(());
        }

        // Otherwise a literal from the text content.
        let datatype = attr_value(pe, RDF_NS, "datatype", &ctx.ns)?;
        let lang = if datatype.is_some() {
            None
        } else {
            ctx.lang.clone()
        };
        self.push(
            subject.clone(),
            predicate,
            Term::Literal {
                value: pe.text().trim().to_string(),
                datatype,
                lang,
            },
        );
        Ok(())
    }

    fn push(&mut self, s: Term, p: Term, o: Term) {
        self.triples.push(Triple::new(s, p, o));
    }
}

fn iri_type() -> Term {
    Term::Iri(format!("{RDF_NS}type"))
}

/// Expand an element/datatype qualified name to `(namespace, local)`.
fn expand_name(qname: &str, ns: &HashMap<String, String>) -> Result<(String, String)> {
    match qname.split_once(':') {
        Some((prefix, local)) => {
            let iri = ns.get(prefix).ok_or_else(|| GStoreError::RdfParse {
                line: 0,
                msg: format!("undefined namespace prefix '{prefix}:'"),
            })?;
            Ok((iri.clone(), local.to_string()))
        }
        None => {
            let iri = ns.get("").cloned().unwrap_or_default();
            Ok((iri, qname.to_string()))
        }
    }
}

/// Expand an *attribute* qualified name. Unprefixed attributes are not in the
/// default namespace (XML namespaces rule); they get an empty namespace.
fn expand_attr_name(qname: &str, ns: &HashMap<String, String>) -> Result<(String, String)> {
    match qname.split_once(':') {
        Some((prefix, local)) => {
            let iri = ns.get(prefix).ok_or_else(|| GStoreError::RdfParse {
                line: 0,
                msg: format!("undefined namespace prefix '{prefix}:'"),
            })?;
            Ok((iri.clone(), local.to_string()))
        }
        None => Ok((String::new(), qname.to_string())),
    }
}

/// Find an RDF-syntax attribute's value by namespace+local, tolerating the
/// common unprefixed spelling (`about` when the writer omitted the `rdf:`).
fn attr_value(
    el: &XmlElement,
    want_ns: &str,
    want_local: &str,
    ns: &HashMap<String, String>,
) -> Result<Option<String>> {
    for (raw, value) in &el.attrs {
        let (a_ns, a_local) = expand_attr_name(raw, ns)?;
        let matches = (a_ns == want_ns && a_local == want_local)
            || (a_ns.is_empty() && a_local == want_local && want_ns == RDF_NS);
        if matches {
            return Ok(Some(value.clone()));
        }
    }
    Ok(None)
}

/// Is this attribute RDF/XML *syntax* (not data)? Covers `xmlns*`, `xml:*`, and
/// the core `rdf:` syntax attributes.
fn is_syntax_attr(raw: &str, a_ns: &str, a_local: &str) -> bool {
    if raw == "xmlns" || raw.starts_with("xmlns:") {
        return true;
    }
    if a_ns == XML_NS {
        return true;
    }
    const SYNTAX: &[&str] = &[
        "about",
        "ID",
        "nodeID",
        "resource",
        "datatype",
        "parseType",
        "type",
    ];
    if a_ns == RDF_NS && SYNTAX.contains(&a_local) {
        return true;
    }
    // Unprefixed spelling of the identity attributes.
    a_ns.is_empty() && matches!(a_local, "about" | "ID" | "nodeID" | "resource")
}

/// Resolve a (possibly relative) IRI reference against `base` — a practical
/// subset: absolute references pass through; `#frag` and relative references are
/// joined to the base.
fn resolve(base: &str, reference: &str) -> String {
    if reference.is_empty() {
        return base.to_string();
    }
    if is_absolute(reference) {
        return reference.to_string();
    }
    if base.is_empty() {
        return reference.to_string();
    }
    if let Some(frag) = reference.strip_prefix('#') {
        let stem = base.split('#').next().unwrap_or(base);
        return format!("{stem}#{frag}");
    }
    // Relative path: replace the last segment of the base.
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..=i], reference),
        None => reference.to_string(),
    }
}

/// Does `s` have an absolute-IRI scheme (`scheme:` where scheme starts with a
/// letter)?
fn is_absolute(s: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(ts: &'a [Triple], p: &str) -> Vec<&'a Triple> {
        ts.iter().filter(|t| t.predicate == Term::iri(p)).collect()
    }

    #[test]
    fn description_with_about_and_literal_property() {
        let doc = r#"<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/alice">
    <ex:name>Alice</ex:name>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].subject, Term::iri("http://ex/alice"));
        assert_eq!(ts[0].predicate, Term::iri("http://ex/name"));
        assert_eq!(ts[0].object, Term::plain_literal("Alice"));
    }

    #[test]
    fn typed_node_emits_rdf_type() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <ex:Person rdf:about="http://ex/bob"><ex:age>42</ex:age></ex:Person>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        let types = find(&ts, &format!("{RDF_NS}type"));
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].object, Term::iri("http://ex/Person"));
        assert_eq!(types[0].subject, Term::iri("http://ex/bob"));
    }

    #[test]
    fn resource_property_is_iri_object() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/a">
    <ex:knows rdf:resource="http://ex/b"/>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].object, Term::iri("http://ex/b"));
    }

    #[test]
    fn datatype_and_lang_literals() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/" xmlns:xsd="http://www.w3.org/2001/XMLSchema#">
  <rdf:Description rdf:about="http://ex/a">
    <ex:salary rdf:datatype="http://www.w3.org/2001/XMLSchema#integer">2500</ex:salary>
    <ex:label xml:lang="fr">chat</ex:label>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        let salary = find(&ts, "http://ex/salary");
        assert_eq!(
            salary[0].object,
            Term::typed_literal("2500", "http://www.w3.org/2001/XMLSchema#integer")
        );
        let label = find(&ts, "http://ex/label");
        assert_eq!(
            label[0].object,
            Term::Literal {
                value: "chat".into(),
                datatype: None,
                lang: Some("fr".into())
            }
        );
    }

    #[test]
    fn rdf_id_resolves_against_xml_base() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/" xml:base="http://ex/base">
  <rdf:Description rdf:ID="x"><ex:p>v</ex:p></rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts[0].subject, Term::iri("http://ex/base#x"));
    }

    #[test]
    fn property_attribute_shorthand() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/a" ex:name="Alice" ex:city="NYC"/>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 2);
        assert!(ts
            .iter()
            .any(|t| t.predicate == Term::iri("http://ex/name")
                && t.object == Term::plain_literal("Alice")));
        assert!(ts
            .iter()
            .any(|t| t.predicate == Term::iri("http://ex/city")
                && t.object == Term::plain_literal("NYC")));
    }

    #[test]
    fn parse_type_resource_makes_blank_node() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/a">
    <ex:addr rdf:parseType="Resource">
      <ex:city>NYC</ex:city>
    </ex:addr>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        // (a ex:addr _:b) and (_:b ex:city "NYC")
        assert_eq!(ts.len(), 2);
        let addr = find(&ts, "http://ex/addr");
        let b = match &addr[0].object {
            Term::Blank(l) => l.clone(),
            other => panic!("expected blank, got {other:?}"),
        };
        let city = find(&ts, "http://ex/city");
        assert_eq!(city[0].subject, Term::Blank(b));
        assert_eq!(city[0].object, Term::plain_literal("NYC"));
    }

    #[test]
    fn striped_nested_node() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <ex:Person rdf:about="http://ex/a">
    <ex:knows>
      <ex:Person rdf:about="http://ex/b"><ex:name>Bob</ex:name></ex:Person>
    </ex:knows>
  </ex:Person>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        // a type Person ; a knows b ; b type Person ; b name Bob = 4
        assert_eq!(ts.len(), 4);
        let knows = find(&ts, "http://ex/knows");
        assert_eq!(knows[0].object, Term::iri("http://ex/b"));
    }

    #[test]
    fn rdf_li_lowers_to_membership_properties() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Seq rdf:about="http://ex/s">
    <rdf:li rdf:resource="http://ex/a"/>
    <rdf:li rdf:resource="http://ex/b"/>
  </rdf:Seq>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert!(ts
            .iter()
            .any(|t| t.predicate == Term::iri(format!("{RDF_NS}_1"))
                && t.object == Term::iri("http://ex/a")));
        assert!(ts
            .iter()
            .any(|t| t.predicate == Term::iri(format!("{RDF_NS}_2"))
                && t.object == Term::iri("http://ex/b")));
        // The container is typed rdf:Seq.
        assert!(ts.iter().any(|t| t.predicate == iri_type()
            && t.object == Term::iri(format!("{RDF_NS}Seq"))));
    }

    #[test]
    fn entity_decoding_in_text_and_attrs() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/a">
    <ex:note>a &lt; b &amp; c</ex:note>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts[0].object, Term::plain_literal("a < b & c"));
    }

    #[test]
    fn bad_xml_errors() {
        assert!(parse_str("<rdf:RDF><unclosed></rdf:RDF>").is_err());
    }
}
