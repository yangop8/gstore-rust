//! Turtle and TriG reader (streaming).
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
//! * with [`parse_reader_trig`], TriG graph blocks (see [`crate::parser::trig`])
//!
//! # Streaming
//!
//! The reader pulls characters from a [`BufRead`] through a
//! [`CharSource`](crate::parser::stream::CharSource): memory holds the
//! reader's buffer, a few characters of lookahead and the statement being
//! parsed — never the document. Each completed statement's triples go to a
//! [`StatementSink`] (inside a TriG block too, so one huge graph block is not
//! buffered either).
//!
//! # Errors and recovery
//!
//! A statement that does not parse is reported to the sink. A strict sink
//! stops the parse there. A tolerant sink lets it go on the way upstream
//! gStore's `TurtleParser` recovers (`RDFParser::parseFile` +
//! `TurtleParser::discardLine`): the statement's triples are dropped, the rest
//! of the line the error was detected on is skipped, and parsing resumes at
//! the next line (still inside the current TriG block, if any).
//!
//! # Extension: line breaks in short strings
//!
//! Like upstream's lexer (`TurtleParser::Lexer::lexString`), a `"…"` /
//! `'…'` string may contain a raw line break (W3C Turtle allows them only in
//! `"""…"""` strings). The exporters never write one (they escape `\n` /
//! `\r`). Recovery stays per line: a statement whose short string ran
//! across a line break and that then fails — a missing closing quote,
//! usually — is reported on the literal's line and read again from its line
//! break, so the next lines are not swallowed (upstream loses them). A short
//! string may run on for at most
//! [`MAX_LITERAL_SPAN`](crate::parser::stream::MAX_LITERAL_SPAN) (256 KiB)
//! after its first line break; one that reaches the end of the input or
//! runs further is an error of its statement too.
//!
//! # Extension: lenient local names
//!
//! Like upstream's lexer, which ends a prefixed name only at whitespace or
//! one of `[](),;:.`, a local name may also contain `/ # ? = & ~ ! $ * + @`
//! (`ex:a/b`, `ex:page#top`) — strict Turtle requires a `\` escape for them.
//! A `#` *inside* a name is therefore part of it (`ex:a#b`); a comment starts
//! at a `#` after whitespace. Standard `:` in local names, `%HH` and `\`
//! escapes are accepted too. This applies to the RDF loader only, not to
//! SPARQL, which has its own lexer.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::error::{GStoreError, Result};
use crate::model::{Term, Triple};
use crate::parser::nquads::Quad;
use crate::parser::sparql::ast::{xsd, RDF_TYPE};
use crate::parser::stream::{
    expansion_budget, expansion_error, BlankLabeler, CharSource, CollectQuads, StatementSink,
    SyntaxError, DEFAULT_WINDOW, MAX_LOOKAHEAD,
};

/// Parse a Turtle document into triples.
pub fn parse_str(content: &str) -> Result<Vec<Triple>> {
    collect(content.as_bytes(), BlankLabeler::keep(), false)
}

/// Like [`parse_str`], but mints anonymous blank nodes as
/// `genid-<n>-<bnode_scope>` (see [`BlankLabeler::keep_with_suffix`]). A
/// caller that parses several documents into one graph passes a distinct
/// scope per document so their anonymous `[ … ]` / `( … )` nodes stay
/// distinct instead of both restarting at `genid-0`. Explicit labels are kept
/// (one starting `genid-` is escaped, so a minted label never collides with
/// an explicit blank node either).
pub fn parse_str_scoped(content: &str, bnode_scope: &str) -> Result<Vec<Triple>> {
    collect(
        content.as_bytes(),
        BlankLabeler::keep_with_suffix(bnode_scope),
        false,
    )
}

/// Parse a Turtle file into triples (streamed from disk).
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let file = std::fs::File::open(path)?;
    collect(
        BufReader::with_capacity(DEFAULT_WINDOW, file),
        BlankLabeler::keep(),
        false,
    )
}

fn collect<R: BufRead>(reader: R, labels: BlankLabeler, trig: bool) -> Result<Vec<Triple>> {
    let mut sink = CollectQuads::default();
    run(reader, labels, trig, &mut sink)?;
    Ok(sink.quads.into_iter().map(Quad::into_triple).collect())
}

/// Stream a Turtle document from `reader` into `sink`, one statement at a
/// time (every quad's graph is `None`). Blank nodes are named by `labels`.
pub fn parse_reader<R: BufRead>(
    reader: R,
    labels: BlankLabeler,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    run(reader, labels, false, sink)
}

/// Stream a TriG document from `reader` into `sink`: Turtle plus `{ … }`,
/// `<g> { … }` and `GRAPH <g> { … }` blocks, whose statements carry the
/// block's graph label.
pub fn parse_reader_trig<R: BufRead>(
    reader: R,
    labels: BlankLabeler,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    run(reader, labels, true, sink)
}

fn run<R: BufRead>(
    reader: R,
    labels: BlankLabeler,
    trig: bool,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    let mut p = Parser {
        src: CharSource::new(reader),
        prefixes: HashMap::new(),
        base: None,
        out: Vec::new(),
        graph: None,
        in_block: false,
        skip_block: false,
        trig,
        labels,
        depth: 0,
        ctx: [None, None, None],
        stmt_start: 0,
        held: 0,
    };
    p.run(sink)
}

struct Parser<R> {
    src: CharSource<R>,
    prefixes: HashMap<String, String>,
    base: Option<String>,
    /// The triples of the statement being parsed (as quads of `graph`).
    out: Vec<Quad>,
    /// TriG: the graph of the block being read (`None` = default graph).
    graph: Option<Term>,
    /// TriG: inside a `{ … }` block.
    in_block: bool,
    /// TriG: inside a block whose label could not be read (an error on the
    /// line that opened it). Its statements are reported as errors rather
    /// than filed under a graph the document does not name.
    skip_block: bool,
    trig: bool,
    labels: BlankLabeler,
    /// Current `[ … ]` / `( … )` nesting depth, bounded by `MAX_NESTING_DEPTH` so
    /// adversarially deep input errors out instead of overflowing the stack.
    depth: usize,
    /// Subject / predicate / object of the current statement, for the error
    /// log context.
    ctx: [Option<Term>; 3],
    /// Where the current statement began ([`CharSource::consumed_bytes`]).
    stmt_start: u64,
    /// Bytes of RDF terms the current statement holds (its quads, the items
    /// of its collections), bounded by [`expansion_budget`].
    held: usize,
}

