//! Replacing and removing database directories (review durability#15 and
//! the concrete paths it names): every replacement is staged beside the
//! live database and swapped in, every removal moves the directory aside
//! durably before deleting it, so neither a failure nor a crash part-way
//! leaves a half-replaced or half-deleted database under its live name.
//!
//! * `gbuild --incremental --force` (disk and RocksDB) that fails keeps the
//!   old database (durability#5).
//! * `gdrop`, the REST `/drop` and the `/api` `drop` killed part-way leave
//!   the live name gone and a leftover no scan reads, which the next drop
//!   of the name removes (durability#10).
//! * `gbackup` refuses a destination that is, or lies inside, the
//!   database directory, and leaves its write-ahead log alone (bulkcli#4).
//! * A backup copy never pairs a new `kvstore.kv` with an old `vstree.kv`
//!   (durability#4); the `/api` `rename` reports registry and grant
//!   failures and undoes the rename (durability#14); a legacy-shadow
//!   reconcile killed part-way is finished by the next open (durability#9).
//!
//! Crash windows are reached deterministically: a child process (a
//! `gstore` tool, or a re-run of this test binary as a helper) parks at the
//! window through a test hook environment variable and says so on stderr,
//! then is killed with SIGKILL.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::query::{scoped_vs_policy, VsPolicy};
use gstore::{Database, QueryResult};

// ---- helpers -------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_layout_replacement_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(fs::canonicalize(&d).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A child process killed (SIGKILL) and reaped when dropped, so a failing
/// test leaves none behind.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run gStore tool `tool` (a binary of this package) in `cwd`.
fn tool(tool: &str, cwd: &Path, args: &[&str]) -> Output {
    let exe = match tool {
        "gbuild" => env!("CARGO_BIN_EXE_gbuild"),
        "gbackup" => env!("CARGO_BIN_EXE_gbackup"),
        "gdrop" => env!("CARGO_BIN_EXE_gdrop"),
        "gshow" => env!("CARGO_BIN_EXE_gshow"),
        other => panic!("unknown tool {other}"),
    };
    Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Read `child`'s stderr until a line containing `marker`; `false` when the
/// stream ends first (the child never reached the hook).
fn wait_for_line(child: &mut Child, marker: &str) -> bool {
    let err = child.stderr.take().expect("piped stderr");
    let mut lines = BufReader::new(err).lines();
    let found = lines.any(|l| l.map(|l| l.contains(marker)).unwrap_or(false));
    // Keep draining in the background: a full pipe must not block the child.
    std::thread::spawn(move || for _ in lines {});
    found
}

/// `n` triples `<http://e/s{i}> <http://e/p> <http://e/o{i}>` for `i` in `range`.
fn triples(prefix: &str, range: std::ops::Range<u32>) -> String {
    range
        .map(|i| format!("<http://e/{prefix}s{i}> <http://e/p> <http://e/{prefix}o{i}> .\n"))
        .collect()
}

/// The sorted subjects of `?s <http://e/p> ?o` under `policy` (a unique
/// comment bypasses the query cache).
fn subjects(db: &mut Database, policy: VsPolicy) -> Vec<String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let _policy = scoped_vs_policy(policy);
    let q = format!("# layout_replacement {n}\nSELECT ?s WHERE {{ ?s <http://e/p> ?o }}");
    let mut v: Vec<String> = match db.query(&q).unwrap() {
        QueryResult::Select(rs) => rs.rows.into_iter().map(|r| r[0].clone().unwrap()).collect(),
        other => panic!("{other:?}"),
    };
    v.sort();
    v
}

