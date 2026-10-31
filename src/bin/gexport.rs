//! `gexport` — dump all triples of a database as N-Triples.
//!
//! Mirrors gStore's `gexport` (export a database to a `.nt` file):
//!
//! ```text
//! gexport <db_name> <out.nt>
//! gexport -db <db_name> [-f <dir | out.nt>]   # upstream 1.3
//! ```
//!
//! Upstream's form writes `<db_name>_<timestamp>.nt` (milliseconds since the
//! epoch, as upstream) into the `-f` directory (created when missing), or
//! into the current directory without `-f`; a `-f` path with an extension
//! (`out.nt`) that is not a directory is taken as the output file itself.
//!
//! Every triple is written in N-Triples surface syntax (`<s> <p> <o> .`). The
//! dictionary keys are already stored in that form, so decoding is exact.

#[path = "common/cli.rs"]
mod cli;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

use gstore::{db_dir_for, Database, TripleSource};

const TOOL: cli::Tool = cli::Tool {
    name: "gexport",
    usage: "gexport <db_name> <out.nt>\n\
            gexport -db <db_name> [-f <dir | out.nt>]",
    help: "export a database as N-Triples\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name.\n\
           \t-f,--file PATH\t\tThe directory receiving <db_name>_<timestamp>.nt (default:\n\
           \t\t\t\tthe current directory), or the output file when PATH has an\n\
           \t\t\t\textension and is not a directory.",
};

const FILE: cli::Opt = cli::Opt {
    short: "f",
    long: "file",
};

/// The command line in the positional form (`<prog> <db_name> <out>`, then
/// any other argument): upstream's `-db NAME [-f PATH]` is translated, the
/// output path chosen as upstream does. `Err` carries the exit status
/// (`--help`, invalid arguments).
fn command_line() -> Result<Vec<String>, ExitCode> {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB, FILE]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return Err(ExitCode::SUCCESS);
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return Err(TOOL.usage_error(&e)),
    };
    if let Some(o) = args.unknown_option() {
        return Err(TOOL.usage_error(&format!("unknown option '{o}'")));
    }
    let mut out = vec![TOOL.name.to_string()];
    if !args.any_legacy() {
        out.append(&mut args.rest);
        return Ok(out);
    }
    let name = match args.value(0) {
        Some(db) => db.to_string(),
        None => match args.rest.iter().position(|a| !a.starts_with('-')) {
            Some(i) => args.rest.remove(i),
            None => return Err(TOOL.usage_error("missing the database name")),
        },
    };
    let file = format!("{}_{}.nt", cli::db_base_name(&name), cli::timestamp());
    let target = match args.value(1) {
        Some(p) => cli::file_in_dir_or_file(p, &file),
        None => std::path::PathBuf::from(file),
    };
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("cannot create '{}': {e}", parent.display());
            return Err(ExitCode::FAILURE);
        }
    }
    out.push(name);
    out.push(target.to_string_lossy().into_owned());
    out.append(&mut args.rest);
    Ok(out)
}

fn main() -> ExitCode {
    let args = match command_line() {
        Ok(a) => a,
        Err(code) => return code,
    };
    if args.len() != 3 {
        return TOOL.usage_error("");
    }
    let dir = db_dir_for(&args[1]);
    let out = &args[2];

    let db = match Database::open(&dir) {
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
