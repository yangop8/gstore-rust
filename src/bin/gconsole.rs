//! `gconsole` — an interactive SPARQL REPL over a database.
//!
//! Mirrors gStore's `gconsole`/`gserver` console loop:
//!
//! ```text
//! gconsole <db_name>     # load existing <db_name>.db, or start a fresh one
//! ```
//!
//! Type a SPARQL query terminated by `;` (or a blank line) to run it. Dot
//! commands: `help`, `stats`, `save`, `load <name>`, `import <file.nt>`,
//! `quit`/`exit`.

use std::io::{self, BufRead, Write};

use gstore::{db_dir_for, Database, QueryResult};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "scratch".to_string());

    let dir = db_dir_for(&name);
    let mut db = match Database::load(&dir) {
        Ok(db) => {
            println!(
                "Loaded database '{}' ({} triples).",
                db.name(),
                db.triple_num()
            );
            db
        }
        Err(_) => {
            println!("No database at '{dir}'. Started a new in-memory database '{name}'.");
            Database::new(name.clone())
        }
    };

    println!("gStore-rust console. End a query with ';'. Type 'help' for commands.");
    let stdin = io::stdin();
    let mut buffer = String::new();
    print_prompt(&buffer);

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();

        // Dot commands only when not mid-query.
        if buffer.trim().is_empty() {
            match trimmed {
                "" => {
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
                    let dir = db_dir_for(db.name());
                    match db.save(&dir) {
                        Ok(()) => println!("saved to {dir}"),
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
                    match Database::load(db_dir_for(other)) {
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
        Ok(QueryResult::Update { changed }) => println!("[updated {changed} triple(s)]"),
        Err(e) => eprintln!("error: {e}"),
    }
}

fn import_file(db: &mut Database, path: &str) {
    use gstore::Triple;
    let mut count = 0usize;
    let res = gstore_import(path, |t| {
        if db.insert_triple(&t) {
            count += 1;
        }
    });
    match res {
        Ok(()) => println!("imported {count} new triple(s) from {path}"),
        Err(e) => eprintln!("import failed: {e}"),
    }

    // Local import helper kept here to avoid widening the library's public API.
    fn gstore_import<F: FnMut(Triple)>(path: &str, mut f: F) -> gstore::Result<()> {
        gstore::parser::ntriples::for_each_triple_file(path, |t| {
            f(t);
            Ok(())
        })
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
