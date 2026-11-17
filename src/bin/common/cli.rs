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
        gstore::outln!("\ngStore {} — {}\n", self.name, first_line(self.help));
        let mut lines = self.usage.lines();
        if let Some(first) = lines.next() {
            gstore::outln!("Usage:\t{first}");
        }
        for l in lines {
            gstore::outln!("\t{l}");
        }
        let rest: Vec<&str> = self.help.lines().skip(1).collect();
        if !rest.is_empty() {
            gstore::outln!("{}", rest.join("\n"));
        }
        gstore::outln!();
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

/// `--unzip-max-size BYTES`: the most a zip package named on the command
/// line may unpack to (`0`: no limit).
pub const UNZIP_MAX_SIZE: Opt = Opt {
    short: "",
    long: "unzip-max-size",
};

/// `--unzip-max-entries N`: the most entries such a package may hold (`0`:
/// no limit).
pub const UNZIP_MAX_ENTRIES: Opt = Opt {
    short: "",
    long: "unzip-max-entries",
};

/// `base` with the values of the [`UNZIP_MAX_SIZE`] / [`UNZIP_MAX_ENTRIES`]
/// options (options `size` and `entries` of `args`) in place of its own; `0`
/// lifts a limit. An invalid number is the message to report.
pub fn zip_limits(
    args: &Args,
    size: usize,
    entries: usize,
    base: gstore::zipio::ZipLimits,
) -> Result<gstore::zipio::ZipLimits, String> {
    let mut limits = base;
    if let Some(v) = args.value(size) {
        let n: u64 = v
            .parse()
            .map_err(|_| format!("--unzip-max-size takes a number of bytes, not '{v}'"))?;
        limits.max_size = if n == 0 { u64::MAX } else { n };
    }
    if let Some(v) = args.value(entries) {
        let n: usize = v
            .parse()
            .map_err(|_| format!("--unzip-max-entries takes a number, not '{v}'"))?;
        limits.max_entries = if n == 0 { usize::MAX } else { n };
    }
    Ok(limits)
}

/// A zip error, with a hint at the options when it is a limit that refused
/// the package.
pub fn zip_error(e: impl std::fmt::Display) -> String {
    let e = e.to_string();
    if e.contains("unzip_max_") {
        format!("{e}; raise the limit with --unzip-max-size BYTES / --unzip-max-entries N (0 = no limit)")
    } else {
        e
    }
}

/// Unpack every local `.zip` file among `paths` (upstream 1.3's `-f x.zip`
/// of `gbuild` / `gadd` / `gsub`) under `limits` into a scratch directory
/// beside the database directory `dir`, replacing the path by that
/// directory, which is then read like any directory source; the scratch
/// directories, removed when dropped. Each is a new directory with an
/// unpredictable name (`<dir>.unzip-tmp-<pid>-<random>`), created
/// exclusively ([`ScratchDir::create_unique`](gstore::zipio::ScratchDir::create_unique)):
/// another tool's scratch directory is never reused or wiped (review
/// bulkcli#6). The tools call this only once they hold the database's
/// directory (`gbuild` its build claim, `gadd` / `gsub` the open database).
/// Every entry is accepted (as upstream); the directory walk reads the RDF
/// files among them. Without the `zip` feature a package is an error naming
/// it — never parsed as RDF.
pub fn unpack_packages(
    dir: &str,
    paths: &mut [String],
    limits: &gstore::zipio::ZipLimits,
) -> Result<Vec<gstore::zipio::ScratchDir>, String> {
    use gstore::zipio::{self, ScratchDir};
    let mut unpacked = Vec::new();
    for p in paths.iter_mut() {
        let path = Path::new(p.as_str());
        if gstore::parser::import::is_url(p) || !path.is_file() || !zipio::is_zip_name(path) {
            continue;
        }
        if !zipio::AVAILABLE {
            return Err(format!("cannot read '{p}': {}", zipio::FEATURE_MISSING));
        }
        let scratch = ScratchDir::create_unique(Path::new(&format!("{dir}.unzip-tmp")))
            .map_err(|e| format!("cannot unpack '{p}': {e}"))?;
        zipio::unpack(path, scratch.path(), limits, &|_| true)
            .map_err(|e| format!("cannot unpack '{p}': {}", zip_error(e)))?;
        gstore::outln!("unpacked '{p}'");
        *p = scratch.path().to_string_lossy().into_owned();
        unpacked.push(scratch);
    }
    Ok(unpacked)
}

/// An output file written under a temporary name beside its destination —
/// `<dest>.part-<pid>-<n>`, created exclusively — and renamed over the
/// destination only once it is complete and synced
/// ([`commit`](Self::commit)). Until then the destination is untouched (a
/// previous file of that name stays as it was), and a temporary file that
/// is not committed is removed when this is dropped — on an error, a panic,
/// a failed open of the database (review bulkcli#10, #11: `gquery -f`
/// truncated its result file before the database was even opened, and
/// `gexport` wrote in place, leaving a partial file behind on a write
/// error).
///
/// The destinations the tools used to write through keep working as they
/// did ([`create`](Self::create)): a symbolic link to a file stays a link
/// (the file it names is replaced), a replaced file keeps its permission
/// bits and, where allowed, its owner, a file the user may not write is
/// refused, and a destination that is not a regular file — a FIFO, a
/// device such as `/dev/null`, a dangling link — is written in place,
/// opened only when the output is ready (renaming over it would replace the
/// FIFO or the device with a regular file).
pub struct OutputFile {
    /// Where the output goes (a symbolic link resolved to the file it
    /// names).
    dest: PathBuf,
    /// The temporary file renamed over `dest`; `None` when the output is
    /// written into `dest` itself.
    tmp: Option<PathBuf>,
    /// The open file (in place: opened by the first [`writer`](Self::writer)).
    file: Option<std::io::BufWriter<std::fs::File>>,
    /// Whether [`finish`](Self::finish) closed the file.
    finished: bool,
    committed: bool,
}

