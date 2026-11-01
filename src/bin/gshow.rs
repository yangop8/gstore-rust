//! `gshow` — list the databases (upstream's `gshow`), or print one
//! database's counts and status.
//!
//! ```text
//! gshow [--db-home DIR]          # every database in DIR (default: .)
//! gshow <db_name>                # one database's counts (gstore-rust extension)
//! gshow -db <db_name>
//! ```
//!
//! Without a database name, `gshow` lists every database found in the
//! directory — like the other command-line tools, the current one, or
//! `--db-home DIR` (e.g. a server's `db_home`) — as upstream's table of
//! `database`, `creator` and `built_time`. Creator and build time come from
//! the server's registry (`system.json`) when the directory holds one; a
//! database it does not know is shown as built by `root` at its directory's
//! modification time (what the server records when it first finds it).
//!
//! With a database name, it prints triple/entity/literal/predicate counts
//! plus index and transaction state, using [`Database::stats`].
//!
//! Exit status: 0 shown, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use gstore::json::{self, Json};
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gshow",
    usage: "gshow [--db-home DIR]\n\
            gshow <db_name>\n\
            gshow -db <db_name>",
    help: "list the databases, or show one database's counts\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t--db-home DIR\t\tList the databases in DIR (default: the current directory).\n\
           \t-db,--database NAME\tShow the counts and status of database NAME.",
};

const DB_HOME: cli::Opt = cli::Opt {
    short: "",
    long: "db-home",
};

fn main() -> ExitCode {
    let args = match cli::parse(&cli::argv(), &[cli::DB, DB_HOME]) {
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
        (Some(db), []) => Some(db.to_string()),
        (None, [db]) => Some(db.clone()),
        (None, []) => None,
        _ => return TOOL.usage_error("too many arguments"),
    };
    match name {
        Some(_) if args.value(1).is_some() => {
            TOOL.usage_error("--db-home lists databases; it does not take a database name")
        }
        Some(name) => show_one(&name),
        None => list_all(Path::new(args.value(1).unwrap_or("."))),
    }
}

/// Upstream's `gshow`: every database in `home`.
fn list_all(home: &Path) -> ExitCode {
    let entries = match std::fs::read_dir(home) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot read '{}': {e}", home.display());
            return ExitCode::FAILURE;
        }
    };
    let registry = read_registry(home);
    let mut rows: BTreeMap<String, [String; 3]> = BTreeMap::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file_name.strip_suffix(gstore::DB_SUFFIX) else {
            continue;
        };
        let path = entry.path();
        if name.is_empty() || !path.is_dir() || Database::storage_kind(&path).is_none() {
            continue;
        }
        let (creator, built) = registry.get(name).cloned().unwrap_or_else(|| {
            let built = gstore::sysdb::modified_datetime(&path).unwrap_or_default();
            ("root".to_string(), built)
        });
        rows.insert(name.to_string(), [name.to_string(), creator, built]);
    }
    print_table(
        &["database", "creator", "built_time"],
        &rows.into_values().collect::<Vec<_>>(),
    );
    ExitCode::SUCCESS
}

/// `database → (creator, built_time)` from the server registry
/// (`<home>/system.json`), if there is a readable one.
fn read_registry(home: &Path) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(home.join(gstore::sysdb::SYSTEM_FILE)) else {
        return out;
    };
    let Ok(doc) = json::parse(&text) else {
        return out;
    };
    for item in doc
        .get("databases")
        .and_then(Json::as_array)
        .unwrap_or_default()
    {
        let field = |k: &str| item.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        let name = field("database");
        if !name.is_empty() {
            out.insert(name, (field("creator"), field("built_time")));
        }
    }
    out
}

/// Print an ASCII table as upstream's `Util::printConsole` (`PrettyPrint`).
fn print_table(headers: &[&str], rows: &[[String; 3]]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let rule: String = widths
        .iter()
        .map(|w| format!("+{}", "-".repeat(w + 2)))
        .collect::<String>()
        + "+";
    let line = |cells: Vec<&str>| -> String {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("| {c:<w$} "))
            .collect::<String>()
            + "|"
    };
    gstore::outln!("{rule}");
    gstore::outln!("{}", line(headers.to_vec()));
    gstore::outln!("{rule}");
    for row in rows {
        gstore::outln!("{}", line(row.iter().map(String::as_str).collect()));
    }
    gstore::outln!("{rule}");
}

/// One database's counts and status.
fn show_one(name: &str) -> ExitCode {
    let dir = db_dir_for(name);

    let db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let s = db.stats();
    gstore::outln!("database  : {}", s.name);
    gstore::outln!("directory : {dir}");
    gstore::outln!("storage   : {}", db.storage());
    gstore::outln!("triples   : {}", s.triple_num);
    gstore::outln!("entities  : {}", s.entity_num);
    gstore::outln!("literals  : {}", s.literal_num);
    gstore::outln!("predicates: {}", s.predicate_num);
    gstore::outln!("named_graphs: {}", db.named_graphs().len());
    gstore::outln!(
        "index     : {}",
        if s.index_valid { "valid" } else { "stale" }
    );
    gstore::outln!("in_txn    : {}", s.in_transaction);
    ExitCode::SUCCESS
}
