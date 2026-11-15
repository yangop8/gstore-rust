//! W3C-style SPARQL 1.1 conformance subset (decision D5; review parser#18,
//! query#18, sparqlconf#18).
//!
//! The other SPARQL tests of this repository compare the engine with itself
//! (the streaming executor against the materializing one, one storage
//! layout against another, the streaming readers against frozen copies of
//! the old ones), so a bug the implementation shares everywhere passes all
//! of them. The cases under `tests/w3c/` are an oracle independent of the
//! implementation: data, a query or update and the expected answer, written
//! by hand from the text of the SPARQL 1.1 Query Language / Update
//! recommendations (each case cites the section it checks). The format is
//! documented in `tests/w3c/README.md`; in short, a case is a directory
//! `tests/w3c/<area>/<case>/` holding
//!
//! * `data.ttl` or `data.trig` (optional; the dataset, named graphs from TriG),
//! * `query.rq` (SELECT / ASK / CONSTRUCT / DESCRIBE) or `update.ru`,
//! * `expected.srj` (SPARQL 1.1 Query Results JSON, SELECT and ASK),
//!   `expected.nt` (the CONSTRUCT / DESCRIBE graph) or `expected.nq` (the
//!   whole dataset after the update, as N-Quads),
//! * `xfail` (optional): the case exposes a known engine bug; the first line
//!   names its review id. The run fails if an `xfail` case passes, so the
//!   package that fixes the bug deletes the marker.
//!
//! # What runs
//!
//! Every case runs on every storage layout — the in-memory build
//! ([`Database::build_from_files`]), the same saved as a snapshot directory
//! and opened from it ([`Database::save`] + [`Database::open`]), the on-disk
//! B+ tree store opened in Materialize mode and in Stream mode
//! ([`Database::build_disk`] + [`Database::open_with`], the mode given
//! explicitly so `GSTORE_DISK_QUERY_MODE` does not matter) and, with
//! `--features rocksdb`, RocksDB — and through three executor settings:
//!
//! * `full` — [`TopKOptions::NONE`]: the materializing evaluator (every
//!   solution, then sort / distinct / slice), the query as written;
//! * `stream` — the streaming executor with the bounded heap
//!   (`early_termination` + `bounded_heap`, DP-B off);
//! * `dpb` — the streaming executor with DP-B forced ([`DpbMode::Always`]).
//!
//! The streaming executor only runs for a query with a `LIMIT` (SELECT and
//! CONSTRUCT: early termination, or the Top-K heap / DP-B under `ORDER BY`)
//! and for `ASK` / `EXISTS`. So for `stream` and `dpb` a SELECT or
//! CONSTRUCT without a top-level `LIMIT` gets `LIMIT 1000000` appended to
//! its solution modifiers (before a trailing `VALUES` block, see
//! [`inject_limit`]) — larger than any case's answer, so it cuts nothing.
//! [`streaming_settings_take_the_streaming_paths`] checks through
//! [`EvalStats`] that this really reaches the streaming code. Aggregate
//! queries never stream: all three settings run the full path there. An
//! update runs on a fresh build for each layout and setting (`full` and
//! `stream`), and the dataset it leaves is read back with the N-Quads export
//! (not through the query engine) — on a layout with files also after the
//! database is closed and opened again (a snapshot is saved first, as it
//! persists only on save).
//!
//! # Comparison
//!
//! SELECT answers are compared as bags of solutions (variables by name),
//! ordered only when the top-level query has `ORDER BY`: then the rows must
//! come in the expected order, except that rows tied on the sort keys may
//! come in any order among themselves (ties are computed from the expected
//! rows when every key is a projected plain variable; with any other key the
//! expected order is strict, so such cases must not have ties). Graphs and
//! datasets are compared as sets. Blank nodes match up to a consistent
//! one-to-one relabelling over the whole answer (a backtracking search).
//! RDF terms compare exactly — lexical form, datatype, language tag; numbers
//! are not normalised (`"01"^^xsd:integer` is not `"1"^^xsd:integer`) — with
//! the two identities RDF 1.1 defines: a simple literal is an `xsd:string`
//! literal, and language tags compare case-insensitively.
//!
//! # Running
//!
//! `cargo test --test w3c_subset` runs everything; `W3C_FILTER=<substring>`
//! runs the cases whose `area/case` name contains the substring, and
//! `-- --nocapture` prints a line per case.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use gstore::json::Json;
use gstore::query::{DpbMode, EvalStats, Evaluator, TopKOptions};
use gstore::zipio::{write_export, ExportFormat};
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult, Term};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";

/// The `LIMIT` appended for the streaming settings (see the module docs).
const FORCED_LIMIT: usize = 1_000_000;

/// The cap on the blank-node isomorphism search, in candidate pairs tried.
const ISO_STEP_LIMIT: usize = 2_000_000;

// ---- temporary directories ------------------------------------------------

