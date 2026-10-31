//! Bulk RDF import: formats, sources, tolerant parsing and the parse error log.
//!
//! Every bulk-load path — [`Database::build_from_files`](crate::Database::build_from_files)
//! and its siblings, the `--disk` / `--rocksdb` builds, `gadd` / `gsub`,
//! `/api` `build` / `batchInsert` / `batchRemove` and SPARQL `LOAD` — reads RDF
//! through this module:
//!
//! 1. A [`Source`] (a file, every RDF file under a directory, or an
//!    `http(s)://` URL the [`LoadPolicy`] allows) is opened and its
//!    [`RdfFormat`] chosen by extension (`.nt .nq .ttl .trig .rdf/.xml/.owl
//!    .jsonld .html`, unknown → Turtle; `x.nt.gz` with the `gzip` feature).
//! 2. N-Triples, N-Quads, Turtle and TriG are parsed *streaming* (see
//!    [`crate::parser::stream`]): memory holds a read window of
//!    [`ImportOptions::window`] bytes and one statement, never the file.
//!    RDF/XML, JSON-LD and RDFa are parsed as a whole document, refused above
//!    [`ImportOptions::max_document_bytes`]; so is a URL body.
//! 3. Each statement is checked — the subject an IRI or blank node, the
//!    predicate an IRI, a graph label an IRI or blank node — and, with
//!    [`ImportOptions::upstream_checks`], its object literal the way upstream
//!    gStore's `RDFParser::parseFile` checks it (`xsd:integer`/`long`/`int`/
//!    `short`/`byte` must parse in range, `xsd:double`/`float` must parse and
//!    not be `NaN`).
//! 4. Valid quads reach a [`QuadSink`] in batches of
//!    [`ImportOptions::batch_size`].
//!
//! # Strict and tolerant
//!
//! A *strict* import stops at the first statement that fails (the library
//! default, `gbuild --strict`). A *tolerant* one (upstream's behaviour: `/api`
//! `build` / `batchInsert` / `batchRemove`, and `gbuild` / `gadd` / `gsub` by
//! default) skips the statement, counts it in [`ImportReport::failed`] — the
//! `failed_num` of the `/api` answers — reports it to an [`ErrorLog`] and goes
//! on (Turtle resynchronises at the next line, like upstream's
//! `TurtleParser::discardLine`). A document parsed as a whole (RDF/XML,
//! JSON-LD, RDFa) that does not parse counts as one failed statement.
//!
//! # The parse error log
//!
//! [`ParseErrorLog`] writes upstream's `parse_error.log` format, appending to
//! `<database directory>/parse_error.log` as upstream's `Database::build`,
//! `batch_insert` and `batch_remove` do: one `Info` line per source read —
//! `Info <YYYY-mm-dd HH:MM:SS> <build|batch insert|batch remove> parser info,
//! file path <path>` — and one line per failed statement — `Line <n>
//! (<what was read of the statement>): <message>`. Upstream derives
//! `failed_num` from the log's line count minus the `Info` lines; every entry
//! here is a single line, so the count and the log agree.
//!
//! # Blank nodes
//!
//! With [`BlankNodes::PerDocument`] (the default) every source gets a fresh
//! blank-node scope ([`BlankLabeler::fresh_scope`]): `_:b` in two files, or in
//! two `gadd` / `batchInsert` calls, are two nodes. (Upstream keeps labels
//! verbatim and numbers anonymous nodes from `_:_0` per parse, so a second
//! batch insert silently merges with the first.) Generated labels use only
//! `[A-Za-z0-9_-]` and round-trip through
//! [`parse_term`](crate::parser::ntriples::parse_term) and the exporters.
//!
//! # Named graphs
//!
//! N-Quads and TriG statements keep their graph. A blank-node graph label is
//! stored as the IRI [`BNODE_GRAPH_PREFIX`]`<label>` (named graphs are keyed
//! by IRI).

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::db::{LoadPolicy, LoadSource};
use crate::error::{GStoreError, Result};
use crate::model::Term;
use crate::parser::nquads::Quad;
use crate::parser::stream::{BlankLabeler, StatementSink, SyntaxError, DEFAULT_WINDOW};
use crate::parser::{jsonld, nquads, ntriples, rdfa, rdfxml, trig, turtle};

