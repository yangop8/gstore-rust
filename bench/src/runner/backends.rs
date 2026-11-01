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
//! afterwards is `gbuild`'s own peak RSS.
//!
//! The `lib` builder calls the library's build functions in-process instead.
//! It exists for the harness's own tests when the gstore binaries are not
//! built (`cargo test -p gstore-bench` alone does not build them); results
//! record which builder ran (`config.builder`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::schema::BuildResult;
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
    fs::create_dir_all(work_dir).map_err(|e| format!("cannot create {}: {e}", work_dir.display()))?;
    let mut res = match builder {
        "cli" => build_cli(backend, bin_dir, gbuild_args, work_dir, data, w)?,
        "lib" => build_lib(backend, &dir, data, w)?,
        other => return Err(format!("unknown builder '{other}' (cli, lib)")),
    };
    let (bytes, files) =
        sys::dir_bytes(&dir).map_err(|e| format!("cannot size {}: {e}", dir.display()))?;
    res.dir_bytes = bytes;
    res.files = files;
    Ok(res)
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
    let status = Command::new(&exe)
        .args(&args)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .status()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let peak_rss_mib = sys::children_peak_rss_mib();
    if !status.success() {
        let text = fs::read_to_string(&log_path).unwrap_or_default();
        let tail: String = text.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(format!("{} {} failed ({status}):\n{tail}", exe.display(), args.join(" ")));
    }
    let mut command = vec![exe.to_string_lossy().into_owned()];
    command.extend(args);
    Ok(BuildResult {
        wall_ms,
        peak_rss_mib,
        command,
        ..BuildResult::default()
    })
}

fn build_lib(backend: &str, dir: &Path, data: &Path, w: &dyn Workload) -> Result<BuildResult, String> {
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