/// A directory under the system temp dir, removed when dropped — also when
/// the test fails (the drop runs while unwinding).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let tag: String = tag
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let dir = std::env::temp_dir().join(format!(
            "gstore_w3c_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create a temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- RDF terms and answers ----------------------------------------------------

/// An RDF term as the comparison sees it (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum T {
    Iri(String),
    Blank(String),
    /// Lexical form, datatype IRI, language tag (lower case; empty if none).
    Lit(String, String, String),
}

impl T {
    fn lit(lex: &str, datatype: Option<&str>, lang: Option<&str>) -> T {
        match lang.filter(|l| !l.is_empty()) {
            Some(l) => T::Lit(
                lex.to_string(),
                RDF_LANG_STRING.into(),
                l.to_ascii_lowercase(),
            ),
            None => T::Lit(
                lex.to_string(),
                datatype.unwrap_or(XSD_STRING).to_string(),
                String::new(),
            ),
        }
    }

    fn from_term(t: &Term) -> T {
        match t {
            Term::Iri(i) => T::Iri(i.clone()),
            Term::Blank(b) => T::Blank(b.clone()),
            Term::Literal {
                value,
                datatype,
                lang,
            } => T::lit(value, datatype.as_deref(), lang.as_deref()),
        }
    }

    /// A result cell of the engine (an N-Triples term).
    fn from_cell(cell: &str) -> Result<T, String> {
        gstore::parser::ntriples::parse_term(cell)
            .map(|t| T::from_term(&t))
            .map_err(|e| format!("the engine returned the cell {cell:?}, not an RDF term: {e}"))
    }

    fn is_blank(&self) -> bool {
        matches!(self, T::Blank(_))
    }
}

impl fmt::Display for T {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            T::Iri(i) => write!(f, "<{i}>"),
            T::Blank(b) => write!(f, "_:{b}"),
            T::Lit(lex, dt, lang) => {
                write!(f, "{lex:?}")?;
                if !lang.is_empty() {
                    write!(f, "@{lang}")
                } else if dt != XSD_STRING {
                    write!(f, "^^<{dt}>")
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// One solution (or one quad: subject, predicate, object, graph).
type Row = Vec<Option<T>>;

fn show_row(row: &Row) -> String {
    let cells: Vec<String> = row
        .iter()
        .map(|c| c.as_ref().map_or("UNDEF".to_string(), T::to_string))
        .collect();
    format!("({})", cells.join(" "))
}

/// An answer, expected or actual.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    /// SELECT: the variables (sorted) and the rows over them.
    Select(Vec<String>, Vec<Row>),
    Ask(bool),
    /// CONSTRUCT / DESCRIBE graph or post-update dataset: distinct quads
    /// (the graph term is `None` for the default graph).
    Quads(Vec<Row>),
}

impl Answer {
    fn describe(&self) -> String {
        match self {
            Answer::Select(vars, rows) => {
                let mut s = format!("SELECT ?{} — {} row(s)", vars.join(" ?"), rows.len());
                for r in rows {
                    s.push_str("\n      ");
                    s.push_str(&show_row(r));
                }
                s
            }
            Answer::Ask(b) => format!("ASK {b}"),
            Answer::Quads(qs) => {
                let mut s = format!("{} quad(s)", qs.len());
                for q in qs {
                    s.push_str("\n      ");
                    s.push_str(&show_row(q));
                }
                s
            }
        }
    }
}

/// `vars` sorted, with each row's cells permuted to match.
fn select_answer(vars: &[String], rows: Vec<Row>) -> Answer {
    let mut order: Vec<usize> = (0..vars.len()).collect();
    order.sort_by(|&a, &b| vars[a].cmp(&vars[b]));
    let sorted_vars = order.iter().map(|&i| vars[i].clone()).collect();
    let rows = rows
        .into_iter()
        .map(|r| order.iter().map(|&i| r[i].clone()).collect())
        .collect();
    Answer::Select(sorted_vars, rows)
}

fn quads_answer(quads: impl IntoIterator<Item = Row>) -> Answer {
    let set: BTreeSet<Row> = quads.into_iter().collect();
    Answer::Quads(set.into_iter().collect())
}

// ---- reading expected answers ----------------------------------------------

/// A SPARQL 1.1 Query Results JSON document.
fn parse_srj(text: &str) -> Result<Answer, String> {
    let doc = gstore::json::parse(text).map_err(|e| format!("expected.srj: {e}"))?;
    if let Some(b) = doc.get("boolean") {
        return b
            .as_bool()
            .map(Answer::Ask)
            .ok_or_else(|| "expected.srj: \"boolean\" is not true/false".to_string());
    }
    let vars: Vec<String> = doc
        .get("head")
        .and_then(|h| h.get("vars"))
        .and_then(Json::as_array)
        .ok_or("expected.srj: no head.vars")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or("expected.srj: a var is not a string")
        })
        .collect::<Result<_, _>>()?;
    let bindings = doc
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .ok_or("expected.srj: no results.bindings")?;
    let mut rows = Vec::new();
    for b in bindings {
        let obj = b
            .as_object()
            .ok_or("expected.srj: a binding is not an object")?;
        let mut row: Row = vec![None; vars.len()];
        for (name, term) in obj {
            let i = vars
                .iter()
                .position(|v| v == name)
                .ok_or_else(|| format!("expected.srj: ?{name} is bound but not in head.vars"))?;
            row[i] = Some(srj_term(term)?);
        }
        rows.push(row);
    }
    Ok(select_answer(&vars, rows))
}

/// One RDF term of a results document.
fn srj_term(t: &Json) -> Result<T, String> {
    let field = |k: &str| t.get(k).and_then(Json::as_str);
    let value = field("value").ok_or("expected.srj: a term has no string \"value\"")?;
    match field("type") {
        Some("uri") => Ok(T::Iri(value.to_string())),
        Some("bnode") => Ok(T::Blank(value.to_string())),
        Some("literal") | Some("typed-literal") => {
            Ok(T::lit(value, field("datatype"), field("xml:lang")))
        }
        other => Err(format!("expected.srj: unknown term type {other:?}")),
    }
}

/// N-Triples / N-Quads text as quads.
fn parse_quads(text: &str, what: &str) -> Result<Answer, String> {
    let quads = gstore::parser::nquads::parse_str(text).map_err(|e| format!("{what}: {e}"))?;
    Ok(quads_answer(quads.iter().map(|q| {
        vec![
            Some(T::from_term(&q.subject)),
            Some(T::from_term(&q.predicate)),
            Some(T::from_term(&q.object)),
            q.graph.as_ref().map(T::from_term),
        ]
    })))
}

// ---- comparison -------------------------------------------------------------

/// Whether `actual` matches `expected` (see the module docs). `ties`: for an
/// ordered SELECT, the group number of each expected row (rows of one group
/// may come in any order among themselves); `None` compares as a bag.
fn answers_match(expected: &Answer, actual: &Answer, ties: Option<&[usize]>) -> Result<(), String> {
    match (expected, actual) {
        (Answer::Ask(e), Answer::Ask(a)) if e == a => Ok(()),
        (Answer::Ask(e), Answer::Ask(a)) => Err(format!("expected ASK {e}, got {a}")),
        (Answer::Select(ev, er), Answer::Select(av, ar)) => {
            if ev != av {
                return Err(format!("variables differ: expected {ev:?}, got {av:?}"));
            }
            rows_match(er, ar, ties)
        }
        (Answer::Quads(e), Answer::Quads(a)) => rows_match(e, a, None),
        _ => Err("different kinds of answer".to_string()),
    }
}

/// [`answers_match`] over rows: a bijection between the expected and the
/// actual rows (within tie groups when ordered) under one consistent,
/// one-to-one blank-node relabelling.
fn rows_match(expected: &[Row], actual: &[Row], ties: Option<&[usize]>) -> Result<(), String> {
    if expected.len() != actual.len() {
        return Err(format!(
            "expected {} row(s), got {}",
            expected.len(),
            actual.len()
        ));
    }
    // The group of row `i`: by position, the same boundaries on both sides.
    let group = |i: usize| ties.map_or(0, |t| t[i]);
    let has_blank = |r: &Row| r.iter().flatten().any(T::is_blank);
    // Rows without blank nodes can only match an identical row: compare
    // those as multisets per group, then search over the rest.
    let mut ground: BTreeMap<(usize, &Row), isize> = BTreeMap::new();
    let (mut exp_b, mut act_b) = (Vec::new(), Vec::new());
    for (i, r) in expected.iter().enumerate() {
        if has_blank(r) {
            exp_b.push(i);
        } else {
            *ground.entry((group(i), r)).or_default() += 1;
        }
    }
    for (i, r) in actual.iter().enumerate() {
        if has_blank(r) {
            act_b.push(i);
        } else {
            *ground.entry((group(i), r)).or_default() -= 1;
        }
    }
    for ((g, row), n) in &ground {
        if *n > 0 {
            let at = if ties.is_some() {
                format!(" in tie group {g}")
            } else {
                String::new()
            };
            return Err(format!("missing{at}: {} (x{n})", show_row(row)));
        }
        if *n < 0 {
            let at = if ties.is_some() {
                format!(" in tie group {g}")
            } else {
                String::new()
            };
            return Err(format!("unexpected{at}: {} (x{})", show_row(row), -n));
        }
    }
    // Candidates of each expected row with blank nodes: the actual rows of
    // its group with the same shape (same ground terms, blank nodes where
    // it has them).
    let shape_fits = |e: &Row, a: &Row| {
        e.len() == a.len()
            && e.iter().zip(a).all(|(x, y)| match (x, y) {
                (Some(T::Blank(_)), Some(T::Blank(_))) => true,
                (x, y) => x == y,
            })
    };
    let mut cands: Vec<(usize, Vec<usize>)> = exp_b
        .iter()
        .map(|&e| {
            let c = act_b
                .iter()
                .copied()
                .filter(|&a| group(a) == group(e) && shape_fits(&expected[e], &actual[a]))
                .collect();
            (e, c)
        })
        .collect();
    // Most constrained first.
    cands.sort_by_key(|(_, c)| c.len());
    let mut search = Iso {
        expected,
        actual,
        cands: &cands,
        used: vec![false; actual.len()],
        fwd: HashMap::new(),
        back: HashMap::new(),
        steps: 0,
    };
    match search.solve(0) {
        Some(true) => Ok(()),
        Some(false) => Err(
            "no one-to-one blank-node relabelling maps the expected rows onto the actual ones"
                .to_string(),
        ),
        None => Err(format!(
            "the blank-node isomorphism search gave up after {ISO_STEP_LIMIT} steps"
        )),
    }
}

/// The backtracking search of [`rows_match`].
struct Iso<'a> {
    expected: &'a [Row],
    actual: &'a [Row],
    cands: &'a [(usize, Vec<usize>)],
    used: Vec<bool>,
    /// Expected label → actual label, and back.
    fwd: HashMap<String, String>,
    back: HashMap<String, String>,
    steps: usize,
}

