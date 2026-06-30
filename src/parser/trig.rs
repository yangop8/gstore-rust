//! TriG parser.
//!
//! TriG (<https://www.w3.org/TR/trig/>) is Turtle extended with *graph blocks*:
//! the same prefixes, abbreviations, lists and blank-node syntax as Turtle, plus
//! `{ … }` groups that assign their triples to a named graph.
//!
//! ```text
//! @prefix : <http://ex/> .
//! :s :p :o .                 # the default graph
//! { :s2 :p :o2 }             # also the default graph (wrapped)
//! :g1 { :a :b :c }           # the named graph :g1
//! GRAPH :g2 { :d :e :f }     # the named graph :g2 (GRAPH keyword)
//! ```
//!
//! Rather than re-implement the whole of Turtle, this reader is a *structural*
//! splitter: it carves the document into its prefix/base directives, its
//! default-graph statements, and its graph blocks (respecting string and IRI
//! literals so braces inside them don't confuse the split), then hands each
//! region — with the collected directives prepended — to the bundled
//! [`turtle`](crate::parser::turtle) parser. Every statement becomes a
//! [`Quad`] tagged with its graph (`None` for the default graph).

use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::Term;
use crate::parser::nquads::Quad;
use crate::parser::turtle;

/// Parse a TriG document into quads.
pub fn parse_str(content: &str) -> Result<Vec<Quad>> {
    let mut splitter = Splitter::new(content);
    let parts = splitter.split()?;
    let mut out = Vec::new();

    // Default graph: all unwrapped statements + all `{ … }` (label-less) blocks,
    // parsed together with the collected directives.
    let default_doc = format!("{}\n{}", parts.preamble, parts.default_text);
    for t in turtle::parse_str(&default_doc)? {
        out.push(Quad::new(t.subject, t.predicate, t.object, None));
    }

    // Named graphs: each block parsed against the same directive preamble.
    for (label_text, inner) in parts.named {
        let label = resolve_label(&parts.preamble, &label_text)?;
        let doc = format!("{}\n{}", parts.preamble, inner);
        for t in turtle::parse_str(&doc)? {
            out.push(Quad::new(
                t.subject,
                t.predicate,
                t.object,
                Some(label.clone()),
            ));
        }
    }
    Ok(out)
}

/// Parse a TriG file into quads.
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Quad>> {
    let content = std::fs::read_to_string(path)?;
    parse_str(&content)
}

/// Convenience: parse a document and drop the graph labels, yielding triples.
pub fn parse_str_triples(content: &str) -> Result<Vec<crate::model::Triple>> {
    Ok(parse_str(content)?.iter().map(Quad::to_triple).collect())
}

/// The structural pieces of a TriG document.
struct Parts {
    /// All `@prefix`/`@base`/`PREFIX`/`BASE` directives, verbatim.
    preamble: String,
    /// Concatenated default-graph statements and label-less `{ … }` block bodies.
    default_text: String,
    /// `(label_text, block_body)` for each named graph block.
    named: Vec<(String, String)>,
}

/// Resolve a graph label (an IRI, prefixed name, or blank node) to a [`Term`] by
/// reusing Turtle's own prefix/base resolution: parse a throwaway statement whose
/// subject is the label and read back the subject term.
fn resolve_label(preamble: &str, label: &str) -> Result<Term> {
    let probe = format!("{preamble}\n{label} <urn:gstore:trig-label> <urn:gstore:trig-label> .");
    let ts = turtle::parse_str(&probe).map_err(|_| GStoreError::RdfParse {
        line: 0,
        msg: format!("invalid graph label '{label}'"),
    })?;
    ts.into_iter()
        .next()
        .map(|t| t.subject)
        .ok_or_else(|| GStoreError::RdfParse {
            line: 0,
            msg: format!("invalid graph label '{label}'"),
        })
}

/// What the top-level scan stops on.
enum Stop {
    /// A graph block opens at this index (`{`).
    Brace(usize),
    /// A statement terminator (`.`) at this index.
    Dot(usize),
    /// End of input.
    Eof,
}

struct Splitter {
    chars: Vec<char>,
    pos: usize,
}