/// The IRI prefix a blank-node graph label is stored under.
pub const BNODE_GRAPH_PREFIX: &str = "urn:gstore:bnode:";

/// Default limit for documents parsed whole (RDF/XML, JSON-LD, RDFa) and for
/// URL bodies: 256 MiB.
pub const DEFAULT_MAX_DOCUMENT_BYTES: u64 = 256 << 20;

/// Default number of quads handed to a [`QuadSink`] at once.
pub const DEFAULT_BATCH_SIZE: usize = 64 * 1024;

/// An RDF serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RdfFormat {
    NTriples,
    NQuads,
    Turtle,
    TriG,
    RdfXml,
    JsonLd,
    Rdfa,
}

impl RdfFormat {
    /// The format an extension names (case-insensitive), `None` when it is
    /// not an RDF extension. `.nt`, `.n3` and `.txt` are read as Turtle (a
    /// superset of N-Triples: a `.nt` file with `@prefix` lines still loads,
    /// as upstream's single Turtle-based parser reads every file).
    pub fn from_extension(ext: &str) -> Option<RdfFormat> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "nt" | "ntriples" | "n3" | "txt" | "ttl" | "turtle" => RdfFormat::Turtle,
            "nq" | "nquads" => RdfFormat::NQuads,
            "trig" => RdfFormat::TriG,
            "rdf" | "xml" | "owl" | "rdfs" => RdfFormat::RdfXml,
            "jsonld" | "json" => RdfFormat::JsonLd,
            "html" | "xhtml" | "htm" => RdfFormat::Rdfa,
            _ => return None,
        })
    }

    /// The format of a file name (`None` = no RDF extension) and whether it
    /// is gzip-compressed (`x.nt.gz`).
    pub fn detect(name: &str) -> (Option<RdfFormat>, bool) {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let (stem, gz) = match base.rsplit_once('.') {
            Some((stem, ext)) if ext.eq_ignore_ascii_case("gz") => (stem, true),
            _ => (base, false),
        };
        let fmt = stem
            .rsplit_once('.')
            .and_then(|(_, ext)| RdfFormat::from_extension(ext));
        (fmt, gz)
    }

    /// The format to read `name` with: by extension, unknown → Turtle.
    pub fn for_name(name: &str) -> RdfFormat {
        RdfFormat::detect(name).0.unwrap_or(RdfFormat::Turtle)
    }

    /// Whether the format can carry named graphs (N-Quads, TriG).
    pub fn has_graphs(self) -> bool {
        matches!(self, RdfFormat::NQuads | RdfFormat::TriG)
    }

    /// Whether the reader streams (bounded memory) rather than parsing the
    /// whole document.
    pub fn is_streaming(self) -> bool {
        matches!(
            self,
            RdfFormat::NTriples | RdfFormat::NQuads | RdfFormat::Turtle | RdfFormat::TriG
        )
    }
}

/// Whether `name` is something a directory import picks up: an RDF extension,
/// or `<rdf ext>.gz` when the `gzip` feature is on.
pub fn is_rdf_file_name(name: &str) -> bool {
    match RdfFormat::detect(name) {
        (Some(_), false) => true,
        (Some(_), true) => cfg!(feature = "gzip"),
        (None, _) => false,
    }
}

/// How blank-node labels are scoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlankNodes {
    /// Labels as written; anonymous nodes `genid-<n>` (per document). See
    /// [`BlankLabeler::keep`].
    Keep,
    /// Labels exactly as written; anonymous nodes under a fresh unique scope
    /// (they match nothing stored). For removals: `gsub` / `batchRemove`
    /// with a file naming stored blank nodes (e.g. an export) removes them.
    Verbatim,
    /// A fresh scope per source document (the default for loads).
    PerDocument,
}

