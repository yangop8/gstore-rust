//! Data test: upstream gStore's `scripts/test/parser_test.sh` suite — 50 SPARQL
//! cases — run in-process (GAP_PLAN §8, the Phase 3 acceptance gate).
//!
//! The 50 cases are vendored verbatim under `tests/data/upstream/parser_test/`
//! (`parser_d<N>.ttl` data, `parser_q<N>.sql` query, `parser_r<N>.txt`
//! upstream's printed result). For every case this test does what the
//! upstream script does over `/api`, but against an in-memory [`Database`]:
//!
//! 1. **build** — import `parser_d<N>.ttl` with the crate's normal
//!    format-by-extension loader ([`Database::build_from_files`]) and compare
//!    the imported triple count with the script's `success_expected[N]`;
//! 2. **query** — run `parser_q<N>.sql` and compare the answer count with the
//!    script's `ans_expected[N]` (upstream's `AnsNum`);
//! 3. **rows** — additionally compare the actual result rows with
//!    `parser_r<N>.txt`: upstream's ASCII table is parsed (column widths are
//!    ignored), every cell is read back as an RDF term and the two tables are
//!    compared as multisets of rows (in order when the query has an
//!    `ORDER BY`). ASK results are upstream's one-row `?_askResult` table; the
//!    graph-function cases print the aggregated JSON document instead of a
//!    table and are compared as JSON values (object keys unordered).
//!
//! W3C SPARQL 1.1 semantics are the oracle. Where the script's `ans_expected`
//! and the result file disagree, or upstream deviates from W3C, the engine
//! follows the spec and the case's expectation is overridden in the single
//! [`DIVERGENCES`] table below, with the reason. Cases that could only fail
//! because of RDF-import tolerance / Turtle leniency (Phase 5, GAP_PLAN §4.2)
//! would be listed in [`DEFERRED_TO_PHASE5`]; the list is currently empty —
//! every vendored data file imports cleanly.
//!
//! Run it with the per-case table printed (the in-process counterpart of
//! upstream's `bash scripts/test/parser_test.sh`):
//!
//! ```text
//! cargo test --test dt_upstream_parser_suite -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use gstore::parser::ntriples::parse_term;
use gstore::{Database, QueryResult, Term};

/// Number of upstream cases (`TOTAL_CASE` of `parser_test.sh`).
const TOTAL_CASE: usize = 50;

/// `ans_expected` of upstream `parser_test.sh` (index = case number; index 0
/// unused). Upstream's `AnsNum` of each query.
const ANS_EXPECTED: [i64; TOTAL_CASE + 1] = [
    0, //
    2, 0, 1, 1, 1, 1, 1, 1, 3, 2, // 1..=10
    2, 4, 4, 2, 0, 1, 0, 1, 2, 1, // 11..=20
    1, 1, 1, 2, 1, 1, 1, 1, 1, 1, // 21..=30
    1, 3, 1, 1, 1, 1, 3, 1, 1, 1, // 31..=40
    1, 1, 1, 1, 1, 1, 1, 8, 8, 7, // 41..=50
];

/// `success_expected` of upstream `parser_test.sh` (index = case number):
/// the `success_num` (triples imported) of each `build`.
const SUCCESS_EXPECTED: [u64; TOTAL_CASE + 1] = [
    0, //
    5, 3, 3, 3, 3, 4, 4, 4, 6, 4, // 1..=10
    4, 6, 6, 6, 10, 1, 1, 6, 6, 4, // 11..=20
    4, 3, 3, 4, 4, 4, 4, 3, 3, 2, // 21..=30
    3, 9, 15, 15, 15, 4, 4, 4, 4, 4, // 31..=40
    4, 4, 4, 4, 4, 4, 4, 9, 9, 9, // 41..=50
];

/// One documented divergence from upstream: the spec-correct value this
/// engine produces instead of the script's `ans_expected[case]`.
struct Divergence {
    case: usize,
    /// The answer count W3C SPARQL 1.1 prescribes (replaces `ans_expected`).
    ans_num: i64,
    /// Why upstream's recorded number is not followed.
    reason: &'static str,
}

