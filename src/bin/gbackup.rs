//! `gbackup` — back up a database into a backup directory.
//!
//! Mirrors gStore's `gbackup` (`Database::backup`):
//!
//! ```text
//! gbackup [-z|--zip [0|1]] <db_name> <backup_dir | backup.zip>
//! ```
//!
//! For an in-memory (bincode) database this writes a consistent snapshot into
//! `<backup_dir>`. For an on-disk (KVstore) database it copies the database
//! files — including any pending write-ahead logs — verbatim via
//! [`Database::backup_dir`], without loading the data. For a RocksDB database
//! (built with `--features rocksdb`) it writes a RocksDB checkpoint, a
//! consistent copy of every column family; without the feature the store
//! directory is copied file by file (only safe while no process has it open).
//!
//! `-z` / `--zip` (upstream's `gbackup -z 1`), or a destination ending in
//! `.zip`, packs the backup into a zip archive instead (feature `zip`):
//! `<backup_dir>.zip` (or the `.zip` named), the backup's files at the archive
//! root — the form the `/api` `backup compress=true` writes and `grestore` /
//! the `/api` `restore` accept.

use std::path::PathBuf;
use std::process::ExitCode;

use gstore::db::StorageKind;
use gstore::{db_dir_for, zipio, Database};

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    // `-z` / `--zip`, optionally followed by upstream's `0` / `1`.
    let mut zip = false;
    if let Some(i) = args.iter().position(|a| a == "-z" || a == "--zip") {
        args.remove(i);
        zip = match args.get(i).map(String::as_str) {
            Some("0") => {
                args.remove(i);
                false
            }
            Some("1") => {
                args.remove(i);
                true
            }
            _ => true,
        };
    }
    if args.len() != 3 {
        eprintln!("usage: {} [-z|--zip] <db_name> <backup_dir>", args[0]);
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);
    let backup = &args[2];
    // A zipped backup is written into `<archive>.backup-tmp`, then packed.
    let archive = if backup.ends_with(".zip") {
        Some(PathBuf::from(backup))
    } else if zip {
        Some(PathBuf::from(format!("{backup}.zip")))
    } else {
        None
    };
    if archive.is_some() && !zipio::AVAILABLE {
        eprintln!("gbackup: {}", zipio::FEATURE_MISSING);
        return ExitCode::FAILURE;
    }
    let target = match &archive {
        Some(a) => format!("{}.backup-tmp", a.display()),
        None => backup.clone(),
    };

    if !std::path::Path::new(&dir).is_dir() {
        eprintln!("database '{dir}' does not exist");
        return ExitCode::FAILURE;
    }

    let result = match Database::storage_kind(&dir) {
        // On-disk store: copy the files directly (no in-RAM materialization).
        Some(StorageKind::Disk) => Database::backup_dir(&dir, &target),
        Some(StorageKind::RocksDb) if !cfg!(feature = "rocksdb") => {
            Database::backup_dir(&dir, &target)
        }
        _ => match Database::open(&dir) {
            Ok(db) => db.backup(&target),
            Err(e) => {
                eprintln!("cannot load database '{dir}': {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let result = match (&archive, result) {
        (Some(a), Ok(())) => Database::pack_backup(&target, a).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&target);
        }),
        (_, r) => r,
    };
    if let Err(e) = result {
        eprintln!("backup of '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    let shown = archive.map_or_else(|| backup.clone(), |a| a.display().to_string());
    println!("Backed up '{dir}' to '{shown}'");
    ExitCode::SUCCESS
}
