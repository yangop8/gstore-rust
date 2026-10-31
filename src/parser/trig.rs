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
//! The reader is the streaming [`turtle`](crate::parser::turtle) reader in its
//! TriG mode ([`turtle::parse_reader_trig`]): directives apply from where
//! they appear, statements come out in document order — inside a block one
//! statement at a time, so a huge graph block is never buffered — and every
//! statement becomes a [`Quad`] tagged with its graph (`None` for the default
//! graph). As in the TriG grammar a block's last statement may omit its `.`;
//! leniently, a `.` may also follow a block's `}`. Blank-node labels are
//! scoped to the document (shared by all its blocks); anonymous `[]` / `()`
//! nodes are distinct across blocks.

use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::error::Result;
use crate::parser::nquads::Quad;
use crate::parser::stream::{BlankLabeler, CollectQuads, StatementSink, DEFAULT_WINDOW};
use crate::parser::turtle;

/// Parse a TriG document into quads.
pub fn parse_str(content: &str) -> Result<Vec<Quad>> {
    let mut sink = CollectQuads::default();
    parse_reader(content.as_bytes(), BlankLabeler::keep(), &mut sink)?;
    Ok(sink.quads)
}

/// Parse a TriG file into quads (streamed from disk).
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Quad>> {
    let file = std::fs::File::open(path)?;
    let mut sink = CollectQuads::default();
    parse_reader(
        BufReader::with_capacity(DEFAULT_WINDOW, file),
        BlankLabeler::keep(),
        &mut sink,
    )?;
    Ok(sink.quads)
}

/// Convenience: parse a document and drop the graph labels, yielding triples.
pub fn parse_str_triples(content: &str) -> Result<Vec<crate::model::Triple>> {
    Ok(parse_str(content)?.iter().map(Quad::to_triple).collect())
}

/// Stream a TriG document from `reader` into `sink`, one statement at a time.
pub fn parse_reader<R: BufRead>(
    reader: R,
    labels: BlankLabeler,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    turtle::parse_reader_trig(reader, labels, sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Term;

    fn graphs_of(qs: &[Quad]) -> Vec<Option<Term>> {
        qs.iter().map(|q| q.graph.clone()).collect()
    }

    #[test]
    fn non_ascii_graph_label_does_not_panic() {
        // The graph-label region `:café` is >= 5 bytes but byte 5 falls inside
        // the `é`; `strip_graph_keyword` must not slice on a non-char boundary.
        let doc = "@prefix : <http://e/> .\n:café { :s :p :o }";
        let qs = parse_str(doc).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].graph, Some(Term::iri("http://e/café")));
    }

    #[test]
    fn anonymous_blank_nodes_do_not_collide_across_named_graphs() {
        // Each block is parsed separately; without a per-block blank-node scope
        // both `[ … ]` would restart at `g~0` and merge into one node.
        let doc = "@prefix : <http://e/> .\n\
                   :g1 { :s :p [ :a :b ] }\n\
                   :g2 { :x :y [ :c :d ] }";
        let qs = parse_str(doc).unwrap();
        let blank_of = |pred: &str| {
            qs.iter()
                .find(|q| q.predicate == Term::iri(format!("http://e/{pred}")))
                .map(|q| q.object.clone())
                .unwrap()
        };
        let b1 = blank_of("p"); // the [ … ] in :g1
        let b2 = blank_of("y"); // the [ … ] in :g2
        assert!(matches!(b1, Term::Blank(_)) && matches!(b2, Term::Blank(_)));
        assert_ne!(b1, b2, "anonymous nodes in different graphs must stay distinct");
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