/// How an import reads and checks its input.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Stop at the first failed statement (`true`) or skip and count it.
    pub strict: bool,
    pub blank_nodes: BlankNodes,
    /// Apply upstream's numeric literal checks (see the module docs).
    pub upstream_checks: bool,
    /// Largest document parsed whole (RDF/XML, JSON-LD, RDFa) or URL body.
    pub max_document_bytes: u64,
    /// Quads per [`QuadSink`] call.
    pub batch_size: usize,
    /// The read window: the buffer size of the reader over a file.
    pub window: usize,
}

impl Default for ImportOptions {
    fn default() -> ImportOptions {
        ImportOptions::strict()
    }
}

impl ImportOptions {
    /// Stop at the first failure (the library default).
    pub fn strict() -> ImportOptions {
        ImportOptions {
            strict: true,
            blank_nodes: BlankNodes::PerDocument,
            upstream_checks: false,
            max_document_bytes: DEFAULT_MAX_DOCUMENT_BYTES,
            batch_size: DEFAULT_BATCH_SIZE,
            window: DEFAULT_WINDOW,
        }
    }

    /// Upstream's behaviour: skip, count and log failed statements, with its
    /// literal checks.
    pub fn tolerant() -> ImportOptions {
        ImportOptions {
            strict: false,
            upstream_checks: true,
            ..ImportOptions::strict()
        }
    }

    /// These options for a removal: blank-node labels read
    /// [verbatim](BlankNodes::Verbatim).
    pub fn for_removal(mut self) -> ImportOptions {
        self.blank_nodes = BlankNodes::Verbatim;
        self
    }

    fn labeler(&self) -> BlankLabeler {
        match self.blank_nodes {
            BlankNodes::Keep => BlankLabeler::keep(),
            BlankNodes::Verbatim => BlankLabeler::verbatim(),
            BlankNodes::PerDocument => BlankLabeler::fresh_scope(),
        }
    }
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Quads delivered to the sink (valid statements, duplicates included).
    pub statements: u64,
    /// Of those, quads in a named graph.
    pub named: u64,
    /// Statements skipped (tolerant mode): upstream's `failed_num`.
    pub failed: u64,
    /// Source documents read.
    pub documents: u64,
}

impl ImportReport {
    /// Add another report's counts.
    pub fn absorb(&mut self, other: &ImportReport) {
        self.statements += other.statements;
        self.named += other.named;
        self.failed += other.failed;
        self.documents += other.documents;
    }
}

/// Receives valid quads in batches (drain `batch`; it is cleared anyway).
pub trait QuadSink {
    fn quads(&mut self, batch: &mut Vec<Quad>) -> Result<()>;
}

impl<F: FnMut(&mut Vec<Quad>) -> Result<()>> QuadSink for F {
    fn quads(&mut self, batch: &mut Vec<Quad>) -> Result<()> {
        self(batch)
    }
}

/// Receives the statements a tolerant import skipped.
pub trait ErrorLog {
    /// A source is about to be read (`operation`: `build`, `batch insert`, …).
    fn begin(&mut self, _operation: &str, _source: &str) {}
    /// A statement of `source` failed.
    fn failed(&mut self, source: &str, err: &SyntaxError);
}

/// No log.
impl ErrorLog for () {
    fn failed(&mut self, _source: &str, _err: &SyntaxError) {}
}

/// Collects the failures (tests, callers that report them themselves).
impl ErrorLog for Vec<(String, SyntaxError)> {
    fn failed(&mut self, source: &str, err: &SyntaxError) {
        self.push((source.to_string(), err.clone()));
    }
}

