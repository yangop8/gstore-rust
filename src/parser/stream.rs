//! Streaming building blocks shared by the RDF readers.
//!
//! * [`CharSource`] decodes UTF-8 from any [`BufRead`] one character at a
//!   time, keeping only the reader's own buffer (its *window*) plus a few
//!   characters of lookahead in memory — never the whole document. A
//!   multi-byte character split across two buffer refills is reassembled; an
//!   invalid byte sequence decodes to U+FFFD and is reported through
//!   [`CharSource::take_bad_utf8`] so a reader can fail (strict) or skip
//!   (tolerant) the statement it occurred in.
//! * [`StatementSink`] receives a reader's output one *statement* at a time
//!   (the quads one `s p o .` / `s p o g .` / Turtle statement produced), and
//!   decides what a syntax error means: a strict sink returns the error (the
//!   parse stops), a tolerant one records it and returns `Ok` (the reader
//!   resynchronises and goes on, like upstream gStore's `RDFParser`).
//! * [`BlankLabeler`] names blank nodes. Parsers call it for every labelled
//!   `_:x` and every anonymous `[]` / `( … )` node they mint. In *keep* mode
//!   (the plain `parse_*` APIs) explicit labels stay as written and minted
//!   ones are `genid-<n>`; in *scoped* mode (every bulk load) both are
//!   prefixed with a scope unique to that one document, so blank nodes of two
//!   files, or of two `gadd` / `batchInsert` calls, can never merge. Every
//!   label it produces uses only `[A-Za-z0-9_-]`, so it round-trips through
//!   [`ntriples::parse_term`](crate::parser::ntriples::parse_term) and every
//!   exporter.

use std::collections::VecDeque;
use std::io::{self, BufRead};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{GStoreError, Result};
use crate::parser::nquads::Quad;

/// Characters of lookahead a reader may ask for ([`CharSource::peek_at`]).
/// The Turtle/TriG reader needs at most 6 (`PREFIX` / `GRAPH` keywords).
pub const MAX_LOOKAHEAD: usize = 8;

/// The read window the bulk loaders use: the capacity of the `BufReader`
/// wrapped around a file, i.e. how much undecoded input is held at once.
pub const DEFAULT_WINDOW: usize = 64 * 1024;

/// A bounded-memory UTF-8 character stream over a [`BufRead`].
///
/// Memory held: the reader's own buffer (the window), at most
/// [`MAX_LOOKAHEAD`] decoded characters and a 4-byte carry for a character
/// split across refills. Nothing else of the input is retained.
pub struct CharSource<R> {
    inner: R,
    /// Decoded lookahead: `(char, encoded byte length)`.
    look: VecDeque<(char, u8)>,
    /// 1-based line of the next character [`bump`](Self::bump) returns.
    line: usize,
    eof: bool,
    io_error: Option<io::Error>,
    bad_utf8: bool,
    /// Bytes of the characters consumed so far.
    consumed: u64,
}

impl<R: BufRead> CharSource<R> {
    pub fn new(inner: R) -> CharSource<R> {
        CharSource {
            inner,
            look: VecDeque::with_capacity(MAX_LOOKAHEAD),
            line: 1,
            eof: false,
            io_error: None,
            bad_utf8: false,
            consumed: 0,
        }
    }

    /// The line (1-based) of the next character.
    pub fn line(&self) -> usize {
        self.line
    }

    /// Bytes of input consumed so far (characters returned by `bump`).
    pub fn consumed_bytes(&self) -> u64 {
        self.consumed
    }

    /// Characters currently decoded ahead of the cursor (never more than
    /// [`MAX_LOOKAHEAD`]).
    pub fn lookahead_len(&self) -> usize {
        self.look.len()
    }

    /// The I/O error that ended the input early, if any (taken once).
    pub fn take_io_error(&mut self) -> Option<io::Error> {
        self.io_error.take()
    }

    /// Whether an invalid UTF-8 sequence was decoded (as U+FFFD) since the
    /// last call; resets the flag.
    pub fn take_bad_utf8(&mut self) -> bool {
        std::mem::take(&mut self.bad_utf8)
    }