/// The single table of documented divergences from upstream's expectations.
///
/// In both entries only the script's `ans_expected` differs: upstream's own
/// result file agrees with W3C. The script's `get_json_val` reads a missing
/// `AnsNum` as -1 and counts 0 and -1 as equal, so a 0 there means upstream's
/// `/api` answered 0 rows *or* no `AnsNum` at all; either way it is not the
/// spec's answer. (Upstream's `TempResult::doMinus` itself keeps every row
/// when the two sides share no variable, as W3C does.)
const DIVERGENCES: &[Divergence] = &[
    Divergence {
        case: 15,
        ans_num: 4,
        reason: "nested MINUS (a variant of the SPARQL 1.1 §8.2 example): the inner \
                 MINUS keeps only :bob2 (:bob1 is a Jones), the outer one removes :bob2, \
                 DISTINCT ?s leaves :alice :bob1 :carol :dave = 4 rows, exactly \
                 parser_r15.txt; the script records 0",
    },
    Divergence {
        case: 17,
        ans_num: 1,
        reason: "MINUS { :a :b :c } binds no variable, so no solution shares a variable \
                 with it and nothing is removed (SPARQL 1.1 §8.3.2, this very example): \
                 1 row, exactly parser_r17.txt; the script records 0",
    },
];

/// Cases whose failure is Phase 5 work (tolerant import / Turtle leniency,
/// GAP_PLAN §4.2): `(case, reason)`. Such a case still runs and checks the
/// query part where possible, but its import count is not asserted. Empty:
/// every vendored data file imports cleanly with the strict Turtle parser.
const DEFERRED_TO_PHASE5: &[(usize, &str)] = &[];

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

fn case_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/upstream/parser_test")
}

fn case_file(kind: &str, n: usize, ext: &str) -> PathBuf {
    case_dir().join(format!("parser_{kind}{n}.{ext}"))
}

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// upstream result files
// ---------------------------------------------------------------------------

