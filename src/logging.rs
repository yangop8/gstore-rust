//! Diagnostic logging for the servers — the analogue of upstream's `SLOG_*`
//! macros over log4cplus (`src/Util/GlobalTypedef.h`, configured by
//! `Util::configure()` from the properties file `conf.ini`'s `log_mode` names).
//!
//! Dependency-free: five [levels](Level) (`error`, `warn`, `info`, `debug`,
//! `trace`), two sinks — standard error and a log file that rolls over daily
//! and keeps a bounded number of days — and the [`gs_error!`], [`gs_warn!`],
//! [`gs_info!`], [`gs_debug!`] and [`gs_trace!`] macros, which format their
//! arguments only when some sink records the level.
//!
//! # Configuration
//!
//! [`LogConfig::from_gstore_config`] mirrors upstream:
//!
//! 1. `conf.ini` `[log] log_mode` (default `conf/slog.properties`) names a
//!    log4cplus properties file. When it exists, the subset of it upstream's
//!    shipped files use is honoured (see [`LogConfig::from_properties`]):
//!    `log4cplus.rootLogger=LEVEL, APPENDER…`, `ConsoleAppender`s (standard
//!    error here) and one `DailyRollingFileAppender` (`File`,
//!    `MaxBackupIndex` — the days kept —, `Schedule=DAILY`), plus a per
//!    appender `Threshold`. When it does not exist, upstream falls back to
//!    log4cplus' basic configuration — console only; here: standard error at
//!    `info`.
//! 2. The gstore-rust extension keys of `[log]` then override it: `log_level`
//!    (`trace`…`error`, `off`), `log_file` (a path, or `off`), `log_days` and
//!    `log_stderr` (`on`/`off`).
//! 3. `gserver` finally applies the `GSTORE_LOG` environment variable (a
//!    level), when set ([`LogConfig::apply_env`]).
//!
//! # The log file
//!
//! Like log4cplus' `DailyRollingFileAppender` with `Schedule=DAILY`, records
//! go to the configured file (upstream: `./logs/all.log`); the first record of
//! a new day renames it to `<file>.YYYY-MM-DD` (the day it holds) and starts a
//! new one. A file left by an earlier run on an earlier day is rolled the same
//! way when the logger opens it. Rolled files more than `days` days old are
//! deleted whenever the logger opens or rolls (`days = 0` keeps them all).
//! Dates are local time. The clock is injectable ([`Clock`],
//! [`ManualClock`]) so rolling and retention are testable.
//!
//! Several processes may share one file (the `deamon=on` supervisor and its
//! server): each appends whole lines, and a process only rolls the file it
//! has open itself, so a file another process already rolled is not rolled
//! twice.
//!
//! Every record is one line: `YYYY-MM-DD HH:MM:SS.mmm [LEVEL] [target]
//! message` (the target is the Rust module, or the binary's name).
//!
//! # Process-wide logger
//!
//! [`install`] makes a [`Logger`] the one the macros write to. Until then (or
//! after [`reset`]) records at `info` and above go to standard error. Writes
//! never fail a caller: a sink that cannot be written is skipped.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::GStoreConfig;
use crate::sysdb::clock::{self, LocalTime};

// ---------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------

/// A record's severity, most severe first (`Error < Warn < … < Trace` in the
/// derived order, i.e. by verbosity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    /// Every level, most severe first.
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    /// The upper-case name (`ERROR`, `WARN`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// Parse a threshold, case-insensitively: a level name (`warning` and
    /// log4cplus' `fatal` are accepted, as `warn` and `error`), or `off` —
    /// `Ok(None)`. `Err(())` for anything else.
    #[allow(clippy::result_unit_err)]
    pub fn parse_filter(s: &str) -> Result<Option<Level>, ()> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" | "fatal" => Ok(Some(Level::Error)),
            "warn" | "warning" => Ok(Some(Level::Warn)),
            "info" => Ok(Some(Level::Info)),
            "debug" => Ok(Some(Level::Debug)),
            "trace" | "all" => Ok(Some(Level::Trace)),
            "off" | "none" => Ok(None),
            _ => Err(()),
        }
    }

    /// Parse a level name (see [`parse_filter`](Self::parse_filter); `off`
    /// is not a level).
    pub fn parse(s: &str) -> Option<Level> {
        Level::parse_filter(s).ok().flatten()
    }

    fn from_u8(n: u8) -> Option<Level> {
        Level::ALL.into_iter().find(|l| *l as u8 == n)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// The name of a threshold (`off` for `None`).
pub fn filter_name(level: Option<Level>) -> &'static str {
    match level {
        None => "off",
        Some(Level::Error) => "error",
        Some(Level::Warn) => "warn",
        Some(Level::Info) => "info",
        Some(Level::Debug) => "debug",
        Some(Level::Trace) => "trace",
    }
}

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

