//! `gdrop` — delete a database directory.
//!
//! Mirrors gStore's `gdrop`. Deleting data is irreversible, so a confirmation
//! flag is required:
//!
//! ```text
//! gdrop <db_name> --yes              # or -y / --force
//! gdrop -db <db_name> --yes          # upstream 1.3's spelling
//! ```
//!
//! Without the flag the tool prints what it *would* delete and exits non-zero
//! (status 1). Upstream's `gdrop` deletes without asking; the confirmation is
//! kept here on purpose, for the upstream spelling too.
//!
//! Exit status: 0 dropped, 1 failed or not confirmed, 2 invalid arguments;
//! `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::process::ExitCode;

use gstore::db_dir_for;

const TOOL: cli::Tool = cli::Tool {
    name: "gdrop",
    usage: "gdrop <db_name> (--yes | -y | --force)\n\
            gdrop -db <db_name> (--yes | -y | --force)",
    help: "delete a database\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-y,--yes,--force\tConfirm the deletion (required: it cannot be undone).",
};

fn main() -> ExitCode {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    let confirmed = args.take_flag(&["-y", "--yes", "--force"]);
    if let Some(o) = args.unknown_option() {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    let name = match (args.value(0), args.rest.as_slice()) {
        (Some(db), []) => db.to_string(),
        (None, [db]) => db.clone(),
        (None, []) => return TOOL.usage_error("missing the database name"),
        _ => return TOOL.usage_error("too many arguments"),
    };
    let dir = db_dir_for(&name);

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
    gstore::outln!("Dropped database '{dir}'");
    ExitCode::SUCCESS
}