/// Upstream's `parse_error.log` (see the module docs), appended to.
pub struct ParseErrorLog {
    path: PathBuf,
    file: File,
}

/// The file name upstream gives the log inside a database directory.
pub const PARSE_ERROR_LOG: &str = "parse_error.log";

impl ParseErrorLog {
    /// Open (create) `path` for appending.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<ParseErrorLog> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(ParseErrorLog { path, file })
    }

    /// Open `<db_dir>/parse_error.log`.
    pub fn in_dir(db_dir: &Path) -> io::Result<ParseErrorLog> {
        ParseErrorLog::open(db_dir.join(PARSE_ERROR_LOG))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ErrorLog for ParseErrorLog {
    fn begin(&mut self, operation: &str, source: &str) {
        let now = crate::sysdb::clock::now().datetime();
        // Best effort, like upstream (a log that cannot be written does not
        // fail the load).
        let _ = writeln!(
            self.file,
            "Info {now} {operation} parser info, file path {source}"
        );
    }

    fn failed(&mut self, _source: &str, err: &SyntaxError) {
        let _ = writeln!(
            self.file,
            "Line {} ({}): {}",
            err.line, err.context, err.message
        );
    }
}

/// Count the failure lines of a `parse_error.log` (lines that are not `Info`
/// lines) — what upstream's `failed_num` arithmetic amounts to.
pub fn count_logged_failures(path: &Path) -> u64 {
    fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty() && !l.starts_with("Info "))
                .count() as u64
        })
        .unwrap_or(0)
}

/// An input to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Url(String),
}

impl Source {
    /// How the source is named in logs and errors.
    pub fn name(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Url(u) => u.clone(),
        }
    }
}

/// Whether `s` is an `http://` / `https://` URL.
pub fn is_url(s: &str) -> bool {
    let l = s.trim_start();
    l.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || l.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

/// Expand inputs into sources, checking each against `policy`: an
/// `http(s)://` URL stays a URL (refused unless the policy allows remote
/// sources); a directory becomes every RDF file below it — recursively,
/// sorted by path like the file list of an upstream zip package, hidden
/// entries and symlinked directories skipped; anything else is a file
/// (`file:` IRIs accepted).
pub fn expand_sources<S: AsRef<str>>(inputs: &[S], policy: &LoadPolicy) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for input in inputs {
        let input = input.as_ref();
        if is_url(input) {
            match policy.resolve(input)? {
                LoadSource::Remote(u) => out.push(Source::Url(u)),
                LoadSource::Local(p) => out.push(Source::File(p)),
            }
            continue;
        }
        if Path::new(input).is_dir() {
            let dir = policy.allowed_local_dir(Path::new(input)).ok_or_else(|| {
                GStoreError::Query(format!(
                    "directory '{input}' is not allowed by the load policy"
                ))
            })?;
            let mut files = Vec::new();
            walk_rdf_files(&dir, &mut files)?;
            files.sort();
            out.extend(files.into_iter().map(Source::File));
            continue;
        }
        match policy.resolve(input)? {
            LoadSource::Local(p) => out.push(Source::File(p)),
            LoadSource::Remote(u) => out.push(Source::Url(u)),
        }
    }
    Ok(out)
}

/// Every RDF file below `dir` (see [`expand_sources`]).
pub fn walk_rdf_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type()?;
        let path = entry.path();
        if ft.is_dir() {
            walk_rdf_files(&path, out)?;
        } else if (ft.is_file() || (ft.is_symlink() && path.is_file())) && is_rdf_file_name(&name) {
            out.push(path);
        }
    }
    Ok(())
}

/// The validating, batching [`StatementSink`] between a reader and a
/// [`QuadSink`].
struct Pipeline<'a> {
    opts: &'a ImportOptions,
    source: &'a str,
    log: &'a mut dyn ErrorLog,
    sink: &'a mut dyn QuadSink,
    batch: Vec<Quad>,
    report: ImportReport,
}