/// Where the logger reads the time: the record timestamps, the day the log
/// file rolls on, and the age of rolled files.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> SystemTime;
    /// `t` broken down in the zone log dates are in (local time).
    fn local(&self, t: SystemTime) -> LocalTime {
        clock::at(t)
    }
}

/// The real clock, in the server's local time zone.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A settable clock for tests. Its zone is UTC (a [`SystemTime`] is broken
/// down without an offset), so dates are deterministic everywhere.
#[derive(Debug)]
pub struct ManualClock {
    unix_millis: Mutex<i64>,
}

impl ManualClock {
    /// A clock reading `secs` seconds after the Unix epoch.
    pub fn at_unix(secs: i64) -> ManualClock {
        ManualClock {
            unix_millis: Mutex::new(secs.saturating_mul(1000)),
        }
    }

    /// A clock reading midnight (UTC) of the given day.
    pub fn at_date(year: i64, month: u32, day: u32) -> ManualClock {
        ManualClock::at_unix(days_from_civil(year, month, day) * 86_400)
    }

    /// Set the time to `secs` seconds after the epoch.
    pub fn set_unix(&self, secs: i64) {
        *self.millis() = secs.saturating_mul(1000);
    }

    /// Move the clock forward by `d`.
    pub fn advance(&self, d: Duration) {
        let mut m = self.millis();
        *m = m.saturating_add(i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    }

    /// The current reading, seconds after the epoch.
    pub fn unix_secs(&self) -> i64 {
        self.millis().div_euclid(1000)
    }

    fn millis(&self) -> std::sync::MutexGuard<'_, i64> {
        self.unix_millis.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        let ms = *self.millis();
        if ms >= 0 {
            UNIX_EPOCH + Duration::from_millis(ms as u64)
        } else {
            UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
        }
    }

    fn local(&self, t: SystemTime) -> LocalTime {
        let (secs, millis) = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (i64::try_from(d.as_secs()).unwrap_or(i64::MAX / 2), d.subsec_millis()),
            Err(e) => (-(i64::try_from(e.duration().as_secs()).unwrap_or(0)), 0),
        };
        clock::from_unix(secs, millis)
    }
}

