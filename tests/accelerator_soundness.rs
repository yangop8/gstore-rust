//! The substring accelerator (the dictionary `StringIndex` that prunes the
//! candidates of `CONTAINS` / `STRSTARTS` / `STRENDS` / plain `REGEX`
//! filters) must never change a result: its candidates are a sound superset
//! of the true matches, whatever string the filter compares.
//!
//! review query#1, gap-accel-soundness#2: `STR(?v)` is the term's own
//! lexical form (`"0012"^^xsd:integer` is `"0012"`), while a bare `?v` is
//! compared through its value's canonical form (`"12"`). The index used to
//! hold only the canonical form, so `CONTAINS(STR(?c), "001")` lost
//! `"0012"^^xsd:integer` (and leading-zero decimals, signed and derived
//! integers, exponent floats, padded booleans …) on every layout with an
//! in-memory dictionary. The oracle here is the same filter made
//! non-mandatory (`|| false`), which the planner never pushes down.

use std::path::{Path, PathBuf};

use gstore::db::OpenOptions;
use gstore::query::Value;
use gstore::{Database, QueryResult};

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!(
            "gstore_accel_soundness_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Copy)]
enum Layout {
    Memory,
    Stream,
    Materialize,
    #[cfg(feature = "rocksdb")]
    Rocks,
}

const LAYOUTS: &[Layout] = &[
    Layout::Memory,
    Layout::Stream,
    Layout::Materialize,
    #[cfg(feature = "rocksdb")]
    Layout::Rocks,
];

fn open(layout: Layout, dir: &Path, text: &str) -> Database {
    match layout {
        Layout::Memory => Database::build_from_str("accel", text).unwrap(),
        Layout::Stream | Layout::Materialize => {
            let src = dir.join("data.nt");
            std::fs::write(&src, text).unwrap();
            let db_dir = dir.join("db");
            Database::build_disk(&db_dir, &[&src]).unwrap();
            let options = match layout {
                Layout::Stream => OpenOptions::stream(),
                _ => OpenOptions::materialize(),
            };
            Database::open_with(&db_dir, &options).unwrap()
        }
        #[cfg(feature = "rocksdb")]
        Layout::Rocks => Database::build_rocksdb_from_str("accel", dir.join("db"), text).unwrap(),
    }
}

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// The typed literals whose `STR()` differs from their canonical value form
/// (and a few whose forms agree, as controls): `(lexical, datatype)`.
const VALUES: &[(&str, &str)] = &[
    ("0012", "integer"),
    ("+0012", "integer"),
    ("-007", "integer"),
    (" 42 ", "int"),
    ("005", "unsignedByte"),
    ("0099", "long"),
    ("1e3", "double"),
    ("1E3", "double"),
    ("1.0E3", "float"),
    ("0042.50", "decimal"),
    ("+3.140", "decimal"),
    ("19.90", "decimal"),
    ("1", "boolean"),
    ("0", "boolean"),
    (" true", "boolean"),
    ("2024-01-05", "date"),
    ("abc", "string"),
];

/// 80 filler literals (the index is built only past 64 dictionary entries)
/// and one subject per value of [`VALUES`].
fn dataset() -> String {
    let mut s = String::new();
    for i in 0..80 {
        s.push_str(&format!(
            "<http://ex/f{i}> <http://ex/label> \"filler {i}\" .\n"
        ));
    }
    for (i, (lex, dt)) in VALUES.iter().enumerate() {
        s.push_str(&format!(
            "<http://ex/s{i}> <http://ex/v> \"{lex}\"^^<{XSD}{dt}> .\n"
        ));
    }
    s
}

/// The rows of SELECT `q`, each row rendered as one string, sorted.
fn select(db: &Database, q: &str) -> Vec<Vec<Option<String>>> {
    match db.query_read(q, None, None) {
        Ok(QueryResult::Select(rs)) => rs.rows,
        other => panic!("{q}: {other:?}"),
    }
}

fn rows(db: &Database, q: &str) -> Vec<String> {
    let mut out: Vec<String> = select(db, q).iter().map(|row| format!("{row:?}")).collect();
    out.sort();
    out
}

/// Every needle of 3 to 5 bytes taken from each value's lexical form (what
/// `STR(?v)` compares) and from its canonical value form (what a bare `?v`
/// compares), plus each whole form.
fn needles() -> Vec<String> {
    let mut forms: Vec<String> = Vec::new();
    for (lex, dt) in VALUES {
        forms.push(lex.to_string());
        let term = gstore::Term::Literal {
            value: lex.to_string(),
            datatype: Some(format!("{XSD}{dt}")),
            lang: None,
        };
        forms.push(Value::from_term(&term).lexical());
    }
    let mut out = Vec::new();
    for f in &forms {
        let b = f.as_bytes();
        for len in 3..=5 {
            for w in b.windows(len) {
                if let Ok(s) = std::str::from_utf8(w) {
                    out.push(s.to_string());
                }
            }
        }
        if b.len() >= 3 {
            out.push(f.clone());
        }
    }
    // Needles go into a SPARQL string literal: keep the harmless ones.
    out.retain(|n| !n.contains(['"', '\\']));
    out.sort();
    out.dedup();
    out
}

fn regex_safe(n: &str) -> bool {
    !n.bytes().any(|b| b".^$*+?()[]{}|\\".contains(&b))
}

#[test]
fn the_documented_cases_match_with_the_index_on() {
    let db = Database::build_from_str("accel", &dataset()).unwrap();
    let s = |i: usize| format!("{:?}", vec![Some(format!("<http://ex/s{i}>"))]);
    let q = |f: &str| format!("SELECT ?s WHERE {{ ?s <http://ex/v> ?v FILTER({f}) }}");
    // query#1: "0012"^^xsd:integer (and "+0012") under STR().
    let got = rows(&db, &q("CONTAINS(STR(?v), \"001\")"));
    assert_eq!(got, vec![s(0), s(1)], "CONTAINS(STR(?v), \"001\")");
    // gap-accel-soundness#2: a leading-zero decimal, a signed integer, a
    // derived integer type, an exponent float and a padded boolean.
    for (needle, i) in [
        ("0042", 9),
        ("+00", 1),
        ("005", 4),
        ("1.0E", 8),
        (" tr", 14),
    ] {
        let got = rows(&db, &q(&format!("STRSTARTS(STR(?v), \"{needle}\")")));
        let want = rows(
            &db,
            &q(&format!("STRSTARTS(STR(?v), \"{needle}\") || false")),
        );
        assert_eq!(got, want, "STRSTARTS(STR(?v), {needle:?})");
        assert!(
            want.iter().any(|r| r == &s(i)),
            "the oracle finds s{i} for {needle:?}: {want:?}"
        );
    }
    // A bare variable compares the canonical form: "0012" is 12.
    let got = rows(&db, &q("CONTAINS(?v, \"001\")"));
    let want = rows(&db, &q("CONTAINS(?v, \"001\") || false"));
    assert_eq!(got, want);
}

/// The battery: for every layout, every needle, each substring built-in and
/// both targets (`?v`, `STR(?v)`), the pushed-down filter returns exactly the
/// rows of its non-mandatory twin.
#[test]
fn pushdown_agrees_with_the_no_index_oracle_on_every_layout() {
    let text = dataset();
    for &layout in LAYOUTS {
        let tmp = TempDir::new(&format!("{layout:?}"));
        let db = open(layout, tmp.path(), &text);
        let needles = needles();
        assert!(needles.len() > 50, "{layout:?}: {} needles", needles.len());
        let mut checked = 0usize;
        let mut matched = 0usize;
        for n in &needles {
            for target in ["?v", "STR(?v)"] {
                for func in ["CONTAINS", "STRSTARTS", "STRENDS", "REGEX"] {
                    if func == "REGEX" && !regex_safe(n) {
                        continue;
                    }
                    let f = format!("{func}({target}, \"{n}\")");
                    let pushed = format!("SELECT ?s WHERE {{ ?s <http://ex/v> ?v FILTER({f}) }}");
                    let oracle =
                        format!("SELECT ?s WHERE {{ ?s <http://ex/v> ?v FILTER({f} || false) }}");
                    let got = rows(&db, &pushed);
                    let want = rows(&db, &oracle);
                    assert_eq!(got, want, "{layout:?}: {f}");
                    checked += 1;
                    matched += want.len();
                }
            }
        }
        // The battery is not vacuous: it matched rows through both targets.
        assert!(checked > 400, "{layout:?}: {checked} filters checked");
        assert!(matched > 100, "{layout:?}: {matched} rows matched");
    }
}