impl Pipeline<'_> {
    fn fail(&mut self, err: SyntaxError) -> Result<()> {
        if self.opts.strict {
            return Err(err.to_error());
        }
        self.report.failed += 1;
        self.log.failed(self.source, &err);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.batch.is_empty() {
            self.sink.quads(&mut self.batch)?;
            self.batch.clear();
        }
        Ok(())
    }
}

impl StatementSink for Pipeline<'_> {
    fn statement(&mut self, quads: &mut Vec<Quad>) -> Result<()> {
        for mut q in quads.drain(..) {
            if let Some(msg) = invalid_quad(&q, self.opts.upstream_checks) {
                let err = SyntaxError::new(0, msg).with_context(format!(
                    "{} {} {}",
                    q.subject, q.predicate, q.object
                ));
                self.fail(err)?;
                continue;
            }
            if let Some(Term::Blank(l)) = &q.graph {
                q.graph = Some(Term::Iri(format!("{BNODE_GRAPH_PREFIX}{l}")));
            }
            if q.graph.is_some() {
                self.report.named += 1;
            }
            self.report.statements += 1;
            self.batch.push(q);
            if self.batch.len() >= self.opts.batch_size.max(1) {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn error(&mut self, err: SyntaxError) -> Result<()> {
        self.fail(err)
    }
}

/// Why a parsed quad cannot be stored (`None` = it can).
fn invalid_quad(q: &Quad, upstream_checks: bool) -> Option<String> {
    if !matches!(q.subject, Term::Iri(_) | Term::Blank(_)) {
        return Some("invalid subject".into());
    }
    if !matches!(q.predicate, Term::Iri(_)) {
        return Some("invalid predicate".into());
    }
    if matches!(q.graph, Some(Term::Literal { .. })) {
        return Some("invalid graph label".into());
    }
    if upstream_checks {
        if let Some(msg) = upstream_literal_check(&q.object) {
            return Some(msg.into());
        }
    }
    None
}

/// Upstream `RDFParser::parseFile`'s object checks: `None` when the object
/// passes, else its message.
pub fn upstream_literal_check(object: &Term) -> Option<&'static str> {
    let Term::Literal {
        value,
        datatype: Some(dt),
        ..
    } = object
    else {
        return None;
    };
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let local = dt.strip_prefix(XSD)?;
    let int_check = |min: i64, max: i64, invalid, range| match c_stoll(value) {
        None => Some(invalid),
        Some(None) => Some(range),
        Some(Some(v)) if v < min || v > max => Some(range),
        Some(Some(_)) => None,
    };
    match local {
        "integer" => int_check(
            i64::MIN,
            i64::MAX,
            "Object integer value invalid",
            "Object integer out of range",
        ),
        "long" => int_check(
            i64::MIN,
            i64::MAX,
            "Object long value invalid",
            "Object long value out of range",
        ),
        "int" => int_check(
            i32::MIN.into(),
            i32::MAX.into(),
            "Object int value invalid",
            "Object int value out of range",
        ),
        "short" => int_check(
            i16::MIN.into(),
            i16::MAX.into(),
            "Object short value invalid",
            "Object short value out of range",
        ),
        "byte" => int_check(
            i8::MIN.into(),
            i8::MAX.into(),
            "Object byte value invalid",
            "Object byte value out of range",
        ),
        "double" | "float" => {
            let (invalid, nan) = if local == "double" {
                ("Object double value invalid", "Object double value is NaN")
            } else {
                ("Object float value invalid", "Object float value is NaN")
            };
            if !c_strtod_ok(value, local == "float") {
                Some(invalid)
            } else if value.len() == 3 && value.eq_ignore_ascii_case("nan") {
                Some(nan)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// C `std::stoll`: leading whitespace, a sign, then base-10 digits (trailing
/// text ignored). `None` = no digits (invalid_argument); `Some(None)` =
/// out of range.
fn c_stoll(s: &str) -> Option<Option<i64>> {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: &str = &rest[..rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len())];
    if digits.is_empty() {
        return None;
    }
    let text = if neg {
        format!("-{digits}")
    } else {
        digits.to_string()
    };
    Some(text.parse::<i64>().ok())
}

/// C `std::stod` / `std::stof` succeed: a numeric prefix (decimal with
/// optional exponent, hex, `inf`/`infinity`/`nan`) that is in range.
fn c_strtod_ok(s: &str, float: bool) -> bool {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let lower = body.to_ascii_lowercase();
    if lower.starts_with("inf") || lower.starts_with("nan") {
        return true;
    }
    if (lower.starts_with("0x"))
        && lower[2..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_hexdigit() || c == '.')
    {
        return true;
    }
    // The longest decimal prefix: digits [. digits] [e [sign] digits].
    let b = body.as_bytes();
    let mut i = 0;
    let mut mantissa_digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        mantissa_digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            mantissa_digits += 1;
        }
    }
    if mantissa_digits == 0 {
        return false;
    }
    let mut end = i;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            end = j;
        }
    }
    let Ok(v) = body[..end].parse::<f64>() else {
        return false;
    };
    // Out of range (std::out_of_range) counts as a failure here.
    if v.is_infinite() || (float && (v.abs() > f64::from(f32::MAX))) {
        return false;
    }
    true
}

