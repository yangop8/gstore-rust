//! Failure paths of the command-line tools and of the bulk build's locking
//! (review bulkcli#2, #3, #6, #13, #17 and the CLI items around them): a
//! build claims its directory in one step and keeps every other thread of
//! the process out while it runs; a failed `gbuild` never removes a
//! directory another process holds, and a successful one keeps its database
//! when the summary cannot be printed; zip packages are unpacked into fresh,
//! unpredictable scratch directories once the database's lock is held.
//!
//! The concurrency tests synchronise deterministically: a FIFO source parks
//! `gbuild` while it reads its input (the test's open of the FIFO for
//! writing returns once the tool opened it), and a parse-error log parks an
//! in-process build; the timeouts are hang guards only.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget};
use gstore::db::dirlock::{BuildGuard, DirLock};
use gstore::error::ErrorKind;
use gstore::parser::import::{ErrorLog, ImportOptions, RdfFormat};
use gstore::parser::stream::SyntaxError;
use gstore::Database;

/// Waits that only guard against a hang (never the assertion).
const HANG_GUARD: Duration = Duration::from_secs(120);

const OLD: &str = "<http://e/s> <http://e/p> <http://e/old> .\n";
const NEW: &str = "<http://e/s> <http://e/p> <http://e/new1> .\n\
                   <http://e/s> <http://e/p> <http://e/new2> .\n";

/// A temp directory (canonical path) removed when dropped, also when the
/// test fails.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_cli_failure_{tag}_{}_{n}",
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

fn tool(name: &str) -> &'static str {
    match name {
        "gbuild" => env!("CARGO_BIN_EXE_gbuild"),
        "gquery" => env!("CARGO_BIN_EXE_gquery"),
        "gexport" => env!("CARGO_BIN_EXE_gexport"),
        "gserver" => env!("CARGO_BIN_EXE_gserver"),
        other => panic!("no tool {other}"),
    }
}

/// Run `tool` in `cwd` (no `conf/conf.ini` there unless a test writes one).
fn run(name: &str, args: &[&str], cwd: &Path) -> Output {
    Command::new(tool(name))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn bulk_text(dir: &Path, text: &str, log: &mut dyn ErrorLog) -> gstore::Result<bulk::BulkReport> {
    bulk::build(
        dir,
        "t",
        BulkInput::Text {
            text,
            format: RdfFormat::Turtle,
            name: "t.nt",
        },
        &ImportOptions::tolerant(),
        log,
        BulkTarget::Disk,
        &BulkOptions::default(),
    )
}

// ---- bulkcli#13: the claim and the lock are one step --------------------------

/// A parse-error log that parks the build at its first failed statement
/// until `park` has run (on the build's thread, with the claim held).
struct ParkingLog<F: FnMut()> {
    park: Option<F>,
}

impl<F: FnMut()> ErrorLog for ParkingLog<F> {
    fn failed(&mut self, _source: &str, _err: &SyntaxError) {
        if let Some(mut park) = self.park.take() {
            park();
        }
    }
}

/// Review bulkcli#13: before the fix the "open in this process?" check and
/// the lock were two steps and nothing refused an open after them — a
/// thread of the same process opened the directory in the middle of the
/// build (sharing the build's lock), and the install then replaced the
/// files under that handle (an update through it was acknowledged and
/// lost). Now the build's claim refuses every other thread's open as a lock
/// conflict until the build is done.
#[test]
fn a_database_opened_during_a_bulk_build_is_refused() {
    let base = TmpDir::new("open_during_build");
    let dir = base.0.join("b.db");
    bulk_text(&dir, OLD, &mut ()).unwrap();

    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (res_tx, res_rx) = mpsc::channel::<Result<u64, (ErrorKind, String)>>();
    let opener = {
        let dir = dir.clone();
        thread::spawn(move || {
            if go_rx.recv_timeout(HANG_GUARD).is_err() {
                return;
            }
            let opened = Database::open(&dir)
                .map(|db| db.triple_num())
                .map_err(|e| (e.kind(), e.to_string()));
            let _ = res_tx.send(opened);
        })
    };
    let mut during = None;
    let mut log = ParkingLog {
        park: Some(|| {
            assert!(BuildGuard::active(&dir), "parked inside the build");
            go_tx.send(()).unwrap();
            during = Some(
                res_rx
                    .recv_timeout(HANG_GUARD)
                    .expect("the opener answered"),
            );
        }),
    };
    // One statement that does not parse: the tolerant build logs it (and
    // parks there), then goes on.
    let input = format!("{NEW}<http://e/broken> <http://e/p> .\n");
    let report = bulk_text(&dir, &input, &mut log).unwrap();
    drop(log);
    opener.join().unwrap();
    assert_eq!(report.import.failed, 1);
    match during.expect("the build parked at the bad statement") {
        Ok(n) => panic!(
            "a database of this process opened the directory during the build ({n} triples): \
             its writes would go to files the install replaces"
        ),
        Err((kind, msg)) => {
            assert_eq!(kind, ErrorKind::Locked, "{msg}");
            assert!(msg.contains("being rebuilt"), "{msg}");
        }
    }
    // The build is done: the directory opens (as the new database).
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 2);
}

