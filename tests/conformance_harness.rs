//! `scripts/conformance.sh`'s own bookkeeping, run against the binaries of
//! this test build (no upstream suite runs: only the Python one is selected,
//! and a `python3` without `requests` makes it skip).
//!
//! * review infra#11: a suite that cannot run (the Python SDK suite without
//!   python3's `requests`) used to count as passed — "all suites passed",
//!   exit 0 — so CI stayed green with the suite silently gone. A skipped
//!   suite now fails the run unless `--allow-skip` accepts it.
//! * review tests-b#17: the script picked the server's port by binding a
//!   probe socket in python3, closing it and starting gserver on that port
//!   later (falling back to a fixed port without python3), so the port could
//!   be taken in between. The server now binds a port of the kernel's choice
//!   (`port=0`) and the script writes the port the server reports into
//!   `conf/conf.ini` for the upstream scripts.
//!
//! Every run gets its own `TMPDIR` below the test target directory (the
//! script's server home lives there), removed afterwards, also on failure.
#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "conformance_harness_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("shim")).unwrap();
        fs::create_dir_all(p.join("tmp")).unwrap();
        TmpDir(p)
    }

    /// A `python3` on `PATH` without the `requests` module (any `-c` program
    /// fails), which prints `probe_port` when asked to bind a probe socket —
    /// the way the script used to pick its port.
    fn python3_shim(&self, probe_port: Option<u16>) {
        let body = match probe_port {
            Some(p) => {
                format!("#!/bin/sh\ncase \"$*\" in *socket*) echo {p}; exit 0 ;; esac\nexit 1\n")
            }
            None => "#!/bin/sh\nexit 1\n".to_string(),
        };
        let shim = self.0.join("shim/python3");
        fs::write(&shim, body).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `bash scripts/conformance.sh --no-build ARGS` with this build's binaries,
/// the shim first on `PATH` and `TMPDIR` inside `dir`.
fn conformance(dir: &TmpDir, args: &[&str]) -> (Output, String) {
    let gserver = Path::new(env!("CARGO_BIN_EXE_gserver"));
    let bin_dir = gserver.parent().unwrap();
    let target = bin_dir.parent().unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/conformance.sh"))
        .arg("--no-build");
    if bin_dir.file_name().is_some_and(|n| n == "release") {
        cmd.arg("--release");
    }
    let path = format!(
        "{}:{}",
        dir.0.join("shim").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = cmd
        .args(args)
        .env("PATH", path)
        .env("TMPDIR", dir.0.join("tmp"))
        .env("CARGO_TARGET_DIR", target)
        .env_remove("GSTORE_GSERVER_BACKGROUND")
        .env_remove("GSTORE_GSERVER_SUPERVISOR")
        .stdin(Stdio::null())
        .output()
        .expect("run bash scripts/conformance.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, text)
}

/// The summary line of `suite`: its words after the suite name.
fn summary_of<'a>(text: &'a str, suite: &str) -> Option<Vec<&'a str>> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next() == Some(suite)).then(|| w.collect())
    })
}

/// The port of the summary header `summary (gserver on port N)`.
fn summary_port(text: &str) -> Option<u16> {
    let rest = text.split("summary (gserver on port ").nth(1)?;
    rest.split(')').next()?.parse().ok()
}

#[test]
fn a_skipped_suite_fails_the_run_unless_allowed() {
    let dir = TmpDir::new("skip");
    dir.python3_shim(None);
    let (out, text) = conformance(&dir, &["python"]);
    assert_eq!(
        summary_of(&text, "python").as_deref(),
        Some(&["SKIP"][..]),
        "{text}"
    );
    // review infra#11: this used to print "all suites passed" and exit 0.
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("conformance: FAILED: suite(s) skipped: python"),
        "{text}"
    );
    assert!(!text.contains("all suites passed"), "{text}");

    let dir = TmpDir::new("allow");
    dir.python3_shim(None);
    let (out, text) = conformance(&dir, &["--allow-skip", "python"]);
    assert!(out.status.success(), "{text}");
    assert_eq!(
        summary_of(&text, "python").as_deref(),
        Some(&["SKIP"][..]),
        "{text}"
    );
    assert!(text.contains("skipped (--allow-skip): python"), "{text}");
    assert!(!text.contains("all suites passed"), "{text}");
}

#[test]
fn the_server_binds_its_own_port_and_the_scripts_read_it_from_conf_ini() {
    // A port that is free when probed but taken by the time the server
    // binds it — the race: the old probe hands out this port, held here.
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_port = taken.local_addr().unwrap().port();
    let dir = TmpDir::new("port");
    dir.python3_shim(Some(taken_port));
    let (out, text) = conformance(&dir, &["--allow-skip", "--keep", "python"]);
    assert!(out.status.success(), "{text}");
    let port = summary_port(&text).unwrap_or_else(|| panic!("no port in the summary: {text}"));
    assert_ne!(port, taken_port, "{text}");
    assert_ne!(port, 0, "{text}");
    // The kept server home: its conf.ini names the port the server bound,
    // in the first line containing "port=" (where the upstream scripts look).
    let home = fs::read_dir(dir.0.join("tmp"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("gstore-conformance."))
        })
        .unwrap_or_else(|| panic!("no kept server home: {text}"));
    let conf = fs::read_to_string(home.join("conf/conf.ini")).unwrap();
    let first = conf.lines().find(|l| l.contains("port=")).unwrap();
    assert_eq!(first, format!("port={port}"), "{text}");
    drop(taken);
}