/// Read a whole document of at most `max` bytes.
fn read_bounded<R: Read>(mut reader: R, max: u64, what: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader.by_ref().take(max.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(GStoreError::RdfParse {
            line: 0,
            msg: format!(
                "{what} is larger than {max} bytes, the limit for documents parsed whole"
            ),
        });
    }
    Ok(buf)
}

/// Parse `reader` as `format` into `sink` (one statement at a time).
pub fn parse_into<R: BufRead>(
    reader: R,
    format: RdfFormat,
    labels: BlankLabeler,
    max_document_bytes: u64,
    sink: &mut dyn StatementSink,
) -> Result<()> {
    match format {
        RdfFormat::NTriples => ntriples::parse_reader(reader, &labels, sink),
        RdfFormat::NQuads => nquads::parse_reader(reader, &labels, sink),
        RdfFormat::Turtle => turtle::parse_reader(reader, labels, sink),
        RdfFormat::TriG => trig::parse_reader(reader, labels, sink),
        RdfFormat::RdfXml | RdfFormat::JsonLd | RdfFormat::Rdfa => {
            let what = match format {
                RdfFormat::RdfXml => "an RDF/XML document",
                RdfFormat::JsonLd => "a JSON-LD document",
                _ => "an RDFa document",
            };
            let parsed = read_bounded(reader, max_document_bytes, what).and_then(|bytes| {
                let text = String::from_utf8(bytes).map_err(|_| GStoreError::RdfParse {
                    line: 0,
                    msg: "invalid UTF-8".into(),
                })?;
                let mut labels = labels;
                match format {
                    RdfFormat::RdfXml => rdfxml::parse_str_with(&text, &mut labels),
                    RdfFormat::JsonLd => jsonld::parse_str_with(&text, &mut labels),
                    _ => rdfa::parse_str_with(&text, &mut labels),
                }
            });
            match parsed {
                Ok(triples) => {
                    let mut one = Vec::with_capacity(1);
                    for t in triples {
                        one.push(Quad::new(t.subject, t.predicate, t.object, None));
                        sink.statement(&mut one)?;
                        one.clear();
                    }
                    Ok(())
                }
                Err(GStoreError::Io(e)) => Err(GStoreError::Io(e)),
                Err(e) => sink.error(SyntaxError::from_error(&e)),
            }
        }
    }
}

