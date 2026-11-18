//! `gbuild` without `--force` never rebuilds over a database another
//! process finished between its "is the directory new?" check and its
//! claim of the directory (round-4 review of bulkcli#2's fix).
//!
//! The check and the claim are two steps; the claim keeps every other
//! process out from then on, but a database built and released entirely
//! between them was rebuilt instead of refused. `gbuild` now checks again
//! under the claim. A debug-build hook (`GSTORE_GBUILD_PARK_BEFORE_CLAIM`)
//! parks it between the two steps on a FIFO, so the test builds the same
//! name in that gap deterministically.

#![cfg(all(unix, debug_assertions))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gstore::Database;

/// Waits that only guard against a hang (never the assertion).
const HANG_GUARD: Duration = Duration::from_secs(120);

const FIRST: &str = "<http://e/s> <http://e/p> <http://e/first> .\n";
const SECOND: &str = "<http://e/s> <http://e/p> <http://e/second1> .\n\
                      <http://e/s> <http://e/p> <http://e/second2> .\n";

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_gbuild_recheck_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(fs::canonicalize(&p).unwrap())
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A child process killed when dropped (a failing test leaves none behind).
struct Child(Option<std::process::Child>);

impl Child {
    fn wait(mut self) -> Output {
        self.0
            .take()
            .unwrap()
            .wait_with_output()
            .expect("wait for the child")
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn mkfifo(path: &Path) {
    let st = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(st.success(), "mkfifo {}", path.display());
}

/// Open the FIFO `path` for writing once a reader has opened it (the open
/// blocks until then).
fn open_fifo_writer(path: &Path) -> fs::File {
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(fs::OpenOptions::new().write(true).open(&p));
    });
    rx.recv_timeout(HANG_GUARD)
        .expect("gbuild reached its park point")
        .unwrap()
}

fn gbuild(cwd: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gbuild"));
    c.args(args)
        .current_dir(cwd)
        .env_remove("GSTORE_GBUILD_PARK_BEFORE_CLAIM")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

#[test]
fn a_database_finished_before_the_claim_is_refused_and_kept() {
    let base = TmpDir::new("race");
    fs::write(base.0.join("first.nt"), FIRST).unwrap();
    fs::write(base.0.join("second.nt"), SECOND).unwrap();
    let park = base.0.join("park");
    mkfifo(&park);
    let dir = base.0.join("race.db");

    // The second gbuild finds the name new, then parks before its claim.
    let mut parked = gbuild(&base.0, &["race", "second.nt"]);
    parked.env("GSTORE_GBUILD_PARK_BEFORE_CLAIM", &park);
    let parked = Child(Some(parked.spawn().unwrap()));
    let release = open_fifo_writer(&park);
    assert!(
        !dir.exists(),
        "the parked gbuild claimed the directory early"
    );

    // Another process builds the same name, start to finish, in the gap.
    let first = gbuild(&base.0, &["race", "first.nt"]).output().unwrap();
    assert!(first.status.success(), "{}", text(&first));
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 1);

    // The parked gbuild goes on: it must refuse, as it would have had the
    // database been there when it started, and leave it as it is.
    drop(release);
    let out = parked.wait();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists"),
        "{}",
        text(&out)
    );
    assert_eq!(
        Database::open(&dir).unwrap().triple_num(),
        1,
        "the parked gbuild rebuilt the other process's database: {}",
        text(&out)
    );
}

/// With `--force` the parked gbuild rebuilds, as it would have anyway.
#[test]
fn a_forced_gbuild_still_rebuilds_after_the_gap() {
    let base = TmpDir::new("force");
    fs::write(base.0.join("first.nt"), FIRST).unwrap();
    fs::write(base.0.join("second.nt"), SECOND).unwrap();
    let park = base.0.join("park");
    mkfifo(&park);
    let dir = base.0.join("race.db");
    let mut parked = gbuild(&base.0, &["--force", "race", "second.nt"]);
    parked.env("GSTORE_GBUILD_PARK_BEFORE_CLAIM", &park);
    let parked = Child(Some(parked.spawn().unwrap()));
    let release = open_fifo_writer(&park);
    let first = gbuild(&base.0, &["race", "first.nt"]).output().unwrap();
    assert!(first.status.success(), "{}", text(&first));
    drop(release);
    let out = parked.wait();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 2);
}
