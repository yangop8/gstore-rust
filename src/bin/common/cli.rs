//! Argument handling shared by the command-line tools (`gbuild`, `gquery`,
//! `gadd`, `gsub`, `gdrop`, `gexport`, `gbackup`, `grestore`, `gshow`,
//! `gmonitor`), included into each with `#[path = "common/cli.rs"] mod cli;`.
//!
//! Besides each tool's own (positional) arguments, the tools accept the
//! option spellings of upstream gStore 1.3's tools (`src/Main/discard/*.cpp`:
//! `-db/--database NAME`, `-f/--file PATH`, `-p/--path PATH`, …, read by
//! `Util::getArgValue`, which takes the word after the option). Every tool
//! answers `-h` / `--help` on standard output with status 0.
//!
//! Exit statuses are the same for every tool: [`EXIT_OK`] (`0`) on success
//! and for `--help`, [`EXIT_FAILURE`] (`1`) when the operation failed, and
//! [`EXIT_USAGE`] (`2`) for invalid arguments — reported on standard error as
//! `usage: …` plus upstream's hint `Input "<tool> -h" for help.`.

#![allow(dead_code)] // each tool uses part of this module

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Success (and `--help`).
pub const EXIT_OK: u8 = 0;
/// The operation failed.
pub const EXIT_FAILURE: u8 = 1;
/// Invalid arguments.
pub const EXIT_USAGE: u8 = 2;

/// One of upstream's valued options: `-<short> VALUE` or `--<long> VALUE`
/// (an empty `short` has no single-dash form).
#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub short: &'static str,
    pub long: &'static str,
}

/// `-db` / `--database`: the database name.
pub const DB: Opt = Opt {
    short: "db",
    long: "database",
};

/// A tool's name, usage line and help text.
#[derive(Debug, Clone, Copy)]
pub struct Tool {
    pub name: &'static str,
    /// The synopsis (several lines allowed), without `usage: `.
    pub usage: &'static str,
    /// The rest of `--help`: options and notes.
    pub help: &'static str,
}

impl Tool {
    /// Print the help to standard output.
    pub fn print_help(&self) {
        println!("\ngStore {} — {}\n", self.name, first_line(self.help));
        let mut lines = self.usage.lines();
        if let Some(first) = lines.next() {
            println!("Usage:\t{first}");
        }
        for l in lines {
            println!("\t{l}");
        }
        let rest: Vec<&str> = self.help.lines().skip(1).collect();
        if !rest.is_empty() {
            println!("{}", rest.join("\n"));
        }
        println!();
    }

    /// Report invalid arguments (`msg`, then the usage) on standard error
    /// and return [`EXIT_USAGE`].
    pub fn usage_error(&self, msg: &str) -> ExitCode {
        if !msg.is_empty() {
            eprintln!("{}: {msg}", self.name);
        }
        let mut lines = self.usage.lines();
        if let Some(first) = lines.next() {
            eprintln!("usage: {first}");
        }
        for l in lines {
            eprintln!("       {l}");
        }
        eprintln!("Input \"{} -h\" for help.", self.name);
        ExitCode::from(EXIT_USAGE)
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// The command line split into upstream's valued options and everything
/// else.
#[derive(Debug, Default)]
pub struct Args {
    /// The value of each requested option, in the order the options were
    /// requested (`None`: not given).
    pub values: Vec<Option<String>>,
    /// Every other argument, in order (the tool's own flags and positional
    /// arguments).
    pub rest: Vec<String>,
}

impl Args {
    /// The value of option `i` (in the order given to [`parse`]).
    pub fn value(&self, i: usize) -> Option<&str> {
        self.values.get(i).and_then(|v| v.as_deref())
    }

    /// Whether any upstream option was given.
    pub fn any_legacy(&self) -> bool {
        self.values.iter().any(Option::is_some)
    }

    /// Remove every occurrence of the flag(s) `names` from
    /// [`rest`](Self::rest); whether one was there.
    pub fn take_flag(&mut self, names: &[&str]) -> bool {
        let before = self.rest.len();
        self.rest.retain(|a| !names.contains(&a.as_str()));
        self.rest.len() != before
    }

    /// The first remaining argument that looks like an option (`-x`), if any
    /// — an unknown option once the tool has taken its own flags.
    pub fn unknown_option(&self) -> Option<&str> {
        self.rest
            .iter()
            .map(String::as_str)
            .find(|a| a.len() > 1 && a.starts_with('-'))
    }
}

/// What [`parse`] found.
#[derive(Debug)]
pub enum Parsed {
    /// `-h` / `--help` was given.
    Help,
    Args(Args),
}

/// Split `argv` (without the program name) into the values of `opts` and
/// the rest. `-h`/`--help` anywhere asks for help. An option without a
/// value, or given twice, is an error (the message to report).
pub fn parse(argv: &[String], opts: &[Opt]) -> Result<Parsed, String> {
    let mut out = Args {
        values: vec![None; opts.len()],
        rest: Vec::new(),
    };
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "-h" || a == "--help" {
            return Ok(Parsed::Help);
        }
        let hit = opts.iter().position(|o| {
            a.strip_prefix("--") == Some(o.long)
                || (!o.short.is_empty()
                    && !a.starts_with("--")
                    && a.strip_prefix('-') == Some(o.short))
        });
        match hit {
            Some(k) => {
                let Some(v) = argv.get(i + 1) else {
                    return Err(format!("option {a} needs a value"));
                };
                if out.values[k].is_some() {
                    return Err(format!("option {a} given more than once"));
                }
                out.values[k] = Some(v.clone());
                i += 2;
            }
            None => {
                out.rest.push(argv[i].clone());
                i += 1;
            }
        }
    }
    Ok(Parsed::Args(out))
}

/// The command line of this process, without the program name.
pub fn argv() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// Milliseconds since the epoch — upstream's `TimeUtil::timestamp_str()`,
/// which names exports and backups.
pub fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        .to_string()
}

/// The last path component of a database argument without the `.db` suffix
/// (`/data/lubm.db` → `lubm`).
pub fn db_base_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    base.strip_suffix(gstore::DB_SUFFIX)
        .unwrap_or(&base)
        .to_string()
}

/// Whether upstream's `-f`/`-p` argument `path` names a directory: an
/// existing directory, a path ending with `/`, or one without an extension.
/// A path with an extension that is not a directory (`out.nt`) names a file.
pub fn names_dir(path: &str) -> bool {
    let p = Path::new(path);
    p.is_dir() || path.ends_with('/') || p.extension().is_none()
}

/// Upstream's `-f`/`-p` directory argument: when `path` names a directory
/// ([`names_dir`]), `file_name` goes inside it (`dir/file_name`); otherwise
/// `path` is the file itself.
pub fn file_in_dir_or_file(path: &str, file_name: &str) -> PathBuf {
    if names_dir(path) {
        Path::new(path).join(file_name)
    } else {
        PathBuf::from(path)
    }
}