impl Iso<'_> {
    /// `Some(found)`, or `None` past the step limit.
    fn solve(&mut self, k: usize) -> Option<bool> {
        let Some((e, cands)) = self.cands.get(k) else {
            return Some(true);
        };
        for &a in cands {
            if self.used[a] {
                continue;
            }
            self.steps += 1;
            if self.steps > ISO_STEP_LIMIT {
                return None;
            }
            if let Some(added) = self.bind(&self.expected[*e], &self.actual[a]) {
                self.used[a] = true;
                match self.solve(k + 1) {
                    Some(false) => {}
                    other => return other,
                }
                self.used[a] = false;
                for l in added {
                    if let Some(m) = self.fwd.remove(&l) {
                        self.back.remove(&m);
                    }
                }
            }
        }
        Some(false)
    }

    /// Extend the relabelling so expected row `e` maps onto actual row `a`;
    /// the expected labels it added, or `None` (nothing changed) if that
    /// contradicts it.
    fn bind(&mut self, e: &Row, a: &Row) -> Option<Vec<String>> {
        let mut added: Vec<String> = Vec::new();
        for (x, y) in e.iter().zip(a) {
            let (Some(T::Blank(x)), Some(T::Blank(y))) = (x, y) else {
                continue;
            };
            let ok = match (self.fwd.get(x), self.back.get(y)) {
                (Some(m), _) => m == y,
                (None, Some(_)) => false,
                (None, None) => {
                    self.fwd.insert(x.clone(), y.clone());
                    self.back.insert(y.clone(), x.clone());
                    added.push(x.clone());
                    true
                }
            };
            if !ok {
                for l in added {
                    if let Some(m) = self.fwd.remove(&l) {
                        self.back.remove(&m);
                    }
                }
                return None;
            }
        }
        Some(added)
    }
}

// ---- the query text ---------------------------------------------------------

/// A token of the light scanner below.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Var(String),
    Open,
    Close,
    LParen,
    RParen,
    Other,
}

/// What the harness needs to know about a query's text, scanned without the
/// engine's parser: its form, whether the top level has a `LIMIT`, where a
/// `LIMIT` can be inserted, and its top-level `ORDER BY` keys.
#[derive(Debug, Clone, PartialEq)]
struct QueryShape {
    /// `SELECT`, `ASK`, `CONSTRUCT` or `DESCRIBE` (upper case).
    form: String,
    has_limit: bool,
    /// Byte offset of the top-level trailing `VALUES` keyword, if any.
    values_at: Option<usize>,
    /// `None`: no top-level `ORDER BY`. `Some(keys)`: the keys, each the
    /// variable name of a plain-variable key or `None` for an expression.
    order_by: Option<Vec<Option<String>>>,
}

/// Scan `q` into tokens with their byte offsets and brace depth, skipping
/// comments, strings and IRIs (an IRI is `<` … `>` without whitespace, as
/// SPARQL's IRIREF).
fn scan(q: &str) -> Vec<(usize, usize, Tok)> {
    let b = q.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut depth) = (0usize, 0usize);
    while i < b.len() {
        let c = b[i];
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let long = b[i..].starts_with(&[c, c, c]);
                i += if long { 3 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if long && b[i..].starts_with(&[c, c, c]) {
                        i += 3;
                        break;
                    }
                    if !long && b[i] == c {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push((i, depth, Tok::Other));
            }
            b'<' => {
                let end = b[i + 1..]
                    .iter()
                    .position(|&x| x == b'>' || x <= b' ' || b"<\"{}|^`\\".contains(&x));
                match end {
                    Some(n) if b[i + 1 + n] == b'>' => i += n + 2,
                    _ => i += 1,
                }
                out.push((i, depth, Tok::Other));
            }
            b'{' => {
                out.push((i, depth, Tok::Open));
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                out.push((i, depth, Tok::Close));
                i += 1;
            }
            b'(' => {
                out.push((i, depth, Tok::LParen));
                i += 1;
            }
            b')' => {
                out.push((i, depth, Tok::RParen));
                i += 1;
            }
            b'?' | b'$' => {
                let start = i + 1;
                i = start;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push((start - 1, depth, Tok::Var(q[start..i].to_string())));
            }
            b':' => {
                // A prefixed name with the empty prefix (`:limit`).
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b"_-.".contains(&b[i])) {
                    i += 1;
                }
                out.push((i, depth, Tok::Other));
            }
            c if c.is_ascii_alphabetic() => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b"_-:.".contains(&b[i])) {
                    i += 1;
                }
                let w = &q[start..i];
                // A prefixed name (`ex:a`) is not a keyword.
                let tok = if w.contains(':') {
                    Tok::Other
                } else {
                    Tok::Word(w.trim_end_matches('.').to_ascii_uppercase())
                };
                out.push((start, depth, tok));
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                out.push((i, depth, Tok::Other));
                i += 1;
            }
        }
    }
    out
}