impl Splitter {
    fn new(s: &str) -> Splitter {
        Splitter {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    fn slice(&self, a: usize, b: usize) -> String {
        self.chars[a..b].iter().collect()
    }

    fn err(&self, msg: impl Into<String>) -> GStoreError {
        let line = self.chars[..self.pos.min(self.len())]
            .iter()
            .filter(|&&c| c == '\n')
            .count()
            + 1;
        GStoreError::RdfParse {
            line,
            msg: msg.into(),
        }
    }

    fn skip_ws(&mut self) {
        loop {
            match self.at(self.pos) {
                Some(c) if c.is_whitespace() => self.pos += 1,
                Some('#') => {
                    while let Some(c) = self.at(self.pos) {
                        self.pos += 1;
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    /// If a string/IRI/comment construct starts at `i`, return the index just
    /// past it; otherwise `None`. Lets the top-level scan ignore braces and dots
    /// that live inside literals.
    fn skip_construct(&self, i: usize) -> Option<usize> {
        match self.at(i)? {
            '#' => {
                let mut j = i + 1;
                while let Some(c) = self.at(j) {
                    if c == '\n' {
                        break;
                    }
                    j += 1;
                }
                Some(j)
            }
            '<' => {
                // An IRI reference `<...>` (not the `<<` of RDF-star, which we
                // don't support — treat a lone `<` as an IRI start regardless).
                let mut j = i + 1;
                while let Some(c) = self.at(j) {
                    j += 1;
                    match c {
                        '>' => return Some(j),
                        // Skip an escaped char (e.g. `\u….`) inside the IRI.
                        '\\' if self.at(j).is_some() => j += 1,
                        _ => {}
                    }
                }
                Some(j)
            }
            q @ ('"' | '\'') => {
                let triple = self.at(i + 1) == Some(q) && self.at(i + 2) == Some(q);
                if triple {
                    let mut j = i + 3;
                    while j < self.len() {
                        match self.at(j) {
                            Some('\\') => j += 2,
                            Some(c) if c == q
                                && self.at(j + 1) == Some(q)
                                && self.at(j + 2) == Some(q) =>
                            {
                                return Some(j + 3);
                            }
                            _ => j += 1,
                        }
                    }
                    Some(j)
                } else {
                    let mut j = i + 1;
                    while j < self.len() {
                        match self.at(j) {
                            Some('\\') => j += 2,
                            Some(c) if c == q => return Some(j + 1),
                            _ => j += 1,
                        }
                    }
                    Some(j)
                }
            }
            _ => None,
        }
    }

    /// Is the `.` at `i` a statement terminator (vs. part of a number/pname)?
    fn is_terminator_dot(&self, i: usize) -> bool {
        match self.at(i + 1) {
            None => true,
            Some(c) => c.is_whitespace(),
        }
    }

    /// Scan from `self.pos` to the next top-level `{` or terminator `.`.
    fn scan(&self) -> Stop {
        let mut i = self.pos;
        while i < self.len() {
            if let Some(j) = self.skip_construct(i) {
                i = j;
                continue;
            }
            match self.at(i) {
                Some('{') => return Stop::Brace(i),
                Some('.') if self.is_terminator_dot(i) => return Stop::Dot(i),
                _ => i += 1,
            }
        }
        Stop::Eof
    }

    /// Read a balanced `{ … }` block starting at `open` (a `{`). Returns the
    /// inner text and advances `self.pos` past the closing `}`.
    fn read_block(&mut self, open: usize) -> Result<String> {
        debug_assert_eq!(self.at(open), Some('{'));
        let mut i = open;
        let mut depth = 0usize;
        let mut inner_start = open + 1;
        while i < self.len() {
            if let Some(j) = self.skip_construct(i) {
                i = j;
                continue;
            }
            match self.at(i) {
                Some('{') => {
                    if depth == 0 {
                        inner_start = i + 1;
                    }
                    depth += 1;
                    i += 1;
                }
                Some('}') => {
                    depth -= 1;
                    if depth == 0 {
                        let inner = self.slice(inner_start, i);
                        self.pos = i + 1;
                        return Ok(inner);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        Err(self.err("unterminated graph block (missing '}')"))
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

    /// Read a `@prefix`/`@base` directive verbatim, up to and including its `.`.
    fn read_at_directive(&mut self) -> Result<String> {
        let start = self.pos;
        match self.scan() {
            Stop::Dot(d) => {
                let raw = self.slice(start, d + 1);
                self.pos = d + 1;
                Ok(raw)
            }
            _ => Err(self.err("unterminated directive (missing '.')")),
        }
    }

    /// Read a SPARQL-style `PREFIX`/`BASE` directive verbatim, up to and
    /// including the closing `>` of its IRI reference.
    fn read_keyword_directive(&mut self) -> Result<String> {
        let start = self.pos;
        let mut i = self.pos;
        // Find the IRI reference that ends the directive.
        while i < self.len() {
            if self.at(i) == Some('<') {
                let end = self.skip_construct(i).expect("'<' starts a construct");
                let raw = self.slice(start, end);
                self.pos = end;
                return Ok(raw);
            }
            // Comments / stray strings shouldn't appear here, but stay safe.
            if let Some(j) = self.skip_construct(i) {
                i = j;
                continue;
            }
            i += 1;
        }
        Err(self.err("unterminated PREFIX/BASE directive (missing IRI)"))
    }

    fn split(&mut self) -> Result<Parts> {
        let mut preamble = String::new();
        let mut default_text = String::new();
        let mut named: Vec<(String, String)> = Vec::new();

        loop {
            self.skip_ws();
            if self.pos >= self.len() {
                break;
            }
            match self.at(self.pos) {
                Some('@') => {
                    let d = self.read_at_directive()?;
                    preamble.push_str(&d);
                    preamble.push('\n');
                }
                Some(c) if c.is_ascii_alphabetic() && self.looks_like_keyword_directive() => {
                    let d = self.read_keyword_directive()?;
                    preamble.push_str(&d);
                    preamble.push('\n');
                }
                _ => {
                    let start = self.pos;
                    match self.scan() {
                        Stop::Brace(open) => {
                            let before = self.slice(start, open);
                            let label = strip_graph_keyword(before.trim());
                            // A block's final triple may omit its trailing `.`
                            // (legal TriG); the Turtle parser requires one, so
                            // normalize before delegating.
                            let inner = ensure_terminated(&self.read_block(open)?);
                            if label.is_empty() {
                                if !inner.is_empty() {
                                    default_text.push_str(&inner);
                                    default_text.push('\n');
                                }
                            } else {
                                named.push((label.to_string(), inner));
                            }
                            // An optional `.` may follow a graph block.
                            self.skip_ws();
                            if self.at(self.pos) == Some('.') {
                                self.pos += 1;
                            }
                        }
                        Stop::Dot(d) => {
                            let stmt = self.slice(start, d + 1);
                            default_text.push_str(&stmt);
                            default_text.push('\n');
                            self.pos = d + 1;
                        }
                        Stop::Eof => {
                            // Trailing, unterminated text: let Turtle report it.
                            let rest = self.slice(start, self.len());
                            if !rest.trim().is_empty() {
                                default_text.push_str(&rest);
                                default_text.push('\n');
                            }
                            self.pos = self.len();
                        }
                    }
                }
            }
        }

        Ok(Parts {
            preamble,
            default_text,
            named,
        })
    }
}

/// Ensure a block body ends in a statement terminator `.` (TriG allows the
/// final triple in a block to omit it; the Turtle parser does not). Returns an
/// empty string for whitespace-only input.
fn ensure_terminated(body: &str) -> String {
    let t = body.trim();
    if t.is_empty() {
        String::new()
    } else if t.ends_with('.') {
        t.to_string()
    } else {
        format!("{t} .")
    }
}

/// Strip a leading `GRAPH` keyword (case-insensitive) from a label region,
/// leaving just the label token (possibly empty for the default graph).
fn strip_graph_keyword(before: &str) -> &str {
    if before.len() >= 5 && before[..5].eq_ignore_ascii_case("graph") {
        let rest = &before[5..];
        if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
            return rest.trim();
        }
    }
    before
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Term;

    fn graphs_of(qs: &[Quad]) -> Vec<Option<Term>> {
        qs.iter().map(|q| q.graph.clone()).collect()
    }

    #[test]
    fn plain_turtle_is_default_graph() {
        let doc = "@prefix : <http://e/> . :s :p :o . :s :p :o2 .";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs.iter().all(|q| q.graph.is_none()));
        assert_eq!(qs[0].subject, Term::iri("http://e/s"));
    }

    #[test]
    fn wrapped_default_graph_block() {
        let doc = "@prefix : <http://e/> . { :s :p :o . :a :b :c }";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs.iter().all(|q| q.graph.is_none()));
    }

    #[test]
    fn named_graph_with_label() {
        let doc = "@prefix : <http://e/> . :g1 { :s :p :o . :s :p :o2 }";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs.iter().all(|q| q.graph == Some(Term::iri("http://e/g1"))));
    }

    #[test]
    fn graph_keyword_form() {
        let doc = "@prefix : <http://e/> . GRAPH <http://g/2> { :s :p :o }";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].graph, Some(Term::iri("http://g/2")));
    }

    #[test]
    fn mixed_default_and_named() {
        let doc = "\
@prefix : <http://e/> .
:s :p :o .
:g1 { :a :b :c }
GRAPH :g2 { :d :e :f . :d :e :f2 }
{ :x :y :z }
";
        let qs = parse_str(doc).unwrap();
        // 1 default stmt + 1 g1 + 2 g2 + 1 default-wrapped = 5
        assert_eq!(qs.len(), 5);
        let gs = graphs_of(&qs);
        assert_eq!(gs.iter().filter(|g| g.is_none()).count(), 2);
        assert_eq!(
            gs.iter()
                .filter(|g| **g == Some(Term::iri("http://e/g2")))
                .count(),
            2
        );
    }

    #[test]
    fn braces_inside_literals_do_not_split() {
        let doc = "@prefix : <http://e/> . :s :p \"a { weird } literal .\" .";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].object, Term::plain_literal("a { weird } literal ."));
        assert!(qs[0].graph.is_none());
    }

    #[test]
    fn sparql_style_prefix() {
        let doc = "PREFIX : <http://e/>\n:g { :s :p :o }";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].graph, Some(Term::iri("http://e/g")));
        assert_eq!(qs[0].subject, Term::iri("http://e/s"));
    }

    #[test]
    fn predicate_object_lists_inside_block() {
        let doc = "@prefix : <http://e/> . :g { :s :p :a , :b ; :q :c }";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 3);
        assert!(qs.iter().all(|q| q.graph == Some(Term::iri("http://e/g"))));
    }
}
