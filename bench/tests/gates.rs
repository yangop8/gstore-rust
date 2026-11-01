//! Gate evaluation on synthetic results: the acceptance cases of the gates
//! (uniform 1.3× passes, 1.6× fails, a ≥ 20 ms query slowed 4× fails, a row
//! hash change fails without a retry target, the canary) plus calibration,
//! memory, size and coverage gates.

use gstore_bench::calib::Calibration;
use gstore_bench::gates::{evaluate, retry_targets, self_check, verdict, Phase, Thresholds, Verdict};
use gstore_bench::schema::*;
use gstore_bench::workload::WorkloadSpec;

const MEDIANS: [f64; 14] = [
    0.5, 9.6, 0.6, 10.7, 45.5, 31.5, 55.7, 54.3, 77.0, 43.2, 0.4, 0.2, 35.6, 26.6,
];

fn result(scale: f64) -> BenchResult {
    let queries: Vec<QueryInfo> = (1..=14)
        .map(|i| QueryInfo {
            name: format!("q{i}"),
            text_fnv64: format!("{:016x}", gstore_bench::lubm::QUERY_TEXT_FNV64[i - 1].1),
        })
        .collect();
    let run = |backend: &str| BackendRun {
        backend: backend.into(),
        config: RunConfig::default(),
        build: Some(BuildResult {
            wall_ms: 12_000.0 * scale,
            peak_rss_mib: Some(1300.0),
            dir_bytes: 87_000_000,
            ..BuildResult::default()
        }),
        open: Some(OpenResult {
            first_ms: 500.0 * scale,
            warm_ms: 230.0 * scale,
            warm_samples_ms: vec![230.0 * scale],
            cold: false,
            rss_after_open_mib: Some(127.0),
            hwm_after_open_mib: Some(127.0),
            triple_num: 1_000,
            entity_num: 10,
            literal_num: 10,
            predicate_num: 3,
        }),
        queries: MEDIANS
            .iter()
            .enumerate()
            .map(|(i, m)| QueryMeasure {
                name: format!("q{}", i + 1),
                rows: 10 + i as u64,
                rows_fnv64: format!("{:016x}", 1000 + i),
                first_ms: m * scale * 1.2,
                median_ms: m * scale,
                runs_ms: vec![m * scale],
                stable: true,
            })
            .collect(),
        query_mem: Some(MemInfo {
            rss_mib: Some(150.0),
            hwm_mib: Some(151.0),
        }),
        update: Some(UpdateResult {
            singles: 1,
            singles_ok: true,
            chunk_delta_ok: true,
            answers_unchanged: true,
            reopen_ok: true,
            ..UpdateResult::default()
        }),
        conc: Some(ConcResult {
            points: vec![ConcPoint {
                endpoint: "sparql".into(),
                query: "q4".into(),
                clients: 8,
                qps: 80.0 / scale,
                distinct_bodies: 1,
                ..ConcPoint::default()
            }],
            bodies_equal: true,
            hwm_mib: None,
        }),
        attempts: Default::default(),
    };
    BenchResult {
        schema: SCHEMA.into(),
        kind: "result".into(),
        provisional: false,
        note: None,
        profile: "reference".into(),
        created_unix: 0,
        env: EnvInfo::default(),
        calibration: Some(Calibration {
            schema: "calib/1".into(),
            sort_ms: 10.0,
            hashmap_ms: 10.0,
            format_ms: 10.0,
            fsync_ms: None,
            score_ms: 10.0,
        }),
        workload: WorkloadInfo {
            spec: WorkloadSpec::lubm(8, 0),
            id: "lubm-like".into(),
            version: "lubm-like/1".into(),
            lines: 1_100,
            unique_triples: 1_000,
            bytes: 100_000,
            data_fnv64: "00000000000000aa".into(),
            queries,
            chunk: None,
            gen_ms: None,
        },
        runs: vec![run("snapshot"), run("disk")],
    }
}

fn check(base: &BenchResult, m: &BenchResult, slowdown: f64) -> (Verdict, Vec<gstore_bench::gates::GateOutcome>) {
    let o = evaluate(base, m, &Thresholds::default(), slowdown);
    (verdict(&o), o)
}

#[test]
fn identical_results_pass_every_gate() {
    let b = result(1.0);
    assert!(self_check(&b).iter().all(|o| o.pass));
    let (v, o) = check(&b, &b, 1.0);
    assert_eq!(v, Verdict::Pass, "{o:#?}");
}