impl OutputFile {
    /// The output file for `dest`: a temporary file beside it
    /// ([`beside`](Self::beside)) when `dest` is a regular file (through
    /// symbolic links: beside the file they name, whose permission bits and
    /// owner the temporary file takes, and which must be writable, as an
    /// in-place write needs) or does not exist; `dest` itself, written in
    /// place, when it is something else (a FIFO, a device, a dangling
    /// link). A directory is an error.
    pub fn create(dest: &Path) -> std::io::Result<OutputFile> {
        use std::fs;
        match fs::metadata(dest) {
            Ok(meta) if meta.is_file() => {
                let target = fs::canonicalize(dest)?;
                // Opened without truncation, so nothing changes: a file the
                // user may not write is refused as the in-place write
                // refused it (renaming over it would replace it anyway).
                fs::OpenOptions::new().write(true).open(&target)?;
                match OutputFile::beside(&target) {
                    Ok(out) => {
                        out.keep_attributes(&meta);
                        Ok(out)
                    }
                    // A writable file in a directory where no file can be
                    // created: written in place, as before (still only once
                    // the output is ready).
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        Ok(OutputFile::in_place(&target))
                    }
                    Err(e) => Err(e),
                }
            }
            Ok(meta) if meta.is_dir() => Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "is a directory",
            )),
            Ok(_) => Ok(OutputFile::in_place(dest)),
            // A dangling symbolic link: written through, as before.
            Err(_) if fs::symlink_metadata(dest).is_ok() => Ok(OutputFile::in_place(dest)),
            // A new file (or a path that cannot be reached, which the
            // creation of the temporary file reports).
            Err(_) => OutputFile::beside(dest),
        }
    }

    /// A temporary file beside `dest` (whose directory must exist), renamed
    /// over `dest` on [`commit`](Self::commit) — whatever `dest` is.
    pub fn beside(dest: &Path) -> std::io::Result<OutputFile> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut attempt = 0;
        loop {
            let mut tmp = dest.as_os_str().to_owned();
            tmp.push(format!(
                ".part-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let tmp = PathBuf::from(tmp);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(f) => {
                    return Ok(OutputFile {
                        dest: dest.to_path_buf(),
                        tmp: Some(tmp),
                        file: Some(std::io::BufWriter::new(f)),
                        finished: false,
                        committed: false,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => {
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `dest` itself, opened (created or truncated) by the first
    /// [`writer`](Self::writer).
    fn in_place(dest: &Path) -> OutputFile {
        OutputFile {
            dest: dest.to_path_buf(),
            tmp: None,
            file: None,
            finished: false,
            committed: false,
        }
    }

    /// Give the temporary file the permission bits and, where this process
    /// may, the owner of the file it replaces (best effort: a failure keeps
    /// the defaults).
    fn keep_attributes(&self, meta: &std::fs::Metadata) {
        let Some(w) = &self.file else { return };
        let f = w.get_ref();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Before the mode: a change of owner may clear set-id bits.
            let _ = std::os::unix::fs::fchown(f, Some(meta.uid()), Some(meta.gid()));
        }
        let _ = f.set_permissions(meta.permissions());
    }

    /// The file the content is written to until [`commit`](Self::commit):
    /// the temporary file, or the destination written in place.
    pub fn path(&self) -> &Path {
        self.tmp.as_deref().unwrap_or(&self.dest)
    }

    /// The writer (an error once the file was [`finish`](Self::finish)ed);
    /// a destination written in place is opened by the first call.
    pub fn writer(&mut self) -> std::io::Result<&mut std::io::BufWriter<std::fs::File>> {
        if self.finished {
            return Err(std::io::Error::other("the output file is closed"));
        }
        let file = match self.file.take() {
            Some(f) => f,
            None => std::io::BufWriter::new(std::fs::File::create(&self.dest)?),
        };
        Ok(self.file.insert(file))
    }

    /// Flush the file and close it; a temporary file is synced to disk
    /// first and stays under its temporary name (removed on drop unless
    /// committed). A destination written in place is opened (emptied) even
    /// when nothing was written, and is not synced (a FIFO or a device
    /// cannot be).
    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.writer()?;
        self.finished = true;
        if let Some(w) = self.file.take() {
            let f = w.into_inner().map_err(|e| e.into_error())?;
            if self.tmp.is_some() {
                f.sync_all()?;
            }
        }
        Ok(())
    }

    /// Finish the file and rename it over the destination (then sync the
    /// directory, best effort); a destination written in place is only
    /// finished.
    pub fn commit(mut self) -> std::io::Result<()> {
        self.finish()?;
        if let Some(tmp) = &self.tmp {
            std::fs::rename(tmp, &self.dest)?;
            let parent = self
                .dest
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.file.take());
            if let Some(tmp) = &self.tmp {
                let _ = std::fs::remove_file(tmp);
            }
        }
    }
}