    /// Decode one more character into the lookahead. `false` at the end of
    /// the input (or after an I/O error, kept for [`take_io_error`]).
    fn decode_one(&mut self) -> bool {
        if self.eof {
            return false;
        }
        let buf = loop {
            match self.inner.fill_buf() {
                Ok(b) => break b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.io_error = Some(e);
                    self.eof = true;
                    return false;
                }
            }
        };
        let Some(&b0) = buf.first() else {
            self.eof = true;
            return false;
        };
        if b0 < 0x80 {
            self.inner.consume(1);
            self.look.push_back((b0 as char, 1));
            return true;
        }
        let n = utf8_len(b0);
        if n == 0 {
            self.inner.consume(1);
            self.bad_utf8 = true;
            self.look.push_back(('\u{FFFD}', 1));
            return true;
        }
        if buf.len() >= n {
            let decoded = std::str::from_utf8(&buf[..n])
                .ok()
                .and_then(|s| s.chars().next());
            match decoded {
                Some(c) => {
                    self.inner.consume(n);
                    self.look.push_back((c, n as u8));
                }
                None => {
                    self.inner.consume(1);
                    self.bad_utf8 = true;
                    self.look.push_back(('\u{FFFD}', 1));
                }
            }
            return true;
        }
        // The character straddles the end of the buffer: gather it, one
        // continuation byte at a time. A byte that is not one (an invalid
        // lead byte followed by ASCII, say) is left for the next character,
        // as the path above leaves it — so what is decoded never depends on
        // where the read window happens to end.
        let mut tmp = [0u8; 4];
        tmp[0] = b0;
        self.inner.consume(1);
        let mut have = 1usize;
        while have < n {
            let buf = loop {
                match self.inner.fill_buf() {
                    Ok(b) => break b,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        self.io_error = Some(e);
                        self.eof = true;
                        break &[][..];
                    }
                }
            };
            match buf.first() {
                Some(&b) if b & 0xC0 == 0x80 => {
                    tmp[have] = b;
                    have += 1;
                    self.inner.consume(1);
                }
                _ => break,
            }
        }
        match std::str::from_utf8(&tmp[..have])
            .ok()
            .filter(|_| have == n)
            .and_then(|s| s.chars().next())
        {
            Some(c) => self.look.push_back((c, n as u8)),
            None => {
                self.bad_utf8 = true;
                self.look.push_back(('\u{FFFD}', have.max(1) as u8));
            }
        }
        true
    }

    /// The character `k` positions ahead (`0` = the next one).
    pub fn peek_at(&mut self, k: usize) -> Option<char> {
        debug_assert!(k < MAX_LOOKAHEAD);
        while self.look.len() <= k {
            if !self.decode_one() {
                return None;
            }
        }
        self.look.get(k).map(|&(c, _)| c)
    }

    /// The next character.
    pub fn peek(&mut self) -> Option<char> {
        self.peek_at(0)
    }

    /// Consume and return the next character.
    pub fn bump(&mut self) -> Option<char> {
        if self.look.is_empty() && !self.decode_one() {
            return None;
        }
        let (c, n) = self.look.pop_front()?;
        self.consumed += u64::from(n);
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Whether the input is exhausted.
    pub fn at_eof(&mut self) -> bool {
        self.peek().is_none()
    }

    /// Skip the rest of the current line, including its `\n` (upstream's
    /// `TurtleParser::discardLine`, its error recovery).
    pub fn discard_line(&mut self) {
        while let Some(c) = self.bump() {
            if c == '\n' {
                break;
            }
        }
    }
}

/// Length of the UTF-8 sequence a lead byte starts (0 = not a lead byte).
fn utf8_len(b0: u8) -> usize {
    match b0 {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

/// A statement that could not be parsed (or not stored), as reported to a
/// [`StatementSink`] and written to the parse error log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// 1-based line where the problem was detected.
    pub line: usize,
    /// What went wrong (single line).
    pub message: String,
    /// The parts of the statement read before the error (N-Triples surface
    /// forms), for the log's `(<s> <p> <o>)` context; may be empty.
    pub context: String,
}

impl SyntaxError {
    pub fn new(line: usize, message: impl Into<String>) -> SyntaxError {
        SyntaxError {
            line,
            message: one_line(&message.into()),
            context: String::new(),
        }
    }

    pub fn with_context(mut self, context: impl Into<String>) -> SyntaxError {
        self.context = one_line(&context.into());
        self
    }

    /// The error as a [`GStoreError::RdfParse`].
    pub fn to_error(&self) -> GStoreError {
        GStoreError::RdfParse {
            line: self.line,
            msg: self.message.clone(),
        }
    }

    /// From a [`GStoreError::RdfParse`] (other errors keep their text, line 0).
    pub fn from_error(e: &GStoreError) -> SyntaxError {
        match e {
            GStoreError::RdfParse { line, msg } => SyntaxError::new(*line, msg.as_str()),
            other => SyntaxError::new(0, other.to_string()),
        }
    }
}

/// Collapse a message onto one line (the error log counts lines).
fn one_line(s: &str) -> String {
    if s.contains(['\n', '\r']) {
        s.replace(['\n', '\r'], " ")
    } else {
        s.to_string()
    }
}

/// Where a streaming reader delivers its output.
pub trait StatementSink {
    /// The quads of one complete statement, in document order. The sink may
    /// drain `quads`; the reader clears it afterwards either way.
    fn statement(&mut self, quads: &mut Vec<Quad>) -> Result<()>;

    /// The 1-based line the next [`statement`](Self::statement) was read
    /// from (where it ends), for reports about its quads that only the sink
    /// can make (upstream's literal checks). Readers that know it call this
    /// just before `statement`; the default ignores it.
    fn at_line(&mut self, _line: usize) {}

    /// A statement that failed. `Err` stops the parse with that error
    /// (strict); `Ok` skips the statement and continues (tolerant).
    fn error(&mut self, err: SyntaxError) -> Result<()>;
}

/// A strict sink collecting every quad (the plain `parse_*` APIs).
#[derive(Default)]
pub struct CollectQuads {
    pub quads: Vec<Quad>,
}

impl StatementSink for CollectQuads {
    fn statement(&mut self, quads: &mut Vec<Quad>) -> Result<()> {
        self.quads.append(quads);
        Ok(())
    }

    fn error(&mut self, err: SyntaxError) -> Result<()> {
        Err(err.to_error())
    }
}

/// Process-wide counter feeding [`unique_scope`].
static SCOPE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh blank-node scope: `b` + 16 hex digits, unique across calls and —
/// mixing the clock, the process id and a random hasher key — across
/// processes, so two loads into one database never share a scope.
pub fn unique_scope() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = RandomState::new().build_hasher();
    nanos.hash(&mut h);
    std::process::id().hash(&mut h);
    SCOPE_COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    format!("b{:016x}", h.finish())
}