#[test]
fn a_uniform_slowdown_of_1_3_passes_and_1_6_fails() {
    let b = result(1.0);
    assert_eq!(check(&b, &b, 1.3).0, Verdict::Pass);
    let (v, o) = check(&b, &b, 1.6);
    assert_eq!(v, Verdict::PerfFail);
    assert!(o.iter().any(|g| !g.pass && g.gate == "time.geomean"));
    // The same through the measured numbers rather than the canary factor.
    assert_eq!(check(&b, &result(1.3), 1.0).0, Verdict::Pass);
    assert_eq!(check(&b, &result(1.6), 1.0).0, Verdict::PerfFail);
}

#[test]
fn the_canary_slowdown_fails() {
    let b = result(1.0);
    let (v, o) = check(&b, &b, 3.0);
    assert_eq!(v, Verdict::PerfFail);
    let targets = retry_targets(&o);
    assert!(targets.contains(&("snapshot".to_string(), Phase::Query)));
    assert!(targets.contains(&("disk".to_string(), Phase::Build)) || targets.contains(&("disk".to_string(), Phase::Open)));
}

#[test]
fn one_slow_query_of_20ms_or_more_fails() {
    let b = result(1.0);
    for (i, m) in MEDIANS.iter().enumerate() {
        if *m < 20.0 {
            continue;
        }
        let mut r = result(1.0);
        r.runs[0].queries[i].median_ms *= 4.0;
        let (v, o) = check(&b, &r, 1.0);
        assert_eq!(v, Verdict::PerfFail, "q{} at {m} ms slowed 4×", i + 1);
        let failing: Vec<&str> = o.iter().filter(|g| !g.pass).map(|g| g.gate.as_str()).collect();
        assert_eq!(failing, ["time.query"], "only the per-query gate: {failing:?}");
        assert_eq!(retry_targets(&o), vec![("snapshot".to_string(), Phase::Query)]);
    }
    // A sub-millisecond query slowed 4× stays under the 2 ms floor.
    let mut r = result(1.0);
    r.runs[0].queries[11].median_ms *= 4.0;
    assert_eq!(check(&b, &r, 1.0).0, Verdict::Pass);
}

#[test]
fn a_row_hash_change_fails_exactly_without_retry() {
    let b = result(1.0);
    let mut r = result(1.0);
    r.runs[1].queries[3].rows_fnv64 = "ffffffffffffffff".into();
    let (v, o) = check(&b, &r, 1.0);
    assert_eq!(v, Verdict::ExactFail);
    let failing: Vec<&gstore_bench::gates::GateOutcome> = o.iter().filter(|g| !g.pass).collect();
    assert!(failing.iter().any(|g| g.gate == "exact.rows" && g.subject == "disk/q4"));
    assert!(failing.iter().any(|g| g.gate == "exact.cross_backend"));
    assert!(retry_targets(&o).is_empty(), "exact failures are never retried");
    // self_check alone sees the cross-backend difference.
    assert_eq!(verdict(&self_check(&r)), Verdict::ExactFail);
}

#[test]
fn calibration_normalizes_timings() {
    let b = result(1.0);
    // A machine twice as slow, measuring everything twice as slow, passes.
    let mut r = result(2.0);
    r.calibration.as_mut().unwrap().score_ms = 20.0;
    assert_eq!(check(&b, &r, 1.0).0, Verdict::Pass);
    // Without the calibration it fails.
    let t = Thresholds {
        normalize_by_calibration: false,
        ..Thresholds::default()
    };
    assert_eq!(verdict(&evaluate(&b, &r, &t, 1.0)), Verdict::PerfFail);
    // The factor is clamped: a 10× slower calibration cannot hide 6× times.
    let mut r = result(6.0);
    r.calibration.as_mut().unwrap().score_ms = 100.0;
    assert_eq!(check(&b, &r, 1.0).0, Verdict::PerfFail);
    // A run's own calibration (merged results) wins over the file's.
    let mut r = result(2.0);
    for run in &mut r.runs {
        run.config.calib_score_ms = Some(20.0);
    }
    assert_eq!(check(&b, &r, 1.0).0, Verdict::Pass);
}

