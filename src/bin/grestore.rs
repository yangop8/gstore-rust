//! `grestore` — restore a database from a backup directory.
//!
//! Mirrors gStore's `grestore` (`Database::restore`):
//!
//! ```text
//! grestore <backup_dir> <db_name>
//! ```
//!
//! Restores the backup in `<backup_dir>` — any layout — into the target
//! database directory (`<db_name>.db`), replacing what it held, via
//! [`Database::restore_into`]: a snapshot backup is validated and re-saved;
//! on-disk (KVstore) and RocksDB backups are copied verbatim.

use std::process::ExitCode;

use gstore::{db_dir_for, Database};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <backup_dir> <db_name>", args[0]);
        return ExitCode::FAILURE;
    }
    let backup = &args[1];
    let dir = db_dir_for(&args[2]);

    if !std::path::Path::new(backup).is_dir() {
        eprintln!("backup directory '{backup}' does not exist");
        return ExitCode::FAILURE;
    }

    if let Err(e) = Database::restore_into(backup, &dir) {
        eprintln!("restore of '{backup}' into '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("Restored '{backup}' into '{dir}'");
    ExitCode::SUCCESS
}
