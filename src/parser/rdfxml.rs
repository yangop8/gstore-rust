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
//! * `rdf:parseType="Collection"` (the node elements become an
//!   `rdf:first`/`rdf:rest` list ending in `rdf:nil`)
//! * internal DTD entities: `<!DOCTYPE rdf:RDF [ <!ENTITY xsd "http://…#"> ]>`
//!   and `&xsd;` references in attribute values and text (the OWL idiom
//!   `rdf:resource="&xsd;string"`). A replacement text may refer to other
//!   entities, declared before or after it (XML 1.0 §4.4.7: they are
//!   expanded when the entity is first used); it is inserted as text (not
//!   re-parsed as markup). External entities (`SYSTEM` / `PUBLIC`) are never
//!   fetched — a reference to one is an error — and expansion is bounded
//!   ([`MAX_ENTITY_VALUE`] per entity, [`MAX_ENTITY_EXPANSION`] per
//!   document, which every expanded entity value and every reference in the
//!   content is charged to), so an "entity bomb" (billion laughs, or many
//!   declarations copying one big entity) is refused, not expanded.
//!
//! It is deliberately lenient and does not validate; constructs outside the
//! supported subset are either ignored or surfaced as a parse error.
//!
//! The document is parsed whole; the bulk loaders refuse one larger than
//! [`ImportOptions::max_document_bytes`](crate::parser::import::ImportOptions)
//! (256 MiB by default).

use std::collections::HashMap;
use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::{Term, Triple};
use crate::parser::stream::{BlankLabeler, TermBudget};

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Deepest element nesting accepted. The XML reader, the triple extractor and
/// dropping the element tree all recurse once per level, so an unbounded depth
/// would let one document overflow the stack — which aborts the process. Real
/// RDF/XML nests a handful of levels; this matches the Turtle parser's limit.
const MAX_NESTING_DEPTH: usize = 256;

/// Longest replacement text one DTD entity may expand to (characters).
pub const MAX_ENTITY_VALUE: usize = 64 * 1024;

/// Most characters entity references may add to one document in total:
/// the expanded replacement texts of the entities used, plus every reference
/// in attribute values and text.
pub const MAX_ENTITY_EXPANSION: usize = 4 << 20;

/// One internal DTD entity.
enum Entity {
    /// `SYSTEM` / `PUBLIC`: never fetched.
    External,
    /// A literal value as declared (character references already decoded),
    /// expanded when the entity is first used. Holding it unexpanded keeps the
    /// table as small as the DTD, however many declarations copy one entity,
    /// and lets a value refer to an entity declared after it.
    Declared(Vec<Piece>),
    /// Being expanded: a reference back to it is a loop.
    Expanding,
    /// The replacement text (charged to the document's expansion budget).
    Expanded(String),
}

/// Part of an entity's literal value.
enum Piece {
    Text(String),
    /// `&name;`: a reference to another general entity.
    Ref(String),
}

/// Parse an RDF/XML document into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    parse_str_with(content, &mut BlankLabeler::keep())
}

/// Parse an RDF/XML document, naming blank nodes (`rdf:nodeID` values and
/// anonymous nodes) with `labels`.
pub fn parse_str_with(content: &str, labels: &mut BlankLabeler) -> Result<Vec<Triple>> {
    let root = XmlParser::new(content).parse_document()?;
    let mut ex = Extractor {
        triples: Vec::new(),
        labels: std::mem::take(labels),
        budget: TermBudget::for_input(content.len()),
    };
    let ran = ex.run(&root);
    *labels = ex.labels;
    ran?;
    ex.budget.check()?;
    Ok(ex.triples)
}

/// Parse an RDF/XML file into triples.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let content = crate::parser::import::read_document(path.as_ref())?;
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
    /// Elements currently open, bounded by [`MAX_NESTING_DEPTH`].
    depth: usize,
    /// Internal DTD entities by name.
    entities: HashMap<String, Entity>,
    /// Characters entity expansion has added so far (entity values expanded
    /// plus references in the content), bounded by [`MAX_ENTITY_EXPANSION`].
    expanded: usize,
}

