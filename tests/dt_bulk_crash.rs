//! Crash safety of the bulk build's install (`gstore::bulk::commit`): after a
//! crash at any of its 8 steps, over any prior layout (none, snapshot, disk,
//! RocksDB), the directory opens as either the old or the new database;
//! `gbuild --disk` killed with SIGKILL at random moments or aborted at each
//! step does the same; a directory another process holds is refused, as is
//! a second build of the same directory in one process; a stale staging
//! area is cleaned up by the next build.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget, CRASH_ENV, CRASH_POINTS};
use gstore::parser::import::{ImportOptions, Source};
use gstore::{Database, StorageKind};

const OLD: &str =
    "<http://e/s> <http://e/p> <http://e/old> .\n<http://e/s> <http://e/q> \"old\" .\n";
const NEW: &str = "<http://e/s> <http://e/p> <http://e/new1> .\n\
                   <http://e/s> <http://e/p> <http://e/new2> .\n\
                   <http://e/s> <http://e/q> \"new\" .\n";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_bulkcrash_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Prior {
    None,
    Snapshot,
    Disk,
    RocksDb,
}

/// Put the old database (`OLD`) into `dir` in the `prior` layout.
fn make_prior(dir: &Path, prior: Prior) {
    match prior {
        Prior::None => {}
        Prior::Snapshot => Database::build_from_str("old", OLD)
            .unwrap()
            .save(dir)
            .unwrap(),
        Prior::Disk => {
            let nt = dir.with_extension("old.nt");
            fs::write(&nt, OLD).unwrap();
            Database::build_disk(dir, &[&nt]).unwrap();
        }
        Prior::RocksDb => {
            #[cfg(feature = "rocksdb")]
            drop(Database::build_rocksdb_from_str("old", dir, OLD).unwrap());
            #[cfg(not(feature = "rocksdb"))]
            {
                // Without the feature only the layout's marker matters: a
                // `rocksdb/` directory takes precedence in detection.
                fs::create_dir_all(dir.join("rocksdb")).unwrap();
                fs::write(dir.join("rocksdb/CURRENT"), "MANIFEST-000001\n").unwrap();
            }
        }
    }
}

/// Which database `dir` holds now: `Some(true)` the new one, `Some(false)`
/// the old one, `None` none at all (only right when there was none before).
fn which(dir: &Path, prior: Prior) -> Option<bool> {
    let kind = Database::storage_kind(dir)?;
    if kind == StorageKind::RocksDb && !cfg!(feature = "rocksdb") {
        assert_eq!(
            prior,
            Prior::RocksDb,
            "a RocksDB layout appeared from nowhere"
        );
        assert!(
            dir.join("rocksdb/CURRENT").is_file(),
            "the old store is intact"
        );
        return Some(false);
    }
    let mut db = Database::open(dir).unwrap_or_else(|e| panic!("{prior:?}: {e}"));
    let rows = db
        .select("SELECT ?o WHERE { <http://e/s> <http://e/p> ?o } ORDER BY ?o")
        .unwrap()
        .rows;
    let objs: Vec<String> = rows.into_iter().map(|r| r[0].clone().unwrap()).collect();
    if objs == ["<http://e/new1>", "<http://e/new2>"] {
        assert_eq!(db.triple_num(), 3);
        assert_eq!(kind, StorageKind::Disk);
        Some(true)
    } else {
        assert_eq!(objs, ["<http://e/old>"], "{prior:?}: neither old nor new");
        assert_eq!(db.triple_num(), 2);
        Some(false)
    }
}

fn build_new(dir: &Path) -> gstore::Result<bulk::BulkReport> {
    bulk::build(
        dir,
        "new",
        BulkInput::Text {
            text: NEW,
            format: gstore::parser::import::RdfFormat::Turtle,
            name: "new.nt",
        },
        &ImportOptions::strict(),
        &mut (),
        BulkTarget::Disk,
        &BulkOptions::default(),
    )
}

