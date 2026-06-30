//! `gexport` — dump all triples of a database as N-Triples.
//!
//! Mirrors gStore's `gexport` (export a database to a `.nt` file):
//!
//! ```text
//! gexport <db_name> <out.nt>
//! ```
//!
//! Every triple is written in N-Triples surface syntax (`<s> <p> <o> .`). The
//! dictionary keys are already stored in that form, so decoding is exact.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

use gstore::{db_dir_for, Database, TripleSource};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <db_name> <out.nt>", args[0]);
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);
    let out = &args[2];

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

    let file = match File::create(out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot create '{out}': {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut w = BufWriter::new(file);

    let dict = db.dict();
    let mut count = 0u64;
    for t in db.backend().iter_all() {
        let (Some(s), Some(p), Some(o)) = (
            dict.id_to_string(t.sub),
            dict.predicate_to_string(t.pred),
            dict.id_to_string(t.obj),
        ) else {
            eprintln!("warning: skipping triple with an unresolved dictionary id");
            continue;
        };
        if let Err(e) = writeln!(w, "{s} {p} {o} .") {
            eprintln!("write error on '{out}': {e}");
            return ExitCode::FAILURE;
        }
        count += 1;
    }
    if let Err(e) = w.flush() {
        eprintln!("flush error on '{out}': {e}");
        return ExitCode::FAILURE;
    }
    println!("Exported {count} triple(s) from '{dir}' to '{out}'");
    ExitCode::SUCCESS
}
