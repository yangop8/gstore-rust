//! Data test: the cost-based optimizer's binary-join (bushy) execution path.
//!
//! A "bow-tie" query — two stars (`?x` and `?y`), each pinned by two constant
//! edges, joined by a single bridge edge `?x → ?y` — is exactly the shape where
//! the DP optimizer prefers a *binary join* of the two independently-built
//! sub-results over any left-deep pipeline. These tests drive that query end to
//! end through the public `Database` API and check the result is correct (the
//! bushy executor must agree with the left-deep one).

use gstore::Database;

/// 40 `?x` subjects (each `ta A`, `tb B`), 40 `?y` subjects (each `tc C`,
/// `td D`), and one bridge `x5 -> y7`. Only `x5`/`y7` survive the full join.
fn bowtie_data() -> String {
    let mut s = String::from("@prefix : <http://ex/> .\n");
    for x in 0..40 {
        s += &format!(":x{x} :ta :A ; :tb :B .\n");
    }
    for y in 0..40 {
        s += &format!(":y{y} :tc :C ; :td :D .\n");
    }
    s += ":x5 :bridge :y7 .\n";
    s
}

const BOWTIE_QUERY: &str = "\
SELECT ?x ?y WHERE {
  ?x <http://ex/ta> <http://ex/A> .
  ?x <http://ex/tb> <http://ex/B> .
  ?y <http://ex/tc> <http://ex/C> .
  ?y <http://ex/td> <http://ex/D> .
  ?x <http://ex/bridge> ?y .
}";

#[test]
fn bushy_join_returns_exact_solution() {
    let mut db = Database::build_from_str("opt_bowtie", &bowtie_data()).unwrap();
    let rs = db.select(BOWTIE_QUERY).unwrap();
    assert_eq!(rs.row_count(), 1, "exactly one bridged (x, y) pair");
    assert_eq!(rs.rows[0][0].as_deref(), Some("<http://ex/x5>"));
    assert_eq!(rs.rows[0][1].as_deref(), Some("<http://ex/y7>"));
}

#[test]
fn bushy_join_with_no_bridge_is_empty() {
    // Same two stars but no bridge edge ⇒ the binary join yields nothing.
    let mut data = String::from("@prefix : <http://ex/> .\n");
    for x in 0..40 {
        data += &format!(":x{x} :ta :A ; :tb :B .\n");
    }
    for y in 0..40 {
        data += &format!(":y{y} :tc :C ; :td :D .\n");
    }
    let mut db = Database::build_from_str("opt_nobridge", &data).unwrap();
    let rs = db.select(BOWTIE_QUERY).unwrap();
    assert_eq!(rs.row_count(), 0);
}

#[test]
fn bushy_join_multiple_bridges() {
    // Two bridges ⇒ two surviving pairs; the bushy executor must return both.
    let mut data = bowtie_data();
    data += ":x9 :bridge :y3 .\n";
    let mut db = Database::build_from_str("opt_twobridge", &data).unwrap();
    let rs = db.select(BOWTIE_QUERY).unwrap();
    let mut pairs: Vec<(String, String)> = rs
        .rows
        .iter()
        .map(|r| {
            (
                r[0].clone().unwrap_or_default(),
                r[1].clone().unwrap_or_default(),
            )
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("<http://ex/x5>".to_string(), "<http://ex/y7>".to_string()),
            ("<http://ex/x9>".to_string(), "<http://ex/y3>".to_string()),
        ]
    );
}