impl XmlParser {
    fn new(s: &str) -> XmlParser {
        XmlParser {
            chars: s.chars().collect(),
            pos: 0,
            depth: 0,
            entities: HashMap::new(),
            expanded: 0,
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

    /// `<!DOCTYPE name ExternalID? [ internal subset ]? >`: the internal
    /// subset's `<!ENTITY …>` declarations are recorded, every other
    /// declaration is skipped.
    fn skip_doctype(&mut self) -> Result<()> {
        self.pos += 9; // "<!DOCTYPE"
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated DOCTYPE")),
                Some('>') => {
                    self.bump();
                    return Ok(());
                }
                Some('[') => {
                    self.bump();
                    break;
                }
                Some(q @ ('"' | '\'')) => self.skip_quoted(q)?,
                Some(_) => {
                    self.bump();
                }
            }
        }
        loop {
            self.skip_ws();
            if self.peek() == Some(']') {
                self.bump();
                self.skip_ws();
                if self.bump() != Some('>') {
                    return Err(self.err("expected '>' to close the DOCTYPE"));
                }
                return Ok(());
            } else if self.starts_with("<!--") {
                self.skip_comment()?;
            } else if self.starts_with("<?") {
                while !self.starts_with("?>") {
                    if self.bump().is_none() {
                        return Err(self.err("unterminated processing instruction"));
                    }
                }
                self.pos += 2;
            } else if self.starts_with("<!ENTITY") {
                self.parse_entity_decl()?;
            } else if self.starts_with("<!") {
                self.skip_decl()?;
            } else if self.peek() == Some('%') {
                // A parameter-entity reference: not expanded (ignored).
                while let Some(c) = self.bump() {
                    if c == ';' {
                        break;
                    }
                }
            } else if self.peek().is_none() {
                return Err(self.err("unterminated DOCTYPE"));
            } else {
                return Err(self.err("unexpected character in the DTD internal subset"));
            }
        }
    }

    /// Skip a quoted string starting at `q`.
    fn skip_quoted(&mut self, q: char) -> Result<()> {
        self.bump();
        loop {
            match self.bump() {
                Some(c) if c == q => return Ok(()),
                Some(_) => {}
                None => return Err(self.err("unterminated quoted string in the DTD")),
            }
        }
    }

    /// Skip a markup declaration up to its closing `>` (quotes respected).
    fn skip_decl(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated declaration in the DTD")),
                Some('>') => {
                    self.bump();
                    return Ok(());
                }
                Some(q @ ('"' | '\'')) => self.skip_quoted(q)?,
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// `<!ENTITY name "value">` (recorded; its entity references are expanded
    /// when it is first used), `<!ENTITY name SYSTEM "uri">` (recorded as
    /// external, never fetched) or `<!ENTITY % name …>` (a parameter entity:
    /// ignored).
    fn parse_entity_decl(&mut self) -> Result<()> {
        self.pos += 8; // "<!ENTITY"
        self.skip_ws();
        let parameter = self.peek() == Some('%');
        if parameter {
            self.bump();
            self.skip_ws();
        }
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '>' || c == '"' || c == '\'' {
                break;
            }
            name.push(c);
            self.bump();
        }
        if name.is_empty() {
            return Err(self.err("<!ENTITY> without a name"));
        }
        self.skip_ws();
        let value = match self.peek() {
            Some(q @ ('"' | '\'')) => {
                self.bump();
                let mut raw = String::new();
                loop {
                    match self.bump() {
                        Some(c) if c == q => break,
                        Some(c) => raw.push(c),
                        None => return Err(self.err("unterminated entity value")),
                    }
                }
                Entity::Declared(self.entity_value_pieces(&name, &raw)?)
            }
            _ => Entity::External,
        };
        self.skip_decl()?;
        // The first declaration of a name is binding (XML 1.0 §4.2).
        if !parameter && !self.entities.contains_key(&name) {
            self.entities.insert(name, value);
        }
        Ok(())
    }

    /// Split an entity's literal value into text (character and predefined
    /// references decoded now) and references to other entities (expanded on
    /// first use, by [`XmlParser::expand_entity`]).
    fn entity_value_pieces(&self, name: &str, raw: &str) -> Result<Vec<Piece>> {
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut rest = raw;
        while let Some(i) = rest.find('&') {
            text.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            let end = after
                .find(';')
                .ok_or_else(|| self.err(format!("unterminated reference in entity '{name}'")))?;
            let reference = &after[..end];
            match self.decode_reference(reference)? {
                Some(c) => text.push(c),
                None => {
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    pieces.push(Piece::Ref(reference.to_string()));
                }
            }
            rest = &after[end + 1..];
        }
        text.push_str(rest);
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Ok(pieces)
    }

