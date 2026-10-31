//! `grestore` — restore a database from a backup directory.
//!
//! Mirrors gStore's `grestore` (`Database::restore`):
//!
//! ```text
//! grestore <backup_dir | backup.zip> <db_name>
//! ```
//!
//! Restores the backup in `<backup_dir>` — any layout — into the target
//! database directory (`<db_name>.db`), replacing what it held, via
//! [`Database::restore_into`]: a snapshot backup is validated and re-saved;
//! on-disk (KVstore) and RocksDB backups are copied verbatim.
//!
//! A zipped backup (`….zip`, as `gbackup -z` and the `/api` `backup
//! compress=true` write; upstream `grestore` accepts one too) is unpacked
//! beside the target first, under the default zip limits (feature `zip`;
//! see [`Database::restore_from_zip`]).

use std::path::Path;
use std::process::ExitCode;

use gstore::zipio::ZipLimits;
use gstore::{db_dir_for, Database};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <backup_dir | backup.zip> <db_name>", args[0]);
        return ExitCode::FAILURE;
    }
    let backup = &args[1];
    let dir = db_dir_for(&args[2]);
    let zipped = Path::new(backup).is_file() && backup.to_ascii_lowercase().ends_with(".zip");

    if !zipped && !Path::new(backup).is_dir() {
        eprintln!("backup directory '{backup}' does not exist");
        return ExitCode::FAILURE;
    }

    let restored = if zipped {
        Database::restore_from_zip(backup, &dir, &ZipLimits::default())
    } else {
        Database::restore_into(backup, &dir)
    };
    if let Err(e) = restored {
        eprintln!("restore of '{backup}' into '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("Restored '{backup}' into '{dir}'");
    ExitCode::SUCCESS
}
