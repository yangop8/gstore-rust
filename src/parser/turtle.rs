//! Turtle (subset) parser.
//!
//! gStore's `RDFParser` accepts Turtle as well as N-Triples; several bundled
//! datasets (e.g. `data/lubm/lubm.nt`) are actually Turtle — they open with
//! `@prefix` directives and use prefixed names (`rdf:type`, `ub:University`).
//!
//! Since N-Triples is a syntactic subset of Turtle, this parser is the primary
//! RDF importer. The supported subset covers what real gStore datasets use:
//!
//! * `@prefix p: <iri> .` / `@base <iri> .` (and SPARQL-style `PREFIX`/`BASE`)
//! * prefixed names `p:local`, the `a` keyword (= rdf:type)
//! * predicate-object lists (`;`) and object lists (`,`)
//! * IRIs, blank nodes, plain/typed/lang literals, numeric & boolean literals
//! * `[ … ]` blank-node property lists and `( … )` collections (lowered to an
//!   `rdf:first`/`rdf:rest`/`rdf:nil` chain), including nesting

use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::{Term, Triple};
use crate::parser::sparql::ast::{xsd, RDF_TYPE};

/// Parse a Turtle document into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    let mut p = Parser::new(content);
    p.parse_document()?;
    Ok(p.triples)
}

/// Parse a Turtle file into triples.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let content = std::fs::read_to_string(path)?;
    parse_str(&content)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    prefixes: std::collections::HashMap<String, String>,
    base: Option<String>,
    triples: Vec<Triple>,
    /// Counter for fresh blank nodes minted by `[ … ]` / `( … )`.
    bnode: usize,
}

/// The `rdf:` namespace base (for collection `rdf:first`/`rest`/`nil`).
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

impl Parser {
    fn new(s: &str) -> Parser {
        Parser {
            chars: s.chars().collect(),
            pos: 0,
            line: 1,
            prefixes: std::collections::HashMap::new(),
            base: None,
            triples: Vec::new(),
            bnode: 0,
        }
    }

    /// Mint a fresh, document-unique blank node for an anonymous construct.
    fn fresh_blank(&mut self) -> Term {
        let label = format!("genid{}", self.bnode);
        self.bnode += 1;
        Term::Blank(label)
    }

