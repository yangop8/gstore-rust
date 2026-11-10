//! The expectations of upstream gStore's `scripts/test/parser_test.sh` (50
//! SPARQL cases), shared by the two test targets that run it:
//!
//! * `tests/dt_upstream_parser_suite.rs` — in-process against a
//!   [`Database`](gstore::Database), comparing the imported triple count, the
//!   answer count and the rows with upstream's result files, with the Top-K /
//!   early-termination optimizations on and off;
//! * `tests/upstream_parser.rs` — over the `/api` wire protocol against an
//!   in-process `ApiServer`, as the script drives `gserver` (`build`, `load`,
//!   `query`, `unload`, `drop`).
//!
//! Both read the case files from the one vendored copy,
//! `testdata/upstream/scripts/test/parser_test/` (upstream's own layout; see
//! `testdata/upstream/README.md`), and take the script's numbers and the
//! documented divergences from here, so the two can never disagree.
//!
//! Included with `#[path = "upstream_scripts/parser_cases.rs"] mod
//! parser_cases;` (Cargo does not build files in a `tests/` subdirectory as
//! test targets).

#![allow(dead_code)] // each test target uses a subset

use std::path::PathBuf;

/// Number of upstream cases (`TOTAL_CASE` of `parser_test.sh`).
pub const TOTAL_CASE: usize = 50;

/// `ans_expected` of upstream `parser_test.sh` (index = case number; index 0
/// unused). Upstream's `AnsNum` of each query.
pub const ANS_EXPECTED: [i64; TOTAL_CASE + 1] = [
    0, //
    2, 0, 1, 1, 1, 1, 1, 1, 3, 2, // 1..=10
    2, 4, 4, 2, 0, 1, 0, 1, 2, 1, // 11..=20
    1, 1, 1, 2, 1, 1, 1, 1, 1, 1, // 21..=30
    1, 3, 1, 1, 1, 1, 3, 1, 1, 1, // 31..=40
    1, 1, 1, 1, 1, 1, 1, 8, 8, 7, // 41..=50
];

/// `success_expected` of upstream `parser_test.sh` (index = case number):
/// the `success_num` (triples imported) of each `build`.
pub const SUCCESS_EXPECTED: [u64; TOTAL_CASE + 1] = [
    0, //
    5, 3, 3, 3, 3, 4, 4, 4, 6, 4, // 1..=10
    4, 6, 6, 6, 10, 1, 1, 6, 6, 4, // 11..=20
    4, 3, 3, 4, 4, 4, 4, 3, 3, 2, // 21..=30
    3, 9, 15, 15, 15, 4, 4, 4, 4, 4, // 31..=40
    4, 4, 4, 4, 4, 4, 4, 9, 9, 9, // 41..=50
];

/// One documented divergence from upstream: the spec-correct value this
/// engine produces instead of the script's `ans_expected[case]`.
pub struct Divergence {
    pub case: usize,
    /// The answer count W3C SPARQL 1.1 prescribes (replaces `ans_expected`).
    pub ans_num: i64,
    /// Why upstream's recorded number is not followed.
    pub reason: &'static str,
}

/// The single table of documented divergences from upstream's expectations.
///
/// In both entries only the script's `ans_expected` differs: upstream's own
/// result file agrees with W3C. The script's `get_json_val` reads a missing
/// `AnsNum` as -1 and counts 0 and -1 as equal, so a 0 there means upstream's
/// `/api` answered 0 rows *or* no `AnsNum` at all; either way it is not the
/// spec's answer. (Upstream's `TempResult::doMinus` itself keeps every row
/// when the two sides share no variable, as W3C does.)
pub const DIVERGENCES: &[Divergence] = &[
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

/// The documented divergence of case `case`, if any.
pub fn divergence(case: usize) -> Option<&'static Divergence> {
    DIVERGENCES.iter().find(|d| d.case == case)
}

/// The `AnsNum` case `case` must answer: the script's `ans_expected`, or the
/// divergence's spec-correct number.
pub fn expected_ans_num(case: usize) -> i64 {
    divergence(case).map_or(ANS_EXPECTED[case], |d| d.ans_num)
}

/// The vendored cases: `testdata/upstream/scripts/test/parser_test/`.
pub fn case_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/upstream/scripts/test/parser_test")
}

/// `parser_<kind><n>.<ext>`: `d`/`ttl` data, `q`/`sql` query, `r`/`txt`
/// upstream's printed result.
pub fn case_file(kind: &str, n: usize, ext: &str) -> PathBuf {
    case_dir().join(format!("parser_{kind}{n}.{ext}"))
}
