//! Upstream conformance: `scripts/test/parser_test.sh` — 50 SPARQL parsing
//! cases (vendored as `testdata/upstream/scripts/test/parser_test/`).
//!
//! Each case runs the script's steps over `/api`: `build` `parser_test_<N>`
//! from `parser_d<N>.ttl` (its `success_num` must equal the script's
//! `success_expected[N]`), `load`, `query` `parser_q<N>.sql`, `unload`,
//! `drop`. The answer set must equal `parser_r<N>.txt` — the result file
//! upstream's `PyTest/auto_test.py ParseTest` checks — order-insensitively,
//! with upstream's console formatting normalised (see
//! [`harness::parse_console_table`]); and `AnsNum` must equal its row count.
//!
//! The script's own `ans_expected` agrees with the result files except for
//! cases 15 and 17, where it records 0 although the result files (and SPARQL's
//! MINUS semantics) give 4 and 1 rows; the script's `get_json_val` reads a
//! missing `AnsNum` as -1 and counts 0 and -1 as equal, so those entries
//! record that upstream's `/api` answered no `AnsNum` there. This port
//! follows the result files.
//!
//! Cases needing a later phase are ignored with the phase and feature, one test
//! per case so a phase can un-ignore its own.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use gstore::json::{self, Json};
use harness::*;

/// `success_expected` of `parser_test.sh` (index = case number).
const SUCCESS_EXPECTED: [i64; 51] = [
    0, 5, 3, 3, 3, 3, 4, 4, 4, 6, 4, 4, 6, 6, 6, 10, 1, 1, 6, 6, 4, 4, 3, 3, 4, 4, 4, 4, 3, 3, 2, 3,
    9, 15, 15, 15, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 9, 9, 9,
];

fn case_file(kind: &str, n: usize, ext: &str) -> std::path::PathBuf {
    upstream(&format!("scripts/test/parser_test/parser_{kind}{n}.{ext}"))
}

fn run_case(n: usize) {
    let srv = Server::start(&format!("parser{n}"));
    let db = format!("parser_test_{n}");

    // [1/5] build
    let j = srv.build(&db, &case_file("d", n, "ttl"));
    ok(&j);
    assert_eq!(
        int(&j, "success_num"),
        SUCCESS_EXPECTED[n],
        "case {n}: build success_num"
    );
    // [2/5] load
    srv.load(&db);
    // [3/5] query
    let sparql = read(&case_file("q", n, "sql"));
    let answer = srv.query(&db, &sparql);
    assert_eq!(code(&answer), 0, "case {n}: query failed: {answer}");
    let expected = parse_console_table(&read(&case_file("r", n, "txt")));
    if expected.vars.is_empty() {
        // A graph function's JSON document in the one cell of the one row.
        let got = answer_table(&answer);
        assert_eq!(got.rows.len(), 1, "case {n}: one row expected: {answer}");
        let cell = answer
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(Json::as_array)
            .and_then(|b| b.first())
            .and_then(|b| b.get(&got.vars[0]))
            .and_then(|c| c.get("value"))
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("case {n}: no value in {answer}"))
            .to_string();
        let doc = json::parse(&cell).unwrap_or_else(|e| panic!("case {n}: not JSON ({e}): {cell}"));
        assert_eq!(
            Some(doc.sorted().to_string()),
            expected.rows[0][0],
            "case {n}: graph function result"
        );
    } else {
        let got = answer_table(&answer);
        let want = expected.normalized(&expected.vars);
        let have = got.normalized(&expected.vars);
        assert!(
            want == have,
            "case {n}: answer differs from parser_r{n}.txt\n{}\nanswer: {answer}",
            diff_rows(&want, &have)
        );
        if sparql.to_ascii_lowercase().contains("order by") {
            assert_eq!(
                got.reordered(&expected.vars),
                expected.reordered(&expected.vars),
                "case {n}: ORDER BY order"
            );
        }
    }
    assert_eq!(
        int(&answer, "AnsNum"),
        expected.rows.len() as i64,
        "case {n}: AnsNum"
    );
    // [4/5] unload, [5/5] drop
    srv.unload(&db);
    srv.drop_db(&db);
}

macro_rules! cases {
    ($($(#[$meta:meta])* $name:ident => $n:expr;)*) => {$(
        #[test]
        $(#[$meta])*
        fn $name() {
            run_case($n);
        }
    )*};
}

cases! {
    case_01_basic_bgp => 1;
    case_02_plain_literal_does_not_match_lang_or_typed => 2;
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
    #[ignore = "phase 2: graph functions (cyclePath with a predicate set {...}, upstream JSON paths output)"]
    case_33_cycle_path => 33;
    #[ignore = "phase 2: graph functions (shortestPathLen with a predicate set {...}, upstream JSON paths output)"]
    case_34_shortest_path_len => 34;
    #[ignore = "phase 2: graph functions (kHopReachable with a predicate set {...}, upstream JSON paths output)"]
    case_35_k_hop_reachable => 35;
    case_36_datatype_lang_string => 36;
    case_37_datatype_not_equal => 37;
    case_38_now => 38;
    case_39_year => 39;
    case_40_month => 40;
    case_41_day => 41;
    #[ignore = "phase 3: UCASE/LCASE keep the language tag (ucase(\"Alice\"@en) = \"ALICE\"@en)"]
    case_42_ucase_keeps_language_tag => 42;
    case_43_ucase_str => 43;
    case_44_str_ucase => 44;
    case_45_contains => 45;
    case_46_strstarts_lcase => 46;
    case_47_abs_of_negative_integer => 47;
    case_48_variable_as_object_and_predicate => 48;
    case_49_variable_as_subject_and_predicate => 49;
    #[ignore = "phase 3: a variable joined across predicate and subject/object positions compares predicate ids with entity ids"]
    case_50_variable_chain_through_predicates => 50;
}