    fn err(&self, msg: impl Into<String>) -> GStoreError {
        GStoreError::RdfParse {
            line: self.line,
            msg: msg.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if let Some(c) = c {
            self.pos += 1;
            if c == '\n' {
                self.line += 1;
            }
        }
        c
    }

    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn parse_document(&mut self) -> Result<()> {
        loop {
            self.skip_ws();
            if self.eof() {
                return Ok(());
            }
            match self.peek() {
                Some('@') => self.parse_directive()?,
                Some(c) if c.is_ascii_alphabetic() && self.looks_like_keyword_directive() => {
                    self.parse_sparql_directive()?
                }
                _ => self.parse_triples_statement()?,
            }
        }
    }

    /// Is the upcoming bareword a SPARQL-style `PREFIX`/`BASE` directive?
    fn looks_like_keyword_directive(&self) -> bool {
        let word: String = self.chars[self.pos..]
            .iter()
            .take(6)
            .collect::<String>()
            .to_ascii_uppercase();
        word.starts_with("PREFIX") || word.starts_with("BASE")
    }

    fn parse_directive(&mut self) -> Result<()> {
        self.bump(); // '@'
        let kw = self.read_bareword();
        match kw.as_str() {
            "prefix" => {
                self.skip_ws();
                let ns = self.read_prefix_ns()?;
                self.skip_ws();
                let iri = self.parse_iriref()?;
                self.expect_dot()?;
                self.prefixes.insert(ns, iri);
            }
            "base" => {
                self.skip_ws();
                let iri = self.parse_iriref()?;
                self.expect_dot()?;
                self.base = Some(iri);
            }
            other => return Err(self.err(format!("unknown directive '@{other}'"))),
        }
        Ok(())
    }

    fn parse_sparql_directive(&mut self) -> Result<()> {
        let kw = self.read_bareword().to_ascii_uppercase();
        match kw.as_str() {
            "PREFIX" => {
                self.skip_ws();
                let ns = self.read_prefix_ns()?;
                self.skip_ws();
                let iri = self.parse_iriref()?;
                self.prefixes.insert(ns, iri);
            }
            "BASE" => {
                self.skip_ws();
                let iri = self.parse_iriref()?;
                self.base = Some(iri);
            }
            other => return Err(self.err(format!("unknown directive '{other}'"))),
        }
        Ok(())
    }

    /// Read a prefix namespace token `p:` or `:`, returning the prefix (no colon).
    fn read_prefix_ns(&mut self) -> Result<String> {
        let mut ns = String::new();
        while let Some(c) = self.peek() {
            if c == ':' {
                self.bump();
                return Ok(ns);
            } else if c.is_whitespace() {
                break;
            } else {
                ns.push(c);
                self.bump();
            }
        }
        Err(self.err("expected ':' in prefix declaration"))
    }

    fn parse_triples_statement(&mut self) -> Result<()> {
        self.skip_ws();
        // A `[ … ]` / `( … )` subject already emits its own triples, so the
        // outer predicate-object list is optional (`[ :p :o ] .` is a statement).
        let subject_is_bracket = matches!(self.peek(), Some('[') | Some('('));
        let subject = self.parse_term(false)?;
        self.skip_ws();
        if subject_is_bracket && self.peek() == Some('.') {
            self.expect_dot()?;
            return Ok(());
        }
        self.parse_predicate_object_list(&subject)?;
        self.expect_dot()?;
        Ok(())
    }

    fn parse_predicate_object_list(&mut self, subject: &Term) -> Result<()> {
        loop {
            self.skip_ws();
            let predicate = self.parse_verb()?;
            // object list
            loop {
                let object = self.parse_term(false)?;
                self.triples
                    .push(Triple::new(subject.clone(), predicate.clone(), object));
                self.skip_ws();
                if self.peek() == Some(',') {
                    self.bump();
                    continue;
                }
                break;
            }
            self.skip_ws();
            if self.peek() == Some(';') {
                self.bump();
                self.skip_ws();
                // trailing ';' before '.' is allowed
                if self.peek() == Some('.') || self.eof() {
                    break;
                }
                continue;
            }
            break;
        }
        Ok(())
    }

    fn parse_verb(&mut self) -> Result<Term> {
        self.skip_ws();
        // 'a' keyword (must be standalone)
        if self.peek() == Some('a') && self.peek2().is_none_or(|c| c.is_whitespace() || c == '<') {
            self.bump();
            return Ok(Term::iri(RDF_TYPE));
        }
        self.parse_term(true)
    }

    /// Parse a term. `verb_pos` is true when parsing a predicate (no literals).
    fn parse_term(&mut self, verb_pos: bool) -> Result<Term> {
        self.skip_ws();
        match self.peek() {
            Some('<') => Ok(Term::Iri(self.parse_iriref()?)),
            Some('"') | Some('\'') if !verb_pos => self.parse_literal(),
            Some('[') if !verb_pos => self.parse_blank_node_property_list(),
            Some('(') if !verb_pos => self.parse_collection(),
            Some('_') => self.parse_blank(),
            Some(c) if (c.is_ascii_digit() || c == '+' || c == '-' || c == '.') && !verb_pos => {
                self.parse_numeric()
            }
            Some(_) => self.parse_prefixed_or_keyword(verb_pos),
            None => Err(self.err("expected a term, found end of input")),
        }
    }

    /// `[ predicateObjectList? ]` — a fresh blank node carrying the listed
    /// properties. Returns the blank node so it can fill the enclosing position.
    fn parse_blank_node_property_list(&mut self) -> Result<Term> {
        self.expect('[')?;
        let node = self.fresh_blank();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.bump(); // empty `[]` is just an anonymous blank node
            return Ok(node);
        }
        self.parse_predicate_object_list(&node)?;
        self.skip_ws();
        self.expect(']')?;
        Ok(node)
    }

