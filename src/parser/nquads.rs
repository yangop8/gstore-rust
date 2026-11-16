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
use crate::parser::ntriples::Scanner;
use crate::parser::stream::{
    BlankLabeler, CollectQuads, StatementSink, SyntaxError, DEFAULT_WINDOW,
};

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
    /// The bytes the strings of this quad's terms hold.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.subject.heap_bytes()
            + self.predicate.heap_bytes()
            + self.object.heap_bytes()
            + self.graph.as_ref().map_or(0, Term::heap_bytes)
    }

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

    /// The triple part, consuming the quad.
    pub fn into_triple(self) -> Triple {
        Triple::new(self.subject, self.predicate, self.object)
    }
}

/// Parse the entire content of an N-Quads document into quads.
pub fn parse_str(content: &str) -> Result<Vec<Quad>> {
    let mut sink = CollectQuads::default();
    parse_reader(content.as_bytes(), &BlankLabeler::keep(), &mut sink)?;
    Ok(sink.quads)
}

/// Read an entire `.nq` file into a vector of quads (streamed line by line).
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Quad>> {
    let file = File::open(path)?;
    let mut sink = CollectQuads::default();
    parse_reader(
        BufReader::with_capacity(DEFAULT_WINDOW, file),
        &BlankLabeler::keep(),
        &mut sink,
    )?;
    Ok(sink.quads)
}

/// Convenience: parse a document and drop the graph labels, yielding triples.
pub fn parse_str_triples(content: &str) -> Result<Vec<Triple>> {
    Ok(parse_str(content)?.iter().map(Quad::to_triple).collect())
}

/// Stream an N-Quads document from `reader` into `sink`, one line (one
/// statement) at a time: memory holds the reader's buffer plus the current
/// line. Blank-node labels go through `labels` (verbatim in keep mode).
pub fn parse_reader<R: BufRead>(
    reader: R,
    labels: &BlankLabeler,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    stream_lines(reader, labels, true, sink)
}

/// The line-at-a-time driver behind the N-Triples (`allow_graph == false`)
/// and N-Quads readers. A line that does not parse (or is not UTF-8) is
/// reported to `sink` as one failed statement; I/O errors end the parse.
pub(crate) fn stream_lines<R: BufRead>(
    mut reader: R,
    labels: &BlankLabeler,
    allow_graph: bool,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<Quad> = Vec::with_capacity(1);
    let mut line_no = 0usize;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        line_no += 1;
        let parsed = match std::str::from_utf8(&buf) {
            Ok(line) if allow_graph => parse_line(line, line_no),
            Ok(line) => crate::parser::ntriples::parse_line(line, line_no)
                .map(|t| t.map(|t| Quad::new(t.subject, t.predicate, t.object, None))),
            Err(_) => Err(GStoreError::RdfParse {
                line: line_no,
                msg: "invalid UTF-8".into(),
            }),
        };
        match parsed {
            Ok(Some(mut q)) => {
                relabel_quad(&mut q, labels);
                out.push(q);
                sink.at_line(line_no);
                sink.statement(&mut out)?;
                out.clear();
            }
            Ok(None) => {}
            Err(e) => {
                let text = String::from_utf8_lossy(&buf);
                sink.error(SyntaxError::from_error(&e).with_context(snippet(text.trim())))?;
            }
        }
        // One huge line must not pin its buffer for the rest of the file.
        if buf.capacity() > 4 * DEFAULT_WINDOW {
            buf = Vec::new();
        }
    }
}

/// At most 200 characters of `s` (an error log context).
pub(crate) fn snippet(s: &str) -> String {
    match s.char_indices().nth(200) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

/// Rename the blank nodes of a quad read by a non-minting reader.
fn relabel_quad(q: &mut Quad, labels: &BlankLabeler) {
    if labels.is_keep() {
        return;
    }
    for t in [&mut q.subject, &mut q.predicate, &mut q.object] {
        relabel(t, labels);
    }
    if let Some(g) = q.graph.as_mut() {
        relabel(g, labels);
    }
}

fn relabel(t: &mut Term, labels: &BlankLabeler) {
    if let Term::Blank(l) = t {
        *l = labels.explicit_verbatim(l);
    }
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
                Term::Literal { .. } => return Err(scanner.err("graph label cannot be a literal")),
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
    fn a_trailing_dot_does_not_split_a_graph() {
        // review parser#6: `_:g.` and `_:g .` are one graph, as in TriG.
        let qs = parse_str("<s> <p> <o> _:g.\n<s> <p> <o2> _:g .\n<s> <p> _:b.\n").unwrap();
        assert_eq!(qs[0].graph, Some(Term::blank("g")));
        assert_eq!(qs[1].graph, Some(Term::blank("g")));
        assert_eq!(qs[2].object, Term::blank("b"));
        assert_eq!(qs[2].graph, None);
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
