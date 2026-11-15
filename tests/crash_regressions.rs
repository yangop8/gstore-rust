//! Single-request process killers (review package r1-crash-dos):
//!
//! - `shortestPathCount` on a regular layered graph whose path counts reach
//!   a multiple of 2^32 (review H-8, analytics#1): the count that wraps to 0
//!   was also the "not reached" marker, so the frontier doubled every level
//!   until an allocation failure aborted the process;
//! - `maximumKplex` on a CSR without vertices (gap-gfun-fuzz#1) panicked.

use std::time::{Duration, Instant};

use gstore::parser::ntriples::parse_term;
use gstore::{Database, Term};

/// The lexical form of the plain literal in the only cell of `q`'s result.
fn only_plain(db: &mut Database, q: &str) -> String {
    let rs = db.select(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rs.rows.len(), 1, "{q}: {rs:?}");
    let cell = rs.rows[0][0]
        .clone()
        .unwrap_or_else(|| panic!("{q}: unbound"));
    match parse_term(&cell).unwrap() {
        Term::Literal {
            value,
            datatype: None,
            lang: None,
        } => value,
        other => panic!("{q}: expected a plain literal, got {other:?}"),
    }
}

// ---- shortestPathCount (H-8) ---------------------------------------------------

/// A 2-wide ladder of `levels` levels over `<p>`: `s → {a1, b1}`,
/// `{ai, bi} → {ai+1, bi+1}`, `{aL, bL} → t` (4·L triples, 2^L shortest
/// paths `s → t`), plus — with `chain` — one more path of the same length
/// `s → c1 → … → cL → t`.
fn ladder(levels: usize, chain: bool) -> String {
    let mut nt = String::new();
    let mut edge = |a: &str, b: &str| {
        nt.push_str(&format!(
            "<http://ex/{a}> <http://ex/p> <http://ex/{b}> .\n"
        ));
    };
    edge("s", "a1");
    edge("s", "b1");
    for i in 1..levels {
        for x in ["a", "b"] {
            for y in ["a", "b"] {
                edge(&format!("{x}{i}"), &format!("{y}{}", i + 1));
            }
        }
    }
    edge(&format!("a{levels}"), "t");
    edge(&format!("b{levels}"), "t");
    if chain {
        edge("s", "c1");
        for i in 1..levels {
            edge(&format!("c{i}"), &format!("c{}", i + 1));
        }
        edge(&format!("c{levels}"), "t");
    }
    nt
}

fn path_count(db: &mut Database) -> String {
    only_plain(
        db,
        "SELECT (shortestPathCount(<http://ex/s>, <http://ex/t>, true, {<http://ex/p>}) AS ?c) \
         WHERE {}",
    )
}

fn count_entry(n: i64) -> String {
    format!(
        "{{\"paths\":[{{\"src\":\"<http://ex/s>\", \"dst\":\"<http://ex/t>\", \"count\":{n}}}]}}"
    )
}

/// The count is upstream's C `int` (wrapping, so 2^L + 1 prints as 1 for
/// L >= 32), and the search stays one vertex per level and side. Before the
/// fix the 400-triple ladder (L = 100) aborted the process: once a vertex's
/// count wrapped to exactly 0 it read as "not reached", was enqueued again
/// for every in-edge, and the frontier doubled each level until an
/// allocation failed.
#[test]
fn shortest_path_count_on_a_deep_ladder_completes_with_the_upstream_count() {
    let cases = [
        (20, false, 1 << 20),
        (20, true, (1 << 20) + 1),
        (31, false, i64::from(i32::MIN)), // 2^31 as a C int
        (32, false, 0),
        (70, true, 1),
        (100, false, 0),
        (100, true, 1),
    ];
    for (levels, chain, expected) in cases {
        let nt = ladder(levels, chain);
        let mut db = Database::build_from_str("crash_ladder", &nt).unwrap();
        let started = Instant::now();
        assert_eq!(
            path_count(&mut db),
            count_entry(expected),
            "ladder L={levels} chain={chain}"
        );
        // A hang guard, not the assertion: the fixed search is a few
        // hundred steps.
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "L={levels}: {:?}",
            started.elapsed()
        );
    }
}

// ---- maximumKplex on an empty CSR (gap-gfun-fuzz#1) ----------------------------

/// A database without entity vertices (empty, or literal objects only)
/// answers the empty k-plex for every k (before: `k <= 2` panicked inside
/// the k-plex worker and the query failed with "internal error"; `k = 3`
/// printed `{"kplex":[,,,]}` on the empty database).
#[test]
fn maximum_kplex_on_a_vertexless_graph_is_empty() {
    let empty = Database::new("crash_kplex_empty");
    let literal_only =
        Database::build_from_str("crash_kplex_lit", "<http://ex/d> <http://ex/q> \"lit\" .\n")
            .unwrap();
    for (name, mut db) in [("empty", empty), ("literal-only", literal_only)] {
        for call in [
            "maximumKplex({})",
            "maximumKplex({}, 1)",
            "maximumKplex({}, 2)",
            "maximumKplex({}, 3)",
            "maximumKplex({<http://ex/q>}, 2)",
        ] {
            assert_eq!(
                only_plain(&mut db, &format!("SELECT ({call} AS ?r) WHERE {{}}")),
                r#"{"paths":[{"kplex":[]}]}"#,
                "{name}: {call}"
            );
        }
    }
}