#[test]
fn memory_and_size_gates() {
    let b = result(1.0);
    let mut r = result(1.0);
    r.runs[0].open.as_mut().unwrap().rss_after_open_mib = Some(127.0 * 1.4);
    assert_eq!(check(&b, &r, 1.0).0, Verdict::Pass, "(178+32)/(127+32) = 1.32");
    r.runs[0].open.as_mut().unwrap().rss_after_open_mib = Some(260.0);
    let (v, o) = check(&b, &r, 1.0);
    assert_eq!(v, Verdict::PerfFail);
    assert!(o.iter().any(|g| !g.pass && g.gate == "rss.after_open"));
    assert_eq!(retry_targets(&o), vec![("snapshot".to_string(), Phase::Open)]);

    let mut r = result(1.0);
    r.runs[1].build.as_mut().unwrap().dir_bytes = 87_000_000 * 13 / 10;
    let (v, o) = check(&b, &r, 1.0);
    assert_eq!(v, Verdict::PerfFail);
    assert!(o.iter().any(|g| !g.pass && g.gate == "bytes.dir" && g.subject == "disk"));
}

#[test]
fn build_gate_is_looser_for_disk() {
    let b = result(1.0);
    let mut r = result(1.0);
    r.runs[1].build.as_mut().unwrap().wall_ms = 12_000.0 * 2.3;
    assert_eq!(check(&b, &r, 1.0).0, Verdict::Pass, "disk allows 2.5×");
    let mut r = result(1.0);
    r.runs[0].build.as_mut().unwrap().wall_ms = 12_000.0 * 2.3;
    assert_eq!(check(&b, &r, 1.0).0, Verdict::PerfFail, "snapshot allows 2.0×");
}

#[test]
fn coverage_and_identity_gates() {
    let b = result(1.0);
    // A missing backend.
    let mut r = result(1.0);
    r.runs.pop();
    assert_eq!(check(&b, &r, 1.0).0, Verdict::ExactFail);
    // Another data set.
    let mut r = result(1.0);
    r.workload.data_fnv64 = "00000000000000bb".into();
    assert_eq!(check(&b, &r, 1.0).0, Verdict::ExactFail);
    // A changed query text.
    let mut r = result(1.0);
    r.workload.queries[0].text_fnv64 = "0000000000000001".into();
    assert_eq!(check(&b, &r, 1.0).0, Verdict::ExactFail);
    // triple_num differs from the generated unique triples.
    let mut r = result(1.0);
    r.runs[0].open.as_mut().unwrap().triple_num = 999;
    assert_eq!(check(&b, &r, 1.0).0, Verdict::ExactFail);
    // A failed update round trip, unequal concurrent bodies, an empty or
    // unstable answer.
    for f in [
        (|r: &mut BenchResult| r.runs[0].update.as_mut().unwrap().reopen_ok = false) as fn(&mut BenchResult),
        |r| r.runs[0].conc.as_mut().unwrap().bodies_equal = false,
        |r| r.runs[0].queries[0].stable = false,
        |r| {
            for run in &mut r.runs {
                run.queries[0].rows = 0;
            }
        },
        |r| r.runs[0].update = None,
    ] {
        let mut r = result(1.0);
        f(&mut r);
        assert_eq!(check(&b, &r, 1.0).0, Verdict::ExactFail);
    }
    // Concurrency throughput is report-only.
    let r = result(1.0);
    let mut slow = r.clone();
    slow.runs[0].conc.as_mut().unwrap().points[0].qps = 1.0;
    assert_eq!(check(&b, &slow, 1.0).0, Verdict::Pass);
}

#[test]
fn results_round_trip_through_json() {
    let r = result(1.0);
    let dir = std::env::temp_dir().join(format!("gsb-gates-{}", std::process::id()));
    let p = dir.join("r.json");
    r.save(&p).unwrap();
    let back = BenchResult::load(&p).unwrap();
    // serde_json's float parser may differ in the last bit; compare the rest.
    assert_eq!(back.workload, r.workload);
    assert_eq!(back.runs.len(), r.runs.len());
    for (a, b) in back.runs.iter().zip(&r.runs) {
        assert_eq!(a.backend, b.backend);
        for (x, y) in a.queries.iter().zip(&b.queries) {
            assert_eq!((&x.name, x.rows, &x.rows_fnv64), (&y.name, y.rows, &y.rows_fnv64));
            assert!((x.median_ms - y.median_ms).abs() < 1e-9);
        }
    }
    assert_eq!(verdict(&evaluate(&r, &back, &Thresholds::default(), 1.0)), Verdict::Pass);
    std::fs::write(&p, r#"{"schema":"other"}"#).unwrap();
    assert!(BenchResult::load(&p).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
