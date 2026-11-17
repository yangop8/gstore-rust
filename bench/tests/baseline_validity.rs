//! What may be a baseline (review infra#10): `bless` refuses a result
//! measured by a debug build, from a dirty tree, with the library builder,
//! with gbuild from a debug directory, on a busy machine or with noisy query
//! processes — unless `--provisional`, which records why — and `ci` refuses
//! to gate against a provisional or unfit baseline unless
//! `--allow-provisional` (`check` warns). A result measured with another
//! builder, other repetitions or another number of query processes than
//! the baseline fails `exact.config`.

use std::path::{Path, PathBuf};
use std::process::Command;

use gstore_bench::gates::{baseline_problems, evaluate, verdict, Thresholds, Verdict};
use gstore_bench::schema::{BackendRun, BenchResult};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gstore-bench"))
}

fn smoke() -> BenchResult {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines/smoke.json");
    BenchResult::load(&p).unwrap()
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("gsb-valid-{name}-{}", std::process::id()));
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
fn bench(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(exe()).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn every_run(r: &mut BenchResult) -> Vec<&mut BackendRun> {
    let mut v: Vec<&mut BackendRun> = r.runs.iter_mut().collect();
    for x in &mut r.extra_workloads {
        v.extend(x.runs.iter_mut());
    }
    v
}

type Spoil = fn(&mut BenchResult);

/// Each way a result can be unfit, and a word of the refusal.
fn spoilers() -> Vec<(&'static str, Spoil, &'static str)> {
    vec![
        (
            "debug build",
            |r| r.env.debug_build = true,
            "debug build of gstore-bench",
        ),
        (
            "dirty tree",
            |r| r.env.git_dirty = Some(true),
            "uncommitted changes",
        ),
        (
            "no commit",
            |r| {
                r.env.git_commit = None;
                r.env.git_dirty = None;
            },
            "no git commit",
        ),
        (
            "library builder",
            |r| r.extra_workloads[0].runs[1].config.builder = "lib".into(),
            "watdiv-like:disk was built by the 'lib' builder",
        ),
        (
            "debug gbuild",
            |r| r.runs[0].config.bin_dir = Some("target/debug".into()),
            "a debug build",
        ),
        (
            "busy machine",
            |r| r.env.loadavg = Some([7.5, 6.0, 5.0]),
            "a busy machine: load average 7.50 on 4 CPUs",
        ),
        (
            "noisy processes",
            |r| {
                for q in &mut r.runs[1].queries {
                    let lo = q.proc_medians_ms[0];
                    q.proc_medians_ms[2] = lo * 2.5;
                }
            },
            "a noisy machine: disk's query processes spread",
        ),
    ]
}

#[test]
fn the_smoke_baseline_is_fit_and_each_spoiler_is_named() {
    let t = Thresholds::default();
    let b = smoke();
    assert_eq!(baseline_problems(&b, &t), Vec::<String>::new());
    for (what, spoil, word) in spoilers() {
        let mut r = b.clone();
        spoil(&mut r);
        let p = baseline_problems(&r, &t);
        assert_eq!(p.len(), 1, "{what}: {p:?}");
        assert!(p[0].contains(word), "{what}: {p:?}");
    }
}

#[test]
fn bless_refuses_unfit_results_unless_provisional() {
    let t = Tmp::new("bless");
    let fit = t.path("fit.json");
    smoke().save(Path::new(&fit)).unwrap();
    let out = t.path("out.json");
    let r = bench(&["bless", "--from", &fit, "--to", &out]);
    assert_eq!(r.0, 0, "a fit result blesses: {}", r.2);
    let blessed = BenchResult::load(Path::new(&out)).unwrap();
    assert!(!blessed.provisional);
    std::fs::remove_file(&out).unwrap();

    for (what, spoil, word) in spoilers() {
        let mut bad = smoke();
        spoil(&mut bad);
        let from = t.path("bad.json");
        bad.save(Path::new(&from)).unwrap();
        let r = bench(&["bless", "--from", &from, "--to", &out]);
        assert_eq!(r.0, 1, "{what}: bless must refuse\n{}", r.2);
        assert!(r.2.contains(word), "{what}: {}", r.2);
        assert!(r.2.contains("refusing to bless"), "{what}: {}", r.2);
        assert!(!Path::new(&out).exists(), "{what}: nothing written");
        // --provisional blesses it, marked, with the reason in the note.
        let r = bench(&[
            "bless",
            "--from",
            &from,
            "--to",
            &out,
            "--provisional",
            "--note",
            "test",
        ]);
        assert_eq!(r.0, 0, "{what}: {}", r.2);
        let p = BenchResult::load(Path::new(&out)).unwrap();
        assert!(p.provisional, "{what}");
        let note = p.note.unwrap_or_default();
        assert!(note.starts_with("test (provisional: "), "{what}: {note}");
        assert!(note.contains(word), "{what}: {note}");
        std::fs::remove_file(&out).unwrap();
    }
}

#[test]
fn ci_refuses_a_provisional_or_unfit_baseline() {
    let t = Tmp::new("ci");
    let mut prov = smoke();
    prov.provisional = true;
    let p = t.path("prov.json");
    prov.save(Path::new(&p)).unwrap();
    let mut debug = smoke();
    debug.env.debug_build = true;
    let d = t.path("debug.json");
    debug.save(Path::new(&d)).unwrap();
    for (base, word) in [(&p, "marked provisional"), (&d, "debug build")] {
        // Refused before anything is measured.
        let r = bench(&[
            "ci",
            "--baseline",
            base,
            "--data-dir",
            &t.path("data"),
            "--work-dir",
            &t.path("work"),
        ]);
        assert_eq!(r.0, 2, "{base}: {}", r.2);
        assert!(
            r.2.contains("not fit to gate against") && r.2.contains(word),
            "{}",
            r.2
        );
        assert!(!Path::new(&t.path("data")).exists());
        // --allow-provisional gates anyway; check only warns.
        let r = bench(&[
            "ci",
            "--baseline",
            base,
            "--from",
            base,
            "--allow-provisional",
        ]);
        assert_eq!(r.0, 0, "{}", r.2);
        let r = bench(&["check", "--baseline", base, "--from", base]);
        assert_eq!(r.0, 0, "{}", r.2);
        assert!(r.2.contains("warning: the baseline"), "{}", r.2);
    }
    // The committed baseline is fit: no warning.
    let fit = t.path("fit.json");
    smoke().save(Path::new(&fit)).unwrap();
    let r = bench(&["ci", "--baseline", &fit, "--from", &fit]);
    assert_eq!(r.0, 0, "{}", r.2);
    assert!(!r.2.contains("warning"), "{}", r.2);
}

#[test]
fn another_builder_or_sampling_fails_exact_config() {
    let t = Thresholds::default();
    let b = smoke();
    for (what, change) in [
        (
            "builder",
            (|r: &mut BackendRun| r.config.builder = "lib".into()) as fn(&mut BackendRun),
        ),
        ("reps", |r| r.config.reps = 5),
        ("query processes", |r| r.config.query_procs = 1),
    ] {
        let mut m = b.clone();
        change(every_run(&mut m)[2]);
        let o = evaluate(&b, &m, &t, 1.0);
        assert_eq!(verdict(&o), Verdict::ExactFail, "{what}");
        let f: Vec<&str> = o
            .iter()
            .filter(|g| !g.pass)
            .map(|g| g.subject.as_str())
            .collect();
        assert_eq!(f, ["watdiv-like:snapshot"], "{what}");
        assert!(o
            .iter()
            .any(|g| !g.pass && g.gate == "exact.config" && g.detail.contains(what)));
    }
    // Files that did not record the configuration are not compared.
    let mut old = b.clone();
    for r in every_run(&mut old) {
        r.config = Default::default();
    }
    let o = evaluate(&old, &b, &t, 1.0);
    assert!(!o.iter().any(|g| g.gate == "exact.config"));
    assert_eq!(verdict(&o), Verdict::Pass);
}
