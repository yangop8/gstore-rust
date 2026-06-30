//! `gshow` — print a database's counts and status.
//!
//! Mirrors gStore's `gshow`/database-info reporting:
//!
//! ```text
//! gshow <db_name>
//! ```
//!
//! Prints triple/entity/literal/predicate counts plus index and transaction
//! state, using [`Database::stats`].

use std::process::ExitCode;

use gstore::{db_dir_for, Database};

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
    println!("database  : {}", s.name);
    println!("directory : {dir}");
    println!("triples   : {}", s.triple_num);
    println!("entities  : {}", s.entity_num);
    println!("literals  : {}", s.literal_num);
    println!("predicates: {}", s.predicate_num);
    println!("named_graphs: {}", db.named_graphs().len());
    println!(
        "index     : {}",
        if s.index_valid { "valid" } else { "stale" }
    );
    println!("in_txn    : {}", s.in_transaction);
    ExitCode::SUCCESS
}
