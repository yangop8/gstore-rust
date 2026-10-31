//! `gexport` — dump a database as N-Triples or N-Quads.
//!
//! Mirrors gStore's `gexport` (export a database to a `.nt` file):
//!
//! ```text
//! gexport [--format nt|nq] [-z|--zip [0|1]] <db_name> <out>
//! ```
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

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gstore::zipio::{self, ExportFormat};
use gstore::{db_dir_for, Database};

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

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let (format, zip) = match take_export_flags(&mut args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gexport: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.len() != 3 {
        eprintln!(
            "usage: {} [--format nt|nq] [-z|--zip] <db_name> <out.nt|out.nq|out.zip>",
            args[0]
        );
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);
    let out = &args[2];
    // `x.zip` / `x.nq.zip` name an archive; its export file is `x.<ext>` /
    // `x.nq`.
    let out_path = Path::new(out);
    let zip = zip || zipio::is_zip_name(out_path);
    let base = if zipio::is_zip_name(out_path) {
        out_path.with_extension("")
    } else {
        out_path.to_path_buf()
    };
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
    let format = format
        .or_else(|| ExportFormat::from_path(&base))
        .unwrap_or_else(|| ExportFormat::for_database(&db));

    // A zip's export file is written beside it under a temporary name and
    // packed as `<base name>[.<ext>]`.
    let (file_path, archive) = if zip {
        let mut entry = base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| args[1].clone());
        if ExportFormat::from_path(&base).is_none() {
            entry = format!("{entry}.{}", format.extension());
        }
        let archive = if zipio::is_zip_name(out_path) {
            out_path.to_path_buf()
        } else {
            out_path.with_extension("zip")
        };
        let mut tmp = archive.clone().into_os_string();
        tmp.push(".export-tmp");
        (PathBuf::from(tmp), Some((archive, entry)))
    } else {
        (out_path.to_path_buf(), None)
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
