//! The smoke gate at the magnitudes of the committed smoke baseline
//! (`bench/baselines/smoke.json`): most of its queries answer in 0.01-0.7 ms,
//! where the old 2 ms per-query floor and the geomean's 1 ms cut-off let a
//! 50-300× regression through (review infra#2), while the geomean limit sat
//! inside the baseline's own process-to-process spread (review infra#4).
//!
//! Each case changes a copy of the real baseline and gates it against the
//! original with the shipped thresholds: the baseline against its own
//! slowest processes passes, a 50× slowdown of any single query fails, and
//! the canary fails at 2× as well as 3×.

use std::path::Path;

use gstore_bench::gates::{
    evaluate, geomean_limit, query_spread, verdict, GateOutcome, Thresholds, Verdict,
};
use gstore_bench::schema::{BackendRun, BenchResult};

fn smoke() -> BenchResult {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines/smoke.json");
    BenchResult::load(&p).unwrap()
}

/// Every run of every workload section, mutably, with its subject prefix.
fn runs_mut(r: &mut BenchResult) -> Vec<(String, &mut BackendRun)> {
    let mut v: Vec<(String, &mut BackendRun)> =
        r.runs.iter_mut().map(|x| (String::new(), x)).collect();
    for x in &mut r.extra_workloads {
        let pre = format!("{}:", x.workload.id);
        v.extend(x.runs.iter_mut().map(|run| (pre.clone(), run)));
    }
    v
}

fn gate(base: &BenchResult, m: &BenchResult, slowdown: f64) -> (Verdict, Vec<GateOutcome>) {
    let o = evaluate(base, m, &Thresholds::default(), slowdown);
    (verdict(&o), o)
}

fn failing(o: &[GateOutcome]) -> Vec<String> {
    o.iter()
        .filter(|g| !g.pass)
        .map(|g| format!("{} {}", g.gate, g.subject))
        .collect()
}

/// The baseline's magnitudes are what this file is about: most queries are
/// sub-millisecond, and every run recorded three query processes.
#[test]
fn the_smoke_baseline_is_mostly_sub_millisecond() {
    let mut b = smoke();
    let mut sub_ms = 0;
    let mut all = 0;
    for (_, run) in runs_mut(&mut b) {
        assert_eq!(run.config.query_procs, 3, "{}", run.key());
        for q in &run.queries {
            all += 1;
            sub_ms += usize::from(q.median_ms < 1.0);
            assert_eq!(q.proc_medians_ms.len(), 3, "{}/{}", run.key(), q.name);
        }
    }
    assert_eq!(all, 68);
    assert!(sub_ms >= 40, "{sub_ms} of {all} queries under 1 ms");
}

/// The baseline against itself, and against each of its own query
/// processes (the medians of process k instead of the minimum over the
/// processes): the variance the baseline itself shows never fails a gate.
#[test]
fn the_baseline_passes_against_its_own_process_spread() {
    let b = smoke();
    let (v, o) = gate(&b, &b, 1.0);
    assert_eq!(v, Verdict::Pass, "{:?}", failing(&o));
    for k in 0..3 {
        let mut m = b.clone();
        for (_, run) in runs_mut(&mut m) {
            for q in &mut run.queries {
                q.median_ms = q.proc_medians_ms[k];
            }
        }
        let (v, o) = gate(&b, &m, 1.0);
        assert_eq!(v, Verdict::Pass, "process {k}: {:?}", failing(&o));
    }
    // And against the slowest process of every query at once.
    let mut m = b.clone();
    for (_, run) in runs_mut(&mut m) {
        for q in &mut run.queries {
            q.median_ms = q.proc_medians_ms.iter().copied().fold(0.0, f64::max);
        }
    }
    let (v, o) = gate(&b, &m, 1.0);
    assert_eq!(v, Verdict::Pass, "slowest processes: {:?}", failing(&o));
}

/// The geomean limit sits outside the baseline's own spread: never below
/// 1.5, raised to the spread where that is larger, never above 1.6.
#[test]
fn the_geomean_limit_covers_the_baseline_spread() {
    let mut b = smoke();
    let t = Thresholds::default();
    for (pre, run) in runs_mut(&mut b) {
        let s = query_spread(run, &t);
        let (limit, spread) = geomean_limit(&t, run);
        assert_eq!(s, spread);
        assert!(s > 1.0, "{pre}{}: spread {s}", run.key());
        assert!(
            limit >= s.min(1.6) && (1.5..=1.6).contains(&limit),
            "{pre}{}: limit {limit}, spread {s}",
            run.key()
        );
    }
}

