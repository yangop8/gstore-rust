//! N-Quads parser.
//!
//! N-Quads (<https://www.w3.org/TR/n-quads/>) extends N-Triples with an
//! optional fourth term — the *graph label* — written before the statement
//! terminator `.`:
//!
//! ```text
//! <s> <p> <o> .            # the default graph
//! <s> <p> <o> <g> .        # a named graph <g>
//! <s> <p> "lit" _:g .      # a named graph identified by a blank node
//! ```
//!
//! The graph label is an IRI or a blank node (never a literal). Each statement
//! is parsed into a [`Quad`] (a [`Triple`] plus an optional graph [`Term`]); the
//! default-graph case yields `graph == None`.
//!
//! Like the bundled N-Triples reader this is a proper character scanner (literal
//! values may contain spaces), it is lenient about a missing trailing `.`, and
//! it skips blank lines and `#` comments.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::{Term, Triple};

/// One N-Quads statement: a triple plus the graph it belongs to (`None` ⇒ the
/// default graph). Reused by the [`crate::parser::trig`] reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quad {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
    /// The graph label (an IRI or blank node), or `None` for the default graph.
    pub graph: Option<Term>,
}

impl Quad {
    /// Build a quad in the default graph.
    pub fn new(subject: Term, predicate: Term, object: Term, graph: Option<Term>) -> Quad {
        Quad {
            subject,
            predicate,
            object,
            graph,
        }
    }

    /// The triple part, discarding the graph label.
    pub fn to_triple(&self) -> Triple {
        Triple::new(
            self.subject.clone(),
            self.predicate.clone(),
            self.object.clone(),
        )
    }
}

/// Parse the entire content of an N-Quads document into quads.
pub fn parse_str(content: &str) -> Result<Vec<Quad>> {
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if let Some(q) = parse_line(line, i + 1)? {
            out.push(q);
        }
    }
    Ok(out)
}

/// Read an entire `.nq` file into a vector of quads.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Quad>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if let Some(q) = parse_line(&line, i + 1)? {
            out.push(q);
        }
    }
    Ok(out)
}

/// Convenience: parse a document and drop the graph labels, yielding triples.
pub fn parse_str_triples(content: &str) -> Result<Vec<Triple>> {
    Ok(parse_str(content)?.iter().map(Quad::to_triple).collect())
}

/// Parse one line. Returns `Ok(None)` for blank lines and comments.
pub fn parse_line(line: &str, line_no: usize) -> Result<Option<Quad>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let mut scanner = Scanner::new(trimmed, line_no);
    let subject = scanner.parse_term()?;
    let predicate = scanner.parse_term()?;
    let object = scanner.parse_term()?;
    scanner.skip_ws();
    // Optional fourth term: the graph label (IRI or blank node).
    let graph = match scanner.peek() {
        Some('.') | None => None,
        Some('#') => None,
        Some('<') | Some('_') => {
            let g = scanner.parse_term()?;
            match g {
                Term::Iri(_) | Term::Blank(_) => Some(g),
                Term::Literal { .. } => {
                    return Err(scanner.err("graph label cannot be a literal"))
                }
            }
        }
        Some(c) => return Err(scanner.err(format!("unexpected '{c}' before statement end"))),
    };
    scanner.skip_ws();
    if scanner.peek() == Some('.') {
        scanner.bump();
    }
    scanner.skip_ws();
    if let Some(c) = scanner.peek() {
        if c != '#' {
            return Err(scanner.err(format!("unexpected trailing input near '{c}'")));
        }
    }
    Ok(Some(Quad::new(subject, predicate, object, graph)))
}

/// A cursor over a single line's characters (mirrors the N-Triples scanner).
struct Scanner {
    chars: Vec<char>,
    pos: usize,
    line_no: usize,
}

impl Scanner {
    fn new(s: &str, line_no: usize) -> Scanner {
        Scanner {
            chars: s.chars().collect(),
            pos: 0,
            line_no,
        }
    }

    fn err(&self, msg: impl Into<String>) -> GStoreError {
        GStoreError::RdfParse {
            line: self.line_no,
            msg: msg.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.pos += 1;
        }
    }

    fn parse_term(&mut self) -> Result<Term> {
        self.skip_ws();
        match self.peek() {
            Some('<') => self.parse_iri(),
            Some('"') => self.parse_literal(),
            Some('_') => self.parse_blank(),
            Some(c) => Err(self.err(format!("expected a term, found '{c}'"))),
            None => Err(self.err("expected a term, found end of line")),
        }
    }

    fn parse_iri(&mut self) -> Result<Term> {
        debug_assert_eq!(self.peek(), Some('<'));
        self.bump();
        let mut s = String::new();
        loop {
            match self.bump() {
                Some('>') => return Ok(Term::Iri(s)),
                Some('\\') => self.read_unicode_escape(&mut s)?,
                Some(c) => s.push(c),
                None => return Err(self.err("unterminated IRI (missing '>')")),
            }
        }
    }