/// The claim itself: one step that refuses a holder of this process, keeps
/// every other thread out while it lives, and lets its own thread take the
/// lock and nest builds (how `gbuild` holds it from before it prepares its
/// input through the build).
#[test]
fn a_build_claim_excludes_other_threads_and_nests_on_its_own() {
    let base = TmpDir::new("claim");
    let dir = base.0.join("c.db");
    fs::create_dir_all(&dir).unwrap();
    let other_thread = |what: fn(&Path) -> gstore::Result<()>| {
        let dir = dir.clone();
        thread::spawn(move || what(&dir)).join().unwrap()
    };

    let (guard, lock) = BuildGuard::lock(&dir).unwrap();
    assert!(BuildGuard::active(&dir) && DirLock::held_here(&dir));
    // Its own thread: the lock is shared, and a nested claim (the library
    // build a tool runs under its own claim) nests.
    drop(DirLock::acquire(&dir).unwrap());
    let nested = BuildGuard::lock(&dir).unwrap();
    drop(nested);
    assert!(BuildGuard::active(&dir), "the outer claim stays");
    // Every other thread is refused, as another process would be.
    for (what, f) in [
        (
            "DirLock::acquire",
            (|d: &Path| DirLock::acquire(d).map(drop)) as fn(&Path) -> gstore::Result<()>,
        ),
        ("BuildGuard::lock", |d: &Path| BuildGuard::lock(d).map(drop)),
    ] {
        let e = other_thread(f).expect_err(what);
        assert_eq!(e.kind(), ErrorKind::Locked, "{what}: {e}");
        assert!(e.to_string().contains("being rebuilt"), "{what}: {e}");
    }
    let bulk_on_other_thread = |d: &Path| bulk_text(d, NEW, &mut ()).map(drop);
    let e = other_thread(bulk_on_other_thread).expect_err("a second build");
    assert!(e.to_string().contains("being rebuilt"), "{e}");
    // A build on the claiming thread nests in the claim.
    bulk_text(&dir, NEW, &mut ()).unwrap();
    drop(guard);
    drop(lock);
    assert!(!BuildGuard::active(&dir) && !DirLock::held_here(&dir));
    other_thread(|d| DirLock::acquire(d).map(drop)).expect("free again");

    // A holder of this process (an open database) refuses a claim, in one
    // step with the lock.
    let open = Database::open(&dir).unwrap();
    let e = BuildGuard::lock(&dir).unwrap_err();
    assert!(e.to_string().contains("open in this process"), "{e}");
    drop(open);
    // A claim made by path only (an older-style guard) refuses one too.
    let by_path = BuildGuard::enter(&dir).unwrap();
    let e = BuildGuard::lock(&dir).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Locked, "{e}");
    drop(by_path);
    drop(BuildGuard::lock(&dir).unwrap());
}

// ---- bulkcli#2: a failed gbuild removes only what it holds --------------------

/// `mkfifo path`.
#[cfg(unix)]
fn mkfifo(path: &Path) {
    let st = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(st.success(), "mkfifo {}", path.display());
}

/// Open the FIFO `path` for writing once a reader has opened it (the open
/// blocks until then); `child` is the reader, killed on a hang.
#[cfg(unix)]
fn open_fifo_writer(path: &Path) -> fs::File {
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(fs::OpenOptions::new().write(true).open(&p));
    });
    rx.recv_timeout(HANG_GUARD)
        .expect("the tool opened its FIFO source")
        .unwrap()
}

