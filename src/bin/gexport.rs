//! `gexport` — dump a database as N-Triples or N-Quads.
//!
//! Mirrors gStore's `gexport` (export a database to a `.nt` file):
//!
//! ```text
//! gexport [--format nt|nq] [-z|--zip [0|1]] <db_name> <out>
//! gexport -db <db_name> [-f <dir | out.nt>] [-z 0|1] [--format nt|nq]   # upstream 1.3
//! ```
//!
//! Upstream's form writes `<db_name>_<timestamp>.nt` (milliseconds since the
//! epoch, as upstream) into the `-f` directory (created when missing), or
//! into the current directory without `-f`; a `-f` path with an extension
//! (`out.nt`) that is not a directory is taken as the output file itself.
//! The timestamped name takes the extension of the format (`.nq` for
//! N-Quads), or `.zip` when zipping (upstream's `<db_name>_<timestamp>.zip`).
//!
//! Every statement is written in N-Triples / N-Quads surface syntax (`<s> <p>
//! <o> .`, `<s> <p> <o> <g> .`). The dictionary keys are already stored in
//! that form, so decoding is exact.
//!
//! The format: `--format`, else the extension of `<out>` (`.nq` / `.nquads`
//! → N-Quads, `.nt` → N-Triples), else N-Quads when the database has named
//! graphs and N-Triples (upstream's format: the default graph) otherwise.
//! N-Quads writes the default graph, then every named graph.
//!
//! `-z` / `--zip` (upstream's `gexport -z 1`), or an `<out>` ending in `.zip`,
//! writes a zip archive instead (feature `zip`): `<out>` with its extension
//! replaced by `.zip`, holding the export file by its name (`out.nt`).

#[path = "common/cli.rs"]
mod cli;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gstore::zipio::{self, ExportFormat};
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gexport",
    usage: "gexport [--format nt|nq] [-z|--zip [0|1]] <db_name> <out.nt|out.nq|out.zip>\n\
            gexport -db <db_name> [-f <dir | out.nt>] [-z 0|1] [--format nt|nq]",
    help: "export a database as N-Triples or N-Quads\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name.\n\
           \t-f,--file PATH\t\tThe directory receiving <db_name>_<timestamp>.nt (default:\n\
           \t\t\t\tthe current directory), or the output file when PATH has an\n\
           \t\t\t\textension and is not a directory.\n\
           \t--format nt|nq\t\tN-Triples (the default graph) or N-Quads (every graph);\n\
           \t\t\t\tdefault: by the output extension, else N-Quads when the\n\
           \t\t\t\tdatabase has named graphs.\n\
           \t-z,--zip [0|1]\t\t1: write a zip archive holding the export file\n\
           \t\t\t\t(needs the `zip` cargo feature).",
};

const FILE: cli::Opt = cli::Opt {
    short: "f",
    long: "file",
};

/// Where the export goes.
enum Out {
    /// A path the user named (positional `<out>`, or `-f out.nt`).
    File(PathBuf),
    /// Upstream's timestamped `<dir>/<db_name>_<timestamp>`, without its
    /// extension: `.nt` / `.nq` by the format, `.zip` when zipping.
    Stamped(PathBuf),
}

/// The parsed command line.
struct Cmd {
    db_name: String,
    out: Out,
    format: Option<ExportFormat>,
    zip: bool,
}

/// Take `--format <f>` / `--format=<f>` and `-z` / `--zip [0|1]` out of
/// `args`: the format asked for and whether to zip.
fn take_export_flags(args: &mut Vec<String>) -> Result<(Option<ExportFormat>, bool), String> {
    let mut format = None;
    let mut zip = false;
    let mut rest = Vec::with_capacity(args.len());
    let mut it = std::mem::take(args).into_iter().peekable();
    while let Some(a) = it.next() {
        let value = if a == "--format" {
            Some(it.next().ok_or("--format needs a value (nt or nq)")?)
        } else {
            a.strip_prefix("--format=").map(str::to_string)
        };
        if let Some(v) = value {
            format = Some(
                ExportFormat::parse(&v)
                    .ok_or_else(|| format!("unknown export format '{v}' (use nt or nq)"))?,
            );
        } else if a == "-z" || a == "--zip" {
            zip = match it.peek().map(String::as_str) {
                Some("0") => {
                    it.next();
                    false
                }
                Some("1") => {
                    it.next();
                    true
                }
                _ => true,
            };
        } else {
            rest.push(a);
        }
    }
    *args = rest;
    Ok((format, zip))
}

