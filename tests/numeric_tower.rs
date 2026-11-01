//! End-to-end regressions for the layered numeric tower (Int/BigInt/Decimal/
//! Double): SUM and the unary numeric builtins must handle decimals and big
//! integers instead of ignoring them or type-erroring.

use gstore::Database;

const DEC: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn db(triples: &str) -> Database {
    Database::build_from_str("num", triples).unwrap()
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
}