    /// Expand entity `name` (and, first, every entity its value refers to)
    /// unless that was done already. Each expanded value is bounded by
    /// [`MAX_ENTITY_VALUE`] and charged once to the document's
    /// [`MAX_ENTITY_EXPANSION`] budget; a loop of references is an error.
    /// Iterative, so a long chain of entities cannot overflow the stack.
    fn expand_entity(&mut self, name: &str) -> Result<()> {
        struct Frame {
            name: String,
            pieces: Vec<Piece>,
            next: usize,
            out: String,
        }
        let mut stack: Vec<Frame> = Vec::new();
        let mut want = Some(name.to_string());
        loop {
            if let Some(n) = want.take() {
                match self.entities.get_mut(&n) {
                    Some(Entity::Expanded(_)) => {}
                    Some(slot @ Entity::Declared(_)) => {
                        let Entity::Declared(pieces) = std::mem::replace(slot, Entity::Expanding)
                        else {
                            unreachable!()
                        };
                        stack.push(Frame {
                            name: n,
                            pieces,
                            next: 0,
                            out: String::new(),
                        });
                    }
                    Some(Entity::Expanding) => {
                        return Err(self.err(format!(
                            "entity '&{n};' refers to itself (recursive entity reference)"
                        )))
                    }
                    Some(Entity::External) => {
                        return Err(self.err(format!("external entity '&{n};' is not supported")))
                    }
                    None => {
                        return Err(match stack.last() {
                            Some(f) => self.err(format!(
                                "entity '{}' refers to undeclared entity '&{n};'",
                                f.name
                            )),
                            None => self.err(format!("unknown entity '&{n};'")),
                        })
                    }
                }
            }
            let Some(top) = stack.last_mut() else {
                return Ok(());
            };
            match top.pieces.get(top.next) {
                Some(Piece::Text(t)) => {
                    top.out.push_str(t);
                    top.next += 1;
                }
                Some(Piece::Ref(r)) => match self.entities.get(r) {
                    Some(Entity::Expanded(t)) => {
                        top.out.push_str(t);
                        top.next += 1;
                    }
                    // Expand it first; this piece is read again afterwards.
                    _ => {
                        want = Some(r.clone());
                        continue;
                    }
                },
                None => {
                    let done = stack.pop().expect("a frame is open");
                    self.expanded += done.out.len();
                    if self.expanded > MAX_ENTITY_EXPANSION {
                        return Err(self.err(format!(
                            "entity references expand beyond {MAX_ENTITY_EXPANSION} characters \
                             (entity expansion limit)"
                        )));
                    }
                    self.entities.insert(done.name, Entity::Expanded(done.out));
                    continue;
                }
            }
            if top.out.len() > MAX_ENTITY_VALUE {
                return Err(self.err(format!(
                    "entity '{}' expands beyond {MAX_ENTITY_VALUE} characters (entity expansion limit)",
                    top.name
                )));
            }
        }
    }

    /// A predefined entity or character reference (`lt`, `#x41`, …) as a
    /// character; `None` for any other name.
    fn decode_reference(&self, name: &str) -> Result<Option<char>> {
        Ok(Some(match name {
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
                    return Ok(None);
                }
            }
        }))
    }

    fn parse_document(&mut self) -> Result<XmlElement> {
        self.skip_misc()?;
        if self.peek() != Some('<') {
            return Err(self.err("expected an XML root element"));
        }
        self.parse_element()
    }

    /// Parse an element starting at `<`, refusing nesting deeper than
    /// [`MAX_NESTING_DEPTH`].
    fn parse_element(&mut self) -> Result<XmlElement> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.err(format!(
                "elements nested more than {MAX_NESTING_DEPTH} levels deep"
            )));
        }
        self.depth += 1;
        let el = self.parse_element_inner();
        self.depth -= 1;
        el
    }

    fn parse_element_inner(&mut self) -> Result<XmlElement> {
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
                    self.read_entity(&mut text)?;
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
                    Some('&') => self.read_entity(&mut value)?,
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

    /// Decode one XML entity reference starting at `&` into `out`: a
    /// predefined entity, a character reference, or a DTD entity (counted
    /// against [`MAX_ENTITY_EXPANSION`]).
    fn read_entity(&mut self, out: &mut String) -> Result<()> {
        debug_assert_eq!(self.peek(), Some('&'));
        self.bump();
        let mut name = String::new();
        loop {
            match self.peek() {
                Some(';') => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    name.push(c);
                    self.bump();
                    if name.len() > 256 {
                        return Err(self.err("unterminated entity reference"));
                    }
                }
                None => return Err(self.err("unterminated entity reference")),
            }
        }
        if let Some(c) = self.decode_reference(&name)? {
            out.push(c);
            return Ok(());
        }
        self.expand_entity(&name)?;
        let Some(Entity::Expanded(text)) = self.entities.get(&name) else {
            unreachable!("expand_entity leaves the entity expanded")
        };
        self.expanded += text.len();
        if self.expanded > MAX_ENTITY_EXPANSION {
            return Err(self.err(format!(
                "entity references expand beyond {MAX_ENTITY_EXPANSION} characters \
                 (entity expansion limit)"
            )));
        }
        out.push_str(text);
        Ok(())
    }
}