fn query_shape(q: &str) -> Result<QueryShape, String> {
    let toks = scan(q);
    let form = toks
        .iter()
        .find_map(|(_, d, t)| match t {
            Tok::Word(w)
                if *d == 0 && ["SELECT", "ASK", "CONSTRUCT", "DESCRIBE"].contains(&w.as_str()) =>
            {
                Some(w.clone())
            }
            _ => None,
        })
        .ok_or("query.rq: no SELECT / ASK / CONSTRUCT / DESCRIBE")?;
    // A top-level `VALUES` keyword can only open the trailing VALUES
    // clause (inline VALUES sit inside a group). The solution modifiers lie
    // between the WHERE clause's closing `}` — the last top-level `}`
    // before that clause — and the clause (or the end).
    let values_pos = toks
        .iter()
        .position(|(_, d, t)| *d == 0 && *t == Tok::Word("VALUES".into()));
    let values_at = values_pos.map(|p| toks[p].0);
    let end = values_pos.unwrap_or(toks.len());
    let start = toks[..end]
        .iter()
        .rposition(|(_, d, t)| *d == 0 && *t == Tok::Close)
        .map_or(0, |p| p + 1);
    let tail = &toks[start..end];
    let has_limit = tail.iter().any(|(_, _, t)| *t == Tok::Word("LIMIT".into()));
    let mut order_by = None;
    if let Some(p) = tail
        .iter()
        .position(|(_, _, t)| *t == Tok::Word("ORDER".into()))
    {
        let mut keys = Vec::new();
        let mut rest = tail[p + 1..].iter().map(|(_, _, t)| t).peekable();
        if rest.next() != Some(&Tok::Word("BY".into())) {
            return Err("query.rq: ORDER without BY".into());
        }
        loop {
            match rest.peek() {
                None => break,
                Some(Tok::Word(w)) if ["LIMIT", "OFFSET", "VALUES"].contains(&w.as_str()) => break,
                Some(Tok::Var(v)) => {
                    keys.push(Some(v.clone()));
                    rest.next();
                }
                Some(Tok::Word(w)) if w == "ASC" || w == "DESC" => {
                    rest.next();
                    // `ASC(?v)` is a plain-variable key; anything else in
                    // the parentheses is an expression.
                    let mut inner = Vec::new();
                    let mut depth = 0usize;
                    for t in rest.by_ref() {
                        match t {
                            Tok::LParen => depth += 1,
                            Tok::RParen => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            t => inner.push(t.clone()),
                        }
                    }
                    keys.push(match inner.as_slice() {
                        [Tok::Var(v)] => Some(v.clone()),
                        _ => None,
                    });
                }
                Some(_) => {
                    // An expression key: `( expr )` or a call; skip it.
                    let mut depth = 0usize;
                    for t in rest.by_ref() {
                        match t {
                            Tok::LParen => depth += 1,
                            Tok::RParen => {
                                depth = depth.saturating_sub(1);
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    keys.push(None);
                }
            }
        }
        order_by = Some(keys);
    }
    Ok(QueryShape {
        form,
        has_limit,
        values_at,
        order_by,
    })
}

/// `q` with `LIMIT n` added to its top-level solution modifiers: before a
/// trailing `VALUES` block, else at the end (on a line of its own, so a
/// trailing comment cannot swallow it).
fn inject_limit(q: &str, shape: &QueryShape, n: usize) -> String {
    match shape.values_at {
        Some(at) => format!("{}\nLIMIT {n}\n{}", &q[..at], &q[at..]),
        None => format!("{q}\nLIMIT {n}\n"),
    }
}

/// The tie group of each expected row of an ordered SELECT (see
/// [`answers_match`]); every row its own group unless every key is a
/// projected variable.
fn tie_groups(vars: &[String], rows: &[Row], keys: &[Option<String>]) -> Vec<usize> {
    let cols: Option<Vec<usize>> = keys
        .iter()
        .map(|k| k.as_ref().and_then(|v| vars.iter().position(|x| x == v)))
        .collect();
    let Some(cols) = cols else {
        return (0..rows.len()).collect();
    };
    let mut groups = Vec::with_capacity(rows.len());
    let mut g = 0;
    for (i, r) in rows.iter().enumerate() {
        if i > 0 && cols.iter().any(|&c| r[c] != rows[i - 1][c]) {
            g += 1;
        }
        groups.push(g);
    }
    groups
}

// ---- cases --------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Action {
    Query(String),
    Update(String),
}

#[derive(Debug, Clone)]
struct Case {
    /// `area/case`.
    name: String,
    data: Option<PathBuf>,
    action: Action,
    expected: Answer,
    /// The review id named by the `xfail` marker.
    xfail: Option<String>,
}

fn cases_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/w3c")
}

/// Every case directory under `root` (`<area>/<case>/`), sorted.
fn discover(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut areas: Vec<PathBuf> = fs::read_dir(root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    for area in areas {
        let mut cases: Vec<PathBuf> = fs::read_dir(&area)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        cases.sort();
        dirs.extend(cases);
    }
    dirs
}

/// Read and check case directory `dir`.
fn load_case(dir: &Path) -> Result<Case, String> {
    let name = format!(
        "{}/{}",
        dir.parent()
            .and_then(Path::file_name)
            .unwrap_or_default()
            .to_string_lossy(),
        dir.file_name().unwrap_or_default().to_string_lossy()
    );
    let known = [
        "data.ttl",
        "data.trig",
        "query.rq",
        "update.ru",
        "expected.srj",
        "expected.nt",
        "expected.nq",
        "xfail",
    ];
    for e in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let f = e.map_err(|e| e.to_string())?.file_name();
        let f = f.to_string_lossy();
        if !known.contains(&f.as_ref()) {
            return Err(format!("unknown file '{f}' (see tests/w3c/README.md)"));
        }
    }
    let read = |f: &str| fs::read_to_string(dir.join(f)).map_err(|e| format!("{f}: {e}"));
    let has = |f: &str| dir.join(f).is_file();
    let data = match (has("data.ttl"), has("data.trig")) {
        (true, true) => return Err("both data.ttl and data.trig".into()),
        (true, false) => Some(dir.join("data.ttl")),
        (false, true) => Some(dir.join("data.trig")),
        (false, false) => None,
    };
    let (action, text) = match (has("query.rq"), has("update.ru")) {
        (true, false) => {
            let q = read("query.rq")?;
            (Action::Query(q.clone()), q)
        }
        (false, true) => {
            let u = read("update.ru")?;
            (Action::Update(u.clone()), u)
        }
        _ => return Err("needs exactly one of query.rq / update.ru".into()),
    };
    if !text
        .lines()
        .any(|l| l.trim_start().starts_with("# spec:") && l.len() > "# spec:".len() + 3)
    {
        return Err("the query/update must cite its spec section in a '# spec: …' line".into());
    }
    let expected_files: Vec<&str> = ["expected.srj", "expected.nt", "expected.nq"]
        .into_iter()
        .filter(|f| has(f))
        .collect();
    let expected = match (&action, expected_files.as_slice()) {
        (Action::Query(q), ["expected.srj"]) => {
            let shape = query_shape(q)?;
            let a = parse_srj(&read("expected.srj")?)?;
            match (&a, shape.form.as_str()) {
                (Answer::Select(..), "SELECT") | (Answer::Ask(_), "ASK") => a,
                _ => return Err(format!("expected.srj does not fit a {} query", shape.form)),
            }
        }
        (Action::Query(q), ["expected.nt"]) => {
            let shape = query_shape(q)?;
            if shape.form != "CONSTRUCT" && shape.form != "DESCRIBE" {
                return Err(format!("expected.nt does not fit a {} query", shape.form));
            }
            let a = parse_quads(&read("expected.nt")?, "expected.nt")?;
            if let Answer::Quads(qs) = &a {
                if qs.iter().any(|q| q[3].is_some()) {
                    return Err("expected.nt holds a quad".into());
                }
            }
            a
        }
        (Action::Update(_), ["expected.nq"]) => parse_quads(&read("expected.nq")?, "expected.nq")?,
        _ => {
            return Err(
                "needs exactly one expected file: expected.srj (SELECT/ASK) or expected.nt \
                 (CONSTRUCT/DESCRIBE) for query.rq, expected.nq for update.ru"
                    .into(),
            )
        }
    };
    let xfail = if has("xfail") {
        let id = read("xfail")?
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let well_formed = id.split_once('#').is_some_and(|(area, n)| {
            !area.is_empty()
                && area.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !n.is_empty()
                && n.chars().take_while(char::is_ascii_digit).count() > 0
        });
        if !well_formed {
            return Err(format!(
                "the first line of xfail must name a review id such as query#2, found {id:?}"
            ));
        }
        Some(id)
    } else {
        None
    };
    Ok(Case {
        name,
        data,
        action,
        expected,
        xfail,
    })
}

// ---- layouts and executor settings -------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Built in memory, never saved.
    Memory,
    /// Built in memory, saved as a snapshot directory and opened from it.
    Snapshot,
    DiskMaterialize,
    DiskStream,
    #[cfg(feature = "rocksdb")]
    RocksDb,
}