    /// `( item* )` — an RDF collection, lowered to an `rdf:first`/`rdf:rest`
    /// chain terminated by `rdf:nil`. Returns the chain head (`rdf:nil` if empty).
    fn parse_collection(&mut self) -> Result<Term> {
        self.expect('(')?;
        let nil = Term::iri(format!("{RDF_NS}nil"));
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(')') => {
                    self.bump();
                    break;
                }
                None => return Err(self.err("unterminated collection '('")),
                _ => items.push(self.parse_term(false)?),
            }
        }
        if items.is_empty() {
            return Ok(nil);
        }
        let first = Term::iri(format!("{RDF_NS}first"));
        let rest = Term::iri(format!("{RDF_NS}rest"));
        let head = self.fresh_blank();
        let mut current = head.clone();
        let n = items.len();
        for (i, item) in items.into_iter().enumerate() {
            self.triples
                .push(Triple::new(current.clone(), first.clone(), item));
            let next = if i + 1 < n {
                self.fresh_blank()
            } else {
                nil.clone()
            };
            self.triples
                .push(Triple::new(current.clone(), rest.clone(), next.clone()));
            current = next;
        }
        Ok(head)
    }

    fn parse_iriref(&mut self) -> Result<String> {
        self.expect('<')?;
        let mut s = String::new();
        loop {
            match self.bump() {
                Some('>') => return Ok(self.resolve_iri(&s)),
                Some('\\') => {
                    // \uXXXX / \UXXXXXXXX
                    match self.bump() {
                        Some('u') => self.read_hex(&mut s, 4)?,
                        Some('U') => self.read_hex(&mut s, 8)?,
                        other => return Err(self.err(format!("invalid IRI escape '\\{other:?}'"))),
                    }
                }
                Some(c) => s.push(c),
                None => return Err(self.err("unterminated IRI")),
            }
        }
    }

    fn parse_blank(&mut self) -> Result<Term> {
        self.expect('_')?;
        self.expect(':')?;
        let mut label = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                // Don't absorb a '.' that terminates the statement.
                if c == '.' && self.peek2().is_none_or(|n| n.is_whitespace()) {
                    break;
                }
                label.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if label.is_empty() {
            return Err(self.err("empty blank node label"));
        }
        Ok(Term::Blank(label))
    }

    fn parse_prefixed_or_keyword(&mut self, verb_pos: bool) -> Result<Term> {
        // Could be `prefix:local`, `:local`, or a boolean literal.
        let start = self.pos;
        let mut prefix = String::new();
        while let Some(c) = self.peek() {
            // Stop at the ':' that ends the prefix, or any term separator.
            if c == ':' || c.is_whitespace() || matches!(c, '.' | ';' | ',') {
                break;
            }
            prefix.push(c);
            self.bump();
        }
        if self.peek() == Some(':') {
            self.bump();
            let local = self.read_pn_local();
            let ns = self
                .prefixes
                .get(&prefix)
                .ok_or_else(|| self.err(format!("undefined prefix '{prefix}:'")))?;
            return Ok(Term::Iri(format!("{ns}{local}")));
        }
        // Not a prefixed name: maybe a boolean literal in object position.
        if !verb_pos && (prefix == "true" || prefix == "false") {
            return Ok(Term::typed_literal(prefix, xsd::BOOLEAN));
        }
        self.pos = start;
        Err(self.err(format!(
            "expected a prefixed name or IRI, found '{}'",
            self.peek().unwrap_or(' ')
        )))
    }

    fn read_pn_local(&mut self) -> String {
        let mut local = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '%' {
                local.push(c);
                self.bump();
            } else if c == '.' {
                // A '.' is part of the local name only if a name char follows.
                if self
                    .peek2()
                    .is_some_and(|n| n.is_alphanumeric() || n == '_' || n == '-' || n == '%')
                {
                    local.push('.');
                    self.bump();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        local
    }

    fn parse_literal(&mut self) -> Result<Term> {
        let quote = self.bump().unwrap(); // " or '
        let triple = self.peek() == Some(quote) && self.peek2() == Some(quote);
        if triple {
            self.bump();
            self.bump();
        }
        let mut value = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated literal")),
                Some('\\') => {
                    self.bump();
                    self.read_string_escape(&mut value)?;
                }
                Some(c) if c == quote => {
                    if triple {
                        if self.peek2() == Some(quote)
                            && self.chars.get(self.pos + 2).copied() == Some(quote)
                        {
                            self.bump();
                            self.bump();
                            self.bump();
                            break;
                        }
                        value.push(c);
                        self.bump();
                    } else {
                        self.bump();
                        break;
                    }
                }
                Some(c) => {
                    value.push(c);
                    self.bump();
                }
            }
        }
        // optional @lang or ^^datatype
        match self.peek() {
            Some('@') => {
                self.bump();
                let mut lang = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        lang.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                Ok(Term::Literal {
                    value,
                    datatype: None,
                    lang: Some(lang),
                })
            }
            Some('^') if self.peek2() == Some('^') => {
                self.bump();
                self.bump();
                let dt = match self.parse_term(true)? {
                    Term::Iri(iri) => iri,
                    other => {
                        return Err(self.err(format!("datatype must be an IRI, got {other:?}")))
                    }
                };
                Ok(Term::Literal {
                    value,
                    datatype: Some(dt),
                    lang: None,
                })
            }
            _ => Ok(Term::Literal {
                value,
                datatype: None,
                lang: None,
            }),
        }
    }

    fn parse_numeric(&mut self) -> Result<Term> {
        let mut s = String::new();
        let mut is_decimal = false;
        let mut is_double = false;
        if matches!(self.peek(), Some('+') | Some('-')) {
            s.push(self.bump().unwrap());
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.bump();
            } else if c == '.' && self.peek2().is_some_and(|n| n.is_ascii_digit()) {
                is_decimal = true;
                s.push(c);
                self.bump();
            } else if c == 'e' || c == 'E' {
                is_double = true;
                s.push(c);
                self.bump();
                if matches!(self.peek(), Some('+') | Some('-')) {
                    s.push(self.bump().unwrap());
                }
            } else {
                break;
            }
        }
        if s.is_empty() || s.chars().all(|c| c == '+' || c == '-') {
            return Err(self.err("invalid numeric literal"));
        }
        let dt = if is_double {
            xsd::DOUBLE
        } else if is_decimal {
            xsd::DECIMAL
        } else {
            xsd::INTEGER
        };
        Ok(Term::typed_literal(s, dt))
    }

    fn read_string_escape(&mut self, out: &mut String) -> Result<()> {
        match self.bump() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            Some('u') => self.read_hex(out, 4)?,
            Some('U') => self.read_hex(out, 8)?,
            Some(c) => return Err(self.err(format!("invalid escape '\\{c}'"))),
            None => return Err(self.err("dangling backslash")),
        }
        Ok(())
    }

    fn read_hex(&mut self, out: &mut String, n: usize) -> Result<()> {
        let mut code = 0u32;
        for _ in 0..n {
            let c = self
                .bump()
                .ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| self.err(format!("bad hex digit '{c}'")))?;
            code = code * 16 + d;
        }
        out.push(char::from_u32(code).ok_or_else(|| self.err("invalid code point"))?);
        Ok(())
    }

    fn read_bareword(&mut self) -> String {
        let mut w = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                w.push(c);
                self.bump();
            } else {
                break;
            }
        }
        w
    }

    fn expect(&mut self, c: char) -> Result<()> {
        if self.peek() == Some(c) {
            self.bump();
            Ok(())
        } else {
            Err(self.err(format!("expected '{c}', found {:?}", self.peek())))
        }
    }

    fn expect_dot(&mut self) -> Result<()> {
        self.skip_ws();
        self.expect('.')
    }

    fn resolve_iri(&self, s: &str) -> String {
        match &self.base {
            Some(base) if !is_absolute_iri(s) => resolve_reference(base, s),
            _ => s.to_string(),
        }
    }
}