/// The failpoint matrix: 8 steps × 4 prior layouts. Before the new
/// `kvstore.kv` is in place the old database survives; from then on the new
/// one is there — except that an old `rocksdb/` keeps winning until it is
/// moved aside. A build after the crash cleans up and succeeds.
#[test]
fn every_crash_point_leaves_the_old_or_the_new_database() {
    let base = tmp("matrix");
    for prior in [Prior::None, Prior::Snapshot, Prior::Disk, Prior::RocksDb] {
        for (k, point) in CRASH_POINTS.iter().enumerate() {
            let dir = base.join(format!("{prior:?}_{point}.db"));
            fs::create_dir_all(&dir).unwrap();
            make_prior(&dir, prior);
            bulk::debug_crash_at(&dir, Some(point));
            let e = build_new(&dir).unwrap_err();
            bulk::debug_crash_at(&dir, None);
            assert!(e.to_string().contains("simulated crash"), "{e}");
            let kv_placed = k >= 3; // "kv-renamed" and later
            let rocks_gone = k >= 6; // "layouts-removed" and later
            let want_new = kv_placed && (prior != Prior::RocksDb || rocks_gone);
            match which(&dir, prior) {
                None => assert!(prior == Prior::None && !want_new, "{prior:?} {point}"),
                Some(is_new) => assert_eq!(is_new, want_new, "{prior:?} after '{point}'"),
            }
            // The next build replaces whatever is there and cleans up.
            build_new(&dir).unwrap();
            assert!(!dir.join(bulk::STAGING_DIR).exists());
            assert_eq!(
                which(&dir, prior),
                Some(true),
                "{prior:?} rebuilt after '{point}'"
            );
            assert!(!dir.join("rocksdb").exists());
            let _ = fs::remove_dir_all(&dir);
        }
    }
    let _ = fs::remove_dir_all(&base);
}

/// A real crash: `gbuild --disk --force` aborted at each step through the
/// environment failpoint, then SIGKILLed at moments spread over a whole
/// build — every time the directory opens as the old or the new database.
/// Bounded to well under 30 s.
#[test]
fn killed_gbuild_leaves_the_old_or_the_new_database() {
    let started = Instant::now();
    let base = tmp("kill");
    let gbuild = env!("CARGO_BIN_EXE_gbuild");
    let old_nt = base.join("old.nt");
    fs::write(&old_nt, OLD).unwrap();
    let new_nt = base.join("new.nt");
    fs::write(&new_nt, NEW).unwrap();
    let dir = base.join("k.db");
    let name = base.join("k");
    let rebuild_old = || {
        let _ = fs::remove_dir_all(&dir);
        Database::build_disk(&dir, &[&old_nt]).unwrap();
    };
    let classify = |n: u64| match n {
        2 => false,
        3 | 100_543 => true,
        other => panic!("neither old nor new: {other} triples"),
    };
    let count = || Database::open(&dir).unwrap().triple_num();
    // 1. Abort at every step.
    for (k, point) in CRASH_POINTS.iter().enumerate() {
        rebuild_old();
        // In the scratch directory: an aborted process may leave a core file.
        let out = Command::new(gbuild)
            .args(["--disk", "--force", "--threads", "2"])
            .arg(&name)
            .arg(&new_nt)
            .env(CRASH_ENV, point)
            .current_dir(&base)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{point}: {out:?}");
        assert_eq!(classify(count()), k >= 3, "after aborting at '{point}'");
    }
    // 2. SIGKILL at moments spread over a build of LUBM (~100k triples).
    rebuild_old();
    let t = Instant::now();
    let out = Command::new(gbuild)
        .args(["--disk", "--force"])
        .arg(&name)
        .arg(lubm_nt())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let full = t.elapsed();
    let mut kills = 0;
    let mut seen = [false; 2];
    for i in 0..24u32 {
        if started.elapsed() > Duration::from_secs(25) {
            break;
        }
        rebuild_old();
        let delay = full.mul_f64(f64::from(i % 12) / 11.0 * 1.1);
        let mut child = Command::new(gbuild)
            .args(["--disk", "--force"])
            .arg(&name)
            .arg(lubm_nt())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(delay);
        let _ = child.kill();
        let _ = child.wait();
        kills += 1;
        seen[usize::from(classify(count()))] = true;
    }
    assert!(kills >= 6, "only {kills} kills in time");
    assert!(seen[0], "no kill landed before the install");
    let _ = fs::remove_dir_all(&base);
}