/// Review bulkcli#2: gbuild decided once, at startup, that the directory was
/// its own to remove, and removed it on any failure without the directory's
/// lock — two builds of one new name (one slowed by a zip unpack or a slow
/// source) ended with the loser deleting the winner's directory. Now gbuild
/// claims the directory before it reads its input and removes it on
/// failure only under that claim: while one gbuild reads its input another
/// process cannot take the directory, and a gbuild that could not claim it
/// leaves it alone.
#[cfg(unix)]
#[test]
fn a_failed_gbuild_never_removes_a_directory_another_process_holds() {
    let base = TmpDir::new("race");
    let fifo = base.0.join("slow.nt");
    mkfifo(&fifo);
    let dir = base.0.join("race.db");
    let gbuild = Child(Some(
        Command::new(tool("gbuild"))
            .args(["race", "slow.nt"])
            .current_dir(&base.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    // gbuild reads its source now: the directory it is building exists.
    let mut writer = open_fifo_writer(&fifo);
    assert!(dir.is_dir());
    // Another process (this one) tries to take the directory meanwhile —
    // what a second gbuild, a gserver build or a gadd would do.
    match DirLock::acquire(&dir) {
        Ok(held) => {
            // The unfixed gbuild let it in: whatever it then builds there
            // must survive this gbuild's failure.
            fs::write(dir.join("other-builder"), "").unwrap();
            writer.write_all(NEW.as_bytes()).unwrap();
            drop(writer);
            let out = gbuild.wait();
            assert!(
                dir.join("other-builder").exists(),
                "gbuild removed a directory another process holds:\n{}",
                text(&out)
            );
            drop(held);
        }
        Err(e) => {
            // The fixed gbuild holds it from before it read its input.
            assert_eq!(e.kind(), ErrorKind::Locked, "{e}");
            assert!(e.to_string().contains("locked by another process"), "{e}");
            writer.write_all(NEW.as_bytes()).unwrap();
            drop(writer);
            let out = gbuild.wait();
            assert!(out.status.success(), "{}", text(&out));
            assert_eq!(Database::open(&dir).unwrap().triple_num(), 2);
        }
    }
}

/// The other half of bulkcli#2: a gbuild that loses the race for a new
/// name — the directory exists and is held when it starts — removes
/// nothing, with `--force` too (it then fails on the lock).
#[test]
fn a_gbuild_that_cannot_claim_the_directory_leaves_it_alone() {
    let base = TmpDir::new("loser");
    fs::write(base.0.join("new.nt"), NEW).unwrap();
    let dir = base.0.join("held.db");
    fs::create_dir_all(&dir).unwrap();
    // "Another process" holds the new, still empty, directory.
    let held = DirLock::acquire(&dir).unwrap();
    let marker = dir.join("being-built");
    fs::write(&marker, "").unwrap();
    for args in [&["held", "new.nt"][..], &["--force", "held", "new.nt"]] {
        let out = run("gbuild", args, &base.0);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(marker.exists(), "{args:?}: {}", text(&out));
    }
    drop(held);
}

// ---- bulkcli#3: a failed summary print keeps the database ---------------------

/// Review bulkcli#3: gbuild printed its summary before marking the new
/// database as kept, and a failed write to standard output (here
/// `/dev/full`: ENOSPC) panicked and unwound into the cleanup, which
/// deleted the database it had just built. Now the database is kept as soon
/// as the build succeeded; the failed print is reported (exit status 1).
#[cfg(target_os = "linux")]
#[test]
fn gbuild_keeps_the_built_database_when_its_summary_cannot_be_printed() {
    let base = TmpDir::new("devfull");
    fs::write(base.0.join("new.nt"), NEW).unwrap();
    for (name, layout) in [
        ("snap", &[][..]),
        ("bulk", &["--disk"][..]),
        ("incr", &["--disk", "--incremental"][..]),
    ] {
        let full = fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let out = Command::new(tool("gbuild"))
            .args(layout)
            .args([name, "new.nt"])
            .current_dir(&base.0)
            .stdout(full)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let dir = base.0.join(format!("{name}.db"));
        assert!(
            dir.is_dir(),
            "{name}: the new database was removed:\n{}",
            text(&out)
        );
        assert_eq!(Database::open(&dir).unwrap().triple_num(), 2, "{name}");
        assert_eq!(out.status.code(), Some(1), "{name}: {}", text(&out));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{name}: {err}");
        assert!(err.contains("could not be printed"), "{name}: {err}");
    }
}

// ---- bulkcli#6: zip scratch directories --------------------------------------

#[cfg(feature = "zip")]
fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, data) in entries {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(data.as_bytes()).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

/// The `<db>.db.unzip-tmp*` entries beside the databases in `base`.
#[cfg(feature = "zip")]
fn unzip_scratch(base: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(base)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".unzip-tmp"))
        .collect();
    v.sort();
    v
}

/// Review bulkcli#6: the tools unpacked a zip package into
/// `<db>.db.unzip-tmp-<i>`, removing whatever was there first, and gbuild
/// did so before it took any lock — a gbuild racing a gadd of the same
/// database wiped the scratch directory gadd was reading. Now each unpack
/// creates a fresh directory under an unpredictable name exclusively (an
/// existing one is never reused or removed), and gbuild unpacks only once
/// it holds the database's directory.
#[cfg(feature = "zip")]
#[test]
fn gbuild_unpacks_into_a_fresh_scratch_directory_after_taking_the_lock() {
    let base = TmpDir::new("zipscratch");
    fs::write(base.0.join("pkg.zip"), zip_of(&[("a.nt", NEW)])).unwrap();
    // Another tool's scratch directory under the old, predictable name.
    let theirs = base.0.join("z.db.unzip-tmp-0");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("their.nt"), OLD).unwrap();
    let out = run("gbuild", &["z", "pkg.zip"], &base.0);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        theirs.join("their.nt").exists(),
        "another tool's scratch directory was taken over"
    );
    assert_eq!(
        unzip_scratch(&base.0),
        vec!["z.db.unzip-tmp-0".to_string()],
        "gbuild's own scratch directory is gone"
    );
    assert_eq!(
        Database::open(base.0.join("z.db")).unwrap().triple_num(),
        2,
        "only the package's statements"
    );
    // A database another process holds: nothing is unpacked.
    let dir = base.0.join("z.db");
    let held = DirLock::acquire(&dir).unwrap();
    let out = run("gbuild", &["--force", "z", "pkg.zip"], &base.0);
    drop(held);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("unpacked"),
        "unpacked before the lock: {}",
        text(&out)
    );
    assert_eq!(unzip_scratch(&base.0), vec!["z.db.unzip-tmp-0".to_string()]);
}