/// A calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl Date {
    /// The day of a broken-down time.
    pub fn of(t: &LocalTime) -> Date {
        Date {
            year: t.year,
            month: t.month,
            day: t.day,
        }
    }

    /// `YYYY-MM-DD`, the suffix of a rolled log file (log4cplus' daily
    /// pattern).
    pub fn iso(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Parse `YYYY-MM-DD`.
    pub fn parse_iso(s: &str) -> Option<Date> {
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        let digits = |r: std::ops::Range<usize>| -> Option<u32> {
            let part = s.get(r)?;
            part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
        };
        let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
        ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some(Date {
            year: i64::from(year),
            month,
            day,
        })
    }

    /// Days since 1970-01-01.
    pub fn days(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Days of rolled log files kept by default (upstream's shipped
/// `slog.properties`: `MaxBackupIndex=30`).
pub const DEFAULT_LOG_DAYS: u32 = 30;

/// The environment variable overriding the level ([`LogConfig::apply_env`]).
pub const ENV_LEVEL: &str = "GSTORE_LOG";

/// The daily-rolling log file sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSinkConfig {
    /// The active log file (rolled files sit next to it).
    pub path: PathBuf,
    /// Days of rolled files kept; `0` keeps them all.
    pub days: u32,
    /// The most verbose level this sink records (on top of the logger's
    /// level).
    pub threshold: Level,
}

/// What a [`Logger`] records and where. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// The most verbose level recorded at all (`None`: logging off).
    pub level: Option<Level>,
    /// The standard-error sink's threshold (`None`: no standard-error sink).
    pub stderr: Option<Level>,
    /// The log file, if any.
    pub file: Option<FileSinkConfig>,
}

impl Default for LogConfig {
    /// Standard error at `info` — what upstream's fallback configuration
    /// (no properties file) gives, at upstream's usual level.
    fn default() -> Self {
        LogConfig {
            level: Some(Level::Info),
            stderr: Some(Level::Trace),
            file: None,
        }
    }
}

impl LogConfig {
    /// Read the log4cplus properties text `text` (the file `conf.ini`'s
    /// `log_mode` names). Relative file paths are kept relative (to the
    /// working directory, as log4cplus resolves them). Returns the
    /// configuration and diagnostics for what is not supported.
    ///
    /// Honoured: `log4cplus.rootLogger=LEVEL[, APPENDER…]`;
    /// `log4cplus.appender.A=log4cplus::ConsoleAppender` (standard error);
    /// one `log4cplus::DailyRollingFileAppender` (or `FileAppender`,
    /// `RollingFileAppender`, `TimeBasedRollingFileAppender`, all rolled
    /// daily here) with `.File`, `.MaxBackupIndex` (days kept) and
    /// `.Schedule` (`DAILY`); `.Threshold` on either. Layouts, filters and
    /// non-root loggers are ignored (the record format is fixed).
    pub fn from_properties(text: &str) -> (LogConfig, Vec<String>) {
        let mut warnings = Vec::new();
        let mut props: Vec<(String, String)> = Vec::new();
        for (no, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            match line.split_once('=') {
                Some((k, v)) => props.push((k.trim().to_string(), v.trim().to_string())),
                None => warnings.push(format!("line {}: ignoring '{line}'", no + 1)),
            }
        }
        let get = |key: &str| -> Option<&str> {
            props
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let mut cfg = LogConfig {
            level: Some(Level::Info),
            stderr: None,
            file: None,
        };
        let Some(root) = get("log4cplus.rootLogger") else {
            warnings.push("no log4cplus.rootLogger: logging to standard error".to_string());
            return (LogConfig::default(), warnings);
        };
        let mut parts = root.split(',').map(str::trim);
        let level = parts.next().unwrap_or("");
        if !level.is_empty() {
            match Level::parse_filter(level) {
                Ok(l) => cfg.level = l,
                Err(()) => warnings.push(format!("unknown root level '{level}': using INFO")),
            }
        }
        for name in parts.filter(|n| !n.is_empty()) {
            let key = format!("log4cplus.appender.{name}");
            let Some(class) = get(&key) else {
                warnings.push(format!("appender '{name}' is not defined"));
                continue;
            };
            let threshold = match get(&format!("{key}.Threshold")) {
                None => Level::Trace,
                Some(t) => match Level::parse_filter(t) {
                    Ok(Some(l)) => l,
                    Ok(None) => continue, // Threshold=OFF: the appender records nothing
                    Err(()) => {
                        warnings.push(format!("{key}.Threshold: unknown level '{t}'"));
                        Level::Trace
                    }
                },
            };
            let class = class.rsplit("::").next().unwrap_or(class);
            match class {
                "ConsoleAppender" => cfg.stderr = Some(threshold),
                "DailyRollingFileAppender"
                | "FileAppender"
                | "RollingFileAppender"
                | "TimeBasedRollingFileAppender" => {
                    if cfg.file.is_some() {
                        warnings.push(format!(
                            "appender '{name}': only one log file is supported; ignored"
                        ));
                        continue;
                    }
                    let Some(path) = get(&format!("{key}.File"))
                        .or_else(|| get(&format!("{key}.FilenamePattern")))
                        .filter(|p| !p.is_empty())
                    else {
                        warnings.push(format!("appender '{name}' has no File; ignored"));
                        continue;
                    };
                    let days = match get(&format!("{key}.MaxBackupIndex"))
                        .or_else(|| get(&format!("{key}.MaxHistory")))
                    {
                        None => DEFAULT_LOG_DAYS,
                        Some(v) => v.parse().unwrap_or_else(|_| {
                            warnings.push(format!("{key}.MaxBackupIndex: not a number '{v}'"));
                            DEFAULT_LOG_DAYS
                        }),
                    };
                    if let Some(s) = get(&format!("{key}.Schedule")) {
                        if !s.eq_ignore_ascii_case("DAILY") {
                            warnings.push(format!(
                                "{key}.Schedule={s}: only DAILY rolling is supported; rolling daily"
                            ));
                        }
                    }
                    cfg.file = Some(FileSinkConfig {
                        path: PathBuf::from(path),
                        days,
                        threshold,
                    });
                }
                other => warnings.push(format!("appender '{name}': {other} is not supported")),
            }
        }
        (cfg, warnings)
    }

    /// The logging configuration of a `conf.ini` (see the
    /// [module docs](self)): the properties file `log_mode` names, when it
    /// exists, overridden by the extension keys. Relative paths are taken
    /// relative to the working directory, as upstream. Returns diagnostics
    /// too.
    pub fn from_gstore_config(cfg: &GStoreConfig) -> (LogConfig, Vec<String>) {
        let mut warnings = Vec::new();
        let props = Path::new(&cfg.log_mode);
        let mut out = if !cfg.log_mode.is_empty() && props.is_file() {
            match fs::read_to_string(props) {
                Ok(text) => {
                    let (c, w) = LogConfig::from_properties(&text);
                    warnings.extend(
                        w.into_iter()
                            .map(|w| format!("{}: {w}", props.display())),
                    );
                    c
                }
                Err(e) => {
                    warnings.push(format!(
                        "cannot read log config file '{}': {e}; using the default (standard error)",
                        props.display()
                    ));
                    LogConfig::default()
                }
            }
        } else {
            if cfg.is_set("log_mode") {
                // Upstream: "log config file not exist, use default config".
                warnings.push(format!(
                    "log config file '{}' does not exist; using the default (standard error)",
                    cfg.log_mode
                ));
            }
            LogConfig::default()
        };
        if cfg.is_set("log_level") {
            if let Ok(l) = Level::parse_filter(&cfg.log_level) {
                out.level = l;
            }
        }
        if cfg.is_set("log_file") {
            let v = cfg.log_file.trim();
            if v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") {
                out.file = None;
            } else if !v.is_empty() {
                let (days, threshold) = out
                    .file
                    .as_ref()
                    .map_or((DEFAULT_LOG_DAYS, Level::Trace), |f| (f.days, f.threshold));
                out.file = Some(FileSinkConfig {
                    path: PathBuf::from(v),
                    days,
                    threshold,
                });
            }
        }
        if cfg.is_set("log_days") {
            if let Some(f) = out.file.as_mut() {
                f.days = cfg.log_days;
            }
        }
        if cfg.is_set("log_stderr") {
            out.stderr = cfg.log_stderr.then_some(out.stderr.unwrap_or(Level::Trace));
        }
        (out, warnings)
    }

    /// Apply the `GSTORE_LOG` environment variable (a level, or `off`).
    /// Returns a diagnostic for a value that is not one.
    pub fn apply_env(&mut self) -> Option<String> {
        let v = std::env::var(ENV_LEVEL).ok()?;
        if v.trim().is_empty() {
            return None;
        }
        match Level::parse_filter(&v) {
            Ok(l) => {
                self.level = l;
                None
            }
            Err(()) => Some(format!(
                "{ENV_LEVEL}='{v}' is not a log level (error, warn, info, debug, trace, off); ignored"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The file sink
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FileState {
    file: Option<File>,
    /// The day the open file holds records of.
    date: Option<Date>,
}

#[derive(Debug)]
struct FileSink {
    path: PathBuf,
    days: u32,
    threshold: Level,
    state: Mutex<FileState>,
}

impl FileSink {
    fn open(cfg: &FileSinkConfig, clock: &dyn Clock) -> io::Result<FileSink> {
        if let Some(dir) = cfg.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        if cfg.path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("log file '{}' is a directory", cfg.path.display()),
            ));
        }
        let today = Date::of(&clock.local(clock.now()));
        let sink = FileSink {
            path: cfg.path.clone(),
            days: cfg.days,
            threshold: cfg.threshold,
            state: Mutex::new(FileState {
                file: None,
                date: None,
            }),
        };
        // A file an earlier run left on an earlier day is rolled first.
        if let Ok(meta) = fs::metadata(&sink.path) {
            if let Ok(mtime) = meta.modified() {
                let day = Date::of(&clock.local(mtime));
                if day < today && meta.len() > 0 {
                    sink.roll_to(day)?;
                }
            }
        }
        let file = append(&sink.path)?;
        {
            let mut st = sink.state();
            st.file = Some(file);
            st.date = Some(today);
        }
        sink.prune(today);
        Ok(sink)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FileState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append `line` (already newline-terminated) for a record of day
    /// `today`, rolling the file first when the day changed.
    fn write(&self, today: Date, line: &[u8]) {
        let mut st = self.state();
        if st.date != Some(today) {
            if let Some(old) = st.date {
                // Roll only the file this process has open: another process
                // sharing the file may have rolled it (and started a new one)
                // already.
                if st.file.as_ref().is_some_and(|f| same_file(f, &self.path)) {
                    let _ = self.roll_to(old);
                }
            }
            st.file = append(&self.path).ok();
            st.date = Some(today);
            drop(st);
            self.prune(today);
            st = self.state();
        }
        if let Some(f) = st.file.as_mut() {
            let _ = f.write_all(line);
        }
    }

    /// Rename the active file to `<file>.<day>` (or `<file>.<day>.<n>` when
    /// that exists already).
    fn roll_to(&self, day: Date) -> io::Result<()> {
        let base = self.rolled_name(day);
        let mut target = base.clone();
        let mut n = 1u32;
        while target.exists() {
            target = PathBuf::from(format!("{}.{n}", base.display()));
            n += 1;
        }
        fs::rename(&self.path, &target)
    }

    fn rolled_name(&self, day: Date) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(format!(".{}", day.iso()));
        self.path.with_file_name(name)
    }

    /// Delete the rolled files more than `days` days older than `today`.
    fn prune(&self, today: Date) {
        if self.days == 0 {
            return;
        }
        for (path, day) in rolled_files(&self.path) {
            if today.days() - day.days() > i64::from(self.days) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// The rolled siblings of log file `path` (`<name>.YYYY-MM-DD[.N]`) with
/// their dates, oldest first.
pub fn rolled_files(path: &Path) -> Vec<(PathBuf, Date)> {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from("."),
    };
    let prefix = format!("{name}.");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = fname.strip_prefix(&prefix) else {
            continue;
        };
        let (date, tail) = rest.split_at(rest.len().min(10));
        let Some(day) = Date::parse_iso(date) else {
            continue;
        };
        let tail_ok = tail.is_empty()
            || tail
                .strip_prefix('.')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()));
        if tail_ok && entry.path().is_file() {
            out.push((entry.path(), day));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

fn append(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new().create(true).append(true).open(path)
}

/// Whether `path` is (still) the file `f` has open.
#[cfg(unix)]
fn same_file(f: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (f.metadata(), fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_f: &File, path: &Path) -> bool {
    path.is_file()
}

// ---------------------------------------------------------------------------
// The logger
// ---------------------------------------------------------------------------

/// Which sinks a record goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sinks {
    /// Every configured sink.
    All,
    /// Only the log file (for messages the caller already printed itself).
    FileOnly,
}

/// A configured logger: its level, its sinks, its clock. Thread-safe.
pub struct Logger {
    level: Option<Level>,
    stderr: Option<Level>,
    file: Option<FileSink>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logger")
            .field("level", &self.level)
            .field("stderr", &self.stderr)
            .field("file", &self.file.as_ref().map(|s| &s.path))
            .finish()
    }
}

impl Logger {
    /// A logger on the system clock. Fails when the log file cannot be
    /// opened (its directory is created as needed).
    pub fn new(cfg: &LogConfig) -> io::Result<Logger> {
        Logger::with_clock(cfg, Arc::new(SystemClock))
    }

    /// A logger reading the time from `clock`.
    pub fn with_clock(cfg: &LogConfig, clock: Arc<dyn Clock>) -> io::Result<Logger> {
        let file = match &cfg.file {
            Some(f) if cfg.level.is_some() => Some(FileSink::open(f, clock.as_ref())?),
            _ => None,
        };
        Ok(Logger {
            level: cfg.level,
            stderr: cfg.stderr,
            file,
            clock,
        })
    }

    /// The active log file, if any.
    pub fn file_path(&self) -> Option<&Path> {
        self.file.as_ref().map(|f| f.path.as_path())
    }

    /// The most verbose level some sink records (`None`: nothing at all).
    pub fn max_level(&self) -> Option<Level> {
        let root = self.level?;
        let sinks = [self.stderr, self.file.as_ref().map(|f| f.threshold)];
        sinks.into_iter().flatten().map(|t| t.min(root)).max()
    }

    /// Whether a record at `level` reaches some sink.
    pub fn enabled(&self, level: Level) -> bool {
        self.max_level().is_some_and(|m| level <= m)
    }

    /// Record `args` at `level` from `target` on every sink that takes it.
    pub fn log(&self, level: Level, target: &str, args: fmt::Arguments<'_>) {
        self.log_to(Sinks::All, level, target, args);
    }

    /// Record on the chosen sinks.
    pub fn log_to(&self, sinks: Sinks, level: Level, target: &str, args: fmt::Arguments<'_>) {
        let Some(root) = self.level else {
            return;
        };
        if level > root {
            return;
        }
        let to_stderr = sinks == Sinks::All && self.stderr.is_some_and(|t| level <= t);
        let to_file = self.file.as_ref().filter(|f| level <= f.threshold);
        if !to_stderr && to_file.is_none() {
            return;
        }
        let now = self.clock.local(self.clock.now());
        let line = format_line(&now, level, target, args);
        if to_stderr {
            let _ = io::stderr().lock().write_all(line.as_bytes());
        }
        if let Some(f) = to_file {
            f.write(Date::of(&now), line.as_bytes());
        }
    }
}

/// One record as a line: `YYYY-MM-DD HH:MM:SS.mmm [LEVEL] [target] message\n`.
pub fn format_line(now: &LocalTime, level: Level, target: &str, args: fmt::Arguments<'_>) -> String {
    let mut line = format!("{} [{:>5}] [{target}] {args}", now.datetime_ms(), level);
    line.push('\n');
    line
}

// ---------------------------------------------------------------------------
// The process-wide logger
// ---------------------------------------------------------------------------

/// The most verbose level the process-wide logger records (`0`: none).
static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

static LOGGER: RwLock<Option<Arc<Logger>>> = RwLock::new(None);

/// Make `logger` the process-wide logger the macros write to. Returns it.
pub fn install(logger: Logger) -> Arc<Logger> {
    let logger = Arc::new(logger);
    let max = logger.max_level().map_or(0, |l| l as u8);
    *LOGGER.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&logger));
    MAX_LEVEL.store(max, Ordering::SeqCst);
    logger
}

/// Build a logger from `cfg` and [`install`] it. On failure (the log file
/// cannot be opened) the standard-error part of `cfg` is installed instead
/// and the error returned.
pub fn init(cfg: &LogConfig) -> io::Result<Arc<Logger>> {
    match Logger::new(cfg) {
        Ok(l) => Ok(install(l)),
        Err(e) => {
            let fallback = LogConfig {
                file: None,
                ..cfg.clone()
            };
            // Without a file the constructor cannot fail.
            if let Ok(l) = Logger::new(&fallback) {
                install(l);
            }
            Err(e)
        }
    }
}

/// Back to the default: standard error at `info`, no file.
pub fn reset() {
    *LOGGER.write().unwrap_or_else(|e| e.into_inner()) = None;
    MAX_LEVEL.store(Level::Info as u8, Ordering::SeqCst);
}

/// The installed logger, if any.
pub fn current() -> Option<Arc<Logger>> {
    LOGGER.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The most verbose level recorded process-wide (`None`: logging off).
pub fn max_level() -> Option<Level> {
    Level::from_u8(MAX_LEVEL.load(Ordering::Relaxed))
}

/// Whether a record at `level` would be written (cheap: the macros check it
/// before formatting).
#[inline]
pub fn enabled(level: Level) -> bool {
    level as u8 <= MAX_LEVEL.load(Ordering::Relaxed)
}

/// Write a record through the process-wide logger (what the macros call).
pub fn emit(level: Level, target: &str, args: fmt::Arguments<'_>) {
    let guard = LOGGER.read().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(l) => l.log(level, target, args),
        None => {
            let line = format_line(&clock::now(), level, target, args);
            let _ = io::stderr().lock().write_all(line.as_bytes());
        }
    }
}

/// Print `args` to standard output as is (a user-facing message, e.g. a
/// server's startup banner that scripts read) and record it at `info` in the
/// log file only — the console has it already.
pub fn announce(target: &str, args: fmt::Arguments<'_>) {
    {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{args}");
        let _ = out.flush();
    }
    if enabled(Level::Info) {
        if let Some(l) = current() {
            l.log_to(Sinks::FileOnly, Level::Info, target, args);
        }
    }
}

/// Log at a given [`Level`](crate::logging::Level) through the process-wide
/// logger; the arguments are formatted only when the level is enabled.
#[macro_export]
macro_rules! gs_log {
    ($lvl:expr, $($arg:tt)+) => {{
        let lvl: $crate::logging::Level = $lvl;
        if $crate::logging::enabled(lvl) {
            $crate::logging::emit(lvl, ::std::module_path!(), ::std::format_args!($($arg)+));
        }
    }};
}

/// Log at `error` (upstream `SLOG_ERROR`).
#[macro_export]
macro_rules! gs_error {
    ($($arg:tt)+) => { $crate::gs_log!($crate::logging::Level::Error, $($arg)+) };
}

/// Log at `warn` (upstream `SLOG_WARN`).
#[macro_export]
macro_rules! gs_warn {
    ($($arg:tt)+) => { $crate::gs_log!($crate::logging::Level::Warn, $($arg)+) };
}

/// Log at `info` (upstream `SLOG_INFO`).
#[macro_export]
macro_rules! gs_info {
    ($($arg:tt)+) => { $crate::gs_log!($crate::logging::Level::Info, $($arg)+) };
}

/// Log at `debug` (upstream `SLOG_DEBUG`).
#[macro_export]
macro_rules! gs_debug {
    ($($arg:tt)+) => { $crate::gs_log!($crate::logging::Level::Debug, $($arg)+) };
}

/// Log at `trace` (upstream `SLOG_TRACE`).
#[macro_export]
macro_rules! gs_trace {
    ($($arg:tt)+) => { $crate::gs_log!($crate::logging::Level::Trace, $($arg)+) };
}

/// Print a user-facing message to standard output and record it at `info` in
/// the log file (see [`announce`](crate::logging::announce)).
#[macro_export]
macro_rules! gs_announce {
    ($($arg:tt)+) => {
        $crate::logging::announce(::std::module_path!(), ::std::format_args!($($arg)+))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_order() {
        assert!(Level::Error < Level::Warn && Level::Debug < Level::Trace);
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("fatal"), Some(Level::Error));
        assert_eq!(Level::parse_filter("off"), Ok(None));
        assert_eq!(Level::parse_filter("loud"), Err(()));
        assert_eq!(format!("[{:>5}]", Level::Info), "[ INFO]");
        for l in Level::ALL {
            assert_eq!(Level::parse(l.as_str()), Some(l));
            assert_eq!(Level::parse_filter(filter_name(Some(l))), Ok(Some(l)));
        }
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        let d = Date::parse_iso("2024-02-29").unwrap();
        assert_eq!(d.iso(), "2024-02-29");
        assert_eq!(Date::of(&clock::from_unix(d.days() * 86_400, 0)), d);
        assert!(Date::parse_iso("2024-13-01").is_none());
        assert!(Date::parse_iso("2024-1-01x").is_none());
        let clock = ManualClock::at_date(2026, 10, 31);
        assert_eq!(Date::of(&clock.local(clock.now())).iso(), "2026-10-31");
        clock.advance(Duration::from_secs(86_400));
        assert_eq!(Date::of(&clock.local(clock.now())).iso(), "2026-11-01");
    }

    #[test]
    fn upstream_properties_files() {
        let (c, w) = LogConfig::from_properties(include_str!("../conf/slog.properties.example"));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.level, Some(Level::Info));
        assert_eq!(c.stderr, Some(Level::Trace));
        let f = c.file.unwrap();
        assert_eq!(f.path, PathBuf::from("./logs/all.log"));
        assert_eq!(f.days, 30);
        let (c, w) =
            LogConfig::from_properties(include_str!("../conf/slog.stdout.properties.example"));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!((c.level, c.stderr, c.file), (Some(Level::Trace), Some(Level::Trace), None));
    }
}