/// The helper process of [`a_directory_held_by_another_process_is_refused`]:
/// holds a directory's lock until told to stop (does nothing in a normal run).
#[test]
fn lock_holder_helper() {
    let Ok(dir) = std::env::var("GSTORE_TEST_HOLD_LOCK") else {
        return;
    };
    let _db = Database::open(&dir).unwrap();
    println!("locked");
    let stop = PathBuf::from(format!("{dir}.stop"));
    let until = Instant::now() + Duration::from_secs(30);
    while !stop.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_directory_held_by_another_process_is_refused() {
    let base = tmp("lock");
    let dir = base.join("held.db");
    make_prior(&dir, Prior::Disk);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock_holder_helper",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("GSTORE_TEST_HOLD_LOCK", &dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines.any(|l| l.map(|l| l.contains("locked")).unwrap_or(false)),
        "the helper did not take the lock"
    );
    let e = build_new(&dir).unwrap_err();
    assert!(e.to_string().contains("locked by another process"), "{e}");
    fs::write(dir.with_extension("db.stop"), "").unwrap();
    let _ = child.wait();
    // The old database is intact, and the build goes through now.
    assert_eq!(which(&dir, Prior::Disk), Some(false));
    build_new(&dir).unwrap();
    assert_eq!(which(&dir, Prior::Disk), Some(true));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_second_build_in_this_process_and_a_stale_staging_area() {
    let base = tmp("inproc");
    let dir = base.join("x.db");
    make_prior(&dir, Prior::Disk);
    {
        let _guard = gstore::db::dirlock::BuildGuard::enter(&dir).unwrap();
        assert!(gstore::db::dirlock::BuildGuard::active(&dir));
        let e = build_new(&dir).unwrap_err();
        assert!(e.to_string().contains("being rebuilt"), "{e}");
    }
    assert!(!gstore::db::dirlock::BuildGuard::active(&dir));
    // A database of this process holding the directory is not pulled from
    // under it.
    {
        let _open = Database::open(&dir).unwrap();
        let e = build_new(&dir).unwrap_err();
        assert!(e.to_string().contains("open in this process"), "{e}");
    }
    // What a crashed build leaves behind is removed by the next one.
    let staging = dir.join(bulk::STAGING_DIR);
    fs::create_dir_all(staging.join("gstore-bulk-scratch-1-0")).unwrap();
    fs::write(staging.join("kvstore.kv"), "partial").unwrap();
    fs::write(staging.join("COMMIT"), "kvstore.kv\n").unwrap();
    assert_eq!(
        which(&dir, Prior::Disk),
        Some(false),
        "a stale staging area is not a database"
    );
    build_new(&dir).unwrap();
    assert!(!staging.exists());
    assert_eq!(which(&dir, Prior::Disk), Some(true));
    // A spill directory elsewhere is used and removed. What a crashed or
    // killed build left there (its LOCK file unlocked) is removed by the next
    // build; a live build's (its LOCK held, whatever its pid) is kept.
    let spill = base.join("spill");
    let dead = spill.join("gstore-bulk-scratch-1-0-dead");
    fs::create_dir_all(&dead).unwrap();
    fs::write(dead.join("LOCK"), b"").unwrap();
    fs::write(dead.join("spo-0.run"), vec![0u8; 4096]).unwrap();
    let live = spill.join("gstore-bulk-scratch-1-1-live");
    fs::create_dir_all(&live).unwrap();
    let live_lock = fs::File::create(live.join("LOCK")).unwrap();
    let locked = live_lock.try_lock().is_ok();
    fs::write(live.join("spo-0.run"), b"live").unwrap();
    let r = bulk::build(
        &dir,
        "x",
        BulkInput::Sources(&[Source::File(lubm_nt())]),
        &ImportOptions::strict(),
        &mut (),
        BulkTarget::Disk,
        &BulkOptions {
            memory_budget: 1 << 20,
            temp_dir: Some(spill.clone()),
            ..BulkOptions::default()
        },
    )
    .unwrap();
    assert!(r.spilled_runs > 0);
    if locked {
        assert!(!dead.exists(), "a dead build's spill directory is swept");
        assert_eq!(fs::read(live.join("spo-0.run")).unwrap(), b"live");
        drop(live_lock);
        fs::remove_dir_all(&live).unwrap();
    } else {
        // No advisory locks on this file system: nothing is swept.
        fs::remove_dir_all(&dead).unwrap();
        fs::remove_dir_all(&live).unwrap();
    }
    assert_eq!(fs::read_dir(&spill).unwrap().count(), 0, "scratch removed");
    let _ = fs::remove_dir_all(&base);
}

/// A RocksDB store in use is never replaced. `Database::open` takes the
/// directory lock for that layout too, but an older binary relies on
/// RocksDB's own `rocksdb/LOCK` alone, so the build checks that lock itself
/// before staging anything. A held `rocksdb/LOCK` (and no directory lock)
/// refuses the build and leaves the store as it was; once released, the
/// build replaces it.
#[cfg(target_os = "linux")]
#[test]
fn a_rocksdb_store_in_use_is_not_replaced() {
    let base = tmp("rocks_in_use");
    let dir = base.join("r.db");
    fs::create_dir_all(&dir).unwrap();
    make_prior(&dir, Prior::RocksDb);
    let lock_path = dir.join("rocksdb/LOCK");
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    if holder.try_lock().is_err() {
        // No advisory locks on this file system: nothing to check.
        let _ = fs::remove_dir_all(&base);
        return;
    }
    let e = build_new(&dir).unwrap_err().to_string();
    assert!(e.contains("RocksDB store") && e.contains("in use"), "{e}");
    assert!(dir.join("rocksdb").is_dir(), "the live store is untouched");
    assert!(!dir.join(bulk::STAGING_DIR).exists());
    assert_eq!(which(&dir, Prior::RocksDb), Some(false));
    drop(holder);
    // Released. A child process another test of this binary spawns at this
    // moment shares the descriptor (and so the lock) between its fork and
    // its exec, which closes it: such a refusal is retried briefly.
    let t0 = Instant::now();
    while let Err(e) = build_new(&dir) {
        let e = e.to_string();
        assert!(
            e.contains("in use") && t0.elapsed() < Duration::from_secs(20),
            "{e}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!dir.join("rocksdb").exists());
    assert_eq!(which(&dir, Prior::RocksDb), Some(true));
    let _ = fs::remove_dir_all(&base);
}

/// The same with a real, open RocksDB database of this process: it holds
/// the directory lock (refused as open in this process) as well as RocksDB's
/// own POSIX lock.
#[cfg(all(target_os = "linux", feature = "rocksdb"))]
#[test]
fn an_open_rocksdb_database_is_not_replaced() {
    let base = tmp("rocks_open");
    let dir = base.join("r.db");
    fs::create_dir_all(&dir).unwrap();
    make_prior(&dir, Prior::RocksDb);
    let live = Database::open(&dir).unwrap();
    let e = build_new(&dir).unwrap_err().to_string();
    assert!(e.contains("open in this process"), "{e}");
    assert!(dir.join("rocksdb").is_dir());
    drop(live);
    assert_eq!(which(&dir, Prior::RocksDb), Some(false));
    build_new(&dir).unwrap();
    assert_eq!(which(&dir, Prior::RocksDb), Some(true));
    let _ = fs::remove_dir_all(&base);
}

/// A committed `kvstore.kv.wal` of the store being replaced is replayed
/// before it is removed; a replay that fails part-way stops the build with
/// the log in place (it alone can finish the half-applied batch), never
/// removing it to go on.
#[test]
fn a_failed_replay_of_the_old_wal_keeps_the_log() {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
    let base = tmp("wal_replay");
    let dir = base.join("w.db");
    fs::create_dir_all(&dir).unwrap();
    make_prior(&dir, Prior::Disk);
    let kv = dir.join("kvstore.kv");
    let before = fs::read(&kv).unwrap();
    // A committed batch: page 1 as it is (harmless), then a page at the
    // largest page id, past what the file system can hold (ext4: EFBIG).
    let mut wal = b"GSTOREWL".to_vec();
    wal.extend_from_slice(&2u32.to_le_bytes());
    wal.extend_from_slice(&1u32.to_le_bytes());
    wal.extend_from_slice(&before[4096..8192]);
    wal.extend_from_slice(&u32::MAX.to_le_bytes());
    wal.extend_from_slice(&[0u8; 4096]);
    let crc = crc32(&wal[8..]);
    wal.extend_from_slice(&crc.to_le_bytes());
    wal.extend_from_slice(b"GSTORECM");
    let wal_path = dir.join("kvstore.kv.wal");
    fs::write(&wal_path, &wal).unwrap();
    // Can this file system hold a page at that offset at all?
    let fits = {
        use std::io::{Seek, SeekFrom, Write};
        let probe = base.join("probe");
        let mut f = fs::File::create(&probe).unwrap();
        let ok = f.seek(SeekFrom::Start(u64::from(u32::MAX) * 4096)).is_ok()
            && f.write_all(&[0u8; 4096]).is_ok();
        drop(f);
        fs::remove_file(&probe).unwrap();
        ok
    };
    match build_new(&dir) {
        Ok(_) => {
            // The replay succeeded, and the log was rightly removed.
            assert!(fits, "a replay that failed was ignored");
            assert!(!wal_path.exists());
            assert_eq!(which(&dir, Prior::Disk), Some(true));
        }
        Err(e) => {
            assert!(!fits, "{e}");
            let e = e.to_string();
            assert!(e.contains("write-ahead log"), "{e}");
            assert!(wal_path.is_file(), "the log is kept for the next open");
            assert_eq!(fs::read(&wal_path).unwrap(), wal);
            assert!(!dir.join(bulk::STAGING_DIR).exists());
            assert_eq!(fs::read(&kv).unwrap(), before, "the old store is in place");
            // Once the log is dealt with, the old database is there and a
            // rebuild succeeds.
            fs::remove_file(&wal_path).unwrap();
            assert_eq!(which(&dir, Prior::Disk), Some(false));
            build_new(&dir).unwrap();
            assert_eq!(which(&dir, Prior::Disk), Some(true));
        }
    }
    let _ = fs::remove_dir_all(&base);
}

/// Once the new `kvstore.kv` is in place (the install's commit point), a
/// failing cleanup step no longer fails the build — the new database is
/// live — but is reported as a warning, the remaining steps still run, and
/// the replaced database's reasoning rules (whose ledgers name the old
/// data's triples) are removed as part of the install.
#[test]
fn cleanup_after_the_commit_point_warns_and_drops_the_old_rules() {
    let base = tmp("post_commit");
    let dir = base.join("p.db");
    fs::create_dir_all(&dir).unwrap();
    make_prior(&dir, Prior::Disk);
    let rules = dir.join(gstore::reason::rules::RULE_DIR);
    fs::create_dir_all(rules.join("inferred")).unwrap();
    fs::write(rules.join("r.json"), "{}").unwrap();
    fs::write(
        rules.join("inferred/r.nt"),
        "<http://e/s> <http://e/p> <http://e/new1> .\n",
    )
    .unwrap();
    // A snapshot leftover the install cannot remove (a directory where a
    // file is expected): removing the other layouts fails after the commit.
    fs::create_dir_all(dir.join("store.bin.tmp/x")).unwrap();
    let r = build_new(&dir).expect("the new database is installed");
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(r.warnings[0].contains("other layouts"), "{:?}", r.warnings);
    assert!(!rules.exists(), "the replaced database's rules are gone");
    assert!(dir.join("vstree.kv").is_file(), "later steps still ran");
    assert!(!dir.join(bulk::STAGING_DIR).exists());
    assert_eq!(which(&dir, Prior::Disk), Some(true));
    let _ = fs::remove_dir_all(&base);
}
