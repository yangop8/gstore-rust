//! The storage layouts and how a database of each is built — the one place
//! that knows the `gbuild` command lines.
//!
//! | backend    | build                     | directory                 |
//! |------------|---------------------------|---------------------------|
//! | `snapshot` | `gbuild <name> <data>`    | `dict.bin`, `store.bin`, … |
//! | `disk`     | `gbuild --disk …`         | `kvstore.kv`, `vstree.kv` |
//! | `rocksdb`  | `gbuild --rocksdb …`      | `rocksdb/`                |
//!
//! `gbuild` is taken from `--bin-dir` (default `target/release`) and runs with
//! the backend's work directory as its current directory, so it creates
//! `<work>/<name>.db`. `--gbuild-arg X` values go before the database name.
//! The child that runs it does nothing else, so `getrusage(RUSAGE_CHILDREN)`
//! afterwards is `gbuild`'s own peak RSS. Its output goes to
//! `<work>/gbuild.log`; a bulk build's report there (phases, memory budget
//! and peak, term map, spills) becomes the result's `build.summary`.
//!
//! The `lib` builder calls the library's build functions in-process instead.
//! It exists for the harness's own tests when the gstore binaries are not
//! built (`cargo test -p gstore-bench` alone does not build them); results
//! record which builder ran (`config.builder`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::schema::{BuildResult, BuildSummary};
use crate::sys;
use crate::workload::Workload;

/// Every backend the harness knows, in reporting order.
pub const ALL: [&str; 3] = ["snapshot", "disk", "rocksdb"];

/// The backends this build of the harness can open (`rocksdb` needs the
/// `rocksdb` feature).
pub fn compiled() -> Vec<&'static str> {
    if cfg!(feature = "rocksdb") {
        ALL.to_vec()
    } else {
        vec!["snapshot", "disk"]
    }
}

/// Check a backend name against [`compiled`].
pub fn validate(backend: &str) -> Result<(), String> {
    if !ALL.contains(&backend) {
        return Err(format!(
            "unknown backend '{backend}' (snapshot, disk, rocksdb)"
        ));
    }
    if !compiled().contains(&backend) {
        return Err(format!(
            "backend '{backend}' needs gstore-bench built with --features {backend}"
        ));
    }
    Ok(())
}

/// The `gbuild` flag selecting `backend`.
pub fn gbuild_flag(backend: &str) -> Option<&'static str> {
    match backend {
        "disk" => Some("--disk"),
        "rocksdb" => Some("--rocksdb"),
        _ => None,
    }
}

/// `gbuild` inside `bin_dir`.
pub fn gbuild_path(bin_dir: &Path) -> PathBuf {
    bin_dir.join(format!("gbuild{}", std::env::consts::EXE_SUFFIX))
}

/// The database directory `gbuild` creates in `work_dir`.
pub fn db_dir(work_dir: &Path, w: &dyn Workload) -> PathBuf {
    work_dir.join(format!("{}.db", w.db_name()))
}

/// Build `backend`'s database of `data` in `work_dir` with `builder`
/// (`cli` or `lib`).
pub fn build(
    backend: &str,
    builder: &str,
    bin_dir: &Path,
    gbuild_args: &[String],
    work_dir: &Path,
    data: &Path,
    w: &dyn Workload,
) -> Result<BuildResult, String> {
    let dir = db_dir(work_dir, w);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
    }
    fs::create_dir_all(work_dir)
        .map_err(|e| format!("cannot create {}: {e}", work_dir.display()))?;
    let mut res = match builder {
        "cli" => build_cli(backend, bin_dir, gbuild_args, work_dir, data, w)?,
        "lib" => build_lib(backend, &dir, data, w)?,
        other => return Err(format!("unknown builder '{other}' (cli, lib)")),
    };
    let (bytes, files) =
        sys::dir_bytes(&dir).map_err(|e| format!("cannot size {}: {e}", dir.display()))?;
    res.dir_bytes = bytes.saturating_sub(uncounted_bytes(&dir, &files));
    res.files = files;
    Ok(res)
}

/// gbuild's log of statements that did not parse.
const PARSE_LOG: &str = "parse_error.log";