/// The prefix of a minted label in keep mode. An explicit label starting with
/// it is escaped (`genid-e…`) so it can never equal a minted one.
const KEEP_MINT_PREFIX: &str = "genid-";

/// How a reader names blank nodes; see the [module docs](self).
#[derive(Debug, Clone)]
pub struct BlankLabeler {
    /// `None`: keep or verbatim mode. `Some(scope)`: scoped mode (`scope` is
    /// ASCII alphanumeric, so it is recovered unambiguously from a label).
    scope: Option<String>,
    /// Keep mode only: an extra discriminator for minted labels (see
    /// [`keep_with_suffix`](Self::keep_with_suffix)).
    suffix: String,
    /// Verbatim mode: explicit labels exactly as written, minted ones
    /// `<mint_scope>-<n>` under a fresh unique scope.
    mint_scope: Option<String>,
    next: u64,
}

impl Default for BlankLabeler {
    fn default() -> BlankLabeler {
        BlankLabeler::keep()
    }
}

impl BlankLabeler {
    /// Keep mode: explicit labels as written (one starting `genid-` escaped
    /// to `genid-e…`), minted ones `genid-<n>`. Deterministic; what the plain
    /// `parse_*` APIs use.
    pub fn keep() -> BlankLabeler {
        BlankLabeler {
            scope: None,
            suffix: String::new(),
            mint_scope: None,
            next: 0,
        }
    }

    /// Keep mode whose minted labels are `genid-<n>-<suffix>` (`suffix`
    /// reduced to ASCII alphanumerics), so two documents parsed separately
    /// into one graph mint distinct nodes.
    pub fn keep_with_suffix(suffix: &str) -> BlankLabeler {
        BlankLabeler {
            suffix: alnum(suffix),
            ..BlankLabeler::keep()
        }
    }

    /// Verbatim mode: explicit labels exactly as written (no escaping), minted
    /// ones `<fresh scope>-<n>` — so a document may name stored blank nodes
    /// (a removal file exported from the database), while its anonymous nodes
    /// match nothing stored.
    pub fn verbatim() -> BlankLabeler {
        BlankLabeler {
            mint_scope: Some(unique_scope()),
            ..BlankLabeler::keep()
        }
    }

    /// Scoped mode with a fresh [`unique_scope`].
    pub fn fresh_scope() -> BlankLabeler {
        BlankLabeler::scoped(&unique_scope())
    }

    /// Scoped mode under `scope` (reduced to ASCII alphanumerics; empty → `b`):
    /// explicit `_:x` becomes `<scope>_x`, a minted node `<scope>-<n>`.
    pub fn scoped(scope: &str) -> BlankLabeler {
        let mut s = alnum(scope);
        if s.is_empty() {
            s.push('b');
        }
        BlankLabeler {
            scope: Some(s),
            ..BlankLabeler::keep()
        }
    }

    /// Whether explicit labels of a non-minting reader stay as written (keep
    /// and verbatim modes).
    pub fn is_keep(&self) -> bool {
        self.scope.is_none()
    }