impl Layout {
    fn all() -> Vec<Layout> {
        #[allow(unused_mut)]
        let mut v = vec![
            Layout::Memory,
            Layout::Snapshot,
            Layout::DiskMaterialize,
            Layout::DiskStream,
        ];
        #[cfg(feature = "rocksdb")]
        v.push(Layout::RocksDb);
        v
    }

    fn name(self) -> &'static str {
        match self {
            Layout::Memory => "memory",
            Layout::Snapshot => "snapshot",
            Layout::DiskMaterialize => "disk/materialize",
            Layout::DiskStream => "disk/stream",
            #[cfg(feature = "rocksdb")]
            Layout::RocksDb => "rocksdb",
        }
    }

    /// Where the layout keeps its files under a run's scratch directory
    /// (`None`: memory only). Materialize and Stream share one build.
    fn dir(self, scratch: &Path) -> Option<PathBuf> {
        match self {
            Layout::Memory => None,
            Layout::Snapshot => Some(scratch.join("snapshot")),
            Layout::DiskMaterialize | Layout::DiskStream => Some(scratch.join("disk")),
            #[cfg(feature = "rocksdb")]
            Layout::RocksDb => Some(scratch.join("rocks")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exec {
    Full,
    Stream,
    Dpb,
}

impl Exec {
    fn name(self) -> &'static str {
        match self {
            Exec::Full => "full",
            Exec::Stream => "stream",
            Exec::Dpb => "dpb",
        }
    }

    fn options(self) -> TopKOptions {
        match self {
            Exec::Full => TopKOptions::NONE,
            Exec::Stream => TopKOptions {
                early_termination: true,
                bounded_heap: true,
                dpb: DpbMode::Off,
            },
            Exec::Dpb => TopKOptions {
                early_termination: true,
                bounded_heap: true,
                dpb: DpbMode::Always,
            },
        }
    }

    /// The query text this setting runs (see the module docs).
    fn query_text(self, q: &str, shape: &QueryShape) -> String {
        let streams = matches!(shape.form.as_str(), "SELECT" | "CONSTRUCT");
        if self != Exec::Full && streams && !shape.has_limit {
            inject_limit(q, shape, FORCED_LIMIT)
        } else {
            q.to_string()
        }
    }
}

/// The data file of `case`, or an empty Turtle file in `scratch`.
fn data_file(case: &Case, scratch: &Path) -> PathBuf {
    case.data.clone().unwrap_or_else(|| {
        let p = scratch.join("empty.ttl");
        fs::write(&p, "").unwrap();
        p
    })
}

/// The database of `layout` under `scratch`, built from `data` on first use
/// and opened (again, on later calls) the way that layout opens.
fn open_layout(layout: Layout, data: &Path, scratch: &Path) -> Result<Database, String> {
    let fail = |what: &str, e: gstore::GStoreError| format!("{what}: {e}");
    let dir = layout.dir(scratch);
    let mut db = match (layout, dir.as_deref()) {
        (Layout::Memory, _) | (_, None) => {
            Database::build_from_files("w3c", &[data]).map_err(|e| fail("memory build", e))?
        }
        (Layout::Snapshot, Some(dir)) => {
            if !dir.exists() {
                Database::build_from_files("w3c", &[data])
                    .and_then(|db| db.save(dir))
                    .map_err(|e| fail("snapshot build", e))?;
            }
            Database::open(dir).map_err(|e| fail("snapshot open", e))?
        }
        (Layout::DiskMaterialize | Layout::DiskStream, Some(dir)) => {
            let mode = if layout == Layout::DiskStream {
                DiskQueryMode::Stream
            } else {
                DiskQueryMode::Materialize
            };
            if !Database::is_disk(dir) {
                Database::build_disk(dir, &[data]).map_err(|e| fail("disk build", e))?;
            }
            let db = Database::open_with(dir, &OpenOptions::new().with_disk_query_mode(mode))
                .map_err(|e| fail("disk open", e))?;
            if db.disk_query_mode() != Some(mode) {
                return Err(format!(
                    "disk open: mode {:?}, not {mode:?}",
                    db.disk_query_mode()
                ));
            }
            db
        }
        #[cfg(feature = "rocksdb")]
        (Layout::RocksDb, Some(dir)) => {
            if !Database::is_rocksdb(dir) {
                drop(
                    Database::build_rocksdb_from_files("w3c", dir, &[data])
                        .map_err(|e| fail("rocksdb build", e))?,
                );
            }
            Database::open_rocksdb(dir).map_err(|e| fail("rocksdb open", e))?
        }
    };
    // The cache is keyed by the query text only: it would answer the second
    // setting with the first one's result.
    db.set_query_cache_size(0);
    Ok(db)
}

/// Run update `u` on a fresh database of `layout` with `exec`'s settings,
/// then check the dataset it leaves — and, for a layout with files, the
/// dataset the database has after it is closed and opened again (a
/// snapshot is saved first: it persists only on save).
fn run_update(case: &Case, u: &str, layout: Layout, exec: Exec, data: &Path) -> Result<(), String> {
    let fresh = TempDir::new(&format!("{}_{}", case.name, exec.name()));
    let check = |db: &Database, when: &str| -> Result<(), String> {
        let a = dataset_of(db)?;
        answers_match(&case.expected, &a, None).map_err(|why| {
            format!(
                "{when}{why}\n    expected: {}\n    actual:   {}",
                case.expected.describe(),
                a.describe()
            )
        })
    };
    let mut db = open_layout(layout, data, fresh.path())?;
    db.set_topk_options(exec.options());
    match db.query(u) {
        Ok(QueryResult::Update { .. }) => {}
        Ok(other) => return Err(format!("not an update result: {other:?}")),
        Err(e) => return Err(format!("update failed: {e}")),
    }
    check(&db, "")?;
    let Some(dir) = layout.dir(fresh.path()) else {
        return Ok(());
    };
    if layout == Layout::Snapshot {
        db.save(&dir)
            .map_err(|e| format!("save after the update: {e}"))?;
    }
    drop(db);
    let db = open_layout(layout, data, fresh.path())?;
    check(&db, "after reopening: ")
}

/// The engine's answer as an [`Answer`].
fn answer_of(r: QueryResult) -> Result<Answer, String> {
    match r {
        QueryResult::Select(rs) => {
            let rows = rs
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| c.as_deref().map(T::from_cell).transpose())
                        .collect::<Result<Row, String>>()
                })
                .collect::<Result<Vec<Row>, String>>()?;
            Ok(select_answer(&rs.vars, rows))
        }
        QueryResult::Ask(b) => Ok(Answer::Ask(b)),
        QueryResult::Construct(ts) => Ok(quads_answer(ts.iter().map(|t| {
            vec![
                Some(T::from_term(&t.subject)),
                Some(T::from_term(&t.predicate)),
                Some(T::from_term(&t.object)),
                None,
            ]
        }))),
        QueryResult::Update { changed } => Err(format!("an update result (changed {changed})")),
    }
}

/// The whole dataset of `db`, read through the N-Quads export.
fn dataset_of(db: &Database) -> Result<Answer, String> {
    let mut out = Vec::new();
    write_export(db, ExportFormat::NQuads, &mut out).map_err(|e| format!("export: {e}"))?;
    let text = String::from_utf8(out).map_err(|e| format!("export: {e}"))?;
    parse_quads(&text, "the export")
}

