//! The weekly reference run is compared with the latest post-Phase-6
//! results (review infra#8): its LUBM-like runs with
//! `bench/results/phase6-final-lubm<scale>.json` (the same run keys:
//! snapshot, disk@materialize, disk@stream, rocksdb), and its WatDiv-like
//! section — an extra workload of the run — with
//! `bench/results/phase6-final-watdiv10.json`, whose main workload it is.
//! A report matches a section with the baseline's section of the same id,
//! else with the one of the same workload and parameters.

use std::path::Path;

use gstore_bench::gates::Thresholds;
use gstore_bench::report::table;
use gstore_bench::schema::{BenchResult, WorkloadSection};

fn load(rel: &str) -> BenchResult {
    BenchResult::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

/// What the weekly job writes at univ 8: the LUBM-like runs and the
/// WatDiv-like scale-10 section of one merged result (here the committed
/// Phase 6 closure measurements themselves, so every ratio is 1).
fn weekly() -> BenchResult {
    let mut r = load("results/phase6-final-lubm8.json");
    let w = load("results/phase6-final-watdiv10.json");
    r.extra_workloads.push(WorkloadSection {
        workload: w.workload,
        runs: w.runs,
    });
    r
}

/// The query table's data rows of the WatDiv-like section.
fn watdiv_rows(t: &str) -> Vec<&str> {
    let start = t
        .find("**watdiv-like scale 10**")
        .expect("a WatDiv-like table");
    t[start..]
        .lines()
        .filter(|l| {
            l.starts_with("| L")
                || l.starts_with("| S")
                || l.starts_with("| F")
                || l.starts_with("| C")
        })
        .collect()
}

#[test]
fn the_watdiv_section_compares_with_the_watdiv_result() {
    let t = Thresholds::default();
    let base = load("results/phase6-final-watdiv10.json");
    let q = table("queries", &[(weekly(), Some(base))], &t).unwrap();
    let rows = watdiv_rows(&q);
    assert_eq!(rows.len(), 20, "{q}");
    for row in rows {
        // Every run of every query has its ratio, and they are all 1.
        assert_eq!(row.matches("1.00×").count(), 4, "{row}");
        assert!(!row.contains("| – |"), "{row}");
    }
}

#[test]
fn the_lubm_runs_compare_with_the_lubm_result_run_by_run() {
    let t = Thresholds::default();
    let base = load("results/phase6-final-lubm8.json");
    let o = table("overview", &[(weekly(), Some(base))], &t).unwrap();
    for key in ["snapshot", "disk@materialize", "disk@stream", "rocksdb"] {
        let row = o
            .lines()
            .find(|l| l.starts_with(&format!("| lubm-like univ 8 | {key} |")))
            .unwrap_or_else(|| panic!("no {key} row:\n{o}"));
        assert!(row.ends_with("| 1.00× | 1.00× | 1.00× |"), "{row}");
    }
    // The WatDiv-like section is not in this baseline: no ratios.
    let row = o
        .lines()
        .find(|l| l.starts_with("| watdiv-like scale 10 | snapshot |"))
        .unwrap();
    assert!(row.ends_with("| – | – | – |"), "{row}");
    // Against the WatDiv-like result the LUBM-like runs are not compared
    // (another workload), the WatDiv-like ones are.
    let base = load("results/phase6-final-watdiv10.json");
    let o = table("overview", &[(weekly(), Some(base))], &t).unwrap();
    for l in o.lines().filter(|l| l.starts_with("| lubm-like univ 8 |")) {
        assert!(l.ends_with("| – | – | – |"), "{l}");
    }
    for l in o
        .lines()
        .filter(|l| l.starts_with("| watdiv-like scale 10 |"))
    {
        assert!(l.ends_with("| 1.00× | 1.00× | 1.00× |"), "{l}");
    }
}

#[test]
fn a_section_of_other_parameters_is_not_compared() {
    let t = Thresholds::default();
    let mut base = load("results/phase6-final-watdiv10.json");
    base.workload.spec.scale = 1;
    let q = table("queries", &[(weekly(), Some(base))], &t).unwrap();
    for row in watdiv_rows(&q) {
        assert!(!row.contains('×'), "{row}");
    }
}
