//! `gmonitor` — print monitoring information about a database.
//!
//! Mirrors gStore's `gmonitor` (`getDBMonitorInfo`): counts, index/transaction
//! state, and on-disk size.
//!
//! ```text
//! gmonitor <db_name>
//! ```

use std::path::Path;
use std::process::ExitCode;

use gstore::{db_dir_for, Database, TripleSource};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: {} <db_name>", args[0]);
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);

    let db = if Database::is_disk(&dir) {
        Database::load_disk(&dir)
    } else {
        Database::load(&dir)
    };
    let db = match db {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let s = db.stats();
    let backend = db.backend();
    let disk = dir_size(&dir);

    let rows: [(&str, String); 12] = [
        ("database", s.name.clone()),
        ("directory", dir.clone()),
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
        println!("{k:<width$} : {v}");
    }
    ExitCode::SUCCESS
}

/// Total byte size of the regular files directly inside `dir`.
fn dir_size(dir: &str) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(Path::new(dir)) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
    }
    total
}
