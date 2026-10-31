//! `grestore` — restore a database from a backup directory.
//!
//! Mirrors gStore's `grestore` (`Database::restore`):
//!
//! ```text
//! grestore <db_name> <backup_dir>
//! grestore -db <db_name> -p <backup_dir>    # upstream 1.3
//! grestore -l <db_name>                     # list its backups in ./backups/
//! ```
//!
//! The arguments come in upstream's order — the database, then the backup
//! (`-db`, then `-p`). (Before, the positional form took the backup first;
//! a first argument naming an existing directory while the second does not
//! is reported with a hint about the order, never restored the wrong way.)
//!
//! Restores the backup in `<backup_dir>` — any layout — into the target
//! database directory (`<db_name>.db`), replacing what it held, via
//! [`Database::restore_into`]: a snapshot backup is validated and re-saved;
//! on-disk (KVstore) and RocksDB backups are copied verbatim.
//!
//! `-l`/`--list <db_name>` prints the backups `gbackup -db <db_name>` made in
//! the default backup directory `./backups/` (upstream's `-l`).
//!
//! Exit status: 0 restored, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::path::Path;
use std::process::ExitCode;

use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "grestore",
    usage: "grestore <db_name> <backup_dir>\n\
            grestore -db <db_name> -p <backup_dir>\n\
            grestore -l <db_name>",
    help: "restore a database from a backup\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database to restore into.\n\
           \t-p,--path DIR\t\tThe backup to restore (e.g. ./backups/<db_name>.db_<timestamp>).\n\
           \t-l,--list NAME\t\tList the backups of NAME in the default path ./backups/.",
};

const PATH: cli::Opt = cli::Opt {
    short: "p",
    long: "path",
};
const LIST: cli::Opt = cli::Opt {
    short: "l",
    long: "list",
};

/// Upstream's default backup directory (`conf.ini` `backup_path`).
const DEFAULT_BACKUP_PATH: &str = "./backups/";

fn main() -> ExitCode {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB, PATH, LIST]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    if let Some(name) = args.value(2) {
        if args.value(0).is_some() || args.value(1).is_some() || !args.rest.is_empty() {
            return TOOL.usage_error("-l takes only the database name");
        }
        return list_backups(name);
    }
    if let Some(o) = args.unknown_option() {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    // Upstream's order: the database, then the backup.
    let mut positional = std::mem::take(&mut args.rest).into_iter();
    let name = match args.value(0) {
        Some(db) => Some(db.to_string()),
        None => positional.next(),
    };
    let backup = match args.value(1) {
        Some(p) => Some(p.to_string()),
        None => positional.next(),
    };
    let (Some(name), Some(backup)) = (name, backup) else {
        return TOOL.usage_error("give the database name and the backup directory");
    };
    if positional.next().is_some() {
        return TOOL.usage_error("too many arguments");
    }
    let backup = &backup;
    let dir = db_dir_for(&name);

    if !Path::new(backup).is_dir() {
        eprintln!("backup directory '{backup}' does not exist");
        if Path::new(&name).is_dir() {
            eprintln!(
                "(the order is `grestore <db_name> <backup_dir>`, as upstream's \
                 `grestore -db <db_name> -p <backup_dir>`; '{name}' is a directory)"
            );
        }
        return ExitCode::FAILURE;
    }

    if let Err(e) = Database::restore_into(backup, &dir) {
        eprintln!("restore of '{backup}' into '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("Restored '{backup}' into '{dir}'");
    ExitCode::SUCCESS
}

/// Print the backups of database `name` in the default backup directory:
/// the entries named `<name>.db_<timestamp>` (what `gbackup -db` creates),
/// oldest first.
fn list_backups(name: &str) -> ExitCode {
    let prefix = format!("{}{}_", cli::db_base_name(name), gstore::DB_SUFFIX);
    let mut found: Vec<String> = match std::fs::read_dir(DEFAULT_BACKUP_PATH) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&prefix))
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    println!("Backup Path");
    for f in &found {
        println!("{}", Path::new(DEFAULT_BACKUP_PATH).join(f).display());
    }
    if found.is_empty() {
        eprintln!("no backup of '{name}' in '{DEFAULT_BACKUP_PATH}'");
    }
    ExitCode::SUCCESS
}
