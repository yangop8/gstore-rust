//! `grestore` — restore a database from a backup directory.
//!
//! Mirrors gStore's `grestore` (`Database::restore`):
//!
//! ```text
//! grestore <backup_dir> <db_name>
//! ```
//!
//! Reads the snapshot in `<backup_dir>` and writes it to the target database
//! directory (`<db_name>.db`). On-disk (KVstore) backups are copied verbatim.

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

    let result = if Database::is_disk(backup) {
        // On-disk backup: copy the files into the target directory.
        Database::backup_dir(backup, &dir)
    } else {
        match Database::restore(backup) {
            Ok(db) => db.save(&dir),
            Err(e) => {
                eprintln!("cannot restore from '{backup}': {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    if let Err(e) = result {
        eprintln!("restore into '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("Restored '{backup}' into '{dir}'");
    ExitCode::SUCCESS
}