fn is_absolute_iri(s: &str) -> bool {
    match s.find(':') {
        Some(idx) => is_valid_scheme(&s[..idx]),
        None => false,
    }
}

/// `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` (RFC 3986 §3.1).
/// The first character must be a letter; an empty scheme is invalid.
fn is_valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        }
        _ => false,
    }
}

/// Resolve a relative IRI `reference` against an absolute `base` following the
/// reference-resolution algorithm of RFC 3986 §5.2 (the relative branches; the
/// caller guarantees `reference` has no scheme). This fixes the previous naive
/// concatenation, which produced e.g. `http://ex.com/pathfile` for base
/// `http://ex.com/path` + `file` instead of the correct `http://ex.com/file`.
fn resolve_reference(base: &str, reference: &str) -> String {
    let (b_scheme, b_authority, b_path, b_query, _) = split_iri(base);
    let (_, r_authority, r_path, r_query, r_fragment) = split_iri(reference);

    let (authority, path, query) = if r_authority.is_some() {
        (r_authority, remove_dot_segments(r_path), r_query)
    } else if r_path.is_empty() {
        let query = if r_query.is_some() { r_query } else { b_query };
        (b_authority, b_path.to_string(), query)
    } else if r_path.starts_with('/') {
        (b_authority, remove_dot_segments(r_path), r_query)
    } else {
        let merged = merge_paths(b_authority, b_path, r_path);
        (b_authority, remove_dot_segments(&merged), r_query)
    };

    recompose(b_scheme, authority, &path, query, r_fragment)
}