/// Import one document from `reader` (named `name` in logs) as `format`.
pub fn import_reader<R: BufRead>(
    reader: R,
    format: RdfFormat,
    name: &str,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    let mut pipe = Pipeline {
        opts,
        source: name,
        log,
        sink,
        batch: Vec::with_capacity(opts.batch_size.clamp(1, DEFAULT_BATCH_SIZE)),
        report: ImportReport {
            documents: 1,
            ..ImportReport::default()
        },
    };
    parse_into(reader, format, opts.labeler(), opts.max_document_bytes, &mut pipe)?;
    pipe.flush()?;
    Ok(pipe.report)
}

/// Open a local file for import: its reader (decompressed for `.gz`) and
/// format.
pub fn open_file(path: &Path, opts: &ImportOptions) -> Result<(Box<dyn BufRead>, RdfFormat)> {
    let name = path.to_string_lossy();
    let (fmt, gz) = RdfFormat::detect(&name);
    let fmt = fmt.unwrap_or(RdfFormat::Turtle);
    let file = File::open(path)?;
    if gz {
        #[cfg(feature = "gzip")]
        {
            let dec = flate2::read::GzDecoder::new(BufReader::with_capacity(opts.window, file));
            return Ok((Box::new(BufReader::with_capacity(opts.window, dec)), fmt));
        }
        #[cfg(not(feature = "gzip"))]
        {
            let _ = file;
            return Err(GStoreError::Database(format!(
                "'{name}': gzip-compressed input needs the `gzip` feature"
            )));
        }
    }
    Ok((Box::new(BufReader::with_capacity(opts.window, file)), fmt))
}

/// Fetch an `http(s)://` URL (at most `max` bytes, through
/// [`crate::http_client`]). Errors on a transport failure or a non-2xx
/// status.
pub fn fetch_url(url: &str, max: u64) -> Result<Vec<u8>> {
    let opts = crate::http_client::PostOptions {
        limits: crate::http_client::Limits {
            max_response: usize::try_from(max).unwrap_or(usize::MAX).saturating_add(64 << 10),
            ..crate::http_client::Limits::default()
        },
        ..crate::http_client::PostOptions::default()
    };
    // The response cap covers the headers too; the body is checked below.
    let reply = crate::http_client::get_with(
        url,
        "text/turtle, application/n-triples, application/n-quads, application/trig, \
         application/rdf+xml;q=0.9, application/ld+json;q=0.9, */*;q=0.1",
        &opts,
    )
    .map_err(|e| GStoreError::Database(format!("cannot download '{url}': {e}")))?;
    if !reply.is_success() {
        return Err(GStoreError::Database(format!(
            "cannot download '{url}': HTTP {}",
            reply.status
        )));
    }
    if reply.body.len() as u64 > max {
        return Err(GStoreError::Database(format!(
            "'{url}' is larger than {max} bytes"
        )));
    }
    Ok(reply.body)
}

/// The path part of a URL (no query or fragment), for format detection.
pub fn url_path(url: &str) -> &str {
    let u = url.split(['?', '#']).next().unwrap_or(url);
    match u.find("://") {
        Some(i) => match u[i + 3..].find('/') {
            Some(j) => &u[i + 3 + j..],
            None => "",
        },
        None => u,
    }
}

/// Import one [`Source`], announcing it to `log` as `operation`.
pub fn import_source(
    source: &Source,
    operation: &str,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    let name = source.name();
    log.begin(operation, &name);
    match source {
        Source::File(path) => {
            let (reader, fmt) = open_file(path, opts)?;
            import_reader(reader, fmt, &name, opts, log, sink)
        }
        Source::Url(url) => {
            let body = fetch_url(url, opts.max_document_bytes)?;
            let path = url_path(url);
            let (fmt, gz) = RdfFormat::detect(path);
            let fmt = fmt.unwrap_or(RdfFormat::Turtle);
            if gz {
                #[cfg(feature = "gzip")]
                {
                    let dec = flate2::read::GzDecoder::new(body.as_slice());
                    let reader = BufReader::with_capacity(opts.window, dec);
                    return import_reader(reader, fmt, &name, opts, log, sink);
                }
                #[cfg(not(feature = "gzip"))]
                return Err(GStoreError::Database(format!(
                    "'{url}': gzip-compressed input needs the `gzip` feature"
                )));
            }
            import_reader(body.as_slice(), fmt, &name, opts, log, sink)
        }
    }
}

