//! `gmonitor` — print monitoring information about a database.
//!
//! Mirrors gStore's `gmonitor` (`getDBMonitorInfo`): counts, index/transaction
//! state, storage layout, and on-disk size (including a RocksDB store).
//!
//! ```text
//! gmonitor <db_name>
//! gmonitor -db <db_name>     # upstream 1.3
//! ```
//!
//! Exit status: 0 shown, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::path::Path;
use std::process::ExitCode;

use gstore::{db_dir_for, Database, TripleSource};

const TOOL: cli::Tool = cli::Tool {
    name: "gmonitor",
    usage: "gmonitor <db_name>\n\
            gmonitor -db <db_name>",
    help: "print statistics about a database\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the argument).",
};

fn main() -> ExitCode {
    let args = match cli::parse(&cli::argv(), &[cli::DB]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
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

    let db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let s = db.stats();
    let backend = db.backend();
    let disk = dir_size(&dir);

    let rows: [(&str, String); 13] = [
        ("database", s.name.clone()),
        ("directory", dir.clone()),
        ("storage", db.storage().to_string()),
        ("triple_num", s.triple_num.to_string()),
        ("entity_num", s.entity_num.to_string()),
        ("literal_num", s.literal_num.to_string()),
        ("predicate_num", s.predicate_num.to_string()),
        ("subject_num", backend.distinct_subjects().to_string()),
        ("object_num", backend.distinct_objects().to_string()),
        ("named_graphs", db.named_graphs().len().to_string()),
        (
            "index_status",
            if s.index_valid { "valid" } else { "stale" }.to_string(),
        ),
        ("in_transaction", s.in_transaction.to_string()),
        ("disk_used", format!("{disk} bytes")),
    ];
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &rows {
        gstore::outln!("{k:<width$} : {v}");
    }
    ExitCode::SUCCESS
}

/// Total byte size of the regular files inside `dir`, recursively (so a
/// RocksDB database's `rocksdb/` store is counted).
fn dir_size(dir: impl AsRef<Path>) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir.as_ref()) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += dir_size(entry.path());
                }
            }
        }
    }
    total
}
