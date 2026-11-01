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
//! `import <file|dir|url>` (any RDF format the loaders read, chosen by the
//! extension; N-Quads / TriG keep their named graphs), `quit` / `exit`.
//! `load <iri> …` / `load silent …`, in any case, is SPARQL's `LOAD`, not
//! the command (a database name holds no `<`).
//!
//! Run non-interactively (stdin not a terminal), the exit code is 1 when a
//! statement or command failed.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::parser::import::{self, ImportOptions};
use crate::{db_dir_for, Database, QueryResult};

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
                crate::outln!(
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
        crate::outln!("No database at '{dir}'. Started a new in-memory database '{target}'.");
        Database::new(target.to_string())
    };

    crate::outln!("gStore-rust local console. End a query with ';'. Type 'help' for commands.");
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut failures = 0usize;
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
                    crate::outln!(
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
                        Ok(()) => crate::outln!("saved to {}", dir.display()),
                        Err(e) => {
                            failures += 1;
                            eprintln!("save failed: {e}");
                        }
                    }
                    print_prompt(&buffer);
                    continue;
                }
                cmd if cmd.starts_with("import ") => {
                    let path = cmd["import ".len()..].trim();
                    if !import_file(&mut db, path) {
                        failures += 1;
                    }
                    print_prompt(&buffer);
                    continue;
                }
                cmd if cmd.starts_with("load ") && !is_sparql_load(&cmd["load ".len()..]) => {
                    let other = cmd["load ".len()..].trim();
                    match Database::open(db_dir_for(other)) {
                        Ok(loaded) => {
                            db = loaded;
                            crate::outln!("loaded '{}' ({} triples)", db.name(), db.triple_num());
                        }
                        Err(e) => {
                            failures += 1;
                            eprintln!("load failed: {e}");
                        }
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
            if !run_query(&mut db, &query) {
                failures += 1;
            }
        }
        print_prompt(&buffer);
    }
    crate::outln!("bye.");
    if !interactive && failures > 0 {
        1
    } else {
        0
    }
}

/// Whether the text after `load ` is SPARQL's `LOAD [SILENT] <iri> …`
/// rather than the name of a database to open.
fn is_sparql_load(arg: &str) -> bool {
    let arg = arg.trim_start();
    let after_silent = arg
        .get(..6)
        .filter(|w| w.eq_ignore_ascii_case("silent"))
        .and_then(|_| arg.get(6..))
        .filter(|rest| rest.starts_with(char::is_whitespace));
    arg.starts_with('<') || after_silent.is_some()
}

/// Run one statement, printing its answer; `false` when it failed.
fn run_query(db: &mut Database, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    match db.query(query) {
        Ok(QueryResult::Select(rs)) => {
            crate::out!("{}", rs.to_table_string());
            crate::outln!("[{} row(s)]", rs.row_count());
        }
        Ok(QueryResult::Ask(b)) => crate::outln!("{b}"),
        Ok(QueryResult::Construct(triples)) => {
            for t in &triples {
                crate::outln!("{t}");
            }
            crate::outln!("[{} triple(s)]", triples.len());
        }
        Ok(QueryResult::Update { changed }) => crate::outln!("[updated {changed} triple(s)]"),
        Err(e) => {
            eprintln!("error: {e}");
            return false;
        }
    }
    true
}

/// `import <path>`: stream an RDF file (or every RDF file below a directory,
/// or a URL) into the database through the bulk-load pipeline — strictly, so
/// the first bad statement stops it. `false` when it failed.
fn import_file(db: &mut Database, path: &str) -> bool {
    let mut count = 0u64;
    let res = import::sources_from_paths(&[path]).and_then(|sources| {
        db.insert_sources(&sources, &ImportOptions::strict(), &mut (), &mut count)
    });
    match res {
        Ok(_) => crate::outln!("imported {count} new triple(s) from {path}"),
        Err(e) => {
            eprintln!("import failed after {count} new triple(s): {e}");
            return false;
        }
    }
    true
}

fn print_help() {
    crate::outln!(
        "commands:\n  \
         help              show this help\n  \
         stats             show database statistics\n  \
         save              save the current database to <name>.db\n  \
         load <name>       load a database directory <name>.db\n  \
         \x20                 (LOAD <iri> … is SPARQL's LOAD)\n  \
         import <path>     import an RDF file (format by extension), a directory or a URL\n  \
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
    crate::out!("{p}");
    let _ = io::stdout().flush();
}