/// Import several sources in order into one sink; reports are summed.
pub fn import_sources(
    sources: &[Source],
    operation: &str,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    let mut total = ImportReport::default();
    for s in sources {
        let r = import_source(s, operation, opts, log, sink)?;
        total.absorb(&r);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_by_extension() {
        assert_eq!(RdfFormat::detect("a/b.NT"), (Some(RdfFormat::Turtle), false));
        assert_eq!(RdfFormat::detect("x.nq"), (Some(RdfFormat::NQuads), false));
        assert_eq!(RdfFormat::detect("x.trig.gz"), (Some(RdfFormat::TriG), true));
        assert_eq!(RdfFormat::detect("x.owl"), (Some(RdfFormat::RdfXml), false));
        assert_eq!(RdfFormat::detect("x.jsonld"), (Some(RdfFormat::JsonLd), false));
        assert_eq!(RdfFormat::detect("x.html"), (Some(RdfFormat::Rdfa), false));
        assert_eq!(RdfFormat::detect("README"), (None, false));
        assert_eq!(RdfFormat::detect("x.gz"), (None, true));
        assert_eq!(RdfFormat::for_name("data"), RdfFormat::Turtle);
        assert!(is_rdf_file_name("a.ttl"));
        assert!(!is_rdf_file_name("a.md"));
        assert_eq!(is_rdf_file_name("a.nt.gz"), cfg!(feature = "gzip"));
    }

    #[test]
    fn upstream_literal_checks_follow_stoll_and_stod() {
        let lit = |v: &str, dt: &str| Term::typed_literal(v, format!("http://www.w3.org/2001/XMLSchema#{dt}"));
        assert_eq!(upstream_literal_check(&lit("42", "integer")), None);
        assert_eq!(upstream_literal_check(&lit(" 12abc", "integer")), None);
        assert_eq!(
            upstream_literal_check(&lit("abc", "integer")),
            Some("Object integer value invalid")
        );
        assert_eq!(
            upstream_literal_check(&lit("99999999999999999999", "integer")),
            Some("Object integer out of range")
        );
        assert_eq!(
            upstream_literal_check(&lit("40000", "short")),
            Some("Object short value out of range")
        );
        assert_eq!(upstream_literal_check(&lit("127", "byte")), None);
        assert_eq!(
            upstream_literal_check(&lit("128", "byte")),
            Some("Object byte value out of range")
        );
        assert_eq!(
            upstream_literal_check(&lit("3000000000", "int")),
            Some("Object int value out of range")
        );
        assert_eq!(upstream_literal_check(&lit("1.5e3", "double")), None);
        assert_eq!(
            upstream_literal_check(&lit("NaN", "double")),
            Some("Object double value is NaN")
        );
        assert_eq!(
            upstream_literal_check(&lit("x1", "double")),
            Some("Object double value invalid")
        );
        assert_eq!(
            upstream_literal_check(&lit("1e999", "double")),
            Some("Object double value invalid")
        );
        assert_eq!(
            upstream_literal_check(&lit("nan", "float")),
            Some("Object float value is NaN")
        );
        assert_eq!(upstream_literal_check(&lit("abc", "string")), None);
        assert_eq!(upstream_literal_check(&Term::plain_literal("abc")), None);
    }

    #[test]
    fn url_paths() {
        assert_eq!(url_path("http://h:1/a/b.ttl?x=1#f"), "/a/b.ttl");
        assert_eq!(url_path("https://h"), "");
        assert!(is_url("HTTP://x"));
        assert!(!is_url("file:///x"));
    }
}