    /// The scope (scoped mode).
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// The stored label for an explicit `_:label` of a reader that also mints
    /// labels.
    pub fn explicit(&self, label: &str) -> String {
        match (&self.scope, &self.mint_scope) {
            (Some(s), _) => format!("{s}_{label}"),
            (None, Some(_)) => label.to_string(),
            (None, None) if label.starts_with(KEEP_MINT_PREFIX) => {
                format!("{KEEP_MINT_PREFIX}e{label}")
            }
            (None, None) => label.to_string(),
        }
    }

    /// The stored label for an explicit `_:label` of a reader that never mints
    /// (N-Triples / N-Quads): verbatim unless scoped.
    pub fn explicit_verbatim(&self, label: &str) -> String {
        match &self.scope {
            Some(s) => format!("{s}_{label}"),
            None => label.to_string(),
        }
    }

    /// A fresh label for an anonymous node.
    pub fn fresh(&mut self) -> String {
        let n = self.next;
        self.next += 1;
        match (&self.scope, &self.mint_scope) {
            (Some(s), _) | (None, Some(s)) => format!("{s}-{n}"),
            (None, None) if self.suffix.is_empty() => format!("{KEEP_MINT_PREFIX}{n}"),
            (None, None) => format!("{KEEP_MINT_PREFIX}{n}-{}", self.suffix),
        }
    }
}

fn alnum(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read};

    /// A reader handing out at most `chunk` bytes per `read`.
    struct Trickle<'a> {
        data: &'a [u8],
        chunk: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.chunk.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn decode_all(data: &[u8], chunk: usize) -> (String, bool) {
        let r = BufReader::with_capacity(chunk.max(1), Trickle { data, chunk });
        let mut src = CharSource::new(r);
        let mut out = String::new();
        while let Some(c) = src.bump() {
            out.push(c);
        }
        (out, src.take_bad_utf8())
    }

    #[test]
    fn multibyte_characters_split_across_refills_are_reassembled() {
        let text = "aé漢😀b\nü";
        for chunk in 1..6 {
            let (out, bad) = decode_all(text.as_bytes(), chunk);
            assert_eq!(out, text, "chunk {chunk}");
            assert!(!bad);
        }
    }

    #[test]
    fn invalid_utf8_becomes_replacement_and_is_flagged() {
        let (out, bad) = decode_all(b"a\xffb\xe6\x97", 2);
        assert_eq!(out, "a\u{FFFD}b\u{FFFD}");
        assert!(bad);
    }

    #[test]
    fn an_invalid_lead_byte_at_a_window_boundary_swallows_nothing_after_it() {
        // Latin-1 'é' (0xE6) in a literal: the bytes after it are ASCII, so
        // they must survive whatever the window size.
        let data = b"<s> <p> \"ab\xe6\" .\n<s1> <p> <o1> .\n";
        let want = "<s> <p> \"ab\" .\n<s1> <p> <o1> .\n";
        for chunk in 1..data.len() + 1 {
            let (out, bad) = decode_all(data, chunk);
            assert!(bad, "chunk {chunk}");
            assert_eq!(out.replace('\u{FFFD}', ""), want, "chunk {chunk}");
        }
    }

    #[test]
    fn lines_lookahead_and_consumed_bytes() {
        let mut src = CharSource::new("ab\ncé".as_bytes());
        assert_eq!(src.peek_at(3), Some('c'));
        assert_eq!(src.lookahead_len(), 4);
        assert_eq!(src.consumed_bytes(), 0);
        src.discard_line();
        assert_eq!(src.line(), 2);
        assert_eq!(src.consumed_bytes(), 3);
        assert_eq!(src.bump(), Some('c'));
        assert_eq!(src.bump(), Some('é'));
        assert_eq!(src.consumed_bytes(), 6);
        assert!(src.at_eof());
    }

    #[test]
    fn labeler_modes_are_collision_free_and_round_trip() {
        let mut keep = BlankLabeler::keep();
        assert_eq!(keep.explicit("b0"), "b0");
        assert_eq!(keep.fresh(), "genid-0");
        assert_eq!(keep.explicit("genid-0"), "genid-egenid-0");
        let mut sfx = BlankLabeler::keep_with_suffix("g~1");
        assert_eq!(sfx.fresh(), "genid-0-g1");
        let mut sc = BlankLabeler::scoped("S1");
        assert_eq!(sc.explicit("x"), "S1_x");
        assert_eq!(sc.fresh(), "S1-0");
        let a = unique_scope();
        let b = unique_scope();
        assert_ne!(a, b);
        assert_eq!(a.len(), 17);
        for l in [sc.fresh(), sc.explicit("a.b"), keep.fresh(), a] {
            let t = crate::parser::ntriples::parse_term(&format!("_:{l}")).unwrap();
            assert_eq!(t, crate::model::Term::Blank(l));
        }
    }
}
