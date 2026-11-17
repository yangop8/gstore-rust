//! The update phase sees lost writes (review infra#9): it checkpoints and
//! reopens while the update chunk is inserted (the original count plus the
//! chunk's, and the chunk's delete must remove every chunk triple), then
//! re-checks every query's answer after the final reopen. The test hook
//! `--inject lose-writes:update` makes each checkpoint restore the database
//! as the phase found it — a checkpoint that drops pending inserts and
//! deletes alike, which the old flow (every write undone before its only
//! checkpoint) could not tell from a working one.
//!
//! The runs use the library builder on purpose: the subject is the update
//! phase, not gbuild (tests/cli.rs runs the real gbuild).

use std::path::{Path, PathBuf};
use std::process::Command;

use gstore_bench::schema::BenchResult;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gstore-bench"))
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("gsb-update-{name}-{}", std::process::id()));
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

/// `run` the tiny workload on every compiled backend; (exit code, stdout,
/// stderr, the result when written).
fn run(t: &Tmp, extra: &[&str]) -> (i32, String, String, Option<BenchResult>) {
    let out = t.path("r.json");
    let _ = std::fs::remove_file(&out);
    let backends = if cfg!(feature = "rocksdb") {
        "snapshot,disk,rocksdb"
    } else {
        "snapshot,disk"
    };
    let o = Command::new(exe())
        .args([
            "run",
            "--profile",
            "micro",
            "--workload",
            "tiny",
            "--backends",
            backends,
            "--no-calibrate",
            "--no-conc",
            "--builder",
            "lib",
            "--data-dir",
            &t.path("data"),
            "--work-dir",
            &t.path("work"),
            "--quiet",
            "--out",
            &out,
        ])
        .args(extra)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
        BenchResult::load(Path::new(&out)).ok(),
    )
}

#[test]
fn the_update_phase_reopens_with_the_chunk_and_rechecks_answers() {
    let t = Tmp::new("ok");
    let (code, _, err, r) = run(&t, &[]);
    assert_eq!(code, 0, "{err}");
    let r = r.unwrap();
    for run in &r.runs {
        let u = run.update.as_ref().unwrap();
        assert!(u.round_trip_ok(), "{}: {u:?}", run.backend);
        assert_eq!(
            u.grown_reopen_triple_num,
            Some(285 + u.chunk_triples),
            "{}",
            run.backend
        );
        assert_eq!(u.chunk_deleted_after_reopen, Some(u.chunk_triples));
        assert!(u.chunk_triples > 0);
        assert_eq!(u.grown_reopen_ok, Some(true));
        assert_eq!(u.answers_after_reopen_ok, Some(true));
        assert!(u.reopen_mismatched.is_empty());
        assert_eq!(u.reopen_triple_num, 285);
        assert!(u.grown_checkpoint_ms.is_some() && u.grown_reopen_ms.is_some());
    }
    let left: Vec<_> = std::fs::read_dir(t.path("work")).unwrap().collect();
    assert!(left.is_empty(), "work directory not cleaned: {left:?}");
}

#[test]
fn an_injected_lost_write_fails_the_round_trip() {
    let t = Tmp::new("lost");
    let (code, out, err, r) = run(&t, &["--inject", "lose-writes:update"]);
    // The result is written and fails its own exact check (exit 1).
    assert_eq!(code, 1, "{out}{err}");
    let r = r.expect("the result is written");
    for run in &r.runs {
        let u = run.update.as_ref().unwrap();
        assert_eq!(u.grown_reopen_ok, Some(false), "{}: {u:?}", run.backend);
        assert!(!u.round_trip_ok());
        // The lost chunk: the reopen saw the original count and the delete
        // found nothing to remove.
        assert_eq!(u.grown_reopen_triple_num, Some(285));
        assert_eq!(u.chunk_deleted_after_reopen, Some(0));
        // What the old flow checked still holds: the net-zero reopen count
        // and the answers.
        assert!(u.singles_ok && u.reopen_ok && u.answers_unchanged, "{u:?}");
        assert_eq!(u.answers_after_reopen_ok, Some(true));
    }
    assert!(
        err.contains("exact.update_round_trip") && err.contains("lost writes"),
        "{err}"
    );
    // The hook's copy of the database is gone with the work directory.
    let left: Vec<_> = std::fs::read_dir(t.path("work"))
        .map(|d| d.collect())
        .unwrap_or_default();
    assert!(left.len() <= 1, "{left:?}");
}