/// Split an IRI reference into `(scheme, authority, path, query, fragment)`
/// per RFC 3986 §3. `scheme` and `path` are always present (possibly empty);
/// `authority`, `query` and `fragment` are present only when their delimiters
/// (`//`, `?`, `#`) appear.
#[allow(clippy::type_complexity)]
fn split_iri(s: &str) -> (&str, Option<&str>, &str, Option<&str>, Option<&str>) {
    let mut rest = s;
    let mut fragment = None;
    if let Some(i) = rest.find('#') {
        fragment = Some(&rest[i + 1..]);
        rest = &rest[..i];
    }
    let mut query = None;
    if let Some(i) = rest.find('?') {
        query = Some(&rest[i + 1..]);
        rest = &rest[..i];
    }
    let mut scheme = "";
    if let Some(i) = rest.find(':') {
        if is_valid_scheme(&rest[..i]) {
            scheme = &rest[..i];
            rest = &rest[i + 1..];
        }
    }
    let mut authority = None;
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find('/').unwrap_or(after.len());
        authority = Some(&after[..end]);
        rest = &after[end..];
    }
    (scheme, authority, rest, query, fragment)
}

/// Merge a relative-path reference onto the base path (RFC 3986 §5.3).
fn merge_paths(base_authority: Option<&str>, base_path: &str, ref_path: &str) -> String {
    if base_authority.is_some() && base_path.is_empty() {
        format!("/{ref_path}")
    } else if let Some(i) = base_path.rfind('/') {
        format!("{}{}", &base_path[..=i], ref_path)
    } else {
        ref_path.to_string()
    }
}

/// Remove `.` and `..` path segments (RFC 3986 §5.2.4).
fn remove_dot_segments(path: &str) -> String {
    let mut input = path.to_string();
    let mut output = String::new();
    while !input.is_empty() {
        if input.starts_with("../") {
            input.drain(..3);
        } else if input.starts_with("./") {
            input.drain(..2);
        } else if input.starts_with("/./") {
            input.replace_range(..3, "/");
        } else if input == "/." {
            input.replace_range(..2, "/");
        } else if input.starts_with("/../") {
            input.replace_range(..4, "/");
            remove_last_segment(&mut output);
        } else if input == "/.." {
            input.replace_range(..3, "/");
            remove_last_segment(&mut output);
        } else if input == "." || input == ".." {
            input.clear();
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = match input[start..].find('/') {
                Some(i) => start + i,
                None => input.len(),
            };
            output.push_str(&input[..end]);
            input.drain(..end);
        }
    }
    output
}

fn remove_last_segment(output: &mut String) {
    match output.rfind('/') {
        Some(i) => output.truncate(i),
        None => output.clear(),
    }
}

