//! N-Triples parser.
//!
//! N-Triples is gStore's default bulk-import format (see `data/*.nt`). Each
//! statement is `subject predicate object .` with fields separated by spaces or
//! tabs. Terms are:
//!
//! * IRIs       — `<http://…>`
//! * literals   — `"text"`, optionally `"text"^^<datatype-iri>` or `"text"@lang`
//! * blank nodes— `_:label`
//!
//! Because literal values may themselves contain spaces (e.g.
//! `"Bookug Lobert"`), we cannot split on whitespace; this is a proper
//! character scanner. It is lenient about a missing trailing `.` (gStore's
//! `Triple(string)` is too) and skips blank lines and `#` comments.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::{Term, Triple};

/// Parse the entire content of an N-Triples document into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    let mut out = Vec::new();
    for_each_triple_str(content, |t| {
        out.push(t);
        Ok(())
    })?;
    Ok(out)
}

/// Stream triples from a file on disk, invoking `f` per triple. Reads line by
/// line so multi-gigabyte `.nt` files don't need to be slurped into memory.
pub fn for_each_triple_file<P, F>(path: P, mut f: F) -> Result<()>
where
    P: AsRef<Path>,
    F: FnMut(Triple) -> Result<()>,
{
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if let Some(t) = parse_line(&line, i + 1)? {
            f(t)?;
        }
    }
    Ok(())
}

/// Convenience: read an entire `.nt` file into a vector of triples.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let mut out = Vec::new();
    for_each_triple_file(path, |t| {
        out.push(t);
        Ok(())
    })?;
    Ok(out)
}

/// Stream triples from an in-memory document, invoking `f` per triple. Avoids
/// materializing intermediate vectors for large inputs.
pub fn for_each_triple_str<F>(content: &str, mut f: F) -> Result<()>
where
    F: FnMut(Triple) -> Result<()>,
{
    for (i, line) in content.lines().enumerate() {
        if let Some(t) = parse_line(line, i + 1)? {
            f(t)?;
        }
    }
    Ok(())
}

/// Parse a single RDF term from its N-Triples surface form (the inverse of
/// [`Term`]'s `Display`). Used to reconstruct a [`Term`] from a dictionary key.
pub fn parse_term(s: &str) -> Result<Term> {
    let mut scanner = Scanner::new(s.trim(), 0);
    let term = scanner.parse_term()?;
    scanner.skip_ws();
    if scanner.peek().is_some() {
        return Err(scanner.err("trailing input after term"));
    }
    Ok(term)
}

/// Parse one line. Returns `Ok(None)` for blank lines and comments.
pub fn parse_line(line: &str, line_no: usize) -> Result<Option<Triple>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let mut scanner = Scanner::new(trimmed, line_no);
    let subject = scanner.parse_term()?;
    let predicate = scanner.parse_term()?;
    let object = scanner.parse_term()?;
    scanner.skip_ws();
    // Optional statement terminator '.'
    if scanner.peek() == Some('.') {
        scanner.bump();
    }
    scanner.skip_ws();
    // A '#' here begins a trailing comment; otherwise nothing should remain.
    if let Some(c) = scanner.peek() {
        if c != '#' {
            return Err(scanner.err(format!("unexpected trailing input near '{c}'")));
        }
    }
    Ok(Some(Triple::new(subject, predicate, object)))
}

/// A cursor over a single line's characters.
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

    /// Parse the next RDF term, skipping leading whitespace.
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

    /// `<iri>` — read until the closing `>`, decoding `\u`/`\U` escapes.
    fn parse_iri(&mut self) -> Result<Term> {
        debug_assert_eq!(self.peek(), Some('<'));
        self.bump(); // consume '<'
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

    /// `_:label` — a blank node.
    fn parse_blank(&mut self) -> Result<Term> {
        debug_assert_eq!(self.peek(), Some('_'));
        self.bump(); // consume '_'
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

    /// `"value"` with optional `^^<datatype>` or `@lang`.
    fn parse_literal(&mut self) -> Result<Term> {
        debug_assert_eq!(self.peek(), Some('"'));
        self.bump(); // consume opening '"'
        let mut value = String::new();
        loop {
            match self.bump() {
                Some('"') => break,
                Some('\\') => self.read_string_escape(&mut value)?,
                Some(c) => value.push(c),
                None => return Err(self.err("unterminated literal (missing closing '\"')")),
            }
        }
        // Optional suffix.
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

    /// Decode an escape inside a string literal (the `\` is already consumed).
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

    /// Decode `\uXXXX` / `\UXXXXXXXX` inside an IRI (the `\` already consumed).
    fn read_unicode_escape(&mut self, out: &mut String) -> Result<()> {
        match self.bump() {
            Some('u') => self.push_hex(out, 4),
            Some('U') => self.push_hex(out, 8),
            Some(c) => Err(self.err(format!("invalid IRI escape '\\{c}'"))),
            None => Err(self.err("dangling backslash in IRI")),
        }
    }

    /// Read `n` hex digits and push the resulting code point.
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