/// One run that did not give the expected answer.
#[derive(Debug, Clone)]
struct Miss {
    layout: &'static str,
    exec: &'static str,
    why: String,
}

/// Run `case` everywhere; the runs that missed (empty: the case passes).
fn run_case(case: &Case) -> Vec<Miss> {
    let scratch = TempDir::new(&case.name);
    let data = data_file(case, scratch.path());
    let mut misses = Vec::new();
    for layout in Layout::all() {
        match &case.action {
            Action::Query(q) => {
                let shape = match query_shape(q) {
                    Ok(s) => s,
                    Err(e) => {
                        misses.push(Miss {
                            layout: layout.name(),
                            exec: "-",
                            why: e,
                        });
                        continue;
                    }
                };
                let ties = match (&case.expected, &shape.order_by) {
                    (Answer::Select(vars, rows), Some(keys)) => Some(tie_groups(vars, rows, keys)),
                    _ => None,
                };
                let mut db = match open_layout(layout, &data, scratch.path()) {
                    Ok(db) => db,
                    Err(why) => {
                        misses.push(Miss {
                            layout: layout.name(),
                            exec: "-",
                            why,
                        });
                        continue;
                    }
                };
                for exec in [Exec::Full, Exec::Stream, Exec::Dpb] {
                    db.set_topk_options(exec.options());
                    let text = exec.query_text(q, &shape);
                    let got = db
                        .query(&text)
                        .map_err(|e| format!("query failed: {e}"))
                        .and_then(answer_of);
                    let verdict = got.and_then(|a| {
                        answers_match(&case.expected, &a, ties.as_deref()).map_err(|why| {
                            format!(
                                "{why}\n    expected: {}\n    actual:   {}",
                                case.expected.describe(),
                                a.describe()
                            )
                        })
                    });
                    if let Err(why) = verdict {
                        misses.push(Miss {
                            layout: layout.name(),
                            exec: exec.name(),
                            why,
                        });
                    }
                }
            }
            Action::Update(u) => {
                for exec in [Exec::Full, Exec::Stream] {
                    if let Err(why) = run_update(case, u, layout, exec, &data) {
                        misses.push(Miss {
                            layout: layout.name(),
                            exec: exec.name(),
                            why,
                        });
                    }
                }
            }
        }
    }
    misses
}

/// The verdict on a case: `Ok(line)` when it behaves as marked (passes, or
/// misses while marked `xfail`), `Err(report)` otherwise — a miss without
/// the marker, or an `xfail` case that passes everywhere.
fn verdict(case: &Case) -> Result<String, String> {
    let misses = run_case(case);
    let first = |m: &[Miss]| {
        m.first()
            .map(|m| format!("[{} {}] {}", m.layout, m.exec, m.why))
            .unwrap_or_default()
    };
    match (&case.xfail, misses.is_empty()) {
        (None, true) => Ok(format!("PASS  {}", case.name)),
        (Some(id), false) => Ok(format!(
            "XFAIL {} ({id}; {} run(s) miss): {}",
            case.name,
            misses.len(),
            first(&misses).lines().next().unwrap_or("")
        )),
        (Some(id), true) => Err(format!(
            "XPASS {}: marked xfail ({id}) but it passes on every layout and executor; \
             the bug is fixed, so delete {}/xfail",
            case.name, case.name
        )),
        (None, false) => {
            let mut s = format!("FAIL  {} ({} run(s) miss)", case.name, misses.len());
            for m in &misses {
                s.push_str(&format!("\n  [{} {}] {}", m.layout, m.exec, m.why));
            }
            Err(s)
        }
    }
}

/// Run `cases` on a few threads; the verdicts in case order.
fn run_all(cases: &[Case]) -> Vec<Result<String, String>> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<Result<String, String>>>> = Mutex::new(vec![None; cases.len()]);
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 4);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(case) = cases.get(i) else {
                    break;
                };
                let v = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| verdict(case)))
                    .unwrap_or_else(|p| {
                        let msg = p
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default();
                        Err(format!("FAIL  {}: panicked: {msg}", case.name))
                    });
                out.lock().unwrap()[i] = Some(v);
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect()
}

// ---- the suite ------------------------------------------------------------------

#[test]
fn w3c_subset() {
    let filter = std::env::var("W3C_FILTER").unwrap_or_default();
    let dirs = discover(&cases_root());
    let mut cases = Vec::new();
    let mut broken = Vec::new();
    for dir in &dirs {
        match load_case(dir) {
            Ok(c) if c.name.contains(&filter) => cases.push(c),
            Ok(_) => {}
            Err(e) => broken.push(format!("BAD   {}: {e}", dir.display())),
        }
    }
    assert!(broken.is_empty(), "malformed cases:\n{}", broken.join("\n"));
    if filter.is_empty() {
        assert!(
            cases.len() >= 40,
            "the subset should hold at least 40 cases, found {}",
            cases.len()
        );
    }
    let verdicts = run_all(&cases);
    let (mut pass, mut xfail) = (0, 0);
    let mut failures = Vec::new();
    for v in &verdicts {
        match v {
            Ok(line) => {
                println!("{line}");
                if line.starts_with("PASS") {
                    pass += 1;
                } else {
                    xfail += 1;
                }
            }
            Err(report) => {
                println!("{report}");
                failures.push(report.clone());
            }
        }
    }
    println!(
        "w3c subset: {} case(s), {pass} pass, {xfail} xfail, {} failure(s); layouts {:?}",
        cases.len(),
        failures.len(),
        Layout::all().iter().map(|l| l.name()).collect::<Vec<_>>()
    );
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

// ---- self-tests of the harness ---------------------------------------------------

/// Write a case directory with `files` under a temp dir; returns the guard
/// and the case directory (`selftest/<name>`).
fn scratch_case(name: &str, files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let root = TempDir::new(name);
    let dir = root.path().join("selftest").join(name);
    fs::create_dir_all(&dir).unwrap();
    for (f, text) in files {
        fs::write(dir.join(f), text).unwrap();
    }
    (root, dir)
}

const SELF_DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n:a :p :c .\n";
const SELF_QUERY: &str = "# spec: SPARQL 1.1 Query §2.1\nPREFIX : <http://ex/>\n\
                          SELECT ?o WHERE { :a :p ?o }\n";
const SELF_RIGHT: &str = r#"{"head":{"vars":["o"]},"results":{"bindings":[
    {"o":{"type":"uri","value":"http://ex/b"}},
    {"o":{"type":"uri","value":"http://ex/c"}}]}}"#;
const SELF_WRONG_ROW: &str = r#"{"head":{"vars":["o"]},"results":{"bindings":[
    {"o":{"type":"uri","value":"http://ex/b"}},
    {"o":{"type":"uri","value":"http://ex/d"}}]}}"#;
const SELF_WRONG_COUNT: &str = r#"{"head":{"vars":["o"]},"results":{"bindings":[
    {"o":{"type":"uri","value":"http://ex/b"}}]}}"#;

#[test]
fn a_correct_expectation_passes_everywhere() {
    let (_g, dir) = scratch_case(
        "right",
        &[
            ("data.ttl", SELF_DATA),
            ("query.rq", SELF_QUERY),
            ("expected.srj", SELF_RIGHT),
        ],
    );
    let case = load_case(&dir).unwrap();
    let misses = run_case(&case);
    assert!(misses.is_empty(), "{misses:?}");
    assert!(verdict(&case).unwrap().starts_with("PASS"));
}