/// The `rdf:` namespace base (for collection `rdf:first`/`rest`/`nil`).
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// Maximum `[ … ]` / `( … )` nesting depth (guards the recursive term parser
/// against a stack overflow on pathological input, mirroring the JSON-LD reader).
const MAX_NESTING_DEPTH: usize = 256;

/// A character a (strict Turtle) local name may continue with.
fn is_std_local_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '-' | ':' | '%' | '\u{00B7}')
        || (!c.is_ascii() && !c.is_whitespace() && !c.is_control())
}

/// A character a local name may contain in the lenient loader (see the
/// module docs): the standard ones plus upstream's extras.
fn is_lenient_local_char(c: char) -> bool {
    is_std_local_char(c) || matches!(c, '/' | '#' | '?' | '=' | '&' | '~' | '!' | '$' | '*' | '+' | '@')
}

/// A character a blank node label may contain besides `.`.
fn is_blank_label_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Characters a `\`-escape may produce in a local name (`PN_LOCAL_ESC`).
const LOCAL_ESCAPES: &str = "_~.-!$&'()*+,;=/?#@%";

impl<R: BufRead> Parser<R> {
    /// Drive the parse: one statement at a time into `sink`.
    fn run(&mut self, sink: &mut dyn StatementSink) -> Result<()> {
        loop {
            self.ctx = [None, None, None];
            self.stmt_start = self.src.consumed_bytes();
            self.held = 0;
            // The previous statement parsed (or was replayed): nothing of it
            // is needed any more.
            self.src.drop_recording();
            let item = self.next_item();
            if let Some(e) = self.src.take_io_error() {
                return Err(e.into());
            }
            let item = match item {
                // Decoded as U+FFFD: the statement is not what the file says.
                Ok(_) if self.src.take_bad_utf8() => Err(self.err("invalid UTF-8")),
                other => other,
            };
            match item {
                Ok(true) if self.skip_block && !self.out.is_empty() => {
                    self.out.clear();
                    let e = self.err("statement in a graph block whose label could not be read");
                    sink.error(SyntaxError::from_error(&e).with_context(self.context()))?;
                }
                Ok(true) => {
                    if !self.out.is_empty() {
                        sink.at_line(self.src.line());
                        sink.statement(&mut self.out)?;
                        self.out.clear();
                    }
                }
                Ok(false) => return Ok(()),
                Err(e) => {
                    self.out.clear();
                    self.depth = 0;
                    // A short string ran across a line break and the
                    // statement then failed: its closing quote was probably
                    // missing. Read the input again from that line break, so
                    // the error costs the literal's line only.
                    let e = match self.src.replay() {
                        Some((line, overflow)) => literal_span_error(&e, line, overflow),
                        None => e,
                    };
                    sink.error(SyntaxError::from_error(&e).with_context(self.context()))?;
                    // Tolerant: resynchronise at the next line.
                    self.resync();
                    self.src.take_bad_utf8();
                }
            }
        }
    }

    /// The current statement's subject / predicate / object, for the log.
    fn context(&self) -> String {
        self.ctx
            .iter()
            .flatten()
            .map(Term::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Leave a TriG block.
    fn close_block(&mut self) {
        self.in_block = false;
        self.skip_block = false;
        self.graph = None;
    }

    /// After a failed statement: skip the rest of the line (upstream's
    /// `discardLine`). The parser stops outside any string or IRI when it
    /// fails (see [`Parser::skip_rest_of_literal`]), so in TriG the skipped
    /// text can be scanned for block boundaries (outside strings, IRIs and
    /// comments):
    ///
    /// * inside a block, a `}` still closes it, and parsing resumes right
    ///   after it — otherwise one bad statement on a block's last line would
    ///   pull everything after the block into it;
    /// * outside one, a `{` still opens one, under the label written before
    ///   it — otherwise the block's statements would land in the default
    ///   graph. A label that cannot be read makes the block's statements
    ///   errors (see [`Parser::skip_block`]).
    fn resync(&mut self) {
        if !self.trig {
            self.src.discard_line();
            return;
        }
        let in_block = self.in_block;
        let mut quote: Option<char> = None;
        let mut in_iri = false;
        // The token being scanned and the one before it (outside strings),
        // for the label of a block opened on this line.
        let mut token = String::new();
        let mut prev = String::new();
        while let Some(c) = self.src.bump() {
            let outside = quote.is_none() && !in_iri;
            match c {
                '\n' => return,
                '\\' if quote.is_some() => {
                    if self.src.peek() != Some('\n') {
                        self.src.bump();
                    }
                    continue;
                }
                '"' | '\'' if !in_iri => match quote {
                    Some(q) if q == c => quote = None,
                    None => quote = Some(c),
                    Some(_) => {}
                },
                '<' if quote.is_none() => in_iri = true,
                '>' if in_iri => in_iri = false,
                '#' if outside => {
                    self.src.discard_line();
                    return;
                }
                '}' if outside && in_block => {
                    self.close_block();
                    return;
                }
                '{' if outside && !in_block => {
                    let label = if token.is_empty() { prev } else { token };
                    self.in_block = true;
                    match self.block_label(&label) {
                        Some(graph) => self.graph = graph,
                        None => self.skip_block = true,
                    }
                    return;
                }
                _ => {}
            }
            if outside && c.is_whitespace() {
                if !token.is_empty() {
                    prev = std::mem::take(&mut token);
                }
            } else if token.len() < 4096 {
                token.push(c);
            }
        }
    }

    /// The graph of a block opened by `{` after `text` (found while skipping
    /// a bad line): `Some(None)` for the default graph (`… . {`), `None` when
    /// `text` is not a graph label.
    fn block_label(&mut self, text: &str) -> Option<Option<Term>> {
        if text == "." {
            return Some(None);
        }
        if text == "[]" {
            return Some(Some(self.fresh_blank()));
        }
        if let Some(iri) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            if iri
                .chars()
                .any(|c| c <= ' ' || matches!(c, '<' | '>' | '\\'))
            {
                return None;
            }
            return Some(Some(Term::Iri(self.resolve_iri(iri))));
        }
        if let Some(label) = text.strip_prefix("_:") {
            let ok = !label.is_empty()
                && !label.ends_with('.')
                && label
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'));
            return ok.then(|| Some(Term::Blank(self.labels.explicit(label))));
        }
        let (prefix, local) = text.split_once(':')?;
        if local.ends_with('.') || !local.chars().all(is_lenient_local_char) {
            return None;
        }
        let ns = self.prefixes.get(prefix)?;
        Some(Some(Term::Iri(format!("{ns}{local}"))))
    }

    /// Mint a fresh blank node for an anonymous construct.
    fn fresh_blank(&mut self) -> Term {
        Term::Blank(self.labels.fresh())
    }

    fn err(&self, msg: impl Into<String>) -> GStoreError {
        GStoreError::RdfParse {
            line: self.src.line(),
            msg: msg.into(),
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.src.peek()
    }
    fn peek2(&mut self) -> Option<char> {
        self.src.peek_at(1)
    }
    fn bump(&mut self) -> Option<char> {
        self.src.bump()
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

    fn eof(&mut self) -> bool {
        self.src.at_eof()
    }

    /// Emit one triple of the current statement (in the current graph).
    fn emit(&mut self, s: Term, p: Term, o: Term) -> Result<()> {
        let q = Quad::new(s, p, o, self.graph.clone());
        self.charge(q.heap_bytes())?;
        self.out.push(q);
        Ok(())
    }

    /// Count `bytes` more of RDF terms held for the current statement: an
    /// error past its [`expansion_budget`] (so a long prefix repeated by a
    /// long object list cannot take memory without bound).
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.held = self.held.saturating_add(bytes);
        let read = self.src.consumed_bytes().saturating_sub(self.stmt_start);
        let budget = expansion_budget(usize::try_from(read).unwrap_or(usize::MAX));
        if self.held > budget {
            return Err(expansion_error(self.src.line(), "the statement", budget));
        }
        Ok(())
    }

    /// Whether the next characters spell `word` (ASCII, case-insensitive)
    /// followed by a character `boundary` accepts.
    fn keyword_ahead(&mut self, word: &str, boundary: fn(char) -> bool) -> bool {
        for (i, w) in word.chars().enumerate() {
            match self.src.peek_at(i) {
                Some(c) if c.eq_ignore_ascii_case(&w) => {}
                _ => return false,
            }
        }
        self.src.peek_at(word.len()).is_some_and(boundary)
    }

    /// Parse the next top-level item: a directive, a statement, or (TriG) a
    /// block boundary. `Ok(false)` at the end of the input.
    fn next_item(&mut self) -> Result<bool> {
        self.skip_ws();
        if self.eof() {
            if self.in_block {
                self.close_block();
                return Err(self.err("unterminated graph block (missing '}')"));
            }
            return Ok(false);
        }
        if self.in_block {
            if self.peek() == Some('}') {
                self.bump();
                self.close_block();
                // An optional `.` may follow a graph block (lenient).
                self.skip_ws();
                if self.peek() == Some('.') {
                    self.bump();
                }
                return Ok(true);
            }
            self.parse_block_statement()?;
            return Ok(true);
        }
        match self.peek() {
            Some('@') => self.parse_directive()?,
            Some(_) if self.keyword_ahead("PREFIX", char::is_whitespace) => {
                self.parse_sparql_directive("PREFIX")?
            }
            Some(_) if self.keyword_ahead("BASE", |c| c.is_whitespace() || c == '<') => {
                self.parse_sparql_directive("BASE")?
            }
            Some('{') if self.trig => {
                self.bump();
                self.in_block = true;
                self.graph = None;
            }
            Some(_) if self.trig && self.keyword_ahead("GRAPH", |c| c.is_whitespace() || c == '<') => {
                for _ in 0..5 {
                    self.bump();
                }
                self.skip_ws();
                let label = self.parse_graph_label()?;
                self.skip_ws();
                self.expect('{')?;
                self.in_block = true;
                self.graph = Some(label);
            }
            _ => self.parse_triples_statement()?,
        }
        Ok(true)
    }

    /// A graph label after `GRAPH`: an IRI, prefixed name, blank node or `[]`.
    fn parse_graph_label(&mut self) -> Result<Term> {
        if self.peek() == Some('[') {
            self.bump();
            self.skip_ws();
            self.expect(']')?;
            return Ok(self.fresh_blank());
        }
        match self.parse_term(true)? {
            t @ (Term::Iri(_) | Term::Blank(_)) => Ok(t),
            other => Err(self.err(format!("invalid graph label {other}"))),
        }
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

    fn parse_sparql_directive(&mut self, kw: &str) -> Result<()> {
        for _ in 0..kw.len() {
            self.bump();
        }
        if kw == "PREFIX" {
            self.skip_ws();
            let ns = self.read_prefix_ns()?;
            self.skip_ws();
            let iri = self.parse_iriref()?;
            self.prefixes.insert(ns, iri);
        } else {
            self.skip_ws();
            let iri = self.parse_iriref()?;
            self.base = Some(iri);
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

    /// A top-level statement. In TriG, a subject followed by `{` is a graph
    /// block's label instead.
    fn parse_triples_statement(&mut self) -> Result<()> {
        self.skip_ws();
        // A `[ … ]` blank-node property list subject already emits its own
        // triples, so the outer predicate-object list is optional (`[ :p :o ] .`
        // is a complete statement). A collection subject `( … )` is NOT — the
        // grammar still requires a predicate-object list after it.
        let subject_is_blank_list = self.peek() == Some('[');
        let subject = self.parse_term(false)?;
        self.ctx[0] = Some(subject.clone());
        self.skip_ws();
        if self.trig && self.peek() == Some('{') {
            if !matches!(subject, Term::Iri(_) | Term::Blank(_)) || !self.out.is_empty() {
                return Err(self.err(format!("invalid graph label {subject}")));
            }
            self.bump();
            self.in_block = true;
            self.graph = Some(subject);
            return Ok(());
        }
        if subject_is_blank_list && self.peek() == Some('.') {
            self.expect_dot()?;
            return Ok(());
        }
        self.parse_predicate_object_list(&subject)?;
        self.expect_dot()
    }

    /// A statement inside a TriG block: its final `.` may be omitted before
    /// the closing `}`.
    fn parse_block_statement(&mut self) -> Result<()> {
        let subject_is_blank_list = self.peek() == Some('[');
        let subject = self.parse_term(false)?;
        self.ctx[0] = Some(subject.clone());
        self.skip_ws();
        let standalone = subject_is_blank_list && matches!(self.peek(), Some('.') | Some('}'));
        if !standalone {
            self.parse_predicate_object_list(&subject)?;
            self.skip_ws();
        }
        match self.peek() {
            Some('.') => {
                self.bump();
                Ok(())
            }
            Some('}') => Ok(()),
            other => Err(self.err(format!("expected '.' or '}}', found {other:?}"))),
        }
    }

    fn parse_predicate_object_list(&mut self, subject: &Term) -> Result<()> {
        loop {
            self.skip_ws();
            let predicate = self.parse_verb()?;
            self.ctx[1] = Some(predicate.clone());
            // object list
            loop {
                let object = self.parse_term(false)?;
                self.ctx[2] = Some(object.clone());
                self.emit(subject.clone(), predicate.clone(), object)?;
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
                // trailing ';' before '.' (or `]` / `}`) is allowed
                if matches!(self.peek(), Some('.') | Some(']') | Some('}') | None) {
                    break;
                }
                // `; ;` repeated separators are allowed too
                while self.peek() == Some(';') {
                    self.bump();
                    self.skip_ws();
                }
                if matches!(self.peek(), Some('.') | Some(']') | Some('}') | None) {
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
        if self.peek() == Some('a')
            && self
                .peek2()
                .is_none_or(|c| c.is_whitespace() || matches!(c, '<' | '[' | '(' | '"'))
        {
            self.bump();
            return Ok(Term::iri(RDF_TYPE));
        }
        self.parse_term(true)
    }

    /// Parse a term. `verb_pos` is true when parsing a predicate (no literals).
    fn parse_term(&mut self, verb_pos: bool) -> Result<Term> {
        self.skip_ws();
        // Every `[ … ]` / `( … )` nesting level enters through exactly one
        // `parse_term`, so bounding depth here bounds the whole recursion. On the
        // error path the count is not restored, but that only aborts the
        // statement (the driver resets it); the success path below decrements
        // so sibling terms are unaffected.
        self.depth += 1;
        if self.depth > MAX_NESTING_DEPTH {
            return Err(self.err("blank-node / collection nesting too deep"));
        }
        let out = match self.peek() {
            Some('<') => Ok(Term::Iri(self.parse_iriref()?)),
            Some('"') | Some('\'') if !verb_pos => self.parse_literal(),
            Some('[') if !verb_pos => self.parse_blank_node_property_list(),
            Some('(') if !verb_pos => self.parse_collection(),
            Some('_') if self.peek2() == Some(':') => self.parse_blank(),
            Some(c) if (c.is_ascii_digit() || c == '+' || c == '-' || c == '.') && !verb_pos => {
                self.parse_numeric()
            }
            Some(_) => self.parse_prefixed_or_keyword(verb_pos),
            None => Err(self.err("expected a term, found end of input")),
        };
        self.depth -= 1;
        out
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
                _ => {
                    let item = self.parse_term(false)?;
                    self.charge(item.heap_bytes())?;
                    items.push(item);
                }
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
            self.emit(current.clone(), first.clone(), item)?;
            let next = if i + 1 < n {
                self.fresh_blank()
            } else {
                nil.clone()
            };
            self.emit(current.clone(), rest.clone(), next.clone())?;
            current = next;
        }
        Ok(head)
    }

    fn parse_iriref(&mut self) -> Result<String> {
        self.expect('<')?;
        let mut s = String::new();
        loop {
            // IRIREF excludes spaces and control characters (upstream's lexer
            // also stops at a space). Not consuming it keeps a tolerant
            // import's resync on this line: `<oops .` must not swallow the
            // statements after it while looking for a '>'.
            if matches!(self.peek(), Some(c) if c <= ' ') {
                return Err(self.err("invalid IRI (space or control character before '>')"));
            }
            match self.bump() {
                Some('>') => return Ok(self.resolve_iri(&s)),
                Some('\\') => {
                    // \uXXXX / \UXXXXXXXX
                    let escaped = match self.peek() {
                        Some('u') => {
                            self.bump();
                            self.read_hex(&mut s, 4)
                        }
                        Some('U') => {
                            self.bump();
                            self.read_hex(&mut s, 8)
                        }
                        other => Err(self.err(format!("invalid IRI escape '\\{other:?}'"))),
                    };
                    if let Err(e) = escaped {
                        // Stop after the IRI, not inside it (see `resync`).
                        while let Some(c) = self.peek() {
                            if c <= ' ' {
                                break;
                            }
                            self.bump();
                            if c == '>' {
                                break;
                            }
                        }
                        return Err(e);
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
            if c == '.' {
                // A label never ends with '.' (BLANK_NODE_LABEL): the dots
                // are part of it only if a label character follows them —
                // `_:b.}` / `_:b.# note` / `_:b.<s2> …` end the statement.
                let dots = (0..MAX_LOOKAHEAD - 1)
                    .take_while(|&k| self.src.peek_at(k) == Some('.'))
                    .count();
                if !self.src.peek_at(dots).is_some_and(is_blank_label_char) {
                    break;
                }
                for _ in 0..dots {
                    label.push('.');
                    self.bump();
                }
            } else if is_blank_label_char(c) {
                label.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if label.is_empty() {
            return Err(self.err("empty blank node label"));
        }
        Ok(Term::Blank(self.labels.explicit(&label)))
    }

    fn parse_prefixed_or_keyword(&mut self, verb_pos: bool) -> Result<Term> {
        // Could be `prefix:local`, `:local`, or a boolean literal.
        let mut prefix = String::new();
        while let Some(c) = self.peek() {
            // Stop at the ':' that ends the prefix, or any term separator.
            if c == ':'
                || c.is_whitespace()
                || matches!(c, '.' | ';' | ',' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '"' | '\'')
            {
                break;
            }
            prefix.push(c);
            self.bump();
        }
        if self.peek() == Some(':') {
            self.bump();
            let local = self.read_pn_local()?;
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
        let found = prefix
            .chars()
            .next()
            .or_else(|| self.peek())
            .unwrap_or(' ');
        Err(self.err(format!(
            "expected a prefixed name or IRI, found '{found}'"
        )))
    }

    /// The local part of a prefixed name (lenient, see the module docs).
    fn read_pn_local(&mut self) -> Result<String> {
        let mut local = String::new();
        while let Some(c) = self.peek() {
            if c == '\\' {
                match self.peek2() {
                    Some(e) if LOCAL_ESCAPES.contains(e) => {
                        self.bump();
                        self.bump();
                        local.push(e);
                    }
                    _ => return Err(self.err("invalid escape in prefixed name")),
                }
            } else if c == '.' {
                // A '.' is part of the local name only if a name char follows.
                if self.peek2().is_some_and(is_std_local_char) {
                    local.push('.');
                    self.bump();
                } else {
                    break;
                }
            } else if is_lenient_local_char(c) {
                local.push(c);
                self.bump();
            } else {
                break;
            }
        }
        Ok(local)
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
                // A raw line break in a short string: W3C Turtle forbids it
                // (STRING_LITERAL_QUOTE), upstream's lexer (`lexString`)
                // reads it into the value, and so does this reader — while
                // recording from here, so that a statement that then fails
                // (a closing quote was missing, most likely) is read again
                // from this line break instead of swallowing the lines
                // after it (see `CharSource::record_from`).
                Some(c @ ('\n' | '\r')) if !triple => {
                    let line = self.src.line();
                    self.src.record_from(line);
                    value.push(c);
                    self.bump();
                }
                Some('\\') => {
                    self.bump();
                    if let Err(e) = self.read_string_escape(&mut value) {
                        self.skip_rest_of_literal(quote, triple);
                        return Err(e);
                    }
                }
                Some(c) if c == quote => {
                    if triple {
                        if self.peek2() == Some(quote) && self.src.peek_at(2) == Some(quote) {
                            self.bump();
                            self.bump();
                            self.bump();
                            break;
                        }
                        value.push(c);
                        self.bump();
                    } else {
                        self.bump();
                        self.src.end_literal();
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
                // LANGTAG needs a letter (as the N-Triples reader, and the
                // dictionary's readers, require).
                if lang.is_empty() {
                    return Err(self.err("empty language tag after '@'"));
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

    /// After a bad escape: move past the literal's closing quote(s), so that
    /// a failed statement leaves the parser outside the string (`resync`
    /// relies on it). A short string still stops before a line break.
    fn skip_rest_of_literal(&mut self, quote: char, triple: bool) {
        while let Some(c) = self.peek() {
            match c {
                '\n' | '\r' if !triple => return,
                '\\' => {
                    self.bump();
                    if triple || !matches!(self.peek(), Some('\n' | '\r')) {
                        self.bump();
                    }
                }
                c if c == quote => {
                    if !triple {
                        self.bump();
                        return;
                    }
                    if self.peek2() == Some(quote) && self.src.peek_at(2) == Some(quote) {
                        self.bump();
                        self.bump();
                        self.bump();
                        return;
                    }
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
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
        if matches!(self.peek(), Some('\n' | '\r')) {
            return Err(self.err("invalid escape: '\\' at the end of a line"));
        }
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
            // Peeked, not consumed: a bad digit may be a closing quote.
            let c = self
                .peek()
                .ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| self.err(format!("bad hex digit '{c}'")))?;
            self.bump();
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
            let found = self.peek();
            Err(self.err(format!("expected '{c}', found {found:?}")))
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


/// The error of a statement replayed from the first raw line break of one
/// of its short string literals (opened on `line`): reported on that line,
/// the literal's closing quote being the likely culprit.
fn literal_span_error(e: &GStoreError, line: usize, overflow: bool) -> GStoreError {
    let detail = if overflow {
        format!(
            "a string literal runs across line breaks for more than {} KiB",
            crate::parser::stream::MAX_LITERAL_SPAN / 1024
        )
    } else {
        match e {
            GStoreError::RdfParse { msg, .. } => msg.clone(),
            other => other.to_string(),
        }
    };
    GStoreError::RdfParse {
        line,
        msg: format!(
            "{detail} (a string literal opened on this line runs across a line break: is its \
             closing quote missing?)"
        ),
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
    fn deeply_nested_collection_errors_instead_of_overflowing() {
        // Far more nesting than MAX_NESTING_DEPTH: must return an error rather
        // than recurse until the stack overflows and aborts the process.
        let doc = format!(
            "@prefix : <http://e/> .\n:s :p {}{}",
            "(".repeat(400),
            ")".repeat(400)
        );
        assert!(parse_str(&doc).is_err(), "pathologically deep nesting must be rejected");
    }

    #[test]
    fn nested_blank_and_collection() {
        let doc = "@prefix : <http://e/> . :s :p [ :q ( :a :b ) ] .";
        let ts = parse_str(doc).unwrap();
        // inner: 2 first + 2 rest = 4 ; B :q head ; :s :p B  => 6
        assert_eq!(ts.len(), 6);
    }

    #[test]
    fn standalone_collection_subject_is_rejected() {
        // `( :a :b ) .` has no predicate-object list; only `[ … ]` may stand alone.
        let doc = "@prefix : <http://e/> . ( :a :b ) .";
        assert!(parse_str(doc).is_err());
    }

    #[test]
    fn minted_blank_cannot_collide_with_explicit() {
        // An explicit `_:g` blank and a minted one must stay distinct.
        let doc = "@prefix : <http://e/> . _:g :p [ :q :r ] .";
        let ts = parse_str(doc).unwrap();
        // _:g :p B  and  B :q :r  → two distinct blank subjects
        let blanks: std::collections::HashSet<String> = ts
            .iter()
            .filter_map(|t| match &t.subject {
                Term::Blank(l) => Some(l.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(blanks.len(), 2);
    }

    #[test]
    fn minted_labels_round_trip_through_parse_term() {
        let ts = parse_str("@prefix : <http://e/> . :s :p [ :q ( :a ) ] .").unwrap();
        for t in &ts {
            for term in [&t.subject, &t.object] {
                if let Term::Blank(_) = term {
                    let back = crate::parser::ntriples::parse_term(&term.to_string()).unwrap();
                    assert_eq!(&back, term);
                }
            }
        }
    }

    #[test]
    fn lenient_local_names_like_upstream() {
        let doc = "@prefix ex: <http://e/> .\n\
                   ex:a/b ex:page#top ex:q?x=1&y=2 .\n\
                   ex:a:b ex:p ex:c\\,d .\n\
                   ex:s ex:p ex:o . # a comment after whitespace\n\
                   ex:t ex:p ex:o .# a comment after the dot\n";
        let ts = parse_str(doc).unwrap();
        assert_eq!(ts.len(), 4);
        assert_eq!(ts[0].subject, Term::iri("http://e/a/b"));
        assert_eq!(ts[0].predicate, Term::iri("http://e/page#top"));
        assert_eq!(ts[0].object, Term::iri("http://e/q?x=1&y=2"));
        assert_eq!(ts[1].subject, Term::iri("http://e/a:b"));
        assert_eq!(ts[1].object, Term::iri("http://e/c,d"));
        assert_eq!(ts[3].object, Term::iri("http://e/o"));
    }

    /// A tolerant sink: collects statements and errors.
    #[derive(Default)]
    struct Tolerant {
        quads: Vec<Quad>,
        errors: Vec<SyntaxError>,
    }

    impl StatementSink for Tolerant {
        fn statement(&mut self, quads: &mut Vec<Quad>) -> Result<()> {
            self.quads.append(quads);
            Ok(())
        }
        fn error(&mut self, err: SyntaxError) -> Result<()> {
            self.errors.push(err);
            Ok(())
        }
    }

    #[test]
    fn tolerant_parse_skips_the_bad_statement_and_resyncs_at_the_next_line() {
        let doc = "@prefix : <http://e/> .\n\
                   :a :p :o1 .\n\
                   :b :p undefined:x .\n\
                   :c :p :o2 ; :q :o3 :extra .\n\
                   :d :p :o3 .\n\
                   \"lit\" :p :o4 .\n\
                   :e :p \"bad \\q escape\" .\n\
                   :f :p :o5 .\n";
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let subjects: Vec<String> = sink.quads.iter().map(|q| q.subject.to_string()).collect();
        // `:c`'s statement is dropped whole (no partial `:c :p :o2`); the
        // literal subject is the import layer's business, not a syntax error.
        assert_eq!(
            subjects,
            ["<http://e/a>", "<http://e/d>", "\"lit\"", "<http://e/f>"]
        );
        let lines: Vec<usize> = sink.errors.iter().map(|e| e.line).collect();
        assert_eq!(lines, [3, 4, 7], "{:?}", sink.errors);
        assert!(sink.errors[0].message.contains("undefined prefix"));
        assert!(sink.errors[0].context.contains("<http://e/b>"));
        // A strict parse of the same document stops at line 3.
        let err = parse_str(doc).unwrap_err();
        assert!(matches!(err, GStoreError::RdfParse { line: 3, .. }), "{err}");
    }

    #[test]
    fn an_error_found_on_a_later_line_skips_that_line_like_upstream() {
        // The unterminated collection swallows the next line's terms; the
        // error surfaces on line 3, whose rest is discarded (upstream's
        // `discardLine` does the same), so `:d` is lost and `:e` is kept.
        let doc = "@prefix : <http://e/> .\n\
                   :c :p ( :unterminated\n\
                   :d :p :o3 .\n\
                   :e :p :o4 .\n";
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        assert_eq!(sink.quads.len(), 1);
        assert_eq!(sink.quads[0].subject, Term::iri("http://e/e"));
        assert_eq!(sink.errors.len(), 1);
        assert_eq!(sink.errors[0].line, 3);
    }

    #[test]
    fn an_iri_stops_at_a_space_so_a_missing_gt_costs_one_line() {
        // Upstream's lexer ends an IRI at a space too ("invalid URI"); the
        // next line's statement survives a tolerant import.
        let doc = "@prefix : <http://e/> .\n\
                   :a :p <oops .\n\
                   :b :p :o .\n\
                   :c :p <http://e/x\ty> .\n\
                   :d :p :o .\n";
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let subjects: Vec<String> = sink.quads.iter().map(|q| q.subject.to_string()).collect();
        assert_eq!(subjects, ["<http://e/b>", "<http://e/d>"]);
        let lines: Vec<usize> = sink.errors.iter().map(|e| e.line).collect();
        assert_eq!(lines, [2, 4], "{:?}", sink.errors);
        assert!(sink.errors[0].message.contains("invalid IRI"));
        assert!(parse_str("<http://e/a b> <http://e/p> <http://e/o> .").is_err());
        assert!(parse_str("<http://e/a%20b> <http://e/p> <http://e/o> .").is_ok());
    }

    #[test]
    fn tolerant_trig_recovery_stays_inside_the_block() {
        let doc = "@prefix : <http://e/> .\n\
                   :g {\n\
                   :a :p :o .\n\
                   :b :p ?bad .\n\
                   :c :p :o\n\
                   }\n\
                   :d :p :o .\n";
        let mut sink = Tolerant::default();
        parse_reader_trig(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let got: Vec<(String, Option<Term>)> = sink
            .quads
            .iter()
            .map(|q| (q.subject.to_string(), q.graph.clone()))
            .collect();
        let g = Some(Term::iri("http://e/g"));
        assert_eq!(
            got,
            [
                ("<http://e/a>".to_string(), g.clone()),
                ("<http://e/c>".to_string(), g),
                ("<http://e/d>".to_string(), None)
            ]
        );
        assert_eq!(sink.errors.len(), 1);
    }

    #[test]
    fn a_bad_statement_on_a_block_s_last_line_still_closes_the_block() {
        let doc = "@prefix : <http://e/> .\n\
                   :g { :a :p \"}\" . :bad :p }\n\
                   :d :p :o .\n\
                   :h { :x :p :y . :z :p ?q # } in a comment\n\
                   :w :p :v }\n\
                   :e :p :o .\n";
        let mut sink = Tolerant::default();
        parse_reader_trig(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let got: Vec<(String, Option<Term>)> = sink
            .quads
            .iter()
            .map(|q| (q.subject.to_string(), q.graph.clone()))
            .collect();
        let (g, h) = (Some(Term::iri("http://e/g")), Some(Term::iri("http://e/h")));
        assert_eq!(
            got,
            [
                ("<http://e/a>".to_string(), g),
                ("<http://e/d>".to_string(), None),
                ("<http://e/x>".to_string(), h.clone()),
                ("<http://e/w>".to_string(), h),
                ("<http://e/e>".to_string(), None),
            ]
        );
        assert_eq!(sink.errors.len(), 2);
    }

    /// `(subject, graph)` of each quad a tolerant TriG parse keeps, and its
    /// error count.
    fn tolerant_trig(doc: &str) -> (Vec<(String, Option<Term>)>, usize) {
        let mut sink = Tolerant::default();
        parse_reader_trig(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let got = sink
            .quads
            .iter()
            .map(|q| (q.subject.to_string(), q.graph.clone()))
            .collect();
        (got, sink.errors.len())
    }

    #[test]
    fn an_unterminated_short_literal_fails_only_its_line() {
        let mut doc = String::from("<s0> <p> <o0> .\n<s1> <p> \"unterminated .\n");
        for i in 0..100 {
            doc.push_str(&format!("<s{}> <p> <o> .\n", i + 2));
        }
        // An escaped closing quote, and a backslash ending the line.
        doc.push_str("<x> <p> \"C:\\\\dir\\\" .\n<y> <p> <o> .\n<z> <p> \"a\\\n<w> <p> <o> .\n");
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        assert_eq!(sink.quads.len(), 103, "{:?}", sink.errors);
        let lines: Vec<usize> = sink.errors.iter().map(|e| e.line).collect();
        assert_eq!(lines, [2, 103, 105], "{:?}", sink.errors);
        // Line 2's string ran on to line 103's quote, then the statement
        // failed: replayed from its line break, reported on its line.
        assert!(
            sink.errors[0].message.contains("closing quote missing"),
            "{:?}",
            sink.errors
        );
        // A long string may still span lines.
        let ts = parse_str("<s> <p> \"\"\"a\nb\"\"\" .").unwrap();
        assert_eq!(ts[0].object, Term::plain_literal("a\nb"));
    }

    #[test]
    fn short_strings_may_hold_raw_line_breaks_like_upstream() {
        // Upstream's `lexString` reads a raw line break into the value.
        let ts = parse_str("<s> <p> \"a\nb\" .\n<s> <q> 'c\r\nd'@en .\n<t> <p> <o> .\n").unwrap();
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].object, Term::plain_literal("a\nb"));
        assert_eq!(
            ts[1].object,
            Term::Literal {
                value: "c\r\nd".into(),
                datatype: None,
                lang: Some("en".into())
            }
        );
        // Several lines, an escape and a predicate-object list after it.
        let doc = "<s> <p> \"line 1\nline \\\"2\\\"\n\nline 4\" ; <q> 1 .\n<u> <p> <o> .\n";
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        assert!(sink.errors.is_empty(), "{:?}", sink.errors);
        assert_eq!(sink.quads.len(), 3);
        assert_eq!(
            sink.quads[0].object,
            Term::plain_literal("line 1\nline \"2\"\n\nline 4")
        );
        // In a TriG block too, and the block still closes.
        let (got, errors) =
            tolerant_trig("<g> { <s> <p> \"x\ny\" .\n<t> <p> <o> . }\n<d> <p> <o> .\n");
        assert_eq!(errors, 0);
        let g = Some(Term::iri("g"));
        assert_eq!(
            got,
            [
                ("<s>".to_string(), g.clone()),
                ("<t>".to_string(), g),
                ("<d>".to_string(), None)
            ]
        );
    }

    #[test]
    fn an_unclosed_string_reaching_the_end_or_the_cap_costs_only_its_line() {
        use crate::parser::stream::MAX_LITERAL_SPAN;
        // No closing quote before the end of the input.
        let doc = "<a> <p> <o> .\n<s> <p> \"no end .\n<b> <p> <o> .\n<c> <p> <o> .\n";
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let subjects: Vec<String> = sink.quads.iter().map(|q| q.subject.to_string()).collect();
        assert_eq!(subjects, ["<a>", "<b>", "<c>"], "{:?}", sink.errors);
        assert_eq!(sink.errors.len(), 1);
        assert_eq!(sink.errors[0].line, 2);
        assert!(sink.errors[0].message.contains("unterminated literal"));
        let err = parse_str(doc).unwrap_err();
        assert!(
            matches!(err, GStoreError::RdfParse { line: 2, .. }),
            "{err}"
        );

        // A quote far away: the string would run past the cap.
        let mut doc = String::from("<s> <p> \"far .\n");
        let filler = "<f> <p> <o> .\n";
        let n = MAX_LITERAL_SPAN / filler.len() + 10;
        for _ in 0..n {
            doc.push_str(filler);
        }
        doc.push_str("<z> <p> \"closed\" .\n");
        // A small read window: the replay crosses many refills.
        let reader = BufReader::with_capacity(7, doc.as_bytes());
        let mut sink = Tolerant::default();
        parse_reader(reader, BlankLabeler::keep(), &mut sink).unwrap();
        assert_eq!(sink.errors.len(), 1, "{:?}", sink.errors);
        assert_eq!(sink.errors[0].line, 1);
        assert!(
            sink.errors[0].message.contains("more than 256 KiB"),
            "{:?}",
            sink.errors
        );
        assert_eq!(sink.quads.len(), n + 1);
        assert_eq!(sink.quads[n].object, Term::plain_literal("closed"));
    }

    #[test]
    fn replays_keep_lines_bytes_and_bad_utf8_exact() {
        // The statement after a replayed one is reported on its own line,
        // and invalid UTF-8 inside the replayed text still fails its
        // statement.
        let mut doc =
            b"<s> <p> \"open .\n<a> <p> \"x\xff\" .\n<b> <p> <o> .\n<c> ?bad .\n".to_vec();
        doc.extend_from_slice(b"<d> <p> <o> .\n");
        let mut sink = Tolerant::default();
        parse_reader(&doc[..], BlankLabeler::keep(), &mut sink).unwrap();
        let subjects: Vec<String> = sink.quads.iter().map(|q| q.subject.to_string()).collect();
        assert_eq!(subjects, ["<b>", "<d>"], "{:?}", sink.errors);
        let lines: Vec<usize> = sink.errors.iter().map(|e| e.line).collect();
        assert_eq!(lines, [1, 2, 4], "{:?}", sink.errors);
        assert!(
            sink.errors[1].message.contains("UTF-8"),
            "{:?}",
            sink.errors
        );
    }

    #[test]
    fn a_bad_escape_leaves_the_parser_after_the_literal() {
        // A bad `\u` digit that is the closing quote, and a bad escape in a
        // long string whose text looks like statements.
        let doc = "<a> <p> \"\\u12\" . <x> <p> <o> .\n<b> <p> <o> .\n\
                   <c> <p> \"\"\"\\q\n<inside> <p> <o> .\n\"\"\" .\n<d> <p> <o> .\n\
                   <e> <p> <http://x/\\q\"> .\n<f> <p> <o> .\n";
        let mut sink = Tolerant::default();
        parse_reader(doc.as_bytes(), BlankLabeler::keep(), &mut sink).unwrap();
        let subjects: Vec<String> = sink.quads.iter().map(|q| q.subject.to_string()).collect();
        assert_eq!(subjects, ["<b>", "<d>", "<f>"]);
        assert_eq!(sink.errors.len(), 3);
    }

    #[test]
    fn tolerant_trig_recovery_follows_blocks_after_an_error_in_a_literal() {
        // A bad escape inside a literal on a block's last line: its `}`
        // still closes the block.
        let (got, errors) = tolerant_trig(
            "<g1> {\n<s1> <p> <o1> .\n<s2> <p> \"C:\\path\" }\n<s3> <p> <o3> .\n\
             <g2> { <s4> <p> <o4> . }\n<s5> <p> <o5> .\n",
        );
        let (g1, g2) = (Some(Term::iri("g1")), Some(Term::iri("g2")));
        assert_eq!(
            got,
            [
                ("<s1>".to_string(), g1),
                ("<s3>".to_string(), None),
                ("<s4>".to_string(), g2.clone()),
                ("<s5>".to_string(), None),
            ]
        );
        assert_eq!(errors, 1);
        // A block opened on the line of a bad statement is still opened.
        let (got, errors) = tolerant_trig(
            "@prefix ex: <http://e/> .\n\
             <s0> <p> \"bad\\q\" . <g2> {\n<a> <b> <c> .\n}\n\
             <s1> <p> \"bad\\q\" . ex:g {\n<d> <b> <c> .\n}\n\
             <s2> <p> \"bad\\q\" . GRAPH _:x { <e> <b> <c> . }\n\
             <s3> <p> \"bad\\q\" . { <f> <b> <c> . }\n\
             <z> <p> <o> .\n",
        );
        assert_eq!(
            got,
            [
                ("<a>".to_string(), g2),
                ("<d>".to_string(), Some(Term::iri("http://e/g"))),
                ("<e>".to_string(), Some(Term::Blank("x".into()))),
                ("<f>".to_string(), None),
                ("<z>".to_string(), None),
            ]
        );
        assert_eq!(errors, 4);
        // A block whose label cannot be read: its statements are errors,
        // not default-graph triples.
        let (got, errors) =
            tolerant_trig("\"lit\" {\n<a> <b> <c> .\n<d> <b> <c> .\n}\n<z> <p> <o> .\n");
        assert_eq!(got, [("<z>".to_string(), None)]);
        assert_eq!(errors, 3);
    }

    #[test]
    fn a_blank_node_label_does_not_end_with_a_dot() {
        let (got, errors) = tolerant_trig("<g> { <s> <p> _:b.}\n<g> { _:b <p> <o> . }\n");
        assert_eq!(errors, 0);
        assert_eq!(got.len(), 2);
        let mut sink = CollectQuads::default();
        parse_reader_trig(
            "<g> { <s> <p> _:b.}\n<g> { _:b <p> <o> . }\n".as_bytes(),
            BlankLabeler::keep(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.quads[0].object, Term::Blank("b".into()));
        assert_eq!(sink.quads[1].subject, Term::Blank("b".into()));
        let ts = parse_str("<s> <p> _:b.# note\n<s> <p> _:c.<s2> <p> _:a.b , _:a..b .").unwrap();
        let objects: Vec<Term> = ts.into_iter().map(|t| t.object).collect();
        assert_eq!(
            objects,
            ["b", "c", "a.b", "a..b"].map(|l| Term::Blank(l.into()))
        );
    }

    #[test]
    fn invalid_utf8_fails_only_its_statement() {
        let mut doc = b"<http://e/a> <http://e/p> \"ok\" .\n<http://e/b> <http://e/p> \"".to_vec();
        doc.extend_from_slice(b"\xff\xfe\" .\n<http://e/c> <http://e/p> \"ok\" .\n");
        let mut sink = Tolerant::default();
        parse_reader(doc.as_slice(), BlankLabeler::keep(), &mut sink).unwrap();
        assert_eq!(sink.quads.len(), 2);
        assert_eq!(sink.errors.len(), 1);
        assert!(sink.errors[0].message.contains("UTF-8"));
        let mut strict = CollectQuads::default();
        assert!(parse_reader(doc.as_slice(), BlankLabeler::keep(), &mut strict).is_err());
    }

    #[test]
    fn scoped_labels_prefix_explicit_and_minted_nodes() {
        let mut sink = CollectQuads::default();
        parse_reader(
            "_:x <http://e/p> [] .".as_bytes(),
            BlankLabeler::scoped("S1"),
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.quads[0].subject, Term::blank("S1_x"));
        assert_eq!(sink.quads[0].object, Term::blank("S1-0"));
    }
}