/// A parsed `parser_r<N>.txt`.
#[derive(Debug, Clone, PartialEq)]
enum Expected {
    /// Upstream's ASCII table: header variables (without `?`) and rows of
    /// optional cells (`None` = blank = unbound).
    Table {
        vars: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    /// A graph function's aggregated JSON document.
    Json(Json),
}

/// Split one `| a | b |` line into trimmed cells. `|` inside a quoted literal
/// does not split; the outer bars are dropped.
fn split_table_line(line: &str) -> Vec<String> {
    let line = line.trim();
    let inner = line
        .strip_prefix('|')
        .and_then(|l| l.strip_suffix('|'))
        .unwrap_or(line);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for c in inner.chars() {
        if in_quote {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_quote = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_quote = true;
                cur.push(c);
            }
            '|' => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// Parse upstream's printed result: an ASCII table framed by `---` rules
/// (column widths vary and are ignored) or, for graph functions, JSON.
fn parse_expected(text: &str) -> Expected {
    if text.trim_start().starts_with('{') {
        return Expected::Json(Json::parse(text).unwrap_or_else(|e| panic!("bad JSON: {e}")));
    }
    let mut lines = text.lines().map(str::trim).filter(|l| l.starts_with('|'));
    let header = lines.next().expect("table header");
    let vars: Vec<String> = split_table_line(header)
        .into_iter()
        .map(|v| v.trim_start_matches('?').to_string())
        .collect();
    let rows = lines
        .map(|l| {
            let cells = split_table_line(l);
            assert_eq!(cells.len(), vars.len(), "ragged row `{l}`");
            cells
                .into_iter()
                .map(|c| if c.is_empty() { None } else { Some(c) })
                .collect()
        })
        .collect();
    Expected::Table { vars, rows }
}

// ---------------------------------------------------------------------------
// terms
// ---------------------------------------------------------------------------

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// A cell as a comparable RDF term: parsed from its N-Triples form, with the
/// RDF 1.1 identity `"x"^^xsd:string` = `"x"` applied, and language tags
/// lower-cased (they are case-insensitive).
fn norm_term(cell: &str) -> Term {
    let t = parse_term(cell).unwrap_or_else(|e| panic!("cell `{cell}` is not a term: {e}"));
    match t {
        Term::Literal {
            value,
            datatype,
            lang,
        } => Term::Literal {
            value,
            datatype: datatype.filter(|d| d != XSD_STRING),
            lang: lang.map(|l| l.to_ascii_lowercase()),
        },
        other => other,
    }
}

type Row = Vec<Option<Term>>;

fn norm_row(row: &[Option<String>]) -> Row {
    row.iter().map(|c| c.as_deref().map(norm_term)).collect()
}

fn render_row(row: &Row) -> String {
    let cells: Vec<String> = row
        .iter()
        .map(|c| c.as_ref().map(|t| t.to_string()).unwrap_or_default())
        .collect();
    format!("| {} |", cells.join(" | "))
}

/// Multiset difference report (`missing` = expected but not produced).
fn multiset_diff(want: &[Row], got: &[Row]) -> Option<String> {
    let count = |rows: &[Row]| {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for r in rows {
            *m.entry(render_row(r)).or_default() += 1;
        }
        m
    };
    let (w, g) = (count(want), count(got));
    if w == g {
        return None;
    }
    let mut out = String::new();
    for (row, n) in &w {
        let have = g.get(row).copied().unwrap_or(0);
        if have < *n {
            let _ = writeln!(out, "  missing x{}: {row}", n - have);
        }
    }
    for (row, n) in &g {
        let need = w.get(row).copied().unwrap_or(0);
        if need < *n {
            let _ = writeln!(out, "  extra   x{}: {row}", n - need);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// a minimal JSON reader (graph-function documents only; no new dependency)
// ---------------------------------------------------------------------------

/// A JSON value; objects keep sorted keys so comparison ignores key order.
/// Numbers keep their source text normalised through `f64` (`3` = `3.0`).
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn parse(text: &str) -> Result<Json, String> {
        let chars: Vec<char> = text.chars().collect();
        let mut pos = 0;
        let v = Json::value(&chars, &mut pos)?;
        Json::ws(&chars, &mut pos);
        if pos != chars.len() {
            return Err(format!("trailing input at {pos}"));
        }
        Ok(v)
    }

    fn ws(c: &[char], p: &mut usize) {
        while *p < c.len() && c[*p].is_whitespace() {
            *p += 1;
        }
    }

    fn expect(c: &[char], p: &mut usize, want: char) -> Result<(), String> {
        Json::ws(c, p);
        if c.get(*p) == Some(&want) {
            *p += 1;
            Ok(())
        } else {
            Err(format!("expected `{want}` at {}", *p))
        }
    }

    fn string(c: &[char], p: &mut usize) -> Result<String, String> {
        Json::expect(c, p, '"')?;
        let mut s = String::new();
        loop {
            let ch = *c.get(*p).ok_or("unterminated string")?;
            *p += 1;
            match ch {
                '"' => return Ok(s),
                '\\' => {
                    let e = *c.get(*p).ok_or("bad escape")?;
                    *p += 1;
                    match e {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        'u' => {
                            let hex: String = c.get(*p..*p + 4).ok_or("bad \\u")?.iter().collect();
                            *p += 4;
                            let code = u32::from_str_radix(&hex, 16).map_err(|e| e.to_string())?;
                            s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        other => s.push(other),
                    }
                }
                other => s.push(other),
            }
        }
    }

    fn value(c: &[char], p: &mut usize) -> Result<Json, String> {
        Json::ws(c, p);
        match c.get(*p) {
            Some('{') => {
                *p += 1;
                let mut m = BTreeMap::new();
                Json::ws(c, p);
                if c.get(*p) == Some(&'}') {
                    *p += 1;
                    return Ok(Json::Obj(m));
                }
                loop {
                    let k = Json::string(c, p)?;
                    Json::expect(c, p, ':')?;
                    m.insert(k, Json::value(c, p)?);
                    Json::ws(c, p);
                    match c.get(*p) {
                        Some(',') => *p += 1,
                        Some('}') => {
                            *p += 1;
                            return Ok(Json::Obj(m));
                        }
                        _ => return Err(format!("bad object at {}", *p)),
                    }
                }
            }
            Some('[') => {
                *p += 1;
                let mut a = Vec::new();
                Json::ws(c, p);
                if c.get(*p) == Some(&']') {
                    *p += 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(Json::value(c, p)?);
                    Json::ws(c, p);
                    match c.get(*p) {
                        Some(',') => *p += 1,
                        Some(']') => {
                            *p += 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return Err(format!("bad array at {}", *p)),
                    }
                }
            }
            Some('"') => Ok(Json::Str(Json::string(c, p)?)),
            Some('t') | Some('f') | Some('n') => {
                for (word, v) in [
                    ("true", Json::Bool(true)),
                    ("false", Json::Bool(false)),
                    ("null", Json::Null),
                ] {
                    let w: Vec<char> = word.chars().collect();
                    if c.get(*p..*p + w.len()) == Some(&w[..]) {
                        *p += w.len();
                        return Ok(v);
                    }
                }
                Err(format!("bad literal at {}", *p))
            }
            Some(_) => {
                let start = *p;
                while *p < c.len() && matches!(c[*p], '-' | '+' | '.' | 'e' | 'E' | '0'..='9') {
                    *p += 1;
                }
                let text: String = c[start..*p].iter().collect();
                text.parse::<f64>()
                    .map(Json::Num)
                    .map_err(|_| format!("bad number `{text}` at {start}"))
            }
            None => Err("unexpected end".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// running one case
// ---------------------------------------------------------------------------

/// The outcome of one case.
#[derive(Debug)]
struct Report {
    case: usize,
    success_got: Option<u64>,
    success_want: u64,
    ans_got: Option<i64>,
    ans_want: i64,
    /// Whether the rows match `parser_r<N>.txt` (and the order, when the
    /// query has an ORDER BY).
    rows_ok: bool,
    /// Divergence / deferral marker for the table.
    note: &'static str,
    /// Failure details (empty when the case passes).
    problems: Vec<String>,
}

impl Report {
    fn passed(&self) -> bool {
        self.problems.is_empty()
    }
}

fn divergence(case: usize) -> Option<&'static Divergence> {
    DIVERGENCES.iter().find(|d| d.case == case)
}

fn deferred(case: usize) -> Option<&'static str> {
    DEFERRED_TO_PHASE5
        .iter()
        .find(|(c, _)| *c == case)
        .map(|(_, r)| *r)
}

fn has_order_by(sparql: &str) -> bool {
    let lower = sparql.to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    while let Some(w) = words.next() {
        if w == "order" && words.next() == Some("by") {
            return true;
        }
    }
    false
}

fn run_case(n: usize) -> Report {
    let ans_want = divergence(n).map_or(ANS_EXPECTED[n], |d| d.ans_num);
    let mut report = Report {
        case: n,
        success_got: None,
        success_want: SUCCESS_EXPECTED[n],
        ans_got: None,
        ans_want,
        rows_ok: false,
        note: if divergence(n).is_some() {
            "divergence"
        } else if deferred(n).is_some() {
            "deferred"
        } else {
            ""
        },
        problems: Vec::new(),
    };

    // [1] build
    let mut db =
        match Database::build_from_files(format!("parser_test_{n}"), &[case_file("d", n, "ttl")]) {
            Ok(db) => db,
            Err(e) => {
                report.problems.push(format!("build failed: {e}"));
                return report;
            }
        };
    let success = db.triple_num();
    report.success_got = Some(success);
    if success != report.success_want && deferred(n).is_none() {
        report.problems.push(format!(
            "build success_num {success}, upstream expects {}",
            report.success_want
        ));
    }

    // [2] query
    let sparql = read(&case_file("q", n, "sql"));
    let expected = parse_expected(&read(&case_file("r", n, "txt")));
    let result = match db.query(&sparql) {
        Ok(r) => r,
        Err(e) => {
            report.problems.push(format!("query failed: {e}"));
            return report;
        }
    };

    // [3] answer count + rows
    let (ans, rows_ok) = match (&result, &expected) {
        (QueryResult::Select(rs), Expected::Table { vars, rows }) => {
            let mut ok = true;
            if &rs.vars != vars {
                report
                    .problems
                    .push(format!("columns {:?}, upstream prints {vars:?}", rs.vars));
                ok = false;
            }
            let want: Vec<Row> = rows.iter().map(|r| norm_row(r)).collect();
            let got: Vec<Row> = rs.rows.iter().map(|r| norm_row(r)).collect();
            if let Some(diff) = multiset_diff(&want, &got) {
                report
                    .problems
                    .push(format!("rows differ from parser_r{n}.txt:\n{diff}"));
                ok = false;
            } else if has_order_by(&sparql) && want != got {
                report.problems.push(format!(
                    "ORDER BY order differs from parser_r{n}.txt:\n  want {:?}\n  got  {:?}",
                    want.iter().map(render_row).collect::<Vec<_>>(),
                    got.iter().map(render_row).collect::<Vec<_>>()
                ));
                ok = false;
            }
            (rs.row_count() as i64, ok)
        }
        (QueryResult::Select(rs), Expected::Json(doc)) => {
            // A graph function: one row, one column holding the JSON text.
            let cell = rs.rows.first().and_then(|r| r.first()).cloned().flatten();
            let ok = match cell.as_deref().map(norm_term) {
                Some(Term::Literal { value, .. }) => match Json::parse(&value) {
                    Ok(got) if &got == doc => true,
                    Ok(got) => {
                        report.problems.push(format!(
                            "JSON differs from parser_r{n}.txt:\n  want {doc:?}\n  got  {got:?}"
                        ));
                        false
                    }
                    Err(e) => {
                        report
                            .problems
                            .push(format!("cell is not JSON ({e}): {value}"));
                        false
                    }
                },
                other => {
                    report
                        .problems
                        .push(format!("expected one JSON literal cell, got {other:?}"));
                    false
                }
            };
            (rs.row_count() as i64, ok)
        }
        (QueryResult::Ask(b), Expected::Table { rows, .. }) => {
            // Upstream prints ASK as a one-row `?_askResult` table.
            let want = rows
                .first()
                .and_then(|r| r.first())
                .cloned()
                .flatten()
                .map(|c| norm_term(&c));
            let got = Term::typed_literal(
                if *b { "true" } else { "false" },
                "http://www.w3.org/2001/XMLSchema#boolean",
            );
            let ok = want.as_ref() == Some(&got);
            if !ok {
                report
                    .problems
                    .push(format!("ASK answered {b}, parser_r{n}.txt prints {want:?}"));
            }
            (1, ok)
        }
        (QueryResult::Construct(ts), _) => (ts.len() as i64, true),
        (QueryResult::Update { changed }, _) => (*changed as i64, true),
        (other, exp) => {
            report
                .problems
                .push(format!("result kind {other:?} does not fit {exp:?}"));
            (-1, false)
        }
    };
    report.ans_got = Some(ans);
    report.rows_ok = rows_ok;
    if ans != ans_want {
        report
            .problems
            .push(format!("AnsNum {ans}, expected {ans_want}"));
    }
    report
}

fn render_table(reports: &[Report]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "case | success_num got/want | AnsNum got/want | rows | note       | result"
    );
    let _ = writeln!(
        out,
        "-----+----------------------+-----------------+------+------------+-------"
    );
    for r in reports {
        let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "{:>4} | {:>10} / {:<9} | {:>6} / {:<6} | {:<4} | {:<10} | {}",
            r.case,
            opt(r.success_got.map(|v| v.to_string())),
            r.success_want,
            opt(r.ans_got.map(|v| v.to_string())),
            r.ans_want,
            if r.rows_ok { "ok" } else { "DIFF" },
            r.note,
            if r.passed() { "PASS" } else { "FAIL" }
        );
    }
    let pass = reports.iter().filter(|r| r.passed()).count();
    let _ = writeln!(out, "passed {pass} / {}", reports.len());
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// The whole suite with upstream's per-case summary (see the module docs).
#[test]
fn upstream_parser_suite_all_cases() {
    let reports: Vec<Report> = (1..=TOTAL_CASE).map(run_case).collect();
    let table = render_table(&reports);
    println!("{table}");
    for d in DIVERGENCES {
        println!(
            "divergence #{}: AnsNum {} (upstream script: {}) — {}",
            d.case, d.ans_num, ANS_EXPECTED[d.case], d.reason
        );
    }
    for (c, why) in DEFERRED_TO_PHASE5 {
        println!("deferred to Phase 5 #{c}: {why}");
    }
    let failures: Vec<String> = reports
        .iter()
        .filter(|r| !r.passed())
        .map(|r| format!("case {}:\n  {}", r.case, r.problems.join("\n  ")))
        .collect();
    assert!(
        failures.is_empty(),
        "{table}\n{} failing case(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The divergence / deferral tables only name real cases, once each, and a
/// divergence really changes the script's number.
#[test]
fn documented_exceptions_are_well_formed() {
    let mut seen = std::collections::BTreeSet::new();
    for d in DIVERGENCES {
        assert!((1..=TOTAL_CASE).contains(&d.case));
        assert!(seen.insert(d.case), "case {} listed twice", d.case);
        assert_ne!(
            d.ans_num, ANS_EXPECTED[d.case],
            "case {} diverges from nothing",
            d.case
        );
        assert!(!d.reason.is_empty());
    }
    for (c, why) in DEFERRED_TO_PHASE5 {
        assert!((1..=TOTAL_CASE).contains(c));
        assert!(seen.insert(*c), "case {c} listed twice");
        assert!(!why.is_empty());
    }
}

/// Each divergence's spec-correct number is the row count of upstream's own
/// result file, i.e. only the script's `ans_expected` is off.
#[test]
fn divergences_agree_with_upstream_result_files() {
    for d in DIVERGENCES {
        match parse_expected(&read(&case_file("r", d.case, "txt"))) {
            Expected::Table { rows, .. } => {
                assert_eq!(rows.len() as i64, d.ans_num, "case {}", d.case)
            }
            Expected::Json(_) => panic!("case {}: JSON result", d.case),
        }
    }
}

/// The vendored result-table reader: widths ignored, blanks are unbound,
/// multi-byte text and trailing padding (upstream pads by bytes) tolerated.
#[test]
fn result_table_reader() {
    let t = "-----\n| ?a | ?b  |\n-----\n| \"x|y\"@en  |   |\n| <i> | \"Série\"   |\n-----\n";
    assert_eq!(
        parse_expected(t),
        Expected::Table {
            vars: vec!["a".into(), "b".into()],
            rows: vec![
                vec![Some("\"x|y\"@en".into()), None],
                vec![Some("<i>".into()), Some("\"Série\"".into())],
            ],
        }
    );
    // An empty result keeps its header.
    assert_eq!(
        parse_expected("------\n| ?v |\n------\n------\n"),
        Expected::Table {
            vars: vec!["v".into()],
            rows: vec![]
        }
    );
    assert_eq!(
        Json::parse("{\"b\": [1, 2.0], \"a\": \"s\"}").unwrap(),
        Json::parse("{\"a\":\"s\",\"b\":[1.0,2]}").unwrap()
    );
}

// ---------------------------------------------------------------------------
// root-cause regressions beyond the vendored cases
// ---------------------------------------------------------------------------
//
// The three cases that failed before this suite was added each exposed a
// general engine defect; these pin the general fix, not just the case.

const REG_DATA: &str = r#"
@prefix : <http://ex/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
:a :knows :b .
:a :age 3 .
:knows rdfs:label "knows" .
:knows :knows :c .
:b :p0 :c .
:c :ref :age .
"#;

fn reg_db() -> Database {
    Database::build_from_str("parser-suite-regressions", REG_DATA).expect("build")
}

/// Rows of a SELECT as sorted `|`-joined strings (`-` = unbound); IRIs in
/// the `http://ex/` and `rdfs:` namespaces are abbreviated (`:a`,
/// `rdfs:label`), every other term is printed in N-Triples form.
fn rows(db: &mut Database, q: &str) -> Vec<String> {
    let rs = db.select(q).unwrap_or_else(|e| panic!("`{q}`: {e}"));
    let cell = |c: &Option<String>| match c.as_deref().map(norm_term) {
        None => "-".to_string(),
        Some(Term::Iri(iri)) => match iri.strip_prefix("http://ex/") {
            Some(local) => format!(":{local}"),
            None => match iri.strip_prefix("http://www.w3.org/2000/01/rdf-schema#") {
                Some(local) => format!("rdfs:{local}"),
                None => format!("<{iri}>"),
            },
        },
        Some(t) => t.to_string(),
    };
    let mut out: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect();
    out.sort();
    out
}

/// Case 16's root cause: `SELECT *` listed the variables only a MINUS
/// mentions (SPARQL 1.1 §18.2.1: MINUS has no in-scope variables).
#[test]
fn select_star_omits_minus_only_variables() {
    let mut db = reg_db();
    let rs = db
        .select("SELECT * { ?s <http://ex/age> ?o MINUS { ?x ?y ?z } }")
        .unwrap();
    assert_eq!(rs.vars, vec!["s", "o"]);
    assert_eq!(rs.row_count(), 1);
    // A MINUS that shares ?s still removes, and ?s is listed once.
    let rs = db
        .select("SELECT * { ?s ?p ?o MINUS { ?s <http://ex/knows> ?z } }")
        .unwrap();
    assert_eq!(rs.vars, vec!["s", "p", "o"]);
    assert_eq!(rs.row_count(), 2);
}

/// Case 50's root cause: a variable in predicate position that is also
/// bound some other way compared predicate-dictionary ids with
/// entity-dictionary ids (cases 48/49 only passed because the two id
/// numberings happened to coincide there).
#[test]
fn predicate_variable_joins_with_subject_and_object_positions() {
    let mut db = reg_db();
    // The predicates that have a label (schema exploration).
    assert_eq!(
        rows(
            &mut db,
            "SELECT DISTINCT ?p ?l { ?s ?p ?o . ?p <http://www.w3.org/2000/01/rdf-schema#label> ?l }"
        ),
        vec![":knows|\"knows\""]
    );
    // Subject = predicate within one triple.
    assert_eq!(
        rows(&mut db, "SELECT ?x ?o { ?x ?x ?o }"),
        vec![":knows|:c"]
    );
    // Predicate and subject position (`:knows` is both) …
    assert_eq!(
        rows(&mut db, "SELECT DISTINCT ?s ?x { ?s ?x ?o . ?x ?b ?c }"),
        vec![":a|:knows", ":knows|:knows"]
    );
    // … predicate and object position (`:age` is both).
    assert_eq!(
        rows(&mut db, "SELECT DISTINCT ?s ?x { ?s ?x ?o . ?a ?b ?x }"),
        vec![":a|:age"]
    );
    // OPTIONAL joins on the predicate variable.
    assert_eq!(
        rows(
            &mut db,
            "SELECT DISTINCT ?p ?l { ?s ?p ?o OPTIONAL { ?p <http://www.w3.org/2000/01/rdf-schema#label> ?l } }"
        ),
        vec![
            ":age|-",
            ":knows|\"knows\"",
            ":p0|-",
            ":ref|-",
            "rdfs:label|-"
        ]
    );
    // MINUS on the predicate variable: drop the triples whose predicate has
    // a label.
    assert_eq!(
        rows(&mut db, "SELECT ?s ?p { ?s ?p ?o MINUS { ?p ?q ?r } }"),
        vec![":a|:age", ":b|:p0", ":c|:ref", ":knows|rdfs:label"]
    );
    // GROUP BY / COUNT over a mixed variable.
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?p (COUNT(*) AS ?n) { ?s ?p ?o . ?p ?q ?r } GROUP BY ?p"
        ),
        vec![":knows|\"4\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
}

/// The same defect for a predicate variable bound by UNION, VALUES, BIND
/// or a sub-SELECT: its value was read through the wrong dictionary.
#[test]
fn predicate_variable_bound_elsewhere_resolves_and_joins() {
    let mut db = reg_db();
    assert_eq!(
        rows(
            &mut db,
            "SELECT DISTINCT ?x { { ?x <http://ex/age> ?o } UNION { ?s ?x <http://ex/c> } }"
        ),
        vec![":a", ":knows", ":p0"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?s ?o { VALUES ?p { <http://ex/knows> } ?s ?p ?o }"
        ),
        vec![":a|:b", ":knows|:c"]
    );
    // An IRI that is no predicate matches nothing in predicate position.
    assert!(rows(
        &mut db,
        "SELECT ?s { VALUES ?p { <http://ex/a> } ?s ?p ?o }"
    )
    .is_empty());
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?s ?o { BIND(<http://ex/p0> AS ?p) ?s ?p ?o }"
        ),
        vec![":b|:c"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?s ?p ?o { ?s ?p ?o { SELECT ?p { ?a ?p <http://ex/b> } } }"
        ),
        vec![":a|:knows|:b", ":knows|:knows|:c"]
    );
    // A predicate no entity names (`:p0`) still reads back as its IRI, and
    // joins by term with the same IRI bound in predicate position.
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?x ?s { { SELECT ?x { ?b ?x <http://ex/c> } } ?s ?x <http://ex/c> }"
        ),
        vec![":knows|:knows", ":p0|:b"]
    );
}

/// Case 42's root cause: UCASE/LCASE (and STRBEFORE/STRAFTER/CONCAT)
/// dropped the language tag (SPARQL 1.1 §17.4.3).
#[test]
fn string_functions_keep_the_language_tag() {
    let mut db = reg_db();
    let q = "SELECT (UCASE(\"abc\"@en) AS ?u) (LCASE(\"ABC\"@en) AS ?l) \
             (STRBEFORE(\"abc\"@en, \"c\") AS ?b) (STRAFTER(\"abc\"@en, \"a\") AS ?a) \
             (STRBEFORE(\"abc\"@en, \"z\") AS ?nb) (STRAFTER(\"abc\"@en, \"z\") AS ?na) \
             (CONCAT(\"a\"@en, \"b\"@en) AS ?c) (CONCAT(\"a\"@en, \"b\"@fr) AS ?cm) \
             (CONCAT(\"a\"@en, \"b\") AS ?cs) (UCASE(\"abc\") AS ?us) WHERE {}";
    let rs = db.select(q).unwrap();
    let got: Vec<&str> = rs.rows[0].iter().map(|c| c.as_deref().unwrap()).collect();
    assert_eq!(
        got,
        vec![
            "\"ABC\"@en",
            "\"abc\"@en",
            "\"ab\"@en",
            "\"bc\"@en",
            "\"\"",
            "\"\"",
            "\"ab\"@en",
            "\"ab\"",
            "\"ab\"",
            "\"ABC\"",
        ]
    );
    // So the tagged result equals a tagged literal, as in case 42 …
    assert_eq!(
        rows(&mut db, "SELECT ?x { ?x <http://www.w3.org/2000/01/rdf-schema#label> ?l FILTER(UCASE(STRLANG(?l, \"en\")) = \"KNOWS\"@en) }"),
        vec![":knows"]
    );
    // … and no longer equals the simple literal.
    assert!(rows(&mut db, "SELECT ?x { ?x <http://www.w3.org/2000/01/rdf-schema#label> ?l FILTER(UCASE(STRLANG(?l, \"en\")) = \"KNOWS\") }").is_empty());
}

macro_rules! cases {
    ($($name:ident => $n:expr;)*) => {$(
        #[test]
        fn $name() {
            let r = run_case($n);
            assert!(r.passed(), "case {}:\n  {}", $n, r.problems.join("\n  "));
        }
    )*};
}

cases! {
    case_01_basic_bgp => 1;
    case_02_plain_literal_matches_neither_lang_nor_typed => 2;
    case_03_lang_literal => 3;
    case_04_integer_literal => 4;
    case_05_custom_datatype => 5;
    case_06_regex => 6;
    case_07_regex_case_insensitive => 7;
    case_08_filter_float_comparison => 8;
    case_09_optional => 9;
    case_10_optional_with_filter => 10;
    case_11_two_optionals => 11;
    case_12_union => 12;
    case_13_union_disjoint_vars => 13;
    case_14_union_of_bgps => 14;
    case_15_nested_minus => 15;
    case_16_minus_without_shared_vars => 16;
    case_17_minus_constant_pattern => 17;
    case_18_distinct => 18;
    case_19_optional_chain => 19;
    case_20_ask_true => 20;
    case_21_ask_false => 21;
    case_22_bound => 22;
    case_23_not_bound => 23;
    case_24_filter_equality_and_inequality => 24;
    case_25_is_iri => 25;
    case_26_is_literal => 26;
    case_27_regex_str => 27;
    case_28_lang => 28;
    case_29_lang_matches => 29;
    case_30_regex_anchor_case_insensitive => 30;
    case_31_filter_numeric_comparison => 31;
    case_32_order_by_two_keys => 32;
    case_33_cycle_path => 33;
    case_34_shortest_path_len => 34;
    case_35_k_hop_reachable => 35;
    case_36_datatype_lang_string => 36;
    case_37_datatype_not_equal => 37;
    case_38_now => 38;
    case_39_year => 39;
    case_40_month => 40;
    case_41_day => 41;
    case_42_ucase_keeps_language_tag => 42;
    case_43_ucase_str => 43;
    case_44_str_ucase => 44;
    case_45_contains => 45;
    case_46_strstarts_lcase => 46;
    case_47_abs_of_negative_integer => 47;
    case_48_variable_as_object_and_predicate => 48;
    case_49_variable_as_subject_and_predicate => 49;
    case_50_variable_chain_through_predicates => 50;
}
