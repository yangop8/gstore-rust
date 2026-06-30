//! `gquery` — run a SPARQL query against a built database.
//!
//! Mirrors gStore's `gquery`:
//!
//! ```text
//! gquery <db_name> <query.rq>        # query from a file
//! gquery <db_name> -e "SELECT ..."   # inline query string
//! ```
//!
//! SELECT prints an aligned table; ASK prints true/false; INSERT/DELETE DATA
//! applies the update and re-saves the database.

use std::process::ExitCode;
use std::time::Instant;

use gstore::{db_dir_for, Database, QueryResult};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <db_name> (<query.rq> | -e \"<query>\")", args[0]);
        return ExitCode::FAILURE;
    }
    let name = &args[1];

    let query = if args[2] == "-e" {
        if args.len() < 4 {
            eprintln!("error: -e requires a query string");
            return ExitCode::FAILURE;
        }
        args[3..].join(" ")
    } else {
        match std::fs::read_to_string(&args[2]) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot read query file '{}': {e}", args[2]);
                return ExitCode::FAILURE;
            }
        }
    };

    let dir = db_dir_for(name);
    let mut db = match Database::load(&dir) {
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

    match result {
        QueryResult::Select(rs) => {
            print!("{}", rs.to_table_string());
            eprintln!(
                "[{} row(s) in {:.3}s]",
                rs.row_count(),
                elapsed.as_secs_f64()
            );
        }
        QueryResult::Ask(b) => println!("{b}"),
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
