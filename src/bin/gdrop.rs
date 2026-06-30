//! `gdrop` — delete a database directory.
//!
//! Mirrors gStore's `gdrop`. Deleting data is irreversible, so a confirmation
//! flag is required:
//!
//! ```text
//! gdrop <db_name> --yes        # or -y / --force
//! ```
//!
//! Without the flag the tool prints what it *would* delete and exits non-zero.

use std::process::ExitCode;

use gstore::db_dir_for;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let confirmed = args
        .iter()
        .any(|a| a == "-y" || a == "--yes" || a == "--force");
    args.retain(|a| a != "-y" && a != "--yes" && a != "--force");
    if args.len() != 2 {
        eprintln!("usage: {} <db_name> (--yes | -y | --force)", args[0]);
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);

    if !std::path::Path::new(&dir).is_dir() {
        eprintln!("database '{dir}' does not exist");
        return ExitCode::FAILURE;
    }
    if !confirmed {
        eprintln!(
            "refusing to drop '{dir}' without confirmation; \
             re-run with --yes (or -y / --force) to delete it"
        );
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("failed to drop '{dir}': {e}");
        return ExitCode::FAILURE;
    }
    println!("Dropped database '{dir}'");
    ExitCode::SUCCESS
}
