//! `gconsole --local <db>` — the embedded SPARQL REPL gstore-rust's
//! `gconsole` was before it became a client of gserver (a gstore-rust
//! extension; upstream's gconsole is client-only). It opens the database
//! directly, so no server may have it loaded.
//!
//! `<db>` is a database directory, or a name: `<name>.db` is opened when it
//! exists, otherwise a fresh in-memory database named `<name>` starts. Any
//! layout is opened ([`Database::open`]); on an on-disk or RocksDB database
//! each update is written through to its store as it is applied.
//!
//! Type a SPARQL statement terminated by `;` to run it. Commands (with or
//! without a final `;`): `help`, `stats`, `save`, `load <name>`,
//! `import <file.nt>`, `quit` / `exit`.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::{db_dir_for, Database, QueryResult, Triple};

/// Run the embedded REPL on `target` (see the module docs); the process
/// exit code.
pub fn run(target: &str) -> i32 {
    let dir = if Path::new(target).is_dir() && Database::storage_kind(target).is_some() {
        target.to_string()
    } else {
        db_dir_for(target)
    };
    let mut db = if Path::new(&dir).is_dir() {
        match Database::open(&dir) {
            Ok(db) => {
                println!(
                    "Loaded database '{}' ({} triples, {}).",
                    db.name(),
                    db.triple_num(),
                    db.storage()
                );
                db
            }
            Err(e) => {
                eprintln!("cannot open database '{dir}': {e}");
                return 1;
            }
        }
    } else {
        println!("No database at '{dir}'. Started a new in-memory database '{target}'.");
        Database::new(target.to_string())
    };

    println!("gStore-rust local console. End a query with ';'. Type 'help' for commands.");
    let stdin = io::stdin();
    let mut buffer = String::new();
    print_prompt(&buffer);

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();

        // Commands only when not mid-query; a final `;` is optional.
        if buffer.trim().is_empty() {
            let cmd = trimmed.trim_end_matches(';').trim_end();
            match cmd {
                "" if trimmed.is_empty() => {
                    print_prompt(&buffer);
                    continue;
                }
                "quit" | "exit" => break,
                "help" => {
                    print_help();
                    print_prompt(&buffer);
                    continue;
                }
                "stats" => {
                    println!(
                        "name={} triples={} entities={} literals={} predicates={}",
                        db.name(),
                        db.triple_num(),
                        db.entity_num(),
                        db.literal_num(),
                        db.predicate_num()
                    );
                    print_prompt(&buffer);
                    continue;
                }
                "save" => {
                    // Persist where the database lives (its home directory),
                    // falling back to `<name>.db` for a fresh in-memory one.
                    let dir = db
                        .home()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(db_dir_for(db.name())));
                    match db.save(&dir) {
                        Ok(()) => println!("saved to {}", dir.display()),
                        Err(e) => eprintln!("save failed: {e}"),
                    }
                    print_prompt(&buffer);
                    continue;
                }
                cmd if cmd.starts_with("import ") => {
                    let path = cmd["import ".len()..].trim();
                    import_file(&mut db, path);
                    print_prompt(&buffer);
                    continue;
                }
                cmd if cmd.starts_with("load ") => {
                    let other = cmd["load ".len()..].trim();
                    match Database::open(db_dir_for(other)) {
                        Ok(loaded) => {
                            db = loaded;
                            println!("loaded '{}' ({} triples)", db.name(), db.triple_num());
                        }
                        Err(e) => eprintln!("load failed: {e}"),
                    }
                    print_prompt(&buffer);
                    continue;
                }
                _ => {}
            }
        }

        buffer.push_str(&line);
        buffer.push('\n');

        // A query is complete when a line ends with ';'.
        if trimmed.ends_with(';') {
            let query = buffer.trim().trim_end_matches(';').to_string();
            buffer.clear();
            run_query(&mut db, &query);
        }
        print_prompt(&buffer);
    }
    println!("bye.");
    0
}

fn run_query(db: &mut Database, query: &str) {
    if query.trim().is_empty() {
        return;
    }
    match db.query(query) {
        Ok(QueryResult::Select(rs)) => {
            print!("{}", rs.to_table_string());
            println!("[{} row(s)]", rs.row_count());
        }
        Ok(QueryResult::Ask(b)) => println!("{b}"),
        Ok(QueryResult::Construct(triples)) => {
            for t in &triples {
                println!("{t}");
            }
            println!("[{} triple(s)]", triples.len());
        }
        Ok(QueryResult::Update { changed }) => println!("[updated {changed} triple(s)]"),
        Err(e) => eprintln!("error: {e}"),
    }
}

fn import_file(db: &mut Database, path: &str) {
    let mut count = 0usize;
    let res = crate::parser::ntriples::for_each_triple_file(path, |t: Triple| {
        if db.try_insert_triple(&t)? {
            count += 1;
        }
        Ok(())
    });
    match res {
        Ok(()) => println!("imported {count} new triple(s) from {path}"),
        Err(e) => eprintln!("import failed after {count} new triple(s): {e}"),
    }
}

fn print_help() {
    println!(
        "commands:\n  \
         help              show this help\n  \
         stats             show database statistics\n  \
         save              save the current database to <name>.db\n  \
         load <name>       load a database directory <name>.db\n  \
         import <file.nt>  import triples from an N-Triples file\n  \
         quit | exit       leave the console\n\
         otherwise: type a SPARQL query ending in ';'"
    );
}

fn print_prompt(buffer: &str) {
    let p = if buffer.trim().is_empty() {
        "gsql> "
    } else {
        "  ..> "
    };
    print!("{p}");
    let _ = io::stdout().flush();
}
