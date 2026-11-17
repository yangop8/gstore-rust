//! End-to-end regressions for the layered numeric tower (Int/BigInt/Decimal/
//! Double): SUM and the unary numeric builtins must handle decimals and big
//! integers instead of ignoring them or type-erroring.

use gstore::Database;

const DEC: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const INT: &str = "http://www.w3.org/2001/XMLSchema#integer";

fn db(triples: &str) -> Database {
    Database::build_from_str("num", triples).unwrap()
}

/// The single cell of a one-row, one-column SELECT.
fn single(d: &mut Database, q: &str) -> String {
    let rs = d.select(q).unwrap();
    assert_eq!(rs.row_count(), 1, "{q}: {:?}", rs.rows);
    rs.rows[0][0].clone().unwrap_or_default()
}

#[test]
fn abs_over_decimal_evaluates_not_type_errors() {
    let mut d = db(&format!(
        "<http://ex/a> <http://ex/v> \"-2.5\"^^<{DEC}> .\n"
    ));
    // ABS(-2.5) must evaluate; if it type-errored (pre-fix), the FILTER would
    // drop the row and the count would be 0.
    let n = d
        .select("SELECT ?e WHERE { ?e <http://ex/v> ?x . FILTER(ABS(?x) > 2.0) }")
        .unwrap()
        .row_count();
    assert_eq!(n, 1, "ABS over a decimal must evaluate");
    // review tests-b#13: the value and its datatype, not only that the
    // FILTER passed — ABS(-2.5) is 2.5 and stays an xsd:decimal.
    assert_eq!(
        single(
            &mut d,
            "SELECT (ABS(?x) AS ?a) WHERE { ?e <http://ex/v> ?x }"
        ),
        format!("\"2.5\"^^<{DEC}>")
    );
}

#[test]
fn sum_over_decimals_is_not_zero() {
    let mut d = db(&format!(
        "<http://ex/a> <http://ex/v> \"1.5\"^^<{DEC}> .\n\
         <http://ex/b> <http://ex/v> \"2.25\"^^<{DEC}> .\n"
    ));
    // SUM(?x) = 3.75; a group whose SUM was wrongly 0 would fail the HAVING and
    // return no rows.
    let n = d
        .select(
            "SELECT (SUM(?x) AS ?s) WHERE { ?e <http://ex/v> ?x } \
             GROUP BY ?e HAVING (SUM(?x) > 1.0)",
        )
        .unwrap()
        .row_count();
    assert_eq!(
        n, 2,
        "each decimal row's SUM must exceed 1.0 (was 0 pre-fix)"
    );
    // review tests-b#13: each group above holds one value, so a SUM that
    // returned the first value, the maximum or the truncated sum passed it.
    // The sum over both groups, exactly and as an xsd:decimal:
    assert_eq!(
        single(
            &mut d,
            "SELECT (SUM(?x) AS ?s) WHERE { ?e <http://ex/v> ?x }"
        ),
        format!("\"3.75\"^^<{DEC}>")
    );
    // A decimal plus an integer is a decimal (xsd numeric type promotion).
    d.query(&format!(
        "INSERT DATA {{ <http://ex/c> <http://ex/v> \"2\"^^<{INT}> }}"
    ))
    .unwrap();
    assert_eq!(
        single(
            &mut d,
            "SELECT (SUM(?x) AS ?s) WHERE { ?e <http://ex/v> ?x }"
        ),
        format!("\"5.75\"^^<{DEC}>")
    );
}

#[test]
fn sum_over_integers_beyond_i64_is_exact() {
    // review tests-b#13: the big-integer layer of the tower — the sum of
    // i64::MAX and 1 is exact, an xsd:integer.
    let mut d = db(&format!(
        "<http://ex/a> <http://ex/v> \"9223372036854775807\"^^<{INT}> .\n\
         <http://ex/b> <http://ex/v> \"1\"^^<{INT}> .\n"
    ));
    assert_eq!(
        single(
            &mut d,
            "SELECT (SUM(?x) AS ?s) WHERE { ?e <http://ex/v> ?x }"
        ),
        format!("\"9223372036854775808\"^^<{INT}>")
    );
    assert_eq!(
        single(
            &mut d,
            "SELECT (ABS(-?x) AS ?a) WHERE { ?e <http://ex/v> ?x FILTER(?x > 1) }"
        ),
        format!("\"9223372036854775807\"^^<{INT}>")
    );
}
