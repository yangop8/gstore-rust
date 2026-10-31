//! Data test: the diagnostic logger ([`gstore::logging`]) — levels, the
//! daily-rolling log file (driven by an injected clock), retention, the
//! configuration from `conf.ini` and log4cplus properties, the process-wide
//! macros — and the query / access records of the REST endpoints of
//! [`gstore::http_api::ApiServer`] (client IP, answer count, latency).

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::logging::{
    self, days_from_civil, rolled_files, Clock, Date, FileSinkConfig, Level, LogConfig, Logger,
    ManualClock,
};

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gstore_dt_logging_{tag}_{}_{}",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// Seconds since the epoch of `y-m-d h:mi:s` (UTC, the [`ManualClock`]'s zone).
fn at(y: i64, m: u32, d: u32, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s
}

/// A file-only logger (no standard error) on `clock`.
fn file_logger(path: &Path, level: Level, days: u32, clock: &Arc<ManualClock>) -> Logger {
    let cfg = LogConfig {
        level: Some(level),
        stderr: None,
        file: Some(FileSinkConfig {
            path: path.to_path_buf(),
            days,
            threshold: Level::Trace,
        }),
    };
    let clock: Arc<dyn Clock> = Arc::clone(clock) as Arc<dyn Clock>;
    Logger::with_clock(&cfg, clock).expect("open the log file")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

// ---- levels and the record format -------------------------------------------

#[test]
fn levels_filter_what_each_sink_records() {
    let dir = tmp_dir("levels");
    let path = dir.join("all.log");
    let clock = Arc::new(ManualClock::at_unix(at(2026, 10, 31, 9, 30, 5)));
    let log = file_logger(&path, Level::Info, 30, &clock);
    assert_eq!(log.max_level(), Some(Level::Info));
    assert!(log.enabled(Level::Error) && log.enabled(Level::Info));
    assert!(!log.enabled(Level::Debug) && !log.enabled(Level::Trace));
    log.log(Level::Trace, "t", format_args!("trace {}", 1));
    log.log(Level::Debug, "t", format_args!("debug {}", 2));
    log.log(Level::Info, "t", format_args!("info {}", 3));
    log.log(Level::Warn, "t", format_args!("warn {}", 4));
    log.log(
        Level::Error,
        "gstore::http_api",
        format_args!("error {}", 5),
    );
    let text = read(&path);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert_eq!(lines[0], "2026-10-31 09:30:05.000 [ INFO] [t] info 3");
    assert_eq!(lines[1], "2026-10-31 09:30:05.000 [ WARN] [t] warn 4");
    assert_eq!(
        lines[2],
        "2026-10-31 09:30:05.000 [ERROR] [gstore::http_api] error 5"
    );

    // A sink threshold narrows further; a root `off` silences everything.
    let path2 = dir.join("warn.log");
    let cfg = LogConfig {
        level: Some(Level::Trace),
        stderr: None,
        file: Some(FileSinkConfig {
            path: path2.clone(),
            days: 30,
            threshold: Level::Warn,
        }),
    };
    let log = Logger::with_clock(&cfg, Arc::clone(&clock) as Arc<dyn Clock>).unwrap();
    assert_eq!(log.max_level(), Some(Level::Warn));
    log.log(Level::Info, "t", format_args!("not me"));
    log.log(Level::Warn, "t", format_args!("me"));
    assert_eq!(read(&path2).lines().count(), 1);
    let off = LogConfig { level: None, ..cfg };
    let log = Logger::with_clock(&off, Arc::clone(&clock) as Arc<dyn Clock>).unwrap();
    assert_eq!(log.max_level(), None);
    assert!(!log.enabled(Level::Error));
    let _ = fs::remove_dir_all(&dir);
}

// ---- daily rolling ----------------------------------------------------------

#[test]
fn the_log_file_rolls_over_at_midnight_of_the_injected_clock() {
    let dir = tmp_dir("rolling");
    let path = dir.join("all.log");
    let clock = Arc::new(ManualClock::at_unix(at(2026, 10, 30, 23, 59, 58)));
    let log = file_logger(&path, Level::Info, 30, &clock);
    log.log(Level::Info, "t", format_args!("first day"));
    clock.advance(Duration::from_secs(1));
    log.log(Level::Info, "t", format_args!("still the first day"));
    assert_eq!(names(&dir), ["all.log"], "no roll before midnight");

    clock.advance(Duration::from_secs(3)); // 2026-10-31 00:00:02
    log.log(Level::Info, "t", format_args!("second day"));
    assert_eq!(names(&dir), ["all.log", "all.log.2026-10-30"]);
    let rolled = read(&dir.join("all.log.2026-10-30"));
    assert!(rolled.contains("first day") && rolled.contains("still the first day"));
    assert!(!rolled.contains("second day"));
    let current = read(&path);
    assert_eq!(current.lines().count(), 1, "{current}");
    assert!(
        current.starts_with("2026-10-31 00:00:02.000 [ INFO]"),
        "{current}"
    );

    // Several days later: the file holds the second day only.
    clock.set_unix(at(2026, 11, 3, 12, 0, 0));
    log.log(Level::Warn, "t", format_args!("fifth day"));
    assert_eq!(
        names(&dir),
        ["all.log", "all.log.2026-10-30", "all.log.2026-10-31"]
    );
    assert!(read(&dir.join("all.log.2026-10-31")).contains("second day"));
    assert!(read(&path).contains("fifth day"));
    let listed: Vec<String> = rolled_files(&path).iter().map(|(_, d)| d.iso()).collect();
    assert_eq!(listed, ["2026-10-30", "2026-10-31"]);
    let _ = fs::remove_dir_all(&dir);
}

/// A record stamped before midnight that is written after one stamped after
/// it (threads racing for the file, or a clock stepped back) goes into the
/// current file: the log never rolls backwards, nor splits a day.
#[test]
fn a_late_record_from_the_previous_day_does_not_roll_the_file_back() {
    let dir = tmp_dir("late");
    let path = dir.join("all.log");
    let clock = Arc::new(ManualClock::at_unix(at(2026, 10, 30, 23, 59, 59)));
    let log = file_logger(&path, Level::Info, 0, &clock);
    log.log(Level::Info, "t", format_args!("D record"));
    clock.set_unix(at(2026, 10, 31, 0, 0, 1));
    log.log(Level::Info, "t", format_args!("D+1 record A"));
    clock.set_unix(at(2026, 10, 30, 23, 59, 59));
    log.log(Level::Info, "t", format_args!("D record (late)"));
    clock.set_unix(at(2026, 10, 31, 0, 0, 2));
    log.log(Level::Info, "t", format_args!("D+1 record B"));
    assert_eq!(names(&dir), ["all.log", "all.log.2026-10-30"]);
    assert_eq!(read(&dir.join("all.log.2026-10-30")).lines().count(), 1);
    let current = read(&path);
    assert_eq!(current.lines().count(), 3, "{current}");
    assert!(current.contains("(late)") && current.contains("record B"));
    // The next day rolls the whole of D+1 into one file.
    clock.set_unix(at(2026, 11, 1, 0, 0, 5));
    log.log(Level::Info, "t", format_args!("D+2"));
    assert_eq!(
        names(&dir),
        ["all.log", "all.log.2026-10-30", "all.log.2026-10-31"]
    );
    assert_eq!(read(&dir.join("all.log.2026-10-31")).lines().count(), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_file_left_by_an_earlier_run_on_an_earlier_day_is_rolled_when_opened() {
    let dir = tmp_dir("stale");
    let path = dir.join("all.log");
    fs::write(&path, "old record\n").unwrap();
    let mtime = UNIX_EPOCH + Duration::from_secs(at(2026, 10, 28, 12, 0, 0) as u64);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let clock = Arc::new(ManualClock::at_unix(at(2026, 10, 31, 8, 0, 0)));
    let log = file_logger(&path, Level::Info, 30, &clock);
    assert_eq!(read(&dir.join("all.log.2026-10-28")), "old record\n");
    assert_eq!(read(&path), "");
    log.log(Level::Info, "t", format_args!("new run"));
    assert!(read(&path).contains("new run"));

    // A file from the same day is appended to, not rolled.
    drop(log);
    let same_day = UNIX_EPOCH + Duration::from_secs(at(2026, 10, 31, 7, 0, 0) as u64);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(same_day)
        .unwrap();
    let log = file_logger(&path, Level::Info, 30, &clock);
    log.log(Level::Info, "t", format_args!("same day"));
    let text = read(&path);
    assert!(
        text.contains("new run") && text.contains("same day"),
        "{text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_roll_onto_an_existing_rolled_name_keeps_both() {
    let dir = tmp_dir("collide");
    let path = dir.join("all.log");
    fs::write(dir.join("all.log.2026-10-30"), "earlier copy\n").unwrap();
    let clock = Arc::new(ManualClock::at_unix(at(2026, 10, 30, 10, 0, 0)));
    let log = file_logger(&path, Level::Info, 30, &clock);
    log.log(Level::Info, "t", format_args!("later copy"));
    clock.set_unix(at(2026, 10, 31, 10, 0, 0));
    log.log(Level::Info, "t", format_args!("next day"));
    assert_eq!(read(&dir.join("all.log.2026-10-30")), "earlier copy\n");
    assert!(read(&dir.join("all.log.2026-10-30.1")).contains("later copy"));
    let listed: Vec<String> = rolled_files(&path).iter().map(|(_, d)| d.iso()).collect();
    assert_eq!(listed, ["2026-10-30", "2026-10-30"]);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn two_processes_sharing_a_log_file_roll_it_once() {
    let dir = tmp_dir("shared");
    let path = dir.join("all.log");
    let clock_a = Arc::new(ManualClock::at_unix(at(2026, 10, 30, 22, 0, 0)));
    let clock_b = Arc::new(ManualClock::at_unix(at(2026, 10, 30, 22, 0, 0)));
    let a = file_logger(&path, Level::Info, 30, &clock_a);
    let b = file_logger(&path, Level::Info, 30, &clock_b);
    a.log(Level::Info, "a", format_args!("a day one"));
    b.log(Level::Info, "b", format_args!("b day one"));
    for c in [&clock_a, &clock_b] {
        c.set_unix(at(2026, 10, 31, 1, 0, 0));
    }
    a.log(Level::Info, "a", format_args!("a day two"));
    b.log(Level::Info, "b", format_args!("b day two"));
    assert_eq!(names(&dir), ["all.log", "all.log.2026-10-30"]);
    let old = read(&dir.join("all.log.2026-10-30"));
    assert!(
        old.contains("a day one") && old.contains("b day one"),
        "{old}"
    );
    let new = read(&path);
    assert!(
        new.contains("a day two") && new.contains("b day two"),
        "{new}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ---- retention --------------------------------------------------------------

#[test]
fn rolled_files_beyond_the_retention_are_deleted() {
    let dir = tmp_dir("retention");
    let path = dir.join("all.log");
    for name in [
        "all.log.2026-09-01",   // 60 days old at open: deleted
        "all.log.2026-10-23",   // 8 days: deleted
        "all.log.2026-10-24",   // 7 days: kept
        "all.log.2026-10-25.1", // a collision copy, 6 days: kept
        "all.log.bogus",        // not a rolled file: kept
        "other.log.2020-01-01", // another log: kept
    ] {
        fs::write(dir.join(name), "x\n").unwrap();
    }
    let clock = Arc::new(ManualClock::at_unix(at(2026, 10, 31, 10, 0, 0)));
    let log = file_logger(&path, Level::Info, 7, &clock);
    assert_eq!(
        names(&dir),
        [
            "all.log",
            "all.log.2026-10-24",
            "all.log.2026-10-25.1",
            "all.log.bogus",
            "other.log.2020-01-01"
        ]
    );
    // The next roll prunes again, against the new day.
    log.log(Level::Info, "t", format_args!("today"));
    clock.set_unix(at(2026, 11, 1, 0, 0, 1));
    log.log(Level::Info, "t", format_args!("tomorrow"));
    assert_eq!(
        names(&dir),
        [
            "all.log",
            "all.log.2026-10-25.1",
            "all.log.2026-10-31",
            "all.log.bogus",
            "other.log.2020-01-01"
        ]
    );
    drop(log);

    // `days = 0` keeps everything.
    fs::write(dir.join("all.log.2000-01-01"), "ancient\n").unwrap();
    let _keep = file_logger(&path, Level::Info, 0, &clock);
    assert!(dir.join("all.log.2000-01-01").exists());
    let _ = fs::remove_dir_all(&dir);
}

// ---- configuration ----------------------------------------------------------

#[test]
fn configuration_comes_from_the_properties_file_and_the_log_keys() {
    let dir = tmp_dir("config");
    let props = dir.join("slog.properties");
    fs::write(
        &props,
        format!(
            "log4cplus.rootLogger=DEBUG, FILE\n\
             log4cplus.appender.FILE=log4cplus::DailyRollingFileAppender\n\
             log4cplus.appender.FILE.File={}\n\
             log4cplus.appender.FILE.MaxBackupIndex=12\n\
             log4cplus.appender.FILE.Schedule=HOURLY\n",
            dir.join("logs/all.log").display()
        ),
    )
    .unwrap();
    let mut cfg = GStoreConfig::default();
    cfg.set("log_mode", props.to_str().unwrap()).unwrap();
    let (lc, warnings) = LogConfig::from_gstore_config(&cfg);
    assert_eq!(lc.level, Some(Level::Debug));
    assert_eq!(lc.stderr, None, "no console appender listed");
    let file = lc.file.clone().unwrap();
    assert_eq!(file.path, dir.join("logs/all.log"));
    // log4cplus' MaxBackupIndex limits one day's `.N` backups and deletes
    // no daily file: the rolled files are kept, as upstream keeps them.
    assert_eq!(file.days, 0);
    assert!(
        warnings.len() == 1 && warnings[0].contains("only DAILY"),
        "{warnings:?}"
    );
    // MaxHistory (log4cplus' age-based retention) sets one.
    let (lc, _) = LogConfig::from_properties(
        "log4cplus.rootLogger=INFO, FILE\n\
         log4cplus.appender.FILE=log4cplus::TimeBasedRollingFileAppender\n\
         log4cplus.appender.FILE.FilenamePattern=x.log\n\
         log4cplus.appender.FILE.MaxHistory=12\n",
    );
    assert_eq!(lc.file.unwrap().days, 12);

    // The extension keys override the properties file.
    for (k, v) in [
        ("log_level", "warn"),
        ("log_file", dir.join("diag.log").to_str().unwrap()),
        ("log_days", "3"),
        ("log_stderr", "on"),
    ] {
        cfg.set(k, v).unwrap();
    }
    let (lc, _) = LogConfig::from_gstore_config(&cfg);
    assert_eq!(lc.level, Some(Level::Warn));
    assert_eq!(lc.stderr, Some(Level::Trace));
    let file = lc.file.unwrap();
    assert_eq!((file.path, file.days), (dir.join("diag.log"), 3));
    cfg.set("log_file", "off").unwrap();
    cfg.set("log_level", "off").unwrap();
    let (lc, _) = LogConfig::from_gstore_config(&cfg);
    assert_eq!((lc.level, lc.file), (None, None));

    // No properties file: standard error at info (upstream's fallback), with
    // a diagnostic when log_mode was given explicitly.
    let (lc, warnings) = LogConfig::from_gstore_config(&GStoreConfig::default());
    assert_eq!(lc, LogConfig::default());
    assert_eq!(
        (lc.level, lc.stderr, lc.file),
        (Some(Level::Info), Some(Level::Trace), None)
    );
    let missing = GStoreConfig::from_ini_str("[log]\nlog_mode=./no/such/slog.properties\n");
    let (lc, warnings2) = LogConfig::from_gstore_config(&missing);
    assert_eq!(lc, LogConfig::default());
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(warnings2[0].contains("does not exist"), "{warnings2:?}");

    // An unsupported appender class or an undefined appender is a warning.
    let (lc, w) = LogConfig::from_properties(
        "log4cplus.rootLogger=ERROR, S, X, Y\n\
         log4cplus.appender.S=log4cplus::ConsoleAppender\n\
         log4cplus.appender.S.Threshold=WARN\n\
         log4cplus.appender.X=log4cplus::SocketAppender\n",
    );
    assert_eq!(
        (lc.level, lc.stderr, lc.file),
        (Some(Level::Error), Some(Level::Warn), None)
    );
    assert_eq!(w.len(), 2, "{w:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_log_file_that_cannot_be_opened_is_an_error() {
    let dir = tmp_dir("bad");
    let cfg = LogConfig {
        file: Some(FileSinkConfig {
            path: dir.clone(), // a directory
            days: 30,
            threshold: Level::Trace,
        }),
        ..LogConfig::default()
    };
    assert!(Logger::new(&cfg).is_err());
    let _ = fs::remove_dir_all(&dir);
}

// ---- the process-wide logger and its macros -----------------------------------

#[test]
fn the_macros_write_through_the_installed_logger() {
    let dir = tmp_dir("global");
    let path = dir.join("server.log");
    let cfg = LogConfig {
        level: Some(Level::Debug),
        stderr: None,
        file: Some(FileSinkConfig {
            path: path.clone(),
            days: 30,
            threshold: Level::Trace,
        }),
    };
    let installed = logging::init(&cfg).expect("install");
    assert_eq!(installed.file_path(), Some(path.as_path()));
    assert_eq!(logging::max_level(), Some(Level::Debug));
    assert!(logging::enabled(Level::Debug) && !logging::enabled(Level::Trace));
    let evaluated = std::cell::Cell::new(0);
    let count = || {
        evaluated.set(evaluated.get() + 1);
        evaluated.get()
    };
    gstore::gs_error!("an error {}", count());
    gstore::gs_warn!("a warning {}", count());
    gstore::gs_info!("lifecycle event {}", count());
    gstore::gs_debug!("details {}", count());
    gstore::gs_trace!("not formatted {}", count());
    gstore::gs_announce!("a banner line");
    gstore::gs_announce!("dt_logging: a prefixed banner line");
    assert_eq!(
        evaluated.get(),
        4,
        "a disabled level does not format its arguments"
    );
    // (only this test's records: servers other tests run concurrently log
    // through the same process-wide logger)
    let text = read(&path);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("[dt_logging]"))
        .collect();
    assert_eq!(lines.len(), 6, "{text}");
    assert!(
        lines[0].ends_with("[ERROR] [dt_logging] an error 1"),
        "{text}"
    );
    assert!(
        lines[1].ends_with("[ WARN] [dt_logging] a warning 2"),
        "{text}"
    );
    assert!(
        lines[2].ends_with("[ INFO] [dt_logging] lifecycle event 3"),
        "{text}"
    );
    assert!(
        lines[3].ends_with("[DEBUG] [dt_logging] details 4"),
        "{text}"
    );
    assert!(
        lines[4].ends_with("[ INFO] [dt_logging] a banner line"),
        "{text}"
    );
    // The console's `target: ` prefix is not repeated after the record's target.
    assert!(
        lines[5].ends_with("[ INFO] [dt_logging] a prefixed banner line"),
        "{text}"
    );
    // The timestamp is the real local time.
    let today = Date::of(&SystemClockProbe.local(SystemTime::now())).iso();
    assert!(lines[0].starts_with(&today), "{} vs {today}", lines[0]);

    // A log file that cannot be opened: the standard-error part is installed.
    let bad = LogConfig {
        level: Some(Level::Warn),
        stderr: Some(Level::Trace),
        file: Some(FileSinkConfig {
            path: dir.clone(),
            days: 1,
            threshold: Level::Trace,
        }),
    };
    assert!(logging::init(&bad).is_err());
    let now = logging::current().unwrap();
    assert_eq!(
        (now.file_path(), now.max_level()),
        (None, Some(Level::Warn))
    );

    logging::reset();
    assert!(logging::current().is_none());
    assert_eq!(logging::max_level(), Some(Level::Info));
    let _ = fs::remove_dir_all(&dir);
}

/// The system clock, through the trait (its zone is the local one).
struct SystemClockProbe;
impl Clock for SystemClockProbe {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

// ---- REST query and access records ----------------------------------------------

fn basic_root() -> String {
    // base64("root:root")
    "Basic cm9vdDpyb290".to_string()
}

fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        basic_root(),
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn start(root: &Path, cfg: Option<GStoreConfig>) -> (Arc<ApiServer>, SocketAddr) {
    let mut server = ApiServer::bind(root, "root", "127.0.0.1:0").unwrap();
    if let Some(c) = cfg {
        server = server.with_config(c);
    }
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

/// Every record of the JSON query log under `root` (default path).
fn query_records(root: &Path) -> Vec<Json> {
    let dir = root.join("logs").join("endpoint");
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            for line in read(&e.path()).lines() {
                out.push(json::parse(line).unwrap());
            }
        }
    }
    out
}

fn s<'a>(j: &'a Json, k: &str) -> &'a str {
    j.get(k).and_then(Json::as_str).unwrap_or_default()
}

fn n(j: &Json, k: &str) -> i64 {
    j.get(k).and_then(Json::as_i64).unwrap_or(-1)
}

const DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n:a :p :c .\n:a :name \"A\" .\n";

#[test]
fn rest_queries_are_in_the_json_query_log_with_ip_count_and_latency() {
    let root = tmp_dir("rest_querylog");
    let (_srv, addr) = start(&root, None);
    let (st, body) = request(&addr, "POST", "/build?db_name=lg", DATA);
    assert_eq!(st, 200, "{body}");

    let select = "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }";
    let (st, body) = request(&addr, "POST", "/sparql?db_name=lg", select);
    assert_eq!(st, 200, "{body}");
    let (st, _) = request(
        &addr,
        "POST",
        "/sparql?db_name=lg&format=csv",
        "ASK { <http://ex/a> ?p ?o }",
    );
    assert_eq!(st, 200);
    let update = "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> . <http://ex/x> <http://ex/p> <http://ex/z> }";
    let (st, body) = request(&addr, "POST", "/update?db_name=lg", update);
    assert_eq!(st, 200, "{body}");
    let (st, _) = request(&addr, "POST", "/sparql?db_name=lg", "SELECT nonsense");
    assert_eq!(st, 400);
    // A REST transaction query is recorded too.
    let (st, body) = request(&addr, "POST", "/begin?db_name=lg", "");
    assert_eq!(st, 200, "{body}");
    let tid = json::parse(&body)
        .unwrap()
        .get("TID")
        .and_then(|t| {
            t.as_str()
                .map(str::to_string)
                .or_else(|| t.as_i64().map(|i| i.to_string()))
        })
        .expect("TID");
    let (st, body) = request(
        &addr,
        "POST",
        &format!("/tquery?db_name=lg&tid={tid}"),
        "SELECT ?s WHERE { ?s <http://ex/name> ?n }",
    );
    assert_eq!(st, 200, "{body}");

    let recs = query_records(&root);
    assert_eq!(recs.len(), 5, "{recs:?}");
    let by_sparql = |q: &str| {
        recs.iter()
            .find(|r| s(r, "sparql") == q)
            .unwrap_or_else(|| panic!("{q}: {recs:?}"))
    };
    let sel = by_sparql(select);
    assert_eq!(s(sel, "remoteIP"), "127.0.0.1");
    assert_eq!(s(sel, "dbname"), "lg");
    assert_eq!(n(sel, "ansNum"), 2);
    assert_eq!(n(sel, "statusCode"), 0);
    assert_eq!(s(sel, "statusMsg"), "success");
    assert_eq!(s(sel, "format"), "json");
    assert!(n(sel, "queryTime") >= 0, "{sel}");
    assert_eq!(s(sel, "queryDateTime").len(), 23, "YYYY-MM-DD HH:MM:SS.mmm");
    let ask = by_sparql("ASK { <http://ex/a> ?p ?o }");
    assert_eq!((n(ask, "ansNum"), s(ask, "format")), (1, "csv"));
    let upd = by_sparql(update);
    assert_eq!(n(upd, "ansNum"), 2, "triples changed: {upd}");
    let bad = by_sparql("SELECT nonsense");
    assert_eq!((n(bad, "statusCode"), n(bad, "ansNum")), (400, 0));
    assert!(!s(bad, "statusMsg").is_empty());
    let txq = by_sparql("SELECT ?s WHERE { ?s <http://ex/name> ?n }");
    assert_eq!(n(txq, "ansNum"), 1);

    // The /api protocol lists the REST records with its own.
    let date = fs::read_dir(root.join("logs").join("endpoint"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".log")
        .to_string();
    let (st, body) = request(
        &addr,
        "POST",
        "/api",
        &format!(
            "{{\"operation\":\"querylog\",\"username\":\"root\",\"password\":\"root\",\
              \"date\":\"{date}\",\"pageNo\":1,\"pageSize\":100}}"
        ),
    );
    assert_eq!(st, 200);
    let j = json::parse(&body).unwrap();
    assert_eq!(n(&j, "totalSize"), 5, "{j}");

    // The REST access log ends every line with the client's IP.
    let access = read(&root.join("access.log"));
    let sparql_line = access
        .lines()
        .find(|l| l.contains("\t/sparql\t"))
        .expect("a /sparql access record");
    let fields: Vec<&str> = sparql_line.split('\t').collect();
    assert_eq!(fields.len(), 6, "{sparql_line}");
    assert_eq!(
        &fields[1..],
        ["POST", "/sparql", "root", "200", "127.0.0.1"]
    );
    assert!(
        access.lines().all(|l| l.ends_with("\t127.0.0.1")),
        "{access}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_rest_query_log_follows_querylog_mode_and_path() {
    let root = tmp_dir("rest_querylog_cfg");
    let mut cfg = GStoreConfig::default();
    cfg.querylog_mode = false;
    cfg.ip_deny_path = None;
    let (_srv, addr) = start(&root, Some(cfg));
    assert_eq!(request(&addr, "POST", "/build?db_name=q", DATA).0, 200);
    assert_eq!(
        request(&addr, "POST", "/sparql?db_name=q", "ASK { ?s ?p ?o }").0,
        200
    );
    assert!(
        !root.join("logs").join("endpoint").exists(),
        "query log off"
    );
    // The plain-text REST access log is unconditional.
    assert!(read(&root.join("access.log")).contains("\t/sparql\t"));

    let root2 = tmp_dir("rest_querylog_path");
    let mut cfg = GStoreConfig::default();
    cfg.querylog_path = PathBuf::from("custom/q");
    cfg.ip_deny_path = None;
    let (_srv2, addr2) = start(&root2, Some(cfg));
    assert_eq!(request(&addr2, "POST", "/build?db_name=q", DATA).0, 200);
    assert_eq!(
        request(&addr2, "POST", "/sparql?db_name=q", "ASK { ?s ?p ?o }").0,
        200
    );
    let dates = gstore::gapi::apilog::log_dates(&root2.join("custom").join("q"));
    assert_eq!(dates.len(), 1, "query log under the configured path");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&root2);
}