    fn parse_blank(&mut self) -> Result<Term> {
        debug_assert_eq!(self.peek(), Some('_'));
        self.bump();
        if self.bump() != Some(':') {
            return Err(self.err("blank node must start with '_:'"));
        }
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                s.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if s.is_empty() {
            return Err(self.err("empty blank node label"));
        }
        Ok(Term::Blank(s))
    }

    fn parse_literal(&mut self) -> Result<Term> {
        debug_assert_eq!(self.peek(), Some('"'));
        self.bump();
        let mut value = String::new();
        loop {
            match self.bump() {
                Some('"') => break,
                Some('\\') => self.read_string_escape(&mut value)?,
                Some(c) => value.push(c),
                None => return Err(self.err("unterminated literal (missing closing '\"')")),
            }
        }
        match self.peek() {
            Some('^') => {
                self.bump();
                if self.bump() != Some('^') {
                    return Err(self.err("datatype marker must be '^^'"));
                }
                self.skip_ws();
                if self.peek() != Some('<') {
                    return Err(self.err("datatype must be an <IRI>"));
                }
                let dt = match self.parse_iri()? {
                    Term::Iri(iri) => iri,
                    _ => unreachable!(),
                };
                Ok(Term::Literal {
                    value,
                    datatype: Some(dt),
                    lang: None,
                })
            }
            Some('@') => {
                self.bump();
                let mut lang = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        lang.push(c);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if lang.is_empty() {
                    return Err(self.err("empty language tag after '@'"));
                }
                Ok(Term::Literal {
                    value,
                    datatype: None,
                    lang: Some(lang),
                })
            }
            _ => Ok(Term::Literal {
                value,
                datatype: None,
                lang: None,
            }),
        }
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
            Some('u') => self.push_hex(out, 4)?,
            Some('U') => self.push_hex(out, 8)?,
            Some(c) => return Err(self.err(format!("invalid string escape '\\{c}'"))),
            None => return Err(self.err("dangling backslash at end of literal")),
        }
        Ok(())
    }

    fn read_unicode_escape(&mut self, out: &mut String) -> Result<()> {
        match self.bump() {
            Some('u') => self.push_hex(out, 4),
            Some('U') => self.push_hex(out, 8),
            Some(c) => Err(self.err(format!("invalid IRI escape '\\{c}'"))),
            None => Err(self.err("dangling backslash in IRI")),
        }
    }

    fn push_hex(&mut self, out: &mut String, n: usize) -> Result<()> {
        let mut code: u32 = 0;
        for _ in 0..n {
            let c = self
                .bump()
                .ok_or_else(|| self.err("truncated unicode escape"))?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| self.err(format!("invalid hex digit '{c}' in escape")))?;
            code = code * 16 + d;
        }
        let ch = char::from_u32(code)
            .ok_or_else(|| self.err(format!("invalid unicode code point U+{code:04X}")))?;
        out.push(ch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_without_graph_is_default() {
        let q = parse_line("<a> <b> <c> .", 1).unwrap().unwrap();
        assert_eq!(q.subject, Term::iri("a"));
        assert_eq!(q.object, Term::iri("c"));
        assert!(q.graph.is_none());
    }

    #[test]
    fn quad_with_iri_graph() {
        let q = parse_line("<a> <b> <c> <g> .", 1).unwrap().unwrap();
        assert_eq!(q.graph, Some(Term::iri("g")));
    }

    #[test]
    fn quad_with_blank_graph() {
        let q = parse_line("<a> <b> <c> _:g1 .", 1).unwrap().unwrap();
        assert_eq!(q.graph, Some(Term::blank("g1")));
    }

    #[test]
    fn literal_object_with_graph() {
        let q = parse_line(r#"<a> <b> "hi there"@en <g> ."#, 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            q.object,
            Term::Literal {
                value: "hi there".into(),
                datatype: None,
                lang: Some("en".into())
            }
        );
        assert_eq!(q.graph, Some(Term::iri("g")));
    }

    #[test]
    fn typed_literal_object() {
        let line = "<a> <p> \"2500\"^^<http://www.w3.org/2001/XMLSchema#integer> <g> .";
        let q = parse_line(line, 1).unwrap().unwrap();
        assert_eq!(
            q.object,
            Term::typed_literal("2500", "http://www.w3.org/2001/XMLSchema#integer")
        );
    }

    #[test]
    fn literal_graph_is_rejected() {
        assert!(parse_line(r#"<a> <b> <c> "g" ."#, 1).is_err());
    }

    #[test]
    fn comments_and_blanks_skipped() {
        assert!(parse_line("   ", 1).unwrap().is_none());
        assert!(parse_line("# c", 1).unwrap().is_none());
    }

    #[test]
    fn parse_str_collects_quads() {
        let doc = "<a> <p> <b> .\n# comment\n<a> <p> \"v\" <g> .\n";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs[0].graph.is_none());
        assert_eq!(qs[1].graph, Some(Term::iri("g")));
    }

    #[test]
    fn round_trip_through_triples() {
        let doc = "<s1> <p1> <o1> <g1> .\n<s2> <p2> \"lit\" .\n";
        let ts = parse_str_triples(doc).unwrap();
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].subject, Term::iri("s1"));
        assert_eq!(ts[1].object, Term::plain_literal("lit"));
    }
}