#[test]
fn a_wrong_expectation_fails_on_every_layout_and_executor() {
    for (tag, wrong) in [
        ("wrong_row", SELF_WRONG_ROW),
        ("wrong_count", SELF_WRONG_COUNT),
    ] {
        let (_g, dir) = scratch_case(
            tag,
            &[
                ("data.ttl", SELF_DATA),
                ("query.rq", SELF_QUERY),
                ("expected.srj", wrong),
            ],
        );
        let case = load_case(&dir).unwrap();
        let misses = run_case(&case);
        assert_eq!(
            misses.len(),
            Layout::all().len() * 3,
            "every layout x executor must miss: {misses:?}"
        );
        let report = verdict(&case).unwrap_err();
        assert!(report.starts_with("FAIL"), "{report}");
    }
}

#[test]
fn a_wrong_typed_literal_fails() {
    // Exact terms: the right number with the wrong datatype or lexical form
    // is a miss.
    let data = "@prefix : <http://ex/> .\n:a :n 7 .\n";
    let query = "# spec: SPARQL 1.1 Query §2.3.2\nPREFIX : <http://ex/>\nSELECT ?n { :a :n ?n }\n";
    let int = "http://www.w3.org/2001/XMLSchema#integer";
    let dec = "http://www.w3.org/2001/XMLSchema#decimal";
    for (lex, dt, ok) in [
        ("7", int, true),
        ("07", int, false),
        ("7", dec, false),
        ("7.0", dec, false),
    ] {
        let expected = format!(
            r#"{{"head":{{"vars":["n"]}},"results":{{"bindings":[{{"n":{{"type":"literal","value":"{lex}","datatype":"{dt}"}}}}]}}}}"#
        );
        let (_g, dir) = scratch_case(
            "typed",
            &[
                ("data.ttl", data),
                ("query.rq", query),
                ("expected.srj", &expected),
            ],
        );
        let case = load_case(&dir).unwrap();
        assert_eq!(run_case(&case).is_empty(), ok, "{lex}^^{dt}");
    }
}

#[test]
fn an_unexpectedly_passing_xfail_case_fails_the_run() {
    let (_g, dir) = scratch_case(
        "xpass",
        &[
            ("data.ttl", SELF_DATA),
            ("query.rq", SELF_QUERY),
            ("expected.srj", SELF_RIGHT),
            (
                "xfail",
                "query#999\nthis case passes, so the marker is stale\n",
            ),
        ],
    );
    let case = load_case(&dir).unwrap();
    assert_eq!(case.xfail.as_deref(), Some("query#999"));
    let report = verdict(&case).unwrap_err();
    assert!(report.starts_with("XPASS"), "{report}");
    // ... and the suite's driver reports it as a failure.
    let all = run_all(std::slice::from_ref(&case));
    assert!(all[0].is_err());
}

#[test]
fn a_failing_xfail_case_is_accepted() {
    let (_g, dir) = scratch_case(
        "xfail",
        &[
            ("data.ttl", SELF_DATA),
            ("query.rq", SELF_QUERY),
            ("expected.srj", SELF_WRONG_ROW),
            ("xfail", "query#999\n"),
        ],
    );
    let case = load_case(&dir).unwrap();
    let line = verdict(&case).unwrap();
    assert!(line.starts_with("XFAIL"), "{line}");
}