/// Parse the command line: the positional form, or upstream's `-db NAME [-f
/// PATH]` (the output path chosen as upstream does). `Err` carries the exit
/// status (`--help`, invalid arguments).
fn command_line() -> Result<Cmd, ExitCode> {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB, FILE]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return Err(ExitCode::SUCCESS);
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return Err(TOOL.usage_error(&e)),
    };
    let (format, zip) = take_export_flags(&mut args.rest).map_err(|e| TOOL.usage_error(&e))?;
    if let Some(o) = args.unknown_option() {
        return Err(TOOL.usage_error(&format!("unknown option '{o}'")));
    }
    if !args.any_legacy() {
        let [db_name, out]: [String; 2] = std::mem::take(&mut args.rest)
            .try_into()
            .map_err(|_| TOOL.usage_error(""))?;
        return Ok(Cmd {
            db_name,
            out: Out::File(PathBuf::from(out)),
            format,
            zip,
        });
    }
    let db_name = match args.value(0) {
        Some(db) => db.to_string(),
        None => match args.rest.iter().position(|a| !a.starts_with('-')) {
            Some(i) => args.rest.remove(i),
            None => return Err(TOOL.usage_error("missing the database name")),
        },
    };
    if !args.rest.is_empty() {
        return Err(TOOL.usage_error("too many arguments"));
    }
    let stem = format!("{}_{}", cli::db_base_name(&db_name), cli::timestamp());
    let out = match args.value(1) {
        Some(p) if cli::names_dir(p) => Out::Stamped(Path::new(p).join(&stem)),
        Some(p) => Out::File(PathBuf::from(p)),
        None => Out::Stamped(PathBuf::from(stem)),
    };
    let (Out::File(target) | Out::Stamped(target)) = &out;
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("cannot create '{}': {e}", parent.display());
            return Err(ExitCode::FAILURE);
        }
    }
    Ok(Cmd {
        db_name,
        out,
        format,
        zip,
    })
}

/// `path` with `.ext` appended (`x_123` → `x_123.nt`).
fn with_added_extension(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

fn main() -> ExitCode {
    let cmd = match command_line() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let dir = db_dir_for(&cmd.db_name);
    let zip = cmd.zip || matches!(&cmd.out, Out::File(p) if zipio::is_zip_name(p));
    if zip && !zipio::AVAILABLE {
        eprintln!("gexport: {}", zipio::FEATURE_MISSING);
        return ExitCode::FAILURE;
    }

    let db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    // The export file's path (`x.zip` / `x.nq.zip` name an archive; its
    // export file is `x.<ext>` / `x.nq`), the archive's path, and the format.
    let (base, archive_path, format) = match &cmd.out {
        Out::File(out_path) => {
            let named_zip = zipio::is_zip_name(out_path);
            let base = if named_zip {
                out_path.with_extension("")
            } else {
                out_path.clone()
            };
            let format = cmd
                .format
                .or_else(|| ExportFormat::from_path(&base))
                .unwrap_or_else(|| ExportFormat::for_database(&db));
            let archive = if named_zip {
                out_path.clone()
            } else {
                out_path.with_extension("zip")
            };
            (base, archive, format)
        }
        Out::Stamped(stem) => {
            let format = cmd
                .format
                .unwrap_or_else(|| ExportFormat::for_database(&db));
            (
                with_added_extension(stem, format.extension()),
                with_added_extension(stem, "zip"),
                format,
            )
        }
    };

    // A zip's export file is written beside it under a temporary name and
    // packed as `<base name>[.<ext>]`.
    let (file_path, archive) = if zip {
        let mut entry = base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cmd.db_name.clone());
        if ExportFormat::from_path(&base).is_none() {
            entry = format!("{entry}.{}", format.extension());
        }
        let mut tmp = archive_path.clone().into_os_string();
        tmp.push(".export-tmp");
        (PathBuf::from(tmp), Some((archive_path, entry)))
    } else {
        (base, None)
    };

    let file = match File::create(&file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot create '{}': {e}", file_path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut w = BufWriter::new(file);
    let count = match zipio::write_export(&db, format, &mut w).and_then(|n| {
        w.flush()?;
        Ok(n)
    }) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("write error on '{}': {e}", file_path.display());
            return ExitCode::FAILURE;
        }
    };
    drop(w);
    let written = match archive {
        Some((archive, entry)) => {
            let packed = zipio::pack_file(&file_path, &archive, &entry);
            let _ = std::fs::remove_file(&file_path);
            if let Err(e) = packed {
                eprintln!("gexport: export compress fail: {e}");
                return ExitCode::FAILURE;
            }
            archive
        }
        None => file_path,
    };
    let what = match format {
        ExportFormat::NTriples => "triple(s)",
        ExportFormat::NQuads => "statement(s)",
    };
    println!(
        "Exported {count} {what} from '{dir}' to '{}'",
        written.display()
    );
    ExitCode::SUCCESS
}