// ---- triple extraction ----------------------------------------------------

struct Extractor {
    triples: Vec<Triple>,
    /// Names `rdf:nodeID` values and anonymous nodes.
    labels: BlankLabeler,
    /// Bounds what `triples` holds.
    budget: TermBudget,
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
        Term::Blank(self.labels.fresh())
    }

    /// The node an `rdf:nodeID` names.
    fn node_id(&self, nid: &str) -> Term {
        Term::Blank(self.labels.explicit(nid))
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
            self.node_id(&nid)
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
            let obj = self.node_id(&nid);
            self.push(subject.clone(), predicate, obj);
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
                "Collection" => {
                    // The node elements, in order, become an RDF list.
                    let mut items = Vec::new();
                    for child in pe.elements() {
                        items.push(self.node(child, &ctx)?);
                    }
                    let nil = Term::Iri(format!("{RDF_NS}nil"));
                    if items.is_empty() {
                        self.push(subject.clone(), predicate, nil);
                        return Ok(());
                    }
                    let first = Term::Iri(format!("{RDF_NS}first"));
                    let rest = Term::Iri(format!("{RDF_NS}rest"));
                    let head = self.fresh_blank();
                    self.push(subject.clone(), predicate, head.clone());
                    let n = items.len();
                    let mut cell = head;
                    for (i, item) in items.into_iter().enumerate() {
                        self.push(cell.clone(), first.clone(), item);
                        let next = if i + 1 < n {
                            self.fresh_blank()
                        } else {
                            nil.clone()
                        };
                        self.push(cell, rest.clone(), next.clone());
                        cell = next;
                    }
                    return Ok(());
                }
                _ => { /* other: fall through to generic handling. */ }
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
        if self.budget.admit([&s, &p, &o]) {
            self.triples.push(Triple::new(s, p, o));
        } else {
            self.triples = Vec::new();
        }
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

    /// Elements nested 50 000 deep are a parse error, not a stack overflow
    /// (which would abort the whole server: `/upload` and `/batchInsert`
    /// accept RDF/XML); nesting within the limit still parses.
    #[test]
    fn absurdly_deep_nesting_is_refused_not_a_stack_overflow() {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let open = "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:ex=\"http://ex/\">";
                let deep = format!("{open}{}{}</rdf:RDF>", "<a>".repeat(50_000), "</a>".repeat(50_000));
                let err = parse_str(&deep).unwrap_err().to_string();
                assert!(err.contains("nested more than"), "{err}");
                // Striped node/property nesting just inside the limit.
                let levels = MAX_NESTING_DEPTH / 2 - 1;
                let mut doc = String::from(open);
                for i in 0..levels {
                    doc.push_str(&format!("<rdf:Description rdf:about=\"http://ex/n{i}\"><ex:next>"));
                }
                doc.push_str("<rdf:Description rdf:about=\"http://ex/end\"/>");
                doc.push_str(&"</ex:next></rdf:Description>".repeat(levels));
                doc.push_str("</rdf:RDF>");
                assert_eq!(parse_str(&doc).unwrap().len(), levels);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn bad_xml_errors() {
        assert!(parse_str("<rdf:RDF><unclosed></rdf:RDF>").is_err());
    }

    #[test]
    fn internal_dtd_entities_expand_in_attributes_and_text() {
        let doc = r#"<?xml version="1.0"?>
<!DOCTYPE rdf:RDF [
  <!-- the usual OWL preamble -->
  <!ENTITY xsd "http://www.w3.org/2001/XMLSchema#" >
  <!ENTITY ex 'http://ex/' >
  <!ENTITY exa "&ex;a" >
  <!ENTITY copy "&#169;">
  <!ELEMENT ignored ANY>
  <!ENTITY % pe "ignored">
  %pe;
]>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="&ex;">
  <rdf:Description rdf:about="&exa;">
    <ex:age rdf:datatype="&xsd;integer">7</ex:age>
    <ex:note>&copy; &ex; &amp; more</ex:note>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].subject, Term::iri("http://ex/a"));
        assert_eq!(
            ts[0].object,
            Term::typed_literal("7", "http://www.w3.org/2001/XMLSchema#integer")
        );
        assert_eq!(ts[1].predicate, Term::iri("http://ex/note"));
        assert_eq!(ts[1].object, Term::plain_literal("© http://ex/ & more"));
    }

    #[test]
    fn entity_bombs_and_external_entities_are_refused() {
        // Billion laughs: each level multiplies the expansion by ten.
        let mut dtd = String::from("<!ENTITY lol0 \"lol\">");
        for i in 1..10 {
            let prev = format!("&lol{};", i - 1);
            dtd.push_str(&format!("<!ENTITY lol{i} \"{}\">", prev.repeat(10)));
        }
        let doc = format!(
            "<!DOCTYPE rdf:RDF [{dtd}]><rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:ex=\"http://ex/\">\
             <rdf:Description rdf:about=\"http://ex/a\"><ex:p>&lol9;</ex:p></rdf:Description></rdf:RDF>"
        );
        let err = parse_str(&doc).unwrap_err().to_string();
        assert!(err.contains("entity expansion limit"), "{err}");
        // Many references to a mid-sized entity hit the per-document budget.
        let big = "x".repeat(60_000);
        let refs = "&big;".repeat(100);
        let doc = format!(
            "<!DOCTYPE r [<!ENTITY big \"{big}\">]><rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:ex=\"http://ex/\">\
             <rdf:Description rdf:about=\"http://ex/a\"><ex:p>{refs}</ex:p></rdf:Description></rdf:RDF>"
        );
        let err = parse_str(&doc).unwrap_err().to_string();
        assert!(err.contains("entity expansion limit"), "{err}");
        // An external entity is never fetched.
        let doc = format!(
            "<!DOCTYPE r [<!ENTITY ext SYSTEM \"file:///etc/passwd\">]><rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:ex=\"http://ex/\">\
             <rdf:Description rdf:about=\"http://ex/a\"><ex:p>&ext;</ex:p></rdf:Description></rdf:RDF>"
        );
        let err = parse_str(&doc).unwrap_err().to_string();
        assert!(err.contains("external entity"), "{err}");
        // An undeclared entity is an error, as before.
        assert!(parse_str(&format!(
            "<rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:ex=\"http://ex/\"><rdf:Description rdf:about=\"&nope;\"/></rdf:RDF>"
        ))
        .is_err());
    }

    #[test]
    fn many_declarations_copying_a_big_entity_stay_within_the_budget() {
        // Each `<!ENTITY eN "&big;">` costs ~20 input bytes; storing a copy of
        // `big` for each would multiply a small DTD into gigabytes.
        let big = "a".repeat(60_000);
        let decls: String = (0..200)
            .map(|i| format!("<!ENTITY e{i} \"&big;\">"))
            .collect();
        let doc = |body: &str| {
            format!(
                "<!DOCTYPE r [<!ENTITY big \"{big}\">{decls}]><rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:ex=\"http://ex/\">\
                 <rdf:Description rdf:about=\"http://ex/a\"><ex:p>{body}</ex:p></rdf:Description></rdf:RDF>"
            )
        };
        let unused = doc("v");
        let mut p = XmlParser::new(&unused);
        p.parse_document().unwrap();
        let stored: usize = p
            .entities
            .values()
            .map(|e| match e {
                Entity::Declared(pieces) => pieces
                    .iter()
                    .map(|p| match p {
                        Piece::Text(t) | Piece::Ref(t) => t.len(),
                    })
                    .sum(),
                Entity::Expanded(t) => t.len(),
                Entity::External | Entity::Expanding => 0,
            })
            .sum();
        assert!(
            stored < 2 * unused.len(),
            "the entity table holds {stored} characters"
        );
        assert_eq!(parse_str(&unused).unwrap().len(), 1);
        // Using them is charged to the document budget.
        let used: String = (0..200).map(|i| format!("&e{i};")).collect();
        let err = parse_str(&doc(&used)).unwrap_err().to_string();
        assert!(err.contains("entity expansion limit"), "{err}");
    }

    #[test]
    fn entity_values_may_refer_to_entities_declared_after_them() {
        let doc = |dtd: &str, about: &str| {
            format!(
                "<!DOCTYPE r [{dtd}]><rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:ex=\"http://ex/\">\
                 <rdf:Description rdf:about=\"{about}\"><ex:p>v</ex:p></rdf:Description></rdf:RDF>"
            )
        };
        let ts = parse_str(&doc(
            "<!ENTITY a \"&b;x\"><!ENTITY b \"http://ex/\">",
            "&a;",
        ))
        .unwrap();
        assert_eq!(ts[0].subject, Term::iri("http://ex/x"));
        // An unused entity that refers to an undeclared one is harmless …
        let ts = parse_str(&doc(
            "<!ENTITY a \"&nope;\"><!ENTITY b \"http://ex/\">",
            "&b;s",
        ))
        .unwrap();
        assert_eq!(ts[0].subject, Term::iri("http://ex/s"));
        // … using it is an error, and so is a loop of references.
        let err = parse_str(&doc("<!ENTITY a \"&nope;\">", "&a;"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("undeclared entity '&nope;'"), "{err}");
        let err = parse_str(&doc("<!ENTITY a \"&b;\"><!ENTITY b \"x&a;\">", "&a;"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("recursive entity reference"), "{err}");
        // A long chain expands without recursing once per link.
        let mut dtd = String::from("<!ENTITY c0 \"http://ex/\">");
        for i in 1..5000 {
            dtd.push_str(&format!("<!ENTITY c{i} \"&c{};\">", i - 1));
        }
        let ts = parse_str(&doc(&dtd, "&c4999;z")).unwrap();
        assert_eq!(ts[0].subject, Term::iri("http://ex/z"));
    }

    #[test]
    fn parse_type_collection_builds_an_rdf_list() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/basket">
    <ex:has rdf:parseType="Collection">
      <rdf:Description rdf:about="http://ex/banana"/>
      <ex:Fruit rdf:about="http://ex/apple"/>
      <rdf:Description rdf:about="http://ex/pear"/>
    </ex:has>
    <ex:none rdf:parseType="Collection"></ex:none>
  </rdf:Description>
</rdf:RDF>"#;
        let ts = parse_str(doc).unwrap();
        let nil = Term::iri(format!("{RDF_NS}nil"));
        let has = find(&ts, "http://ex/has");
        assert_eq!(has.len(), 1);
        // Walk the list from its head.
        let mut cell = has[0].object.clone();
        let mut items = Vec::new();
        while cell != nil {
            let first = ts
                .iter()
                .find(|t| t.subject == cell && t.predicate == Term::iri(format!("{RDF_NS}first")))
                .unwrap();
            items.push(first.object.clone());
            cell = ts
                .iter()
                .find(|t| t.subject == cell && t.predicate == Term::iri(format!("{RDF_NS}rest")))
                .unwrap()
                .object
                .clone();
        }
        assert_eq!(
            items,
            [
                Term::iri("http://ex/banana"),
                Term::iri("http://ex/apple"),
                Term::iri("http://ex/pear")
            ]
        );
        // The typed node element still types its node.
        assert!(ts.iter().any(|t| t.subject == Term::iri("http://ex/apple")
            && t.object == Term::iri("http://ex/Fruit")));
        assert_eq!(find(&ts, "http://ex/none")[0].object, nil);
    }

    #[test]
    fn node_ids_and_minted_nodes_follow_the_labeler() {
        let doc = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="http://ex/">
  <rdf:Description rdf:nodeID="n1"><ex:p rdf:parseType="Resource"><ex:q>v</ex:q></ex:p></rdf:Description>
</rdf:RDF>"#;
        let mut labels = BlankLabeler::scoped("S");
        let ts = parse_str_with(doc, &mut labels).unwrap();
        let p = find(&ts, "http://ex/p");
        assert_eq!(p[0].subject, Term::blank("S_n1"));
        assert_eq!(p[0].object, Term::blank("S-0"));
    }
}