#[test]
fn malformed_cases_are_rejected() {
    /// A case name, its files, and a word the rejection must contain.
    type Bad<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
    let bad: &[Bad] = &[
        (
            "no_spec",
            &[
                ("query.rq", "ASK {}"),
                ("expected.srj", r#"{"head":{},"boolean":true}"#),
            ],
            "spec",
        ),
        (
            "no_expected",
            &[("query.rq", "# spec: SPARQL 1.1 Query §16.3\nASK {}")],
            "expected",
        ),
        (
            "wrong_kind",
            &[
                ("query.rq", "# spec: SPARQL 1.1 Query §16.3\nASK {}"),
                ("expected.nt", ""),
            ],
            "expected.nt",
        ),
        (
            "bad_xfail",
            &[
                ("query.rq", "# spec: SPARQL 1.1 Query §16.3\nASK {}"),
                ("expected.srj", r#"{"head":{},"boolean":true}"#),
                ("xfail", "some bug\n"),
            ],
            "review id",
        ),
        (
            "stray_file",
            &[
                ("query.rq", "# spec: SPARQL 1.1 Query §16.3\nASK {}"),
                ("expected.srj", r#"{"head":{},"boolean":true}"#),
                ("notes.txt", ""),
            ],
            "unknown file",
        ),
    ];
    for (name, files, needle) in bad {
        let (_g, dir) = scratch_case(name, files);
        let err = load_case(&dir).unwrap_err();
        assert!(err.contains(needle), "{name}: {err}");
    }
}

fn b(l: &str) -> Option<T> {
    Some(T::Blank(l.into()))
}
fn i(l: &str) -> Option<T> {
    Some(T::Iri(format!("http://ex/{l}")))
}

#[test]
fn blank_nodes_match_up_to_a_consistent_one_to_one_relabelling() {
    let ok = |e: Vec<Row>, a: Vec<Row>| rows_match(&e, &a, None).is_ok();
    // A relabelling.
    assert!(ok(
        vec![vec![b("x"), i("p"), b("y")]],
        vec![vec![b("1"), i("p"), b("2")]]
    ));
    // One node in two places stays one node.
    assert!(!ok(
        vec![vec![b("x"), i("p"), b("x")]],
        vec![vec![b("1"), i("p"), b("2")]]
    ));
    assert!(!ok(
        vec![vec![b("x"), i("p"), b("y")]],
        vec![vec![b("1"), i("p"), b("1")]]
    ));
    // Consistent across rows ...
    assert!(ok(
        vec![vec![b("x"), i("p"), i("o1")], vec![b("x"), i("p"), i("o2")]],
        vec![vec![b("7"), i("p"), i("o2")], vec![b("7"), i("p"), i("o1")]],
    ));
    assert!(!ok(
        vec![vec![b("x"), i("p"), i("o1")], vec![b("x"), i("p"), i("o2")]],
        vec![vec![b("7"), i("p"), i("o1")], vec![b("8"), i("p"), i("o2")]],
    ));
    // ... and one-to-one.
    assert!(!ok(
        vec![vec![b("x"), i("p"), i("o1")], vec![b("y"), i("p"), i("o2")]],
        vec![vec![b("7"), i("p"), i("o1")], vec![b("7"), i("p"), i("o2")]],
    ));
    // A blank node never matches an IRI, and bags count duplicates.
    assert!(!ok(vec![vec![b("x")]], vec![vec![i("x")]]));
    assert!(!ok(vec![vec![i("a")], vec![i("a")]], vec![vec![i("a")]]));
    assert!(!ok(
        vec![vec![i("a")], vec![i("a")], vec![i("b")]],
        vec![vec![i("a")], vec![i("b")], vec![i("b")]]
    ));
    assert!(ok(
        vec![vec![i("a")], vec![i("a")], vec![i("b")]],
        vec![vec![i("b")], vec![i("a")], vec![i("a")]]
    ));
    // Unbound is a value of its own.
    assert!(!ok(vec![vec![None]], vec![vec![i("a")]]));
    assert!(ok(vec![vec![None, i("a")]], vec![vec![None, i("a")]]));
}

#[test]
fn blank_node_matching_backtracks() {
    // A cycle x -> y -> z -> x against a relabelled cycle listed in another
    // order: the first candidate the search tries for the first row is wrong
    // for some later row, so it must undo it.
    let e = vec![
        vec![b("x"), i("p"), b("y")],
        vec![b("y"), i("p"), b("z")],
        vec![b("z"), i("p"), b("x")],
        vec![b("x"), i("q"), i("mark")],
    ];
    let a = vec![
        vec![b("c"), i("p"), b("a")],
        vec![b("a"), i("p"), b("b")],
        vec![b("b"), i("p"), b("c")],
        vec![b("b"), i("q"), i("mark")],
    ];
    assert!(rows_match(&e, &a, None).is_ok());
    // Two cycles of length 2 are not one cycle of length 4.
    let four = vec![
        vec![b("a"), i("p"), b("b")],
        vec![b("b"), i("p"), b("c")],
        vec![b("c"), i("p"), b("d")],
        vec![b("d"), i("p"), b("a")],
    ];
    let two_twos = vec![
        vec![b("a"), i("p"), b("b")],
        vec![b("b"), i("p"), b("a")],
        vec![b("c"), i("p"), b("d")],
        vec![b("d"), i("p"), b("c")],
    ];
    assert!(rows_match(&four, &two_twos, None).is_err());
}

#[test]
fn ordered_answers_compare_in_order_with_ties_as_bags() {
    let rows =
        |v: &[(&str, &str)]| -> Vec<Row> { v.iter().map(|(k, x)| vec![i(k), i(x)]).collect() };
    let vars = vec!["k".to_string(), "x".to_string()];
    let expected = rows(&[("1", "a"), ("2", "b"), ("2", "c"), ("3", "d")]);
    let keys = vec![Some("k".to_string())];
    let ties = tie_groups(&vars, &expected, &keys);
    assert_eq!(ties, vec![0, 1, 1, 2]);
    // Tied rows in either order.
    assert!(rows_match(
        &expected,
        &rows(&[("1", "a"), ("2", "c"), ("2", "b"), ("3", "d")]),
        Some(&ties)
    )
    .is_ok());
    // Out of order across keys.
    assert!(rows_match(
        &expected,
        &rows(&[("2", "b"), ("1", "a"), ("2", "c"), ("3", "d")]),
        Some(&ties)
    )
    .is_err());
    // An expression key makes the order strict.
    let strict = tie_groups(&vars, &expected, &[None]);
    assert_eq!(strict, vec![0, 1, 2, 3]);
    assert!(rows_match(
        &expected,
        &rows(&[("1", "a"), ("2", "c"), ("2", "b"), ("3", "d")]),
        Some(&strict)
    )
    .is_err());
}

#[test]
fn rdf_1_1_term_identities() {
    // A simple literal is an xsd:string literal; language tags ignore case.
    assert_eq!(
        T::from_cell("\"a\"").unwrap(),
        T::from_cell(&format!("\"a\"^^<{XSD_STRING}>")).unwrap()
    );
    assert_eq!(
        T::from_cell("\"a\"@EN").unwrap(),
        T::from_cell("\"a\"@en").unwrap()
    );
    // Nothing else is normalised.
    let int = "http://www.w3.org/2001/XMLSchema#integer";
    assert_ne!(
        T::from_cell(&format!("\"01\"^^<{int}>")).unwrap(),
        T::from_cell(&format!("\"1\"^^<{int}>")).unwrap()
    );
    assert_ne!(
        T::from_cell("\"a\"@en").unwrap(),
        T::from_cell("\"a\"").unwrap()
    );
}

#[test]
fn the_scanner_finds_the_top_level_modifiers() {
    let s = query_shape(
        "PREFIX ex: <http://ex/> # LIMIT in a comment\n\
         SELECT ?a WHERE { ?a ex:p \"LIMIT ORDER BY\" { SELECT ?a WHERE { ?a ?p ?o } ORDER BY ?o LIMIT 3 } }\n\
         ORDER BY DESC(?a) ?b (?c + 1) STRLEN(?d) ASC(?e)",
    )
    .unwrap();
    assert_eq!(s.form, "SELECT");
    assert!(!s.has_limit);
    assert_eq!(s.values_at, None);
    assert_eq!(
        s.order_by,
        Some(vec![
            Some("a".into()),
            Some("b".into()),
            None,
            None,
            Some("e".into())
        ])
    );
    let q = "SELECT * { ?s ?p ?o } OFFSET 1 VALUES ?s { <http://ex/a> } # done";
    let s = query_shape(q).unwrap();
    assert!(!s.has_limit);
    assert_eq!(s.order_by, None);
    let injected = inject_limit(q, &s, 7);
    assert_eq!(
        injected,
        "SELECT * { ?s ?p ?o } OFFSET 1 \nLIMIT 7\nVALUES ?s { <http://ex/a> } # done"
    );
    gstore::parser::sparql::parse(&injected).unwrap();
    let q = "ASK { ?s ?p ?o FILTER(?o < 3 && ?o > 1) } # trailing comment";
    let s = query_shape(q).unwrap();
    assert_eq!(s.form, "ASK");
    let injected = inject_limit(q, &s, 7);
    assert!(
        injected.ends_with("# trailing comment\nLIMIT 7\n"),
        "{injected}"
    );
    assert!(
        query_shape("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o } LIMIT 2")
            .unwrap()
            .has_limit
    );
}

/// The work counters of `q` (as `exec` runs it) over the in-memory build of
/// `data`.
fn stats_of(data: &str, q: &str, exec: Exec) -> EvalStats {
    let db = Database::build_from_str("stats", data).unwrap();
    let shape = query_shape(q).unwrap();
    let query = gstore::parser::sparql::parse(&exec.query_text(q, &shape)).unwrap();
    let ev = Evaluator::new(db.dict(), db.store()).with_topk_options(exec.options());
    ev.evaluate(&query).unwrap();
    ev.stats()
}

#[test]
fn streaming_settings_take_the_streaming_paths() {
    let data = "@prefix : <http://ex/> .\n:a :p 1 . :b :p 2 . :c :p 3 . :a :q :b .\n";
    let plain = "PREFIX : <http://ex/> SELECT ?s WHERE { ?s :p ?o }";
    let ordered = "PREFIX : <http://ex/> SELECT ?s WHERE { ?s :p ?o } ORDER BY DESC(?o)";
    let ask = "PREFIX : <http://ex/> ASK { ?s :p ?o }";
    let construct = "PREFIX : <http://ex/> CONSTRUCT { ?s :r ?o } WHERE { ?s :p ?o }";
    // The materializing evaluator never streams ...
    for q in [plain, ordered, ask, construct] {
        let s = stats_of(data, q, Exec::Full);
        assert_eq!(
            (s.streamed, s.heap_topk, s.dpb_topk),
            (0, 0, 0),
            "{q}: {s:?}"
        );
    }
    // ... the streaming settings do, through the injected LIMIT.
    for q in [plain, ask, construct] {
        for exec in [Exec::Stream, Exec::Dpb] {
            assert!(stats_of(data, q, exec).streamed > 0, "{q} ({exec:?})");
        }
    }
    assert!(stats_of(data, ordered, Exec::Stream).heap_topk > 0);
    let dpb = stats_of(data, ordered, Exec::Dpb);
    assert!(dpb.dpb_topk + dpb.heap_topk > 0, "{dpb:?}");
}
