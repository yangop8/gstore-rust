//! Argument checking of the `gstore-bench` binary: a command that takes
//! options only refuses a stray positional argument (review infra#5), and
//! the concurrency and mixed-workload point durations are validated before
//! anything is generated or built (review infra#16). Every case exits 2 (a
//! usage error) without creating the data or work directory.

use std::path::{Path, PathBuf};
use std::process::Command;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gstore-bench"))
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("gsb-args-{name}-{}", std::process::id()));
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

/// Run the harness; (exit code, stderr).
fn bench(args: &[&str]) -> (i32, String) {
    let out = Command::new(exe()).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn smoke() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("baselines/smoke.json")
        .to_string_lossy()
        .into_owned()
}

/// A usage error naming `word`, before anything was written.
fn refused(t: &Tmp, args: &[&str], word: &str) {
    let r = bench(args);
    assert_eq!(r.0, 2, "{args:?}: {}", r.1);
    assert!(r.1.contains(word), "{args:?}: {}", r.1);
    assert!(r.1.contains("usage:"), "{args:?}: {}", r.1);
    for d in ["data", "work", "out.json", "gen.nt", "bless.json"] {
        assert!(!Path::new(&t.path(d)).exists(), "{args:?} wrote {d}");
    }
}

#[test]
fn stray_positional_arguments_are_usage_errors() {
    let t = Tmp::new("pos");
    let (data, work, out) = (t.path("data"), t.path("work"), t.path("out.json"));
    let base = smoke();
    let dirs = ["--data-dir", &data, "--work-dir", &work];
    // The review's repro: the trailing result was ignored and --from was
    // gated against the default baseline.
    let other = "bench/results/phase6-final-lubm8.json";
    let mut a = vec!["check", "--baseline", &base, "--from", &base, other];
    refused(&t, &a, "unexpected argument 'bench/results/phase6-final-lubm8.json'");
    a.insert(0, "ci");
    a.remove(1);
    refused(&t, &a, "unexpected argument");
    // A forgotten --baseline: measured against the default one.
    let mut a = vec!["check", "my-baseline.json", "--baseline", &base];
    a.extend(dirs);
    refused(&t, &a, "unexpected argument 'my-baseline.json'");
    // `run --profile reference 38` ran univ 8.
    let mut a = vec!["run", "--profile", "micro", "38", "--out", &out];
    a.extend(dirs);
    refused(&t, &a, "unexpected argument '38'");
    let gen = t.path("gen.nt");
    refused(
        &t,
        &["gen", "--scale", "1", "--departments", "1", "tiny", "--out", &gen],
        "unexpected argument 'tiny'",
    );
    let mut a = vec!["queries", "watdiv"];
    a.extend(dirs);
    refused(&t, &a, "unexpected argument 'watdiv'");
    let bl = t.path("bless.json");
    refused(
        &t,
        &["bless", "--from", &base, "--to", &bl, "extra"],
        "unexpected argument 'extra'",
    );
    refused(&t, &["calibrate", "--quick", "now"], "unexpected argument 'now'");
    // Commands with positionals keep them.
    let r = bench(&["compare", &base, &base]);
    assert_eq!(r.0, 0, "{}", r.1);
}

#[test]
fn point_durations_are_checked_before_anything_runs() {
    let t = Tmp::new("secs");
    let (data, work, out) = (t.path("data"), t.path("work"), t.path("out.json"));
    let quick = [
        "run",
        "--profile",
        "micro",
        "--workload",
        "tiny",
        "--no-calibrate",
        "--builder",
        "lib",
        "--data-dir",
        &data,
        "--work-dir",
        &work,
        "--out",
        &out,
    ];
    for (opt, v) in [
        ("--conc-secs", "-1"),
        ("--conc-secs", "NaN"),
        ("--conc-secs", "inf"),
        ("--conc-secs", "1e9"),
        ("--mixed-secs", "-1"),
        ("--mixed-secs", "NaN"),
        ("--mixed-secs", "1e9"),
    ] {
        let mut a = quick.to_vec();
        a.extend([opt, v]);
        refused(&t, &a, &format!("{opt}: "));
    }
    let mut a = quick.to_vec();
    a.extend(["--mixed-secs", "3600", "--mixed-write-ms", "1"]);
    refused(&t, &a, "writes per point");
    let mut a = quick.to_vec();
    a.extend(["--conc-clients", "0"]);
    refused(&t, &a, "--conc-clients");
}
