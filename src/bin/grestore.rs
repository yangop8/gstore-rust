//! `grestore` — restore a database from a backup directory or zip archive.
//!
//! Mirrors gStore's `grestore` (`Database::restore`):
//!
//! ```text
//! grestore <db_name> <backup_dir | backup.zip>
//! grestore -db <db_name> -p <backup_dir | backup.zip>    # upstream 1.3
//! grestore -l <db_name>                                  # list its backups in ./backups/
//! ```
//!
//! The arguments come in upstream's order — the database, then the backup
//! (`-db`, then `-p`). (Before, the positional form took the backup first;
//! a first argument naming an existing backup while the second does not
//! is reported with a hint about the order, never restored the wrong way.)
//!
//! Restores the backup in `<backup_dir>` — any layout — into the target
//! database directory (`<db_name>.db`), replacing what it held, via
//! [`Database::restore_into`]: a snapshot backup is validated and re-saved;
//! on-disk (KVstore) and RocksDB backups are copied verbatim.
//!
//! A zipped backup (`….zip`, as `gbackup -z` and the `/api` `backup
//! compress=true` write; upstream `grestore` accepts one too) is unpacked
//! beside the target first (feature `zip`; see
//! [`Database::restore_from_zip`]). Unsafe entry names are refused; its size
//! is not limited — a backup is the operator's own, and `gbackup -z` packs
//! one of any size — unless `--unzip-max-size BYTES` / `--unzip-max-entries
//! N` set a limit.
//!
//! `-l`/`--list <db_name>` prints the backups `gbackup -db <db_name>` made in
//! the default backup directory `./backups/` (upstream's `-l`), zipped ones
//! included.
//!
//! Exit status: 0 restored, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::path::Path;
use std::process::ExitCode;

use gstore::zipio::ZipLimits;
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "grestore",
    usage: "grestore <db_name> <backup_dir | backup.zip>\n\
            grestore -db <db_name> -p <backup_dir | backup.zip>\n\
            grestore -l <db_name>\n\
            \t[--unzip-max-size BYTES] [--unzip-max-entries N]",
    help: "restore a database from a backup\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database to restore into.\n\
           \t-p,--path PATH\t\tThe backup to restore (e.g. ./backups/<db_name>.db_<timestamp>),\n\
           \t\t\t\ta directory or a zip archive (the latter needs the `zip`\n\
           \t\t\t\tcargo feature).\n\
           \t-l,--list NAME\t\tList the backups of NAME in the default path ./backups/.\n\
           \t--unzip-max-size BYTES\tThe most a zipped backup may unpack to (default: no limit).\n\
           \t--unzip-max-entries N\tThe most entries a zipped backup may hold (default: no limit).",
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

/// Whether `path` is an existing zip archive (`….zip`).
fn is_zip_file(path: &str) -> bool {
    Path::new(path).is_file() && path.to_ascii_lowercase().ends_with(".zip")
}

fn main() -> ExitCode {
    let mut args = match cli::parse(
        &cli::argv(),
        &[
            cli::DB,
            PATH,
            LIST,
            cli::UNZIP_MAX_SIZE,
            cli::UNZIP_MAX_ENTRIES,
        ],
    ) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    if let Some(name) = args.value(2) {
        if args.values.iter().filter(|v| v.is_some()).count() > 1 || !args.rest.is_empty() {
            return TOOL.usage_error("-l takes only the database name");
        }
        return list_backups(name);
    }
    if let Some(o) = args.unknown_option() {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    let limits = match cli::zip_limits(&args, 3, 4, ZipLimits::NONE) {
        Ok(l) => l,
        Err(e) => return TOOL.usage_error(&e),
    };
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
    let zipped = is_zip_file(backup);

    if !zipped && !Path::new(backup).is_dir() {
        eprintln!("backup directory '{backup}' does not exist");
        if Path::new(&name).is_dir() || is_zip_file(&name) {
            eprintln!(
                "(the order is `grestore <db_name> <backup_dir>`, as upstream's \
                 `grestore -db <db_name> -p <backup_dir>`; '{name}' is a backup)"
            );
        }
        return ExitCode::FAILURE;
    }

    let restored = if zipped {
        Database::restore_from_zip(backup, &dir, &limits).map_err(cli::zip_error)
    } else {
        Database::restore_into(backup, &dir).map_err(|e| e.to_string())
    };
    if let Err(e) = restored {
        eprintln!("restore of '{backup}' into '{dir}' failed: {e}");
        return ExitCode::FAILURE;
    }
    gstore::outln!("Restored '{backup}' into '{dir}'");
    ExitCode::SUCCESS
}

/// Print the backups of database `name` in the default backup directory:
/// the entries named `<name>.db_<timestamp>` or `<name>.db_<timestamp>.zip`
/// (what `gbackup -db [-z 1]` creates), oldest first.
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
    gstore::outln!("Backup Path");
    for f in &found {
        gstore::outln!("{}", Path::new(DEFAULT_BACKUP_PATH).join(f).display());
    }
    if found.is_empty() {
        eprintln!("no backup of '{name}' in '{DEFAULT_BACKUP_PATH}'");
    }
    ExitCode::SUCCESS
}