// ---- conf.ini hygiene (bulkcli#7, #8, #9, design#11) --------------------------

/// Review bulkcli#9: an unquoted value was cut at any whitespace + `#` or
/// `;` — `root_password = Tr0ub4dor #3` set the password `Tr0ub4dor`,
/// without a word. The documented rule: an inline comment is a marker that
/// stands as a word of its own (whitespace before and after it, or the
/// end); `root_password` is never cut; every value an inline comment cuts
/// is reported.
#[test]
fn conf_ini_inline_comments_follow_the_documented_rule() {
    use gstore::config::GStoreConfig;
    let c = GStoreConfig::from_ini_str(
        "[system]\nroot_password = Tr0ub4dor #3\ndb_home = /srv/a ;b\nlicensetype = #x\n",
    );
    assert_eq!(c.root_password, "Tr0ub4dor #3");
    assert_eq!(c.db_home, PathBuf::from("/srv/a ;b"));
    assert_eq!(c.licensetype, "#x");
    assert!(c.warnings().is_empty(), "{:?}", c.warnings());

    // A password is taken whole, a word-like marker included.
    let c = GStoreConfig::from_ini_str("[system]\nroot_password = correct horse # battery\n");
    assert_eq!(c.root_password, "correct horse # battery");
    assert!(c.warnings().is_empty(), "{:?}", c.warnings());

    // A comment of its own word cuts the value — and says so.
    let c = GStoreConfig::from_ini_str(
        "[server]\nport = 9001  # the HTTP port\nthread_num = 4 ;\n\
         [system]\nlicensetype = \"a # b\"  ; quoted\n",
    );
    assert_eq!(c.port, 9001);
    assert_eq!(c.thread_num, 4);
    assert_eq!(c.licensetype, "a # b", "nothing inside quotes is a comment");
    assert_eq!(c.warnings().len(), 3, "{:?}", c.warnings());
    assert!(c.warnings()[0].contains("line 2: '# the HTTP port' after the value of 'port'"));
    assert!(c.warnings().iter().all(|w| w.contains("read as a comment")));

    // Upstream's own file still parses as before, without a warning.
    let shipped = GStoreConfig::from_ini_str(include_str!("../conf/conf.ini.example"));
    assert!(shipped.warnings().is_empty(), "{:?}", shipped.warnings());
}