/// Recompose components into an IRI string (RFC 3986 §5.3).
fn recompose(
    scheme: &str,
    authority: Option<&str>,
    path: &str,
    query: Option<&str>,
    fragment: Option<&str>,
) -> String {
    let mut out = String::new();
    if !scheme.is_empty() {
        out.push_str(scheme);
        out.push(':');
    }
    if let Some(a) = authority {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefix_directive_and_prefixed_names() {
        let doc = "\
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix ub: <http://ex/ub#> .
<http://ex/u0> rdf:type ub:University .
<http://ex/u0> ub:name \"University0\" .
";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].predicate, Term::iri(RDF_TYPE));
        assert_eq!(ts[0].object, Term::iri("http://ex/ub#University"));
        assert_eq!(ts[1].object, Term::plain_literal("University0"));
    }

    #[test]
    fn keyword_a_is_rdf_type() {
        let doc = "@prefix : <http://e/> . :x a :T .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts[0].predicate, Term::iri(RDF_TYPE));
        assert_eq!(ts[0].subject, Term::iri("http://e/x"));
        assert_eq!(ts[0].object, Term::iri("http://e/T"));
    }

    #[test]
    fn predicate_object_and_object_lists() {
        let doc = "@prefix : <http://e/> . :s :p :a , :b ; :q :c .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].object, Term::iri("http://e/a"));
        assert_eq!(ts[1].object, Term::iri("http://e/b"));
        assert_eq!(ts[2].predicate, Term::iri("http://e/q"));
    }

    #[test]
    fn ntriples_is_valid_turtle() {
        // tab-separated full IRIs + literal — the gStore small.nt shape.
        let doc = "<root>\t<name>\t\"Bookug Lobert\"\t.\n<root>\t<contain>\t<node0>\t.";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].subject, Term::iri("root"));
        assert_eq!(ts[0].object, Term::plain_literal("Bookug Lobert"));
    }

    #[test]
    fn typed_and_numeric_literals() {
        let doc = "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> . <a> <salary> \"2500\"^^xsd:integer . <a> <age> 42 . <a> <h> 1.5 .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts[0].object, Term::typed_literal("2500", xsd::INTEGER));
        assert_eq!(ts[1].object, Term::typed_literal("42", xsd::INTEGER));
        assert_eq!(ts[2].object, Term::typed_literal("1.5", xsd::DECIMAL));
    }

    #[test]
    fn base_resolves_relative_iris() {
        let doc = "@base <http://ex/> . <a> <p> <b> .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts[0].subject, Term::iri("http://ex/a"));
        assert_eq!(ts[0].object, Term::iri("http://ex/b"));
    }

    #[test]
    fn sparql_style_prefix_without_at() {
        let doc = "PREFIX : <http://e/>\n:s :p :o .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts[0].subject, Term::iri("http://e/s"));
    }

    #[test]
    fn lang_literal() {
        let doc = "<a> <b> \"chat\"@fr .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(
            ts[0].object,
            Term::Literal {
                value: "chat".into(),
                datatype: None,
                lang: Some("fr".into())
            }
        );
    }

    #[test]
    fn undefined_prefix_errors() {
        let e = parse_str("<a> <b> ub:Thing .").unwrap_err();
        assert!(matches!(e, GStoreError::RdfParse { .. }));
    }

    #[test]
    fn comments_and_blank_lines() {
        let doc = "# header\n\n@prefix : <http://e/> .\n# mid\n:s :p :o .\n";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn blank_node_property_list_as_object() {
        let doc = "@prefix : <http://e/> . :s :p [ :q :r ; :u :v ] .";
        let ts = parse_str(doc).unwrap();
        // (B :q :r), (B :u :v), (:s :p B)
        assert_eq!(ts.len(), 3);
        let outer = ts.iter().find(|t| t.predicate == Term::iri("http://e/p")).unwrap();
        let b = match &outer.object {
            Term::Blank(l) => l.clone(),
            other => panic!("expected blank object, got {other:?}"),
        };
        // the blank node carries both inner properties
        assert_eq!(
            ts.iter()
                .filter(|t| matches!(&t.subject, Term::Blank(l) if l == &b))
                .count(),
            2
        );
    }

    #[test]
    fn blank_node_property_list_as_subject_standalone() {
        let doc = "@prefix : <http://e/> . [ :p :o ; :q :r ] .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 2);
        // both share one blank subject
        match (&ts[0].subject, &ts[1].subject) {
            (Term::Blank(a), Term::Blank(b)) => assert_eq!(a, b),
            other => panic!("expected blank subjects, got {other:?}"),
        }
    }

    #[test]
    fn empty_blank_node_is_anonymous() {
        let doc = "@prefix : <http://e/> . :s :p [] .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 1);
        assert!(matches!(ts[0].object, Term::Blank(_)));
    }

    #[test]
    fn collection_lowers_to_rdf_list() {
        let doc = "@prefix : <http://e/> . :s :p ( :a :b :c ) .";
        let ts = parse_str(doc).unwrap();
        // 3 first + 3 rest + 1 outer = 7 triples
        assert_eq!(ts.len(), 7);
        let first = Term::iri(format!("{RDF_NS}first"));
        let rest = Term::iri(format!("{RDF_NS}rest"));
        let nil = Term::iri(format!("{RDF_NS}nil"));
        assert_eq!(ts.iter().filter(|t| t.predicate == first).count(), 3);
        assert_eq!(ts.iter().filter(|t| t.predicate == rest).count(), 3);
        assert!(ts.iter().any(|t| t.object == nil));
        // the items appear in order as rdf:first objects
        let firsts: Vec<&Term> = ts
            .iter()
            .filter(|t| t.predicate == first)
            .map(|t| &t.object)
            .collect();
        assert!(firsts.contains(&&Term::iri("http://e/a")));
        assert!(firsts.contains(&&Term::iri("http://e/c")));
    }

    #[test]
    fn empty_collection_is_nil() {
        let doc = "@prefix : <http://e/> . :s :p () .";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].object, Term::iri(format!("{RDF_NS}nil")));
    }

    #[test]
    fn nested_blank_and_collection() {
        let doc = "@prefix : <http://e/> . :s :p [ :q ( :a :b ) ] .";
        let ts = parse_str(doc).unwrap();
        // inner: 2 first + 2 rest = 4 ; B :q head ; :s :p B  => 6
        assert_eq!(ts.len(), 6);
    }
}
