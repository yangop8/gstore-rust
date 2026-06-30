//! `gbackup` — back up a database into a backup directory.
//!
//! Mirrors gStore's `gbackup` (`Database::backup`):
//!
//! ```text
//! gbackup <db_name> <backup_dir>
//! ```
//!
//! For an in-memory (bincode) database this writes a consistent snapshot into
//! `<backup_dir>`. For an on-disk (KVstore) database it copies the database
//! files verbatim via [`Database::backup_dir`].

use std::process::ExitCode;

use gstore::{db_dir_for, Database};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <db_name> <backup_dir>", args[0]);
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);
    let backup = &args[2];

    if !std::path::Path::new(&dir).is_dir() {
        eprintln!("database '{dir}' does not exist");
        return ExitCode::FAILURE;
    }

    let result = if Database::is_disk(&dir) {
        // On-disk store: copy the files directly (no in-RAM materialization).
        Database::backup_dir(&dir, backup)
    } else {
        match Database::load(&dir) {
            Ok(db) => db.backup(backup),
            Err(e) => {
                eprintln!("cannot load database '{dir}': {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    if let Err(e) = result {
        eprintln!("backup of '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("Backed up '{dir}' to '{backup}'");
    ExitCode::SUCCESS
}