/// Bytes of a database directory that are not database content, so that two
/// builds of the same data count the same: gbuild's `parse_error.log` (its
/// first line names the source path), the disk layout's `LOCK` (it holds
/// the builder's pid, whose digit count varies) and RocksDB's info logs
/// (`rocksdb/LOG*`). They stay listed in `files`.
pub fn uncounted_bytes(dir: &Path, files: &std::collections::BTreeMap<String, u64>) -> u64 {
    let mut n =
        files.get(PARSE_LOG).copied().unwrap_or(0) + files.get("LOCK").copied().unwrap_or(0);
    if let Ok(rd) = fs::read_dir(dir.join("rocksdb")) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("LOG") {
                n += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    n
}

/// gbuild's bulk-build report (`phases:`, `memory:` and `spills:` lines of
/// its output; see `gstore::bulk::BulkReport::summary_for`), if `log` has
/// one.
pub fn parse_summary(log: &str) -> Option<BuildSummary> {
    let num = |s: &str| s.trim().parse::<f64>().ok();
    let mut out = BuildSummary::default();
    let mut found = false;
    for line in log.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("phases:") {
            found = true;
            for item in rest.split_whitespace() {
                if let Some((name, ms)) = item.split_once('=') {
                    if let Some(v) = ms.strip_suffix("ms").and_then(num) {
                        out.phases_ms.push((name.to_string(), v));
                    }
                }
            }
        } else if let Some(rest) = l.strip_prefix("memory:") {
            // "budget 1024 MiB, peak 380.2 MiB; term map 55.1 MiB; threads 2"
            found = true;
            for part in rest.split([';', ',']) {
                let p = part.trim();
                let mib = |prefix: &str| {
                    p.strip_prefix(prefix)
                        .and_then(|x| x.trim().strip_suffix("MiB"))
                        .and_then(num)
                };
                if let Some(v) = mib("budget") {
                    out.budget_mib = Some(v);
                } else if let Some(v) = mib("peak") {
                    out.budget_peak_mib = Some(v);
                } else if let Some(v) = mib("term map") {
                    out.term_map_mib = Some(v);
                } else if let Some(t) = p.strip_prefix("threads") {
                    out.threads = t.trim().parse().ok();
                }
            }
        } else if let Some(rest) = l.strip_prefix("spills:") {
            // "3 run(s), 120.5 MiB; kvstore.kv …"
            found = true;
            let head = rest.split(';').next().unwrap_or("");
            let mut it = head.split(',');
            out.spilled_runs = it
                .next()
                .and_then(|x| x.split_whitespace().next())
                .and_then(|x| x.parse().ok());
            out.spilled_mib = it
                .next()
                .and_then(|x| x.trim().strip_suffix("MiB"))
                .and_then(num);
        }
    }
    found.then_some(out)
}

fn build_cli(
    backend: &str,
    bin_dir: &Path,
    gbuild_args: &[String],
    work_dir: &Path,
    data: &Path,
    w: &dyn Workload,
) -> Result<BuildResult, String> {
    let exe = gbuild_path(bin_dir);
    if !exe.is_file() {
        return Err(format!(
            "{} not found: build it (`cargo build --release -p gstore --bins`) or pass --bin-dir",
            exe.display()
        ));
    }
    // `gbuild` runs in the work directory: make both paths absolute first.
    let exe = fs::canonicalize(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let data = fs::canonicalize(data).map_err(|e| format!("{}: {e}", data.display()))?;
    let mut args: Vec<String> = Vec::new();
    if let Some(f) = gbuild_flag(backend) {
        args.push(f.into());
    }
    args.extend(gbuild_args.iter().cloned());
    args.push(w.db_name().into());
    args.push(data.to_string_lossy().into_owned());
    let log_path = work_dir.join("gbuild.log");
    let log = fs::File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // gbuild ends with this phase child, which ends with the harness.
        let parent = std::process::id();
        // SAFETY: the hook only makes async-signal-safe syscalls.
        unsafe {
            cmd.pre_exec(move || sys::die_with_parent(parent));
        }
    }
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let peak_rss_mib = sys::children_peak_rss_mib();
    if !status.success() {
        let text = fs::read_to_string(&log_path).unwrap_or_default();
        let tail: String = text
            .chars()
            .rev()
            .take(2000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(format!(
            "{} {} failed ({status}):\n{tail}",
            exe.display(),
            args.join(" ")
        ));
    }
    let mut command = vec![exe.to_string_lossy().into_owned()];
    command.extend(args);
    let summary = fs::read_to_string(&log_path)
        .ok()
        .and_then(|t| parse_summary(&t));
    Ok(BuildResult {
        wall_ms,
        peak_rss_mib,
        command,
        summary,
        ..BuildResult::default()
    })
}

fn build_lib(
    backend: &str,
    dir: &Path,
    data: &Path,
    w: &dyn Workload,
) -> Result<BuildResult, String> {
    use gstore::Database;
    let started = Instant::now();
    let files = [data];
    match backend {
        "snapshot" => {
            let db = Database::build_from_files(w.db_name(), &files).map_err(|e| e.to_string())?;
            db.save(dir).map_err(|e| e.to_string())?;
        }
        "disk" => Database::build_disk(dir, &files).map_err(|e| e.to_string())?,
        #[cfg(feature = "rocksdb")]
        "rocksdb" => {
            Database::build_rocksdb_from_files(w.db_name(), dir, &files)
                .map_err(|e| e.to_string())?;
        }
        other => return Err(format!("the lib builder cannot build '{other}'")),
    }
    Ok(BuildResult {
        wall_ms: started.elapsed().as_secs_f64() * 1e3,
        peak_rss_mib: sys::hwm_mib(),
        command: vec![format!("library build ({backend})")],
        ..BuildResult::default()
    })
}
