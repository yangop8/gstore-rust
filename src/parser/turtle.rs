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
//!
//! Not supported (rare in gStore corpora; see REFACTOR_BACKLOG item D):
//! `[ … ]` blank-node property lists and `( … )` collections.

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
}

impl Parser {
    fn new(s: &str) -> Parser {
        Parser {
            chars: s.chars().collect(),
            pos: 0,
            line: 1,
            prefixes: std::collections::HashMap::new(),
            base: None,
            triples: Vec::new(),
        }
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
        let subject = self.parse_term(false)?;
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
        if self.peek() == Some('a') && self.peek2().map_or(true, |c| c.is_whitespace() || c == '<')
        {
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
            Some('_') => self.parse_blank(),
            Some(c) if (c.is_ascii_digit() || c == '+' || c == '-' || c == '.') && !verb_pos => {
                self.parse_numeric()
            }
            Some(_) => self.parse_prefixed_or_keyword(verb_pos),
            None => Err(self.err("expected a term, found end of input")),
        }
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
                if c == '.' && self.peek2().map_or(true, |n| n.is_whitespace()) {
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
            Some(base) if !is_absolute_iri(s) => format!("{base}{s}"),
            _ => s.to_string(),
        }
    }
}

fn is_absolute_iri(s: &str) -> bool {
    if let Some(idx) = s.find(':') {
        let scheme = &s[..idx];
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    } else {
        false
    }
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
}
