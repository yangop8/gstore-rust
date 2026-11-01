//! `gquery` — run a SPARQL query against a built database.
//!
//! Mirrors gStore's `gquery`:
//!
//! ```text
//! gquery <db_name> <query.rq>        # query from a file
//! gquery <db_name> -e "SELECT ..."   # inline query string
//! gquery -db <db_name> -q <query.rq> [-f <result file>]   # upstream 1.3
//! ```
//!
//! SELECT prints an aligned table; ASK prints true/false; INSERT/DELETE DATA
//! applies the update and re-saves the database. Every database layout
//! (snapshot, on-disk, RocksDB) is opened via [`Database::open`]. Upstream's
//! `-f`/`--resultfile FILE` writes the result to `FILE` instead of standard
//! output (the counts still go to standard error).
//!
//! Exit status: 0 answered, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use gstore::{db_dir_for, Database, QueryResult};

const TOOL: cli::Tool = cli::Tool {
    name: "gquery",
    usage: "gquery <db_name> (<query.rq> | -e \"<query>\") [-f <result file>]\n\
            gquery -db <db_name> (-q <query.rq> | -e \"<query>\") [-f <result file>]",
    help: "run a SPARQL query or update against a database\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-q,--queryfile FILE\tThe file holding the SPARQL statement.\n\
           \t-e QUERY\t\tThe SPARQL statement itself (the rest of the line).\n\
           \t-f,--resultfile FILE\tWrite the result to FILE instead of standard output.",
};

const QUERY_FILE: cli::Opt = cli::Opt {
    short: "q",
    long: "queryfile",
};
const RESULT_FILE: cli::Opt = cli::Opt {
    short: "f",
    long: "resultfile",
};

fn main() -> ExitCode {
    let args = match cli::parse(&cli::argv(), &[cli::DB, QUERY_FILE, RESULT_FILE]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    // `-e` takes the rest of the line as the statement.
    let (mut rest, inline) = match args.rest.iter().position(|a| a == "-e") {
        Some(i) if i + 1 < args.rest.len() => {
            (args.rest[..i].to_vec(), Some(args.rest[i + 1..].join(" ")))
        }
        Some(_) => return TOOL.usage_error("-e requires a query string"),
        None => (args.rest.clone(), None),
    };
    if let Some(o) = rest.iter().find(|a| a.len() > 1 && a.starts_with('-')) {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    let name = match args.value(0) {
        Some(db) => db.to_string(),
        None if !rest.is_empty() => rest.remove(0),
        None => return TOOL.usage_error("missing the database name"),
    };
    let query_file = match (args.value(1), rest.as_slice()) {
        (Some(q), []) => Some(q.to_string()),
        (None, [q]) => Some(q.clone()),
        (None, []) => None,
        _ => return TOOL.usage_error("too many arguments"),
    };
    let query = match (inline, query_file) {
        (Some(_), Some(_)) => {
            return TOOL.usage_error("give the query either inline (-e) or in a file, not both")
        }
        (Some(q), None) => q,
        (None, Some(path)) => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot read query file '{path}': {e}");
                return ExitCode::FAILURE;
            }
        },
        // Upstream enters an interactive console here; use gconsole.
        (None, None) => {
            return TOOL
                .usage_error("no query given (use -q FILE or -e QUERY; gconsole is interactive)")
        }
    };
    let name = &name;
    // Where the result goes: standard output, or upstream's `-f FILE`.
    let mut out: Box<dyn Write> = match args.value(2) {
        Some(path) => match std::fs::File::create(path) {
            Ok(f) => Box::new(std::io::BufWriter::new(f)),
            Err(e) => {
                eprintln!("cannot create result file '{path}': {e}");
                return ExitCode::FAILURE;
            }
        },
        None => Box::new(std::io::stdout().lock()),
    };

    let dir = db_dir_for(name);
    // Auto-detect the layout: snapshot, on-disk KVstore, or RocksDB.
    let mut db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let started = Instant::now();
    let result = match db.query(&query) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let elapsed = started.elapsed();

    let written = match &result {
        QueryResult::Select(rs) => write!(out, "{}", rs.to_table_string()),
        QueryResult::Ask(b) => writeln!(out, "{b}"),
        QueryResult::Construct(triples) => triples.iter().try_for_each(|t| writeln!(out, "{t}")),
        QueryResult::Update { .. } => Ok(()),
    };
    // A reader that stopped early (`gquery … | head`) is not an error.
    if let Err(e) = gstore::stdout::quiet_broken_pipe(written.and_then(|()| out.flush())) {
        eprintln!("cannot write the result: {e}");
        return ExitCode::FAILURE;
    }
    drop(out);
    match result {
        QueryResult::Select(rs) => {
            eprintln!(
                "[{} row(s) in {:.3}s]",
                rs.row_count(),
                elapsed.as_secs_f64()
            );
        }
        QueryResult::Ask(_) => {}
        QueryResult::Construct(triples) => {
            eprintln!(
                "[{} triple(s) in {:.3}s]",
                triples.len(),
                elapsed.as_secs_f64()
            );
        }
        QueryResult::Update { changed } => {
            if let Err(e) = db.save(&dir) {
                eprintln!("update applied but saving failed: {e}");
                return ExitCode::FAILURE;
            }
            eprintln!(
                "[updated {changed} triple(s) in {:.3}s]",
                elapsed.as_secs_f64()
            );
        }
    }
    ExitCode::SUCCESS
}
