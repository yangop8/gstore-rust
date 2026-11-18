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
//! A snapshot-layout database (and a new in-memory one) is persisted only by
//! a save: `save` writes it to its directory (`<name>.db` for a new one),
//! and the console saves it by itself when it leaves a database holding
//! updates no save has persisted yet — at `quit` / `exit`, at the end of the
//! input and on `load <other>` — so an update it acknowledged
//! (`[updated N triple(s)]`) is never dropped (review ext#5). When that save
//! fails, `load <other>` keeps the current database, and an interactive
//! `quit` stays in the console (a second `quit` leaves without saving); at
//! the end of the input the failure is reported (exit code 1).
//!
//! Type a SPARQL statement terminated by `;` to run it; it may span lines,
//! and a `;` inside a `{…}` group, a string literal, an `<IRI>` or a `#`
//! comment does not end it (the remote console's rule, see
//! [`super::syntax`]); a script's final statement may lack its `;`.
//! Consecutive update operations completed on one line (`op ; op;`) run as
//! one request. Commands (on a line of their own, with or without a final
//! `;`): `help`, `stats`, `save`, `load <name>`,
//! `import <file|dir|url>` (any RDF format the loaders read, chosen by the
//! extension; N-Quads / TriG keep their named graphs), `quit` / `exit`.
//! `load <iri> …` / `load silent …`, in any case, is SPARQL's `LOAD`, not
//! the command (a database name holds no `<`).
//!
//! Run non-interactively (stdin not a terminal), the exit code is 1 when a
//! statement or command failed.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use super::syntax::{self, Next};
use crate::parser::import::{self, ImportOptions};
use crate::{db_dir_for, Database, QueryResult, StorageKind};

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
        crate::outln!(
            "No database at '{dir}'. Started a new in-memory database '{target}' \
             (written to '{dir}' by 'save', or when you leave it with updates)."
        );
        Database::new(target.to_string())
    };
    // The mutation count the database's directory holds (see `unsaved`).
    let mut saved_at = db.mutation_count();
    // An interactive `quit` whose save failed: the next `quit` leaves anyway.
    let mut quit_discards = false;

    crate::outln!("gStore-rust local console. End a query with ';'. Type 'help' for commands.");
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut failures = 0usize;
    let mut buffer = String::new();
    print_prompt(&buffer);

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        let discard_armed = std::mem::take(&mut quit_discards);

        // Commands only when not mid-query; a final `;` is optional.
        if buffer.trim().is_empty() {
            let cmd = trimmed.trim_end_matches(';').trim_end();
            match cmd {
                "" if trimmed.is_empty() => {
                    quit_discards = discard_armed;
                    print_prompt(&buffer);
                    continue;
                }
                "quit" | "exit" => {
                    if discard_armed {
                        crate::outln!("leaving without saving '{}'.", db.name());
                        return finish(interactive, failures);
                    }
                    if save_if_unsaved(&db, &mut saved_at) {
                        return finish(interactive, failures);
                    }
                    failures += 1;
                    if !interactive {
                        return finish(interactive, failures);
                    }
                    crate::outln!(
                        "The updates to '{}' are not saved: 'save' to retry, or 'quit' again to \
                         leave without them.",
                        db.name()
                    );
                    quit_discards = true;
                    print_prompt(&buffer);
                    continue;
                }
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
                    if !save(&db, &mut saved_at) {
                        failures += 1;
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
                    // Leaving this database: its updates first.
                    if !save_if_unsaved(&db, &mut saved_at) {
                        failures += 1;
                        eprintln!(
                            "load failed: '{}' stays loaded (its updates are not saved)",
                            db.name()
                        );
                        print_prompt(&buffer);
                        continue;
                    }
                    match Database::open(db_dir_for(other)) {
                        Ok(loaded) => {
                            db = loaded;
                            saved_at = db.mutation_count();
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
        failures += run_statements(&mut db, &mut buffer, false);
        print_prompt(&buffer);
    }
    // A script's final statement may lack its `;` (a person's unfinished
    // one is dropped), as in the remote console.
    if !interactive {
        failures += run_statements(&mut db, &mut buffer, true);
    }
    // The end of the input: the updates are kept, whatever the layout.
    if !save_if_unsaved(&db, &mut saved_at) {
        failures += 1;
    }
    finish(interactive, failures)
}

/// Say goodbye; the exit code (1 when a non-interactive run had failures).
fn finish(interactive: bool, failures: usize) -> i32 {
    crate::outln!("bye.");
    if !interactive && failures > 0 {
        1
    } else {
        0
    }
}

/// Whether `db` holds updates that only a save persists and no save has yet
/// (`saved_at`: its mutation count when it was opened or last saved) — a
/// snapshot-layout or new in-memory database; the on-disk and RocksDB
/// layouts write each update through.
fn unsaved(db: &Database, saved_at: u64) -> bool {
    db.storage() == StorageKind::Snapshot && db.mutation_count() != saved_at
}

/// Save `db` where it lives — its home directory, or `<name>.db` for a new
/// in-memory one — and say so; `false` (and the reason printed) when the
/// save failed.
fn save(db: &Database, saved_at: &mut u64) -> bool {
    let dir = db
        .home()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(db_dir_for(db.name())));
    let count = db.mutation_count();
    match db.save(&dir) {
        Ok(()) => {
            *saved_at = count;
            crate::outln!("saved to {}", dir.display());
            true
        }
        Err(e) => {
            eprintln!("save failed: {e}");
            false
        }
    }
}

/// [`save`] `db` when it has [`unsaved`] updates (leaving it must not drop
/// them, review ext#5); `true` when nothing is left unsaved.
fn save_if_unsaved(db: &Database, saved_at: &mut u64) -> bool {
    !unsaved(db, *saved_at) || save(db, saved_at)
}

/// Run the statements `buffer` completes and remove them from it, leaving
/// the start of an unfinished one; with `at_eof` a final statement without
/// `;` is complete too. A statement ends as in the remote console
/// ([`syntax::next_sparql_statement`]: at a `;` outside `{…}` groups,
/// string literals, `<IRI>`s and `#` comments, review ext#9), and the
/// consecutive update operations completed together (`op ; op;` on a line)
/// run as one request, sharing their prologue as SPARQL 1.1 Update reads
/// them ([`syntax::group_updates`]). Returns how many failed.
fn run_statements(db: &mut Database, buffer: &mut String, at_eof: bool) -> usize {
    let mut statements = Vec::new();
    loop {
        match syntax::next_sparql_statement(buffer, at_eof) {
            Next::Statement { text, consumed } => {
                statements.push(text);
                buffer.drain(..consumed);
            }
            Next::Blank { consumed } => {
                buffer.drain(..consumed);
                if buffer.is_empty() {
                    break;
                }
            }
            Next::Incomplete => break,
        }
    }
    syntax::group_updates(statements)
        .iter()
        .filter(|query| !run_query(db, query))
        .count()
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
                // The escaping serializer (review sparqlconf#11).
                crate::outln!("{}", t.to_ntriples());
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
         save              save the current database to its directory (<name>.db)\n  \
         load <name>       load a database directory <name>.db (saving the current\n  \
         \x20                 one first when it has unsaved updates)\n  \
         \x20                 (LOAD <iri> … is SPARQL's LOAD)\n  \
         import <path>     import an RDF file (format by extension), a directory or a URL\n  \
         quit | exit       leave the console (saving unsaved updates first)\n\
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