/// Review design#11 / bulkcli#8: settings that are accepted but cannot take
/// effect were silent — `resource_check=on` (no resource check exists) and
/// a `build_threads` that `build_multi_thread=off` overrides — and the
/// shipped example forced single-threaded bulk builds. Both are reported
/// now, and the example leaves `build_multi_thread` at its default.
#[test]
fn settings_that_cannot_take_effect_are_reported() {
    use gstore::config::GStoreConfig;
    let c = GStoreConfig::from_ini_str("[system]\nresource_check=on\nmin_memory=4096\n");
    assert_eq!(c.warnings().len(), 1, "{:?}", c.warnings());
    assert!(c.warnings()[0].contains("resource_check=on has no effect"));

    let c = GStoreConfig::from_ini_str("[system]\nbuild_multi_thread=off\nbuild_threads=8\n");
    assert_eq!(c.bulk_options().threads, 1);
    assert_eq!(c.warnings().len(), 1, "{:?}", c.warnings());
    assert!(c.warnings()[0].contains("build_threads=8 has no effect"));
    // Consistent settings say nothing.
    for ok in [
        "[system]\nbuild_multi_thread=off\nbuild_threads=1\n",
        "[system]\nbuild_multi_thread=off\n",
        "[system]\nbuild_threads=8\n",
        "[system]\nresource_check=off\nmin_memory=4096\n",
    ] {
        let c = GStoreConfig::from_ini_str(ok);
        assert!(c.warnings().is_empty(), "{ok}: {:?}", c.warnings());
    }

    let shipped = GStoreConfig::from_ini_str(include_str!("../conf/conf.ini.example"));
    assert!(shipped.build_multi_thread);
    assert_eq!(shipped.bulk_options().threads, 0, "every core");
}

/// Review design#11: `GStoreConfig::db_path` named `<db_home>/<name><db_suffix>`,
/// a directory no server opens (they always use `.db`); it now names the
/// servers' directory.
#[test]
fn db_path_names_the_directory_the_servers_use() {
    use gstore::config::GStoreConfig;
    let c = GStoreConfig::from_ini_str("[system]\ndb_home=/data/gs\ndb_suffix=.gdb\n");
    assert_eq!(c.db_path("lubm"), PathBuf::from("/data/gs/lubm.db"));
    assert_eq!(c.db_path("lubm.db"), PathBuf::from("/data/gs/lubm.db"));
    assert_eq!(
        c.db_path("lubm"),
        c.db_home.join(gstore::db_dir_for("lubm")),
        "what the servers open"
    );
}

/// Review bulkcli#7: gbuild read `./conf/conf.ini` but dropped its
/// warnings — a mistyped value silently fell back to the default (here a
/// version-2 disk file instead of the version-1 file asked for). It now
/// prints them, as gserver logs them; the shipped example prints none.
#[test]
fn gbuild_reports_conf_ini_warnings() {
    let base = TmpDir::new("gbuild_conf");
    fs::write(base.0.join("new.nt"), NEW).unwrap();
    fs::create_dir_all(base.0.join("conf")).unwrap();
    fs::write(
        base.0.join("conf/conf.ini"),
        "[system]\nbuild_memory_mb=2G\nid_reclaim=maybe\nbuild_multi_thread=off\nbuild_threads=8\n",
    )
    .unwrap();
    let out = run("gbuild", &["--disk", "w", "new.nt"], &base.0);
    assert!(out.status.success(), "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    for what in [
        "invalid value '2G' for 'build_memory_mb'",
        "invalid value 'maybe' for 'id_reclaim'",
        "build_threads=8 has no effect",
    ] {
        assert!(err.contains(what), "missing '{what}':\n{}", text(&out));
    }
    // The shipped example: nothing to report.
    fs::write(
        base.0.join("conf/conf.ini"),
        include_str!("../conf/conf.ini.example"),
    )
    .unwrap();
    let out = run("gbuild", &["--disk", "x", "new.nt"], &base.0);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("conf.ini"),
        "{}",
        text(&out)
    );
}