/// The triple count of the database in `dir`.
fn count(dir: &Path) -> u64 {
    Database::open(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .triple_num()
}

// ---- durability#5: a failed incremental --force rebuild keeps the old database --

const GOOD: &str = "<http://e/a> <http://e/p> <http://e/b> .\n\
                    <http://e/a> <http://e/p> <http://e/c> .\n\
                    <http://e/b> <http://e/q> \"x\" .\n";
const BAD: &str = "<http://e/n> <http://e/p> <http://e/m> .\nthis is not RDF\n";

/// `gbuild --disk --incremental --strict --force` over a good database with
/// an input that does not parse fails — and the old database is still
/// there, as `gbuild` documents for a failed `--force` rebuild (the review
/// reproduced COUNT 3 -> 0: the incremental path deleted the old store
/// before importing). The bulk path is the control.
#[test]
fn a_failed_incremental_disk_rebuild_keeps_the_old_database() {
    let root = TmpDir::new("incr_disk");
    fs::write(root.path().join("good.nt"), GOOD).unwrap();
    fs::write(root.path().join("bad.nt"), BAD).unwrap();
    let dir = root.path().join("e1.db");
    for extra in [&["--incremental"][..], &[][..]] {
        let out = tool(
            "gbuild",
            root.path(),
            &["--disk", "e1", "good.nt", "--force"],
        );
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(count(&dir), 3);
        let mut args = vec!["--disk", "--strict", "--force"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["e1", "bad.nt"]);
        let out = tool("gbuild", root.path(), &args);
        assert!(!out.status.success(), "{extra:?}: {}", text(&out));
        assert_eq!(count(&dir), 3, "{extra:?}: the old database is kept");
        assert!(!dir.join(gstore::bulk::STAGING_DIR).exists(), "{extra:?}");
        // The kept database's VS-tree still agrees with its store.
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            subjects(&mut db, VsPolicy::Tree),
            subjects(&mut db, VsPolicy::Off)
        );
    }
    // A good incremental rebuild replaces it.
    fs::write(root.path().join("new.nt"), triples("n", 0..5)).unwrap();
    let out = tool(
        "gbuild",
        root.path(),
        &["--disk", "--incremental", "--force", "e1", "new.nt"],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count(&dir), 5);
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(subjects(&mut db, VsPolicy::Tree).len(), 5);
}

/// The file-size limit and the signal it raises (Linux, 64-bit `rlim_t`):
/// an exact stand-in for a disk filling up during a build.
#[cfg(all(feature = "rocksdb", target_os = "linux", target_pointer_width = "64"))]
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

    /// Ignore `SIGXFSZ` and set the soft file-size limit to `bytes`, so a
    /// write past it fails with `EFBIG`. Async-signal-safe (`pre_exec`).
    pub fn limit_file_size(bytes: u64) -> std::io::Result<()> {
        // SAFETY: installs the predefined SIG_IGN disposition; plain
        // syscall wrappers on a properly sized struct.
        unsafe {
            if signal(SIGXFSZ, SIG_IGN) == usize::MAX {
                return Err(std::io::Error::last_os_error());
            }
            let mut cur = RLimit { cur: 0, max: 0 };
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

/// The same for `gbuild --rocksdb --incremental --force`: its build fails
/// inside the new store (here: the disk "fills up" — a file-size limit — as
/// the new store's write-ahead log grows), and the old store is still
/// there. The incremental RocksDB build used to discard the old store
/// before writing the new one in its place.
#[cfg(all(feature = "rocksdb", target_os = "linux", target_pointer_width = "64"))]
#[test]
fn a_failed_incremental_rocksdb_rebuild_keeps_the_old_database() {
    use std::os::unix::process::CommandExt;
    let root = TmpDir::new("incr_rocks");
    fs::write(root.path().join("good.nt"), GOOD).unwrap();
    fs::write(root.path().join("big.nt"), triples("big", 0..20_000)).unwrap();
    let dir = root.path().join("r1.db");
    let out = tool("gbuild", root.path(), &["--rocksdb", "r1", "good.nt"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count(&dir), 3);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gbuild"));
    cmd.args(["--rocksdb", "--incremental", "--force", "r1", "big.nt"])
        .current_dir(root.path());
    // SAFETY: only async-signal-safe calls run between fork and exec.
    unsafe {
        cmd.pre_exec(|| rlimit::limit_file_size(64 << 10));
    }
    let out = cmd.output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert_eq!(count(&dir), 3, "the old store is kept: {}", text(&out));
    assert!(
        !dir.join("rocksdb.tmp").exists(),
        "the staged store is removed"
    );
    // Without the limit the rebuild replaces it.
    let out = tool(
        "gbuild",
        root.path(),
        &["--rocksdb", "--incremental", "--force", "r1", "big.nt"],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count(&dir), 20_000);
    assert!(!dir.join("rocksdb.tmp").exists() && !dir.join("rocksdb.old").exists());
}