/// A 50× slowdown of any single query, however short, fails its per-query
/// gate (review infra#2: under the 2 ms floor every one of the 0.01-0.1 ms
/// queries slowed 50× passed).
#[test]
fn a_50x_slowdown_of_any_one_query_fails() {
    let b = smoke();
    let mut cases = 0;
    let n_runs = runs_mut(&mut b.clone()).len();
    for i in 0..n_runs {
        let n_queries = runs_mut(&mut b.clone())[i].1.queries.len();
        for j in 0..n_queries {
            let mut m = b.clone();
            let mut runs = runs_mut(&mut m);
            let (pre, run) = &mut runs[i];
            let subject = format!("{pre}{}/{}", run.key(), run.queries[j].name);
            run.queries[j].median_ms *= 50.0;
            let (v, o) = gate(&b, &m, 1.0);
            assert_eq!(v, Verdict::PerfFail, "{subject} slowed 50× passed");
            assert!(
                failing(&o).contains(&format!("time.query {subject}")),
                "{subject}: {:?}",
                failing(&o)
            );
            cases += 1;
        }
    }
    assert_eq!(cases, 68);
}

/// The review's suggested acceptance case: a 20× regression of a query of
/// about 0.02 ms (0.015-0.03 ms) fails. (The per-query floor of 0.1 ms is a
/// margin for scheduler jitter: a 0.01 ms query needs about 23× to fail on
/// its own, which the 50× case above covers.)
#[test]
fn a_20x_slowdown_of_a_0_02_ms_query_fails() {
    let b = smoke();
    let mut seen = 0;
    let n_runs = runs_mut(&mut b.clone()).len();
    for i in 0..n_runs {
        let medians: Vec<f64> = runs_mut(&mut b.clone())[i]
            .1
            .queries
            .iter()
            .map(|q| q.median_ms)
            .collect();
        for (j, med) in medians.into_iter().enumerate() {
            if !(0.015..0.03).contains(&med) {
                continue;
            }
            let mut m = b.clone();
            runs_mut(&mut m)[i].1.queries[j].median_ms *= 20.0;
            assert_eq!(gate(&b, &m, 1.0).0, Verdict::PerfFail, "run {i} query {j}");
            seen += 1;
        }
    }
    assert!(seen >= 10, "{seen} queries of 0.015-0.03 ms");
}

/// The verifier's repro of review infra#2: every sub-millisecond query
/// regressed to 3 ms (4-298×) passed all 456 gates.
#[test]
fn every_sub_millisecond_query_at_3_ms_fails() {
    let b = smoke();
    let mut m = b.clone();
    for (_, run) in runs_mut(&mut m) {
        for q in &mut run.queries {
            if q.median_ms < 1.0 {
                q.median_ms = 3.0;
            }
        }
    }
    let (v, o) = gate(&b, &m, 1.0);
    assert_eq!(v, Verdict::PerfFail);
    let f = failing(&o);
    assert!(
        f.contains(&"time.query watdiv-like:snapshot/L4".to_string()),
        "{f:?}"
    );
    assert!(f.contains(&"time.query snapshot/q1".to_string()), "{f:?}");
    // A query of a few ms slowed 4× fails too (q5: 2.3 → 9.2 ms passed).
    let mut m = b.clone();
    let q5 = m.runs[0]
        .queries
        .iter_mut()
        .find(|q| q.name == "q5")
        .unwrap();
    q5.median_ms *= 4.0;
    let (v, o) = gate(&b, &m, 1.0);
    assert_eq!(v, Verdict::PerfFail);
    assert_eq!(failing(&o), ["time.query snapshot/q5"]);
}

/// The canary: the baseline against itself with every measured time
/// multiplied by 2 or 3 fails, and at 2× every workload's geomean fails on
/// every backend; a uniform 1.3× passes.
#[test]
fn the_canary_fails_at_2x_and_3x_and_1_3x_passes() {
    let b = smoke();
    for x in [2.0, 3.0] {
        let (v, o) = gate(&b, &b, x);
        assert_eq!(v, Verdict::PerfFail, "canary {x}×");
        let geomeans: Vec<&GateOutcome> = o.iter().filter(|g| g.gate == "time.geomean").collect();
        assert_eq!(geomeans.len(), 4);
        assert!(
            geomeans.iter().all(|g| !g.pass),
            "{x}×: {:?}",
            geomeans
                .iter()
                .map(|g| (&g.subject, g.ratio, g.limit))
                .collect::<Vec<_>>()
        );
    }
    let (v, o) = gate(&b, &b, 1.3);
    assert_eq!(v, Verdict::Pass, "1.3×: {:?}", failing(&o));
}
