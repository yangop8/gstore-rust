//! End-to-end runs of the `gstore-bench` binary with the `micro` profile and
//! the tiny workload: bless → check passes, the canary exits 1, an injected
//! child crash or timeout exits 2, the data cache regenerates a missing
//! sidecar, results merge, and every backend answers alike.
//!
//! The build phase runs the real `gbuild` from the same target directory
//! when it is there (`cargo test --workspace` builds it; for
//! `--features rocksdb` build it with the feature first, as the bench
//! workflow does). `cargo test -p gstore-bench` alone does not build the
//! gstore binaries; the tests then use the library builder and say so.
//! `GSTORE_BENCH_TEST_BUILDER=cli|lib` forces one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use gstore_bench::gates::{self_check, verdict, Verdict};
use gstore_bench::hash::{file_fnv64, hex};
use gstore_bench::schema::BenchResult;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gstore-bench"))
}

fn bin_dir() -> PathBuf {
    exe().parent().unwrap().to_path_buf()
}

fn gbuild() -> PathBuf {
    bin_dir().join(format!("gbuild{}", std::env::consts::EXE_SUFFIX))
}

/// Whether the `gbuild` next to the harness supports `--rocksdb`.
fn gbuild_has_rocksdb(tmp: &Path) -> bool {
    let dir = tmp.join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let ok = Command::new(gbuild())
        .args(["--rocksdb", "probe"])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

fn builder(tmp: &Path, rocksdb: bool) -> String {
    if let Ok(b) = std::env::var("GSTORE_BENCH_TEST_BUILDER") {
        return b;
    }
    if !gbuild().is_file() {
        eprintln!(
            "note: {} is not built; using the library builder",
            gbuild().display()
        );
        return "lib".into();
    }
    if rocksdb && !gbuild_has_rocksdb(tmp) {
        eprintln!(
            "note: {} lacks --features rocksdb; using the library builder",
            gbuild().display()
        );
        return "lib".into();
    }
    "cli".into()
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("gsb-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the harness; (exit code, stdout, stderr).
fn bench(args: &[String]) -> (i32, String, String) {
    let out = Command::new(exe()).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// The data, work, binary and builder options.
fn common(t: &Tmp, builder: &str) -> Vec<String> {
    vec![
        "--bin-dir".into(),
        bin_dir().to_string_lossy().into_owned(),
        "--builder".into(),
        builder.into(),
        "--data-dir".into(),
        t.path("data"),
        "--work-dir".into(),
        t.path("work"),
        "--quiet".into(),
    ]
}

/// Thresholds with wide timing limits: the timing gates are tested on
/// synthetic results (`tests/gates.rs`); here they would only measure the
/// noise of a debug build on a shared machine.
const RELAXED: &str = r#"{
  "schema": "gstore-bench-gates/1",
  "normalize_by_calibration": true,
  "calibration_clamp": [0.5, 2.0],
  "query": { "floor_ms": 2.0, "max_ratio": 50.0 },
  "geomean": { "max_ratio": 50.0 },
  "build": { "floor_ms": 100.0, "max_ratio": 50.0, "max_ratio_disk": 50.0 },
  "open": { "floor_ms": 20.0, "max_ratio": 50.0 },
  "rss": { "floor_mib": 32.0, "max_ratio": 50.0 },
  "disk_bytes": { "max_ratio": 1.25 },
  "retries": 2
}"#;

fn ok(r: &(i32, String, String), what: &str) {
    assert_eq!(
        r.0, 0,
        "{what}: exit {}\nstdout:\n{}\nstderr:\n{}",
        r.0, r.1, r.2
    );
}

#[test]
fn micro_bless_check_compare_report_and_canary() {
    let t = Tmp::new("micro");
    let b = builder(&t.0, false);
    let c = common(&t, &b);
    let base = t.path("base.json");
    let res = t.path("res.json");
    let relaxed = t.path("relaxed.json");
    std::fs::write(&relaxed, RELAXED).unwrap();

    let mut a = args(&[
        "bless",
        "--profile",
        "micro",
        "--to",
        &base,
        "--provisional",
        "--note",
        "test",
    ]);
    a.extend(c.clone());
    ok(&bench(&a), "bless");
    let bl = BenchResult::load(Path::new(&base)).unwrap();
    assert_eq!(bl.kind, "baseline");
    assert!(bl.provisional);
    assert_eq!(bl.runs.len(), 1);
    let run = &bl.runs[0];
    assert_eq!(run.config.builder, b);
    let rows: Vec<u64> = run.queries.iter().map(|q| q.rows).collect();
    assert_eq!(
        rows,
        [3, 1, 10, 9, 576, 576, 23, 576, 5, 3, 14, 1, 613, 432]
    );
    assert_eq!(run.open.as_ref().unwrap().triple_num, 7_491);
    assert!(run.update.as_ref().unwrap().round_trip_ok());
    let conc = run.conc.as_ref().unwrap();
    assert!(conc.bodies_equal);
    assert!(conc
        .points
        .iter()
        .any(|p| p.endpoint == "sparql" && p.ok > 0));
    assert!(conc.points.iter().any(|p| p.endpoint == "api" && p.ok > 0));
    assert!(
        bl.workload.gen_ms.is_some(),
        "the first run generated the data"
    );

    let mut a = args(&[
        "check",
        "--baseline",
        &base,
        "--thresholds",
        &relaxed,
        "--out",
        &res,
    ]);
    a.extend(c.clone());
    let r = bench(&a);
    ok(&r, "check");
    assert!(r.1.contains("### Gates: PASS"), "{}", r.1);
    let m = BenchResult::load(Path::new(&res)).unwrap();
    assert!(
        m.workload.gen_ms.is_none(),
        "the second run reused the cached data"
    );
    assert_eq!(m.workload.data_fnv64, bl.workload.data_fnv64);

    // Targeted retries re-measure the database the first attempt measured,
    // not the one the update phase (which runs before the gates) changed: a
    // baseline whose open time no machine can match fails the open gate,
    // both retries run, and the run ends as a performance failure (exit 1),
    // its retried opens seeing the original term counts.
    let mut fast = bl.clone();
    fast.runs[0].open.as_mut().unwrap().warm_ms = 1e-6;
    let fast_path = t.path("fast-open.json");
    fast.save(Path::new(&fast_path)).unwrap();
    let strict_open = t.path("strict-open.json");
    std::fs::write(
        &strict_open,
        RELAXED.replace(
            r#""open": { "floor_ms": 20.0, "max_ratio": 50.0 }"#,
            r#""open": { "floor_ms": 0.0, "max_ratio": 2.0 }"#,
        ),
    )
    .unwrap();
    let retried = t.path("retried.json");
    let mut a = args(&[
        "check",
        "--baseline",
        &fast_path,
        "--thresholds",
        &strict_open,
        "--out",
        &retried,
    ]);
    a.extend(c.clone());
    let r = bench(&a);
    assert_eq!(r.0, 1, "retries: {}{}", r.1, r.2);
    assert!(r.1.contains("time.open"), "{}", r.1);
    let rr = BenchResult::load(Path::new(&retried)).unwrap();
    assert_eq!(rr.runs[0].attempts.get("open"), Some(&3));

    // The canary: the baseline against itself, every time tripled.
    let r = bench(&args(&[
        "check",
        "--baseline",
        &base,
        "--from",
        &base,
        "--self-test-slowdown",
        "3",
    ]));
    assert_eq!(r.0, 1, "canary: {}{}", r.1, r.2);
    assert!(r.1.contains("FAIL (performance)"));

    let r = bench(&args(&[
        "compare",
        &base,
        &res,
        "--thresholds",
        &relaxed,
        "--markdown",
    ]));
    ok(&r, "compare");
    let r = bench(&args(&["report", &res, "--markdown", "--compare", &base]));
    ok(&r, "report");
    assert!(r.1.contains("| q14 | 432 |"), "{}", r.1);

    // A changed row hash fails exactly (no timing involved).
    let mut tampered = m.clone();
    tampered.runs[0].queries[3].rows_fnv64 = "0123456789abcdef".into();
    let tp = t.path("tampered.json");
    tampered.save(Path::new(&tp)).unwrap();
    let r = bench(&args(&["compare", &base, &tp, "--thresholds", &relaxed]));
    assert_eq!(r.0, 1);
    assert!(
        r.1.contains("exact.rows") && r.1.contains("FAIL (exact)"),
        "{}",
        r.1
    );
    // The databases are gone.
    let left: Vec<_> = std::fs::read_dir(t.path("work")).unwrap().collect();
    assert!(left.is_empty(), "work directory not cleaned: {left:?}");
}

#[test]
fn injected_crash_and_timeout_exit_2() {
    let t = Tmp::new("inject");
    let c = common(&t, &builder(&t.0, false));
    let base = [
        "run",
        "--profile",
        "micro",
        "--workload",
        "tiny",
        "--no-calibrate",
        "--no-conc",
        "--no-update",
    ];

    let mut a = args(&base);
    a.extend(c.clone());
    a.extend(args(&[
        "--inject",
        "crash:query",
        "--out",
        &t.path("crash.json"),
    ]));
    let r = bench(&a);
    assert_eq!(r.0, 2, "crash: {}{}", r.1, r.2);
    assert!(r.2.contains("crashed"), "{}", r.2);
    assert!(
        !Path::new(&t.path("crash.json")).exists(),
        "no result for a crashed run"
    );

    let mut a = args(&base);
    a.extend(c.clone());
    a.extend(args(&[
        "--inject",
        "timeout:open",
        "--timeout-s",
        "4",
        "--out",
        &t.path("timeout.json"),
    ]));
    let started = Instant::now();
    let r = bench(&a);
    assert_eq!(r.0, 2, "timeout: {}{}", r.1, r.2);
    assert!(r.2.contains("timed out"), "{}", r.2);
    assert!(started.elapsed().as_secs() < 60);
}

#[test]
fn tiny_backends_agree_and_the_data_cache_regenerates() {
    let t = Tmp::new("tiny");
    let rocks = cfg!(feature = "rocksdb");
    let b = builder(&t.0, rocks);
    let c = common(&t, &b);
    let run = |backends: &str, out: &str| {
        let mut a = args(&[
            "run",
            "--profile",
            "micro",
            "--workload",
            "tiny",
            "--no-calibrate",
            "--backends",
            backends,
            "--out",
            out,
        ]);
        a.extend(c.clone());
        bench(&a)
    };
    let a_json = t.path("a.json");
    ok(&run("snapshot", &a_json), "snapshot run");
    let ra = BenchResult::load(Path::new(&a_json)).unwrap();
    assert!(ra.workload.gen_ms.is_some());

    // Without its sidecar the cached file is regenerated.
    std::fs::remove_file(t.path("data/tiny1-x1.nt.json")).unwrap();
    let b_json = t.path("b.json");
    let others = if rocks { "disk,rocksdb" } else { "disk" };
    ok(&run(others, &b_json), "disk run");
    let rb = BenchResult::load(Path::new(&b_json)).unwrap();
    assert!(
        rb.workload.gen_ms.is_some(),
        "regenerated after the sidecar was removed"
    );
    assert_eq!(rb.workload.data_fnv64, ra.workload.data_fnv64);
    for run in &rb.runs {
        assert!(
            run.update.as_ref().unwrap().round_trip_ok(),
            "{}",
            run.backend
        );
        assert!(run.conc.as_ref().unwrap().bodies_equal, "{}", run.backend);
    }

    // Merged, every backend answers alike (the self-check's cross-backend gate).
    let m = t.path("m.json");
    ok(
        &bench(&args(&["merge", "--out", &m, &a_json, &b_json])),
        "merge",
    );
    let merged = BenchResult::load(Path::new(&m)).unwrap();
    assert_eq!(merged.runs.len(), if rocks { 3 } else { 2 });
    let outcomes = self_check(&merged);
    assert_eq!(verdict(&outcomes), Verdict::Pass, "{outcomes:#?}");
    assert!(outcomes.iter().any(|o| o.gate == "exact.cross_backend"));
    // A result without some of the baseline's backends fails, unless
    // --backends limits the comparison.
    assert_eq!(bench(&args(&["compare", &m, &a_json])).0, 1);
    ok(
        &bench(&args(&["compare", &m, &a_json, "--backends", "snapshot"])),
        "compare --backends",
    );
    ok(
        &bench(&args(&[
            "check",
            "--baseline",
            &m,
            "--from",
            &a_json,
            "--backends",
            "snapshot",
        ])),
        "check --from --backends",
    );
    // Merging a result with itself repeats a backend.
    assert_eq!(bench(&args(&["merge", "--out", &m, &a_json, &a_json])).0, 2);
}

#[test]
fn gen_usage_and_calibrate() {
    let t = Tmp::new("gen");
    let out = t.path("micro.nt");
    let r = bench(&args(&[
        "gen",
        "--scale",
        "1",
        "--departments",
        "1",
        "--out",
        &out,
    ]));
    ok(&r, "gen");
    assert!(r.2.contains("unique_triples=7491"), "{}", r.2);
    assert_eq!(
        hex(file_fnv64(Path::new(&out)).unwrap().0),
        "d5f50a8e697fc892"
    );

    assert_eq!(bench(&args(&["bogus"])).0, 2);
    assert_eq!(
        bench(&args(&[
            "run",
            "--backends",
            "nope",
            "--data-dir",
            &t.path("d")
        ]))
        .0,
        2
    );
    assert_eq!(bench(&args(&["run", "--no-such-option"])).0, 2);
    assert_eq!(bench(&args(&["compare", "only-one.json"])).0, 2);
    let r = bench(&args(&["calibrate", "--quick"]));
    ok(&r, "calibrate");
    assert!(r.1.contains("calib/1"), "{}", r.1);
}

/// The WatDiv-like workload through the harness: the templates are bound
/// by an `instantiate` child (and cached next to the data), every phase runs
/// the bound queries, snapshot and disk answer alike, and the mixed phase
/// writes while readers read.
#[test]
fn watdiv_micro_on_snapshot_and_disk() {
    let t = Tmp::new("watdiv");
    let c = common(&t, &builder(&t.0, false));
    let out = t.path("w.json");
    let mut a = args(&[
        "run",
        "--profile",
        "micro",
        "--workload",
        "watdiv",
        "--shrink",
        "10",
        "--backends",
        "snapshot,disk",
        "--no-calibrate",
        "--query-procs",
        "1",
        "--reps",
        "1",
        "--conc-clients",
        "2",
        "--out",
        &out,
    ]);
    a.extend(c.clone());
    ok(&bench(&a), "watdiv run");
    let r = BenchResult::load(Path::new(&out)).unwrap();
    assert_eq!(r.workload.id, "watdiv-like");
    assert!(r.extra_workloads.is_empty(), "--workload drops the extras");
    assert_eq!(r.workload.queries.len(), 20);
    assert!(r.workload.queries.iter().all(|q| q.tries.is_some()));
    assert!(r
        .workload
        .queries
        .iter()
        .filter(|q| !q.name.starts_with('C'))
        .all(|q| q.bindings.len() == 1));
    let outcomes = self_check(&r);
    assert_eq!(verdict(&outcomes), Verdict::Pass, "{outcomes:#?}");
    assert!(outcomes
        .iter()
        .any(|o| o.gate == "exact.cross_backend" && o.subject == "disk/C3"));
    for run in &r.runs {
        assert_eq!(run.queries.len(), 20, "{}", run.backend);
        assert!(run.update.as_ref().unwrap().round_trip_ok());
        let m = run.mixed.as_ref().unwrap();
        assert!(m.bodies_equal && m.writes_ok, "{}: {m:?}", run.backend);
        assert_eq!(m.points.len(), 4, "2 endpoints × check_point off/on");
        assert!(m.points.iter().all(|p| p.writes > 0 && p.reads > 0));
        let b = m.begin.as_ref().expect("the /api begin point");
        assert_eq!(b.samples, 3);
        assert!(b.median_ms > 0.0 && b.max_ms >= b.median_ms);
    }
    // The bound queries are cached next to the data.
    assert!(Path::new(&t.path("data/watdiv-like1-x1-s0-k10.queries.json")).is_file());
    // `queries` prints them from the cache.
    let q = bench(&args(&[
        "queries",
        "--workload",
        "watdiv",
        "--shrink",
        "10",
        "--data-dir",
        &t.path("data"),
        "--work-dir",
        &t.path("work"),
        "--quiet",
    ]));
    ok(&q, "queries");
    assert_eq!(q.1.matches("SELECT").count(), 20, "{}", q.1);
    let left: Vec<_> = std::fs::read_dir(t.path("work")).unwrap().collect();
    assert!(left.is_empty(), "work directory not cleaned: {left:?}");
}

/// The CI case before the smoke baseline is re-blessed: a run that measures
/// a workload the baseline lacks reports it as new and passes. Labelled
/// runs merge into one file and compare with the unlabelled baseline run.
#[test]
fn new_workloads_pass_check_and_labelled_runs_merge() {
    let t = Tmp::new("newwl");
    let c = common(&t, &builder(&t.0, false));
    let relaxed = t.path("relaxed.json");
    std::fs::write(&relaxed, RELAXED).unwrap();
    let quick = [
        "--profile",
        "micro",
        "--workload",
        "tiny",
        "--no-calibrate",
        "--no-conc",
        "--no-update",
        "--query-procs",
        "1",
    ];
    let base = t.path("base.json");
    let mut a = args(&["bless", "--to", &base]);
    a.extend(args(&quick));
    a.extend(c.clone());
    ok(&bench(&a), "bless");
    let res = t.path("res.json");
    let mut a = args(&[
        "check",
        "--baseline",
        &base,
        "--thresholds",
        &relaxed,
        "--extra",
        "watdiv:1/10",
        "--out",
        &res,
    ]);
    a.extend(args(&quick));
    a.extend(c.clone());
    let r = bench(&a);
    ok(&r, "check with a new workload");
    assert!(r.1.contains("### Gates: PASS"), "{}", r.1);
    assert!(
        r.1.contains("new workload(s) not in the baseline"),
        "{}",
        r.1
    );
    assert!(
        r.1.contains("| new | info.new | watdiv-like:workload |"),
        "{}",
        r.1
    );
    assert!(r.1.contains("### watdiv-like scale 1 (÷10)"), "{}", r.1);
    let m = BenchResult::load(Path::new(&res)).unwrap();
    assert_eq!(m.extra_workloads.len(), 1);
    assert_eq!(m.extra_workloads[0].runs[0].queries.len(), 20);

    // Two labelled runs of one backend share a file.
    let run_labelled = |label: &str| {
        let out = t.path(&format!("{label}.json"));
        let mut a = args(&["run", "--label", label, "--out", &out]);
        a.extend(args(&quick));
        a.extend(c.clone());
        ok(&bench(&a), label);
        out
    };
    let (x, y) = (run_labelled("x"), run_labelled("y"));
    let both = t.path("both.json");
    ok(&bench(&args(&["merge", "--out", &both, &x, &y])), "merge");
    let merged = BenchResult::load(Path::new(&both)).unwrap();
    let keys: Vec<String> = merged.runs.iter().map(|r| r.key()).collect();
    assert_eq!(keys, ["snapshot@x", "snapshot@y"]);
    // Both are gated against the baseline's unlabelled snapshot run.
    let r = bench(&args(&["compare", &base, &both, "--thresholds", &relaxed]));
    ok(&r, "compare labelled");
    assert!(
        r.1.contains("snapshot@x/t1") && r.1.contains("snapshot@y/t1"),
        "{}",
        r.1
    );
    // The same label twice is refused.
    assert_eq!(bench(&args(&["merge", "--out", &both, &x, &x])).0, 2);
}

/// The LUBM micro data on the disk layout (slow in a debug build).
#[test]
#[ignore]
fn micro_on_disk() {
    let t = Tmp::new("microdisk");
    let mut a = args(&[
        "run",
        "--profile",
        "micro",
        "--backends",
        "disk",
        "--out",
        &t.path("d.json"),
    ]);
    a.extend(common(&t, &builder(&t.0, false)));
    ok(&bench(&a), "micro disk");
    let r = BenchResult::load(Path::new(&t.path("d.json"))).unwrap();
    let rows: Vec<u64> = r.runs[0].queries.iter().map(|q| q.rows).collect();
    assert_eq!(
        rows,
        [3, 1, 10, 9, 576, 576, 23, 576, 5, 3, 14, 1, 613, 432]
    );
}

/// `--bin-dir` relative to the current directory (`gbuild` runs elsewhere).
#[test]
fn a_relative_bin_dir_works() {
    let t = Tmp::new("relative");
    let b = builder(&t.0, false);
    let dir = bin_dir();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let out = Command::new(exe())
        .args([
            "run",
            "--profile",
            "micro",
            "--workload",
            "tiny",
            "--no-calibrate",
            "--no-conc",
            "--no-update",
            "--bin-dir",
            &name,
            "--builder",
            &b,
            "--data-dir",
            &t.path("data"),
            "--work-dir",
            &t.path("work"),
            "--quiet",
            "--out",
            &t.path("r.json"),
        ])
        .current_dir(dir.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = BenchResult::load(Path::new(&t.path("r.json"))).unwrap();
    assert_eq!(r.runs[0].open.as_ref().unwrap().triple_num, 285);
}

/// The parent pid of `pid` (from `/proc/<pid>/stat`).
#[cfg(target_os = "linux")]
fn ppid_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Whether `pid` has ended (gone, or a zombie nobody reaped yet).
#[cfg(target_os = "linux")]
fn ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Killing the harness (Ctrl-C, `timeout`, a cancelled CI job) ends its
/// phase child and the child's `gbuild` too: each runs in its own process
/// group (so a phase timeout can kill it whole), out of reach of the
/// terminal's or `timeout`'s signal, and they used to run on, writing into
/// the work directory and holding an inherited `flock`.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_harness_leaves_no_phase_child_or_gbuild_behind() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    let t = Tmp::new("orphans");
    let bin = t.path("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pidfile = t.path("gbuild.pid");
    let fake = Path::new(&bin).join("gbuild");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho $$ > {pidfile}.tmp\nmv {pidfile}.tmp {pidfile}\nexec sleep 120\n"),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut harness = Command::new(exe())
        .args([
            "run",
            "--profile",
            "micro",
            "--workload",
            "tiny",
            "--backends",
            "disk",
            "--no-calibrate",
            "--no-conc",
            "--bin-dir",
            &bin,
            "--builder",
            "cli",
            "--data-dir",
            &t.path("data"),
            "--work-dir",
            &t.path("work"),
            "--quiet",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let gbuild = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            break pid;
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = harness.kill();
            panic!("the fake gbuild never started");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let child = ppid_of(gbuild).expect("the phase child");
    assert_eq!(ppid_of(child), Some(harness.id()));
    // SIGTERM, as timeout(1) sends: the harness ends without cleaning up.
    let st = Command::new("kill")
        .args(["-TERM", &harness.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    let _ = harness.wait();
    let started = Instant::now();
    while !(ended(child) && ended(gbuild)) {
        if started.elapsed() > Duration::from_secs(10) {
            let _ = Command::new("kill")
                .args(["-KILL", &child.to_string(), &gbuild.to_string()])
                .status();
            panic!("the phase child or gbuild outlived the harness");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
