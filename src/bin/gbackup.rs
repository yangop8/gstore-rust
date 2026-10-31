//! `gbackup` — back up a database into a backup directory.
//!
//! Mirrors gStore's `gbackup` (`Database::backup`):
//!
//! ```text
//! gbackup [-z|--zip [0|1]] <db_name> <backup_dir | backup.zip>
//! gbackup -db <db_name> [-p <backup_path>] [-z 0|1]   # upstream 1.3
//! ```
//!
//! The positional form writes the backup *into* `<backup_dir>`. Upstream's
//! form writes it, as upstream does, into a new directory
//! `<backup_path>/<db_name>.db_<timestamp>` (milliseconds since the epoch;
//! `backup_path` defaults to `./backups/` and is created when missing) —
//! the path `grestore -db <db_name> -p …` takes.
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
//! `<backup_dir>.zip` (or the `.zip` named; upstream's form:
//! `<backup_path>/<db_name>.db_<timestamp>.zip`), the backup's files at the
//! archive root — the form the `/api` `backup compress=true` writes and
//! `grestore` / the `/api` `restore` accept.

#[path = "common/cli.rs"]
mod cli;

use std::path::PathBuf;
use std::process::ExitCode;

use gstore::db::StorageKind;
use gstore::{db_dir_for, zipio, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gbackup",
    usage: "gbackup [-z|--zip [0|1]] <db_name> <backup_dir | backup.zip>\n\
            gbackup -db <db_name> [-p <backup_path>] [-z 0|1]",
    help: "back up a database\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name.\n\
           \t-p,--path DIR\t\tWhere the backup <db_name>.db_<timestamp> is created\n\
           \t\t\t\t(default ./backups/).\n\
           \t-z,--zip [0|1]\t\t1: pack the backup into a zip archive\n\
           \t\t\t\t(needs the `zip` cargo feature).",
};

const PATH: cli::Opt = cli::Opt {
    short: "p",
    long: "path",
};

/// Upstream's default backup directory (`conf.ini` `backup_path`).
const DEFAULT_BACKUP_PATH: &str = "./backups/";

/// Take `-z` / `--zip`, optionally followed by upstream's `0` / `1`, out of
/// `args`: whether to zip.
fn take_zip_flag(args: &mut Vec<String>) -> bool {
    let mut zip = false;
    while let Some(i) = args.iter().position(|a| a == "-z" || a == "--zip") {
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
    zip
}

/// The command line in the positional form (`<prog> <db_name> <backup_dir>`,
/// then any other argument) and whether to zip: upstream's `-db NAME [-p
/// PATH]` is translated to a new timestamped directory under `PATH`. `Err`
/// carries the exit status (`--help`, invalid arguments).
fn command_line() -> Result<(Vec<String>, bool), ExitCode> {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB, PATH]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return Err(ExitCode::SUCCESS);
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return Err(TOOL.usage_error(&e)),
    };
    let zip = take_zip_flag(&mut args.rest);
    if let Some(o) = args.unknown_option() {
        return Err(TOOL.usage_error(&format!("unknown option '{o}'")));
    }
    let mut out = vec![TOOL.name.to_string()];
    if !args.any_legacy() {
        out.append(&mut args.rest);
        return Ok((out, zip));
    }
    let name = match args.value(0) {
        Some(db) => db.to_string(),
        None => match args.rest.iter().position(|a| !a.starts_with('-')) {
            Some(i) => args.rest.remove(i),
            None => return Err(TOOL.usage_error("missing the database name")),
        },
    };
    let base = args.value(1).unwrap_or(DEFAULT_BACKUP_PATH);
    let target = std::path::Path::new(base).join(format!(
        "{}{}_{}",
        cli::db_base_name(&name),
        gstore::DB_SUFFIX,
        cli::timestamp()
    ));
    out.push(name);
    out.push(target.to_string_lossy().into_owned());
    out.append(&mut args.rest);
    Ok((out, zip))
}

fn main() -> ExitCode {
    let (args, zip) = match command_line() {
        Ok(a) => a,
        Err(code) => return code,
    };
    if args.len() != 3 {
        return TOOL.usage_error("");
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
