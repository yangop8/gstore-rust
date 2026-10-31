//! Upstream conformance: `scripts/test/bfs_test` — the expected answers of
//! upstream's `PyTest/auto_test.py BFSTest` (vendored as
//! `testdata/upstream/scripts/test/bfs_test/<dataset>/<query>.sql` with the
//! answer in `<query>r.txt`).
//!
//! `BFSTest` builds `data/<dataset>/<dataset>.nt` and checks every query's
//! answer set, order-insensitively, against its result file; here each query
//! is its own test over `/api` on a database built once per dataset.
//!
//! **LUBM.** The `bfs_test/lubm` queries and answers were produced from the
//! legacy gStore LUBM encoding, whose predicates and classes are the IRIs
//! `<ub:name>`, `<rdf:type>`, `<ub:GraduateStudent>`, … and whose literal
//! values are IRIs too (`?x <ub:name> <FullProfessor0>`). Upstream's current
//! `data/lubm/lubm.nt` writes them as prefixed names and literals (`ub:name
//! "FullProfessor0"`), so against it these queries answer nothing, upstream
//! included. [`legacy_lubm`] rewrites the current file into the legacy
//! encoding (prefixed name `p:x` → `<p:x>`, literal `"v"` → `<v>`), which
//! reproduces every recorded answer.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use harness::*;

/// One dataset's database, built on first use and shared by its tests.
struct Dataset {
    srv: Server,
    db: String,
}

fn dataset(name: &str) -> &'static Dataset {
    static BBUG: OnceLock<Dataset> = OnceLock::new();
    static LUBM: OnceLock<Dataset> = OnceLock::new();
    static NUM: OnceLock<Dataset> = OnceLock::new();
    static SMALL: OnceLock<Dataset> = OnceLock::new();
    let cell = match name {
        "bbug" => &BBUG,
        "lubm" => &LUBM,
        "num" => &NUM,
        "small" => &SMALL,
        _ => panic!("unknown dataset {name}"),
    };
    cell.get_or_init(|| {
        let root = scratch_fixed(&format!("bfs_{name}"));
        let (srv, file) = if name == "lubm" {
            let dir = scratch_fixed("bfs_lubm_data");
            let file = legacy_lubm(&dir);
            (Server::start_at(root, Some(&dir)), file)
        } else {
            (
                Server::start_at(root, None),
                data(&format!("{name}/{name}.nt")),
            )
        };
        let db = format!("bfs_{name}");
        ok(&srv.build(&db, &file));
        srv.load(&db);
        Dataset { srv, db }
    })
}

/// Rewrite `data/lubm/lubm.nt` (one `subject predicate object .` statement
/// per line after the `@prefix` lines; no literal holds a space) into the
/// legacy encoding the `bfs_test/lubm` answers were recorded from; returns
/// the written file.
fn legacy_lubm(dir: &std::path::Path) -> PathBuf {
    let text = read(&data("lubm/lubm.nt"));
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("@prefix") {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        assert!(
            toks.len() == 4 && toks[3] == ".",
            "unexpected lubm.nt line: {line}"
        );
        for tok in &toks[..3] {
            if tok.starts_with('<') {
                out.push_str(tok);
            } else if let Some(lit) = tok.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                out.push('<');
                out.push_str(lit);
                out.push('>');
            } else {
                out.push('<');
                out.push_str(tok);
                out.push('>');
            }
            out.push(' ');
        }
        out.push_str(".\n");
    }
    let file = dir.join("lubm_legacy.nt");
    fs::write(&file, out).unwrap();
    file
}

fn run_case(name: &str, query: &str) {
    let ds = dataset(name);
    let dir = format!("scripts/test/bfs_test/{name}");
    let sparql = read(&upstream(&format!("{dir}/{query}.sql")));
    let expected = parse_tsv_table(&read(&upstream(&format!("{dir}/{query}r.txt"))));
    let answer = ds.srv.query(&ds.db, &sparql);
    assert_eq!(code(&answer), 0, "{name}/{query}: query failed: {answer}");
    let got = answer_table(&answer);
    let mut vars = expected.vars.clone();
    vars.sort();
    let mut got_vars = got.vars.clone();
    got_vars.sort();
    assert_eq!(got_vars, vars, "{name}/{query}: projected variables");
    let want = expected.normalized(&vars);
    let have = got.normalized(&vars);
    assert!(
        want == have,
        "{name}/{query}: answer differs from {query}r.txt\n{}",
        diff_rows(&want, &have)
    );
    assert_eq!(
        int(&answer, "AnsNum"),
        expected.rows.len() as i64,
        "{name}/{query}: AnsNum"
    );
}

macro_rules! cases {
    ($($(#[$meta:meta])* $fn_name:ident => ($ds:expr, $q:expr);)*) => {$(
        #[test]
        $(#[$meta])*
        fn $fn_name() {
            run_case($ds, $q);
        }
    )*};
}

cases! {
    bbug1 => ("bbug", "bbug1");
    bbug3 => ("bbug", "bbug3");
    bbug4 => ("bbug", "bbug4");
    bbug5 => ("bbug", "bbug5");
    lubm_p0 => ("lubm", "lubm_p0");
    lubm_p2 => ("lubm", "lubm_p2");
    lubm_p3 => ("lubm", "lubm_p3");
    lubm_p4 => ("lubm", "lubm_p4");
    lubm_q0 => ("lubm", "lubm_q0");
    lubm_q1 => ("lubm", "lubm_q1");
    lubm_q2 => ("lubm", "lubm_q2");
    lubm_q3 => ("lubm", "lubm_q3");
    lubm_q4 => ("lubm", "lubm_q4");
    lubm_q5 => ("lubm", "lubm_q5");
    num3 => ("num", "num3");
    small_p0 => ("small", "small_p0");
    small_p1 => ("small", "small_p1");
    small_p2 => ("small", "small_p2");
    small_p3 => ("small", "small_p3");
    small_q0 => ("small", "small_q0");
    small_q1 => ("small", "small_q1");
    small_q2 => ("small", "small_q2");
    small_q3 => ("small", "small_q3");
    small_s0 => ("small", "small_s0");
    small_s1 => ("small", "small_s1");
}
