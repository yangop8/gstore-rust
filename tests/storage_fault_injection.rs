//! Write failures on the on-disk store's commit path, injected by the
//! operating system (review C-1 / M1: durability#1, disk#1, disk#12,
//! durability#16).
//!
//! A process whose `RLIMIT_FSIZE` is the current size of `kvstore.kv` (with
//! `SIGXFSZ` ignored) can write a write-ahead log smaller than that, but any
//! write that grows the main file fails with `EFBIG` — a realistic, exact
//! stand-in for a disk filling up while a commit applies its batch. The limit
//! is per process, so each scenario runs in a child process: a re-run of
//! this test binary limited to one `#[ignore]`d child test, or a `gserver`
//! child limited in `pre_exec`.
//!
//! The review's scenario: a first update fails while its committed batch is
//! applied, a second (larger) one fails too, then the database is dropped
//! cleanly or the process dies. Before the fix the second commit truncated
//! the only complete log and tore it, so the database no longer opened at
//! all ("failed to fill whole buffer"), the pre-existing triples included.
//! Now the first failure leaves the store failed, nothing is written after
//! it, and a reopen replays the complete log: the first update is either
//! wholly there or wholly absent, never half.

#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult};

// ---- helpers -------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_fault_injection_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The file-size limit and the signal it raises (Linux, 64-bit `rlim_t`).
mod rlimit {
    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }

    extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
        fn signal(sig: i32, handler: usize) -> usize;
    }

    const RLIMIT_FSIZE: i32 = 1;
    const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;

    /// Ignore `SIGXFSZ`, so exceeding the limit fails the write with `EFBIG`
    /// instead of killing the process. Async-signal-safe (`pre_exec`).
    pub fn ignore_sigxfsz() -> std::io::Result<()> {
        // SAFETY: installs the predefined SIG_IGN disposition.
        if unsafe { signal(SIGXFSZ, SIG_IGN) } == usize::MAX {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Set the soft file-size limit of this process to `bytes` (the hard
    /// limit stays, so the soft one can be raised again). Async-signal-safe.
    pub fn limit_file_size(bytes: u64) -> std::io::Result<()> {
        let mut cur = RLimit { cur: 0, max: 0 };
        // SAFETY: plain syscall wrappers on a properly sized struct.
        unsafe {
            if getrlimit(RLIMIT_FSIZE, &mut cur) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let new = RLimit {
                cur: bytes.min(cur.max),
                max: cur.max,
            };
            if setrlimit(RLIMIT_FSIZE, &new) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

const BASE: usize = 3000;
const U1: usize = 300;
const U2: usize = 4000;

/// The base data: `n` triples `<s{i}> <p> <o{i}>`.
fn base_nt(n: usize) -> String {
    (0..n)
        .map(|i| format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"))
        .collect()
}

/// An `INSERT DATA` of `n` new triples with predicate `<http://ex/q/{tag}>`.
fn insert(tag: &str, n: usize) -> String {
    let body: String = (0..n)
        .map(|i| format!("<http://ex/{tag}/s{i}> <http://ex/q/{tag}> \"{tag} value {i}\" . "))
        .collect();
    format!("INSERT DATA {{ {body} }}")
}

/// Build the base on-disk database at `dir`.
fn build_base(root: &Path, dir: &Path) {
    let nt = root.join("base.nt");
    fs::write(&nt, base_nt(BASE)).unwrap();
    Database::build_disk(dir, &[&nt]).unwrap();
    fs::remove_file(&nt).unwrap();
}

fn options(mode: DiskQueryMode) -> OpenOptions {
    OpenOptions {
        disk_query_mode: mode,
        ..OpenOptions::default()
    }
}

fn mode_named(name: &str) -> DiskQueryMode {
    match name {
        "stream" => DiskQueryMode::Stream,
        _ => DiskQueryMode::Materialize,
    }
}

fn count(db: &mut Database, pattern: &str) -> u64 {
    let q = format!("SELECT (COUNT(*) AS ?n) WHERE {{ {pattern} }}");
    match db.query(&q).unwrap() {
        QueryResult::Select(rs) => {
            // `"3000"^^<…#integer>` (or a bare number).
            let cell = rs.rows[0][0].clone().unwrap();
            let digits: String = cell
                .trim_start_matches('"')
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().unwrap_or_else(|_| panic!("a count: {cell}"))
        }
        other => panic!("{other:?}"),
    }
}

/// Open `dir` in both modes: it must open, pass the integrity check, hold
/// every base triple and nothing of the second update (nothing is written
/// after the first failure), and the first update either wholly or not at
/// all — when `whole`; an update larger than the page cache is committed in
/// several batches (durable, not crash-atomic as a whole: PHASE6_PLAN D12),
/// so otherwise only a part of it may be there. Returns how many triples of
/// the first update are there.
fn assert_consistent(dir: &Path, whole: bool) -> u64 {
    let mut seen = None;
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let mut db = Database::open_with(dir, &options(mode))
            .unwrap_or_else(|e| panic!("the database must still open ({mode:?}): {e}"));
        db.verify_integrity()
            .unwrap_or_else(|e| panic!("integrity ({mode:?}): {e}"));
        assert_eq!(
            count(&mut db, "?s <http://ex/p> ?o"),
            BASE as u64,
            "the pre-existing triples are intact ({mode:?})"
        );
        let u1 = count(&mut db, "?s <http://ex/q/u1> ?o");
        if whole {
            assert!(
                u1 == 0 || u1 == U1 as u64,
                "the first update is half there ({mode:?}): {u1} of {U1}"
            );
        } else {
            assert!(u1 <= U1 as u64, "({mode:?}) {u1}");
        }
        assert_eq!(
            count(&mut db, "?s <http://ex/q/u2> ?o"),
            0,
            "nothing is written after the first failure ({mode:?})"
        );
        assert_eq!(count(&mut db, "?s ?p ?o"), BASE as u64 + u1, "({mode:?})");
        if let Some(prev) = seen {
            assert_eq!(prev, u1, "both modes see the same state");
        }
        seen = Some(u1);
    }
    seen.unwrap()
}

// ---- the library scenario, in a child process ------------------------------------

const CHILD_DIR: &str = "GSTORE_FAULT_CHILD_DIR";
const CHILD_MODE: &str = "GSTORE_FAULT_CHILD_MODE";
const CHILD_END: &str = "GSTORE_FAULT_CHILD_END";
const CHILD_CACHE: &str = "GSTORE_FAULT_CHILD_CACHE_PAGES";
const CHILD_OK: &str = "fault-injection child finished";
const LOG_REWRITTEN: &str = "the first update's complete log was rewritten";

/// Run the review's two failing updates on `dir` in a child process (`mode`:
/// `memory` | `stream`; `end`: `drop` — a clean drop while the limit still
/// holds — or `forget`, a crash; `cache_pages`: the page cache, `0` for the
/// default). Returns the child's output.
fn run_child(dir: &Path, mode: &str, end: &str, cache_pages: usize) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "child_two_updates_fail_under_a_file_size_limit",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, dir)
        .env(CHILD_MODE, mode)
        .env(CHILD_END, end)
        .env(CHILD_CACHE, cache_pages.to_string())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(CHILD_OK),
        "child ({mode}, {end}) failed: {:?}\n{stdout}\n{stderr}",
        out.status
    );
    eprintln!("child ({mode}, {end}):\n{stdout}");
    stdout.into_owned()
}

/// The child half of the library scenarios (a no-op unless run by
/// [`run_child`]).
#[test]
#[ignore = "run in a child process by the scenarios of this file (RLIMIT_FSIZE)"]
fn child_two_updates_fail_under_a_file_size_limit() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mode = mode_named(&std::env::var(CHILD_MODE).unwrap());
    let end = std::env::var(CHILD_END).unwrap();
    let mut opts = options(mode);
    match std::env::var(CHILD_CACHE).map(|v| v.parse::<usize>().unwrap()) {
        Ok(0) | Err(_) => {}
        Ok(pages) => opts.disk_cache_pages = pages,
    }
    rlimit::ignore_sigxfsz().unwrap();
    let mut db = Database::open_with(&dir, &opts).unwrap();
    let kv = dir.join("kvstore.kv");
    let wal = dir.join("kvstore.kv.wal");
    // The log of the first update fits; growing the main file does not.
    rlimit::limit_file_size(fs::metadata(&kv).unwrap().len()).unwrap();
    let u1 = db.query(&insert("u1", U1));
    assert!(u1.is_err(), "the first update must fail: {u1:?}");
    let wal1 = fs::read(&wal).ok();
    println!(
        "u1: {}; log {:?} bytes",
        u1.unwrap_err(),
        wal1.as_ref().map(Vec::len)
    );
    let u2 = db.query(&insert("u2", U2));
    assert!(u2.is_err(), "the second update must fail: {u2:?}");
    println!(
        "u2: {}; log {:?} bytes",
        u2.unwrap_err(),
        fs::metadata(&wal).ok().map(|m| m.len())
    );
    // The parent checks the outcome on disk first, then this marker.
    if wal1.is_some() && fs::read(&wal).ok() != wal1 {
        println!("{LOG_REWRITTEN}");
    }
    match end.as_str() {
        "drop" => drop(db),
        _ => std::mem::forget(db),
    }
    println!("{CHILD_OK}");
}

fn library_scenario(mode: &str, end: &str, cache_pages: usize) {
    let tmp = TmpDir::new(&format!("lib_{mode}_{end}_{cache_pages}"));
    let dir = tmp.0.join("t.db");
    build_base(&tmp.0, &dir);
    let out = run_child(&dir, mode, end, cache_pages);
    assert_consistent(&dir, cache_pages == 0);
    assert!(!out.contains(LOG_REWRITTEN), "{out}");
}

#[test]
fn materialize_two_failed_updates_then_a_clean_drop_leave_a_consistent_database() {
    library_scenario("memory", "drop", 0);
}

#[test]
fn materialize_two_failed_updates_then_a_crash_leave_a_consistent_database() {
    library_scenario("memory", "forget", 0);
}

#[test]
fn stream_two_failed_updates_then_a_clean_drop_leave_a_consistent_database() {
    library_scenario("stream", "drop", 0);
}

#[test]
fn stream_two_failed_updates_then_a_crash_leave_a_consistent_database() {
    library_scenario("stream", "forget", 0);
}

/// A page cache of 8 pages: term writes outgrow it, so the commit that
/// fails is one the dictionary makes while ending a term write — which used
/// not to break the triple store, so the next update committed again.
#[test]
fn stream_small_cache_two_failed_updates_then_a_clean_drop_leave_a_consistent_database() {
    library_scenario("stream", "drop", 8);
}

#[test]
fn materialize_small_cache_two_failed_updates_then_a_clean_drop_leave_a_consistent_database() {
    library_scenario("memory", "drop", 8);
}

// ---- the review's server scenario: gserver, kill -9, gquery, gcompact ------------

/// One `POST` with a body; `(status, body)`.
fn post(addr: SocketAddr, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let raw = format!(
        "POST {target} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

/// Kills the child when dropped (also when the test fails).
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tool(name: &str, cwd: &Path, args: &[&str], mode: &str) -> (bool, String) {
    let exe = match name {
        "gquery" => env!("CARGO_BIN_EXE_gquery"),
        "gcompact" => env!("CARGO_BIN_EXE_gcompact"),
        other => panic!("{other}"),
    };
    let out = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .env("GSTORE_DISK_QUERY_MODE", mode)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn server_scenario(mode: &str) {
    let tmp = TmpDir::new(&format!("gserver_{mode}"));
    let dir = tmp.0.join("t.db");
    build_base(&tmp.0, &dir);
    let limit = fs::metadata(dir.join("kvstore.kv")).unwrap().len();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gserver"));
    cmd.arg(&dir)
        .args(["-p", "0"])
        .current_dir(&tmp.0)
        .env("GSTORE_DISK_QUERY_MODE", mode)
        .env_remove("GSTORE_GSERVER_BACKGROUND")
        .env_remove("GSTORE_GSERVER_SUPERVISOR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe calls (signal, getrlimit, setrlimit)
    // run between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            rlimit::ignore_sigxfsz()?;
            rlimit::limit_file_size(limit)
        });
    }
    let mut child = Child(cmd.spawn().unwrap());
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let addr: SocketAddr = loop {
        let line = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("gserver announces its address");
        if let Some(a) = line
            .split("http://")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|a| a.trim_end_matches(',').parse().ok())
        {
            break a;
        }
    };
    let (s1, b1) = post(addr, "/update", &insert("u1", U1));
    assert_ne!(s1, 200, "the first update must fail: {b1}");
    let (s2, b2) = post(addr, "/update", &insert("u2", U2));
    assert_ne!(s2, 200, "the second update must fail: {b2}");
    eprintln!("gserver ({mode}): {s1} {b1}\n{s2} {b2}");
    // kill -9 while the disk is still "full".
    drop(child);
    for m in ["memory", "stream"] {
        let (ok, out) = tool(
            "gquery",
            &tmp.0,
            &["t", "-e", "SELECT (COUNT(*) AS ?n) WHERE { ?s <http://ex/p> ?o }"],
            m,
        );
        assert!(ok, "gquery must open the database ({m}): {out}");
        assert!(out.contains(&BASE.to_string()), "({m}) {out}");
        let (ok, out) = tool("gcompact", &tmp.0, &["t", "--check"], m);
        assert!(ok, "gcompact --check must pass ({m}): {out}");
    }
    assert_consistent(&dir, true);
}

#[test]
fn gserver_materialize_two_enospc_updates_and_kill_leave_an_openable_database() {
    server_scenario("memory");
}

#[test]
fn gserver_stream_two_enospc_updates_and_kill_leave_an_openable_database() {
    server_scenario("stream");
}

// ---- an unreadable write-ahead log fails the open (review disk#2, durability#7,
// ---- robust#12) --------------------------------------------------------------

/// A log that exists but cannot be read may hold a committed batch the file
/// still needs: the open fails instead of serving a possibly half-applied
/// file (and a later commit overwriting the log for good). Only a missing
/// log means "no log". A directory in the log's place reads with an error
/// other than "not found" (as `EIO` or `EACCES` would), with no privileges
/// needed.
#[test]
fn an_unreadable_write_ahead_log_fails_the_open_in_both_modes() {
    let tmp = TmpDir::new("wal_unreadable");
    let dir = tmp.0.join("t.db");
    build_base(&tmp.0, &dir);
    let wal = dir.join("kvstore.kv.wal");
    fs::create_dir(&wal).unwrap();
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let e = Database::open_with(&dir, &options(mode))
            .err()
            .unwrap_or_else(|| panic!("the open must fail ({mode:?})"));
        assert!(e.to_string().contains("write-ahead log"), "({mode:?}) {e}");
        assert!(wal.is_dir(), "the log is left alone");
    }
    fs::remove_dir(&wal).unwrap();
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let mut db = Database::open_with(&dir, &options(mode)).unwrap();
        assert_eq!(count(&mut db, "?s ?p ?o"), BASE as u64);
    }
}
