//! `gserver` — the gStore HTTP server, with upstream gStore 1.4's lifecycle CLI.
//!
//! ```text
//! gserver [COMMAND] [OPTIONS] [db_dir]
//!
//! Commands (one at most; default -s):
//!   -s, --start        start in the foreground (refused when already running)
//!   -b, --background   start detached; return once the port answers
//!   -t, --stop         graceful stop (SIGTERM via the pid file), wait for exit
//!   -r, --restart      stop if running, then start again (in the background
//!                      when the running server was a background one)
//!   -k, --kill         force stop (SIGKILL)
//!   -S, --status       show whether the server runs, its pid and port
//!   -h, --help
//!
//! Options:
//!   -c, --config FILE  conf.ini to read (default: conf/conf.ini if it exists)
//!   -p, --port N       HTTP port (conf.ini `port`, default 9000)
//!   --addr HOST        bind address (conf.ini `bind_addr`, default 127.0.0.1)
//!   --db-home DIR      database home (conf.ini `db_home`, default ./dbhome/)
//!   --password PW      root's initial password (conf.ini `root_password`)
//!   --thread-num N     request worker threads (conf.ini `thread_num`, default 30)
//!   --no-checkpoint    turn conf.ini's `check_point` off
//!   --load-dir DIR     let SPARQL LOAD read files under DIR (repeatable)
//!   --tls-cert F --tls-key F   serve HTTPS (feature `tls`)
//!   --api [root]       multi-database mode rooted at `root` (legacy spelling)
//!   db_dir [--rocksdb] legacy single-database mode (see below)
//! ```
//!
//! Settings come from the command line, then `conf.ini`, then built-in
//! defaults (see [`gstore::config`] and `conf/conf.ini.example`).
//!
//! **Multi-database mode** (the default, as upstream 1.4): serve
//! [`gstore::http_api::ApiServer`] rooted at the database home — users/RBAC
//! (user `root`), `/login`, `/build`, `/load`, `/upload`, `/backup`,
//! transactions, `/shutdown`, `/restart`, the web console at `/`, …
//! `gserver --api <root>` is the legacy spelling with an explicit root (its
//! database home).
//!
//! **Pid file.** A multi-database server keeps `<db home>/gstore.pid`, as
//! upstream's `conf/gstore.pid`: line 1 the server's pid, line 2 the password
//! of the `system` account (generated at every start; `/shutdown` and
//! `/restart` accept it), line 3 `1` for a background start, then the port
//! actually bound and the supervisor's pid (`0` without one). It is created
//! atomically (mode 0600) and removed on exit. `-s`/`-b` refuse to start while
//! it names a live gserver; `-t`/`-k`/`-S`/`-r` find the server through it, so
//! they must see the same database home (same `-c`, `--db-home` or `--api`
//! root) as the server. A background server writes its output to
//! `<db home>/gserver.log`.
//!
//! **Supervisor** (`deamon=on` in conf.ini, upstream's spelling): the process
//! keeps the server in a child process and restarts it when it dies from a
//! signal or a panic (bounded exponential backoff, 1 s to 30 s; it gives up
//! after 10 quick failures in a row). A stop (`-t`, Ctrl-C) reaches the
//! supervisor, which forwards it once and does not restart.
//!
//! **Single-database mode** (backward compatible): `gserver <db_dir>` serves
//! that one database over the SPARQL-protocol subset — `GET/POST /sparql`,
//! `POST /update`, `GET /status` and a console at `/` — whatever its layout
//! (bincode snapshot, on-disk `kvstore.kv`, or RocksDB with
//! `--features rocksdb`; `--rocksdb` insists on the latter), persisting updates
//! back to it. A missing or empty directory starts empty; one that cannot be
//! opened is an error. It runs in the foreground only and keeps no pid file.
//!
//! `SIGINT`/`SIGTERM` (and `/shutdown`) stop gracefully: stop accepting, let
//! in-flight requests finish, roll back open transactions, save unsaved
//! changes, exit. A second Ctrl-C terminates immediately. `/restart` does the
//! same and then re-executes `gserver` with the same arguments.

use std::fs;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::server::{on_termination_signal, shutdown_on_signals, Server, ShutdownReport};
use gstore::{Database, LoadPolicy, StorageKind};

/// Set (to `1`) in the environment of the server process a `-b` start spawns.
const ENV_BACKGROUND: &str = "GSTORE_GSERVER_BACKGROUND";
/// Set (to the supervisor's pid) in the environment of a supervised server.
const ENV_SUPERVISOR: &str = "GSTORE_GSERVER_SUPERVISOR";
/// Upstream's documentation site, printed by `-S` like upstream.
const DOCS: &str = "https://www.gstore.cn";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmd {
    Start,
    Background,
    Stop,
    Restart,
    Kill,
    Status,
    Help,
}

/// The parsed command line.
#[derive(Debug, Default)]
struct Opts {
    cmd: Option<Cmd>,
    config: Option<PathBuf>,
    port: Option<u16>,
    addr: Option<String>,
    db_home: Option<PathBuf>,
    password: Option<String>,
    thread_num: Option<usize>,
    no_checkpoint: bool,
    load_dirs: Vec<String>,
    api: bool,
    rocksdb: bool,
    positional: Option<String>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

fn usage() -> String {
    let mut u = String::from(
        "gStore API Server (gserver)\n\n\
         Usage: gserver [COMMAND] [OPTIONS] [db_dir]\n\n\
         Commands:\n\
         \x20 -s, --start        start in the foreground (default)\n\
         \x20 -b, --background   start in the background\n\
         \x20 -t, --stop         stop the server gracefully\n\
         \x20 -r, --restart      restart the server\n\
         \x20 -k, --kill         force-stop the server\n\
         \x20 -S, --status       show the server's status\n\
         \x20 -h, --help         show this message\n\n\
         Options:\n\
         \x20 -c, --config FILE  conf.ini to read (default conf/conf.ini if present)\n\
         \x20 -p, --port N       HTTP port (default 9000)\n\
         \x20 --addr HOST        bind address (default 127.0.0.1)\n\
         \x20 --db-home DIR      database home (default ./dbhome/)\n\
         \x20 --password PW      root's initial password (default 123456)\n\
         \x20 --thread-num N     request worker threads (default 30)\n\
         \x20 --no-checkpoint    keep changes in memory until shutdown/checkpoint\n\
         \x20 --load-dir DIR     let SPARQL LOAD read files under DIR (repeatable)\n\
         \x20 --api [root]       multi-database mode rooted at `root`\n\
         \x20 db_dir [--rocksdb] serve one database (legacy single-database mode)\n",
    );
    if cfg!(feature = "tls") {
        u.push_str("  --tls-cert FILE --tls-key FILE   serve HTTPS\n");
    }
    u
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    let value = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while i < args.len() {
        let a = args[i].as_str();
        let cmd = match a {
            "-s" | "--start" => Some(Cmd::Start),
            "-b" | "--background" => Some(Cmd::Background),
            "-t" | "--stop" => Some(Cmd::Stop),
            "-r" | "--restart" => Some(Cmd::Restart),
            "-k" | "--kill" => Some(Cmd::Kill),
            "-S" | "--status" => Some(Cmd::Status),
            "-h" | "--help" => Some(Cmd::Help),
            _ => None,
        };
        if let Some(c) = cmd {
            if o.cmd.is_some_and(|prev| prev != c) {
                return Err(format!("more than one command given ({a})"));
            }
            o.cmd = Some(c);
            i += 1;
            continue;
        }
        match a {
            "-c" | "--config" => o.config = Some(PathBuf::from(value(&mut i, a)?)),
            "-p" | "--port" => {
                let v = value(&mut i, a)?;
                o.port = Some(v.parse().map_err(|_| format!("invalid port '{v}'"))?);
            }
            "--addr" => o.addr = Some(value(&mut i, a)?),
            "--db-home" => o.db_home = Some(PathBuf::from(value(&mut i, a)?)),
            "--password" => o.password = Some(value(&mut i, a)?),
            "--thread-num" => {
                let v = value(&mut i, a)?;
                match v.parse::<usize>() {
                    Ok(n) if n > 0 => o.thread_num = Some(n),
                    _ => return Err(format!("invalid --thread-num '{v}'")),
                }
            }
            "--no-checkpoint" => o.no_checkpoint = true,
            "--load-dir" => o.load_dirs.push(value(&mut i, a)?),
            "--api" => o.api = true,
            "--rocksdb" => o.rocksdb = true,
            "--tls-cert" => o.tls_cert = Some(value(&mut i, a)?),
            "--tls-key" => o.tls_key = Some(value(&mut i, a)?),
            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(format!("unknown option '{a}'"));
            }
            _ => {
                if o.positional.is_some() {
                    return Err(format!("unexpected argument '{a}'"));
                }
                o.positional = Some(a.to_string());
            }
        }
        i += 1;
    }
    if cfg!(not(feature = "tls")) && (o.tls_cert.is_some() || o.tls_key.is_some()) {
        eprintln!(
            "warning: --tls-cert/--tls-key ignored — gserver was built without the `tls` \
             feature (rebuild with `--features tls` to serve HTTPS)"
        );
        o.tls_cert = None;
        o.tls_key = None;
    }
    if o.tls_cert.is_some() != o.tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be supplied together to serve HTTPS".into());
    }
    Ok(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gserver: {e}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    let cmd = opts.cmd.unwrap_or(Cmd::Start);
    if cmd == Cmd::Help {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }
    let cfg = match load_config(&opts) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gserver: {e}");
            return ExitCode::from(2);
        }
    };
    // `gserver <db_dir>` (no --api): the legacy single-database server.
    if let (Some(dir), false) = (&opts.positional, opts.api) {
        if cmd != Cmd::Start {
            eprintln!(
                "gserver: -b/-t/-r/-k/-S manage the multi-database server (its pid file lives \
                 in the database home); run `gserver {dir}` in the foreground instead"
            );
            return ExitCode::from(2);
        }
        return run_single(&opts, &cfg, dir);
    }
    let code = match cmd {
        Cmd::Start => cmd_start(&opts, &cfg, &args),
        Cmd::Background => cmd_background(&cfg, &args),
        Cmd::Stop => cmd_stop(&cfg),
        Cmd::Restart => cmd_restart(&opts, &cfg, &args),
        Cmd::Kill => cmd_kill(&cfg),
        Cmd::Status => cmd_status(&cfg),
        Cmd::Help => 0,
    };
    ExitCode::from(code)
}

/// The effective configuration: conf.ini (explicit, or the default location)
/// over the built-in defaults, then the command-line flags.
fn load_config(o: &Opts) -> Result<GStoreConfig, String> {
    let mut cfg = GStoreConfig::load_or_default(o.config.as_deref()).map_err(|e| {
        format!(
            "cannot read config file '{}': {e}",
            o.config.as_deref().unwrap_or(Path::new("?")).display()
        )
    })?;
    if let Some(src) = &cfg.source {
        for w in &cfg.warnings {
            eprintln!("gserver: {}: {w}", src.display());
        }
    }
    if let Some(p) = o.port {
        cfg.port = p;
    }
    if let Some(a) = &o.addr {
        cfg.bind_addr = a.clone();
    }
    if let Some(h) = &o.db_home {
        cfg.db_home = h.clone();
    }
    if let (true, Some(root)) = (o.api, &o.positional) {
        cfg.db_home = PathBuf::from(root);
    }
    if let Some(p) = &o.password {
        cfg.root_password = p.clone();
    }
    if let Some(n) = o.thread_num {
        cfg.thread_num = n;
    }
    if o.no_checkpoint {
        cfg.check_point = false;
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// The pid file
// ---------------------------------------------------------------------------

/// The contents of `<db home>/gstore.pid`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PidInfo {
    pid: u32,
    system_password: String,
    background: bool,
    port: u16,
    /// The supervising process (`deamon=on`), or 0.
    supervisor: u32,
}

impl PidInfo {
    fn render(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n",
            self.pid, self.system_password, self.background as u8, self.port, self.supervisor
        )
    }

    fn parse(text: &str) -> Option<PidInfo> {
        let mut lines = text.lines();
        let pid = lines.next()?.trim().parse().ok()?;
        let system_password = lines.next()?.trim().to_string();
        let background = lines.next().map(|l| l.trim() == "1").unwrap_or(false);
        let port = lines
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        let supervisor = lines
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        Some(PidInfo {
            pid,
            system_password,
            background,
            port,
            supervisor,
        })
    }

    /// Whether this names a running gserver (the server, or its supervisor
    /// between two runs of the server) other than the calling process — and,
    /// for a supervised server, other than its own supervisor (whose previous
    /// server died and left this file behind).
    fn running(&self) -> bool {
        let me = std::process::id();
        let my_supervisor: u32 = std::env::var(ENV_SUPERVISOR)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        (self.pid != me && is_live_gserver(self.pid))
            || (self.supervisor != 0
                && self.supervisor != me
                && self.supervisor != my_supervisor
                && is_live_gserver(self.supervisor))
    }
}

fn read_pid_file(path: &Path) -> Option<PidInfo> {
    PidInfo::parse(&fs::read_to_string(path).ok()?)
}

/// The running server recorded in `cfg`'s pid file, if any.
fn running_server(cfg: &GStoreConfig) -> Option<PidInfo> {
    read_pid_file(&cfg.pid_file()).filter(PidInfo::running)
}

/// Create the pid file atomically (complete content or nothing, mode 0600):
/// write a private temporary file, then hard-link it into place, which fails
/// if a pid file exists. A stale one (naming no live gserver) is replaced.
fn create_pid_file(path: &Path, info: &PidInfo) -> io::Result<()> {
    let tmp = path.with_extension(format!("pid.{}.tmp", std::process::id()));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(info.render().as_bytes())?;
        f.sync_all()?;
    }
    let mut result = fs::hard_link(&tmp, path);
    if matches!(&result, Err(e) if e.kind() == io::ErrorKind::AlreadyExists) {
        match read_pid_file(path) {
            Some(other) if other.running() => {}
            _ => {
                let _ = fs::remove_file(path);
                result = fs::hard_link(&tmp, path);
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    result
}

/// Remove the pid file if it still describes server `pid`.
fn remove_pid_file_of(path: &Path, pid: u32) {
    if read_pid_file(path).is_some_and(|i| i.pid == pid) {
        let _ = fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Process control (unix)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod sys {
    use std::os::raw::c_int;

    pub const SIGKILL: c_int = 9;
    pub const SIGTERM: c_int = 15;
    const EPERM: i32 = 1;

    extern "C" {
        fn kill(pid: i32, sig: c_int) -> c_int;
        fn setsid() -> i32;
    }

    /// Send `sig` to `pid`; whether it was delivered.
    pub fn signal(pid: u32, sig: c_int) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(2) with a positive pid only signals that process.
        pid > 0 && unsafe { kill(pid, sig) } == 0
    }

    /// Whether process `pid` exists (signal 0 probes without signalling).
    pub fn alive(pid: u32) -> bool {
        let Ok(p) = i32::try_from(pid) else {
            return false;
        };
        if p <= 0 {
            return false;
        }
        // SAFETY: signal 0 performs only the existence/permission check.
        if unsafe { kill(p, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }

    /// Start a new session (detach from the controlling terminal). Only
    /// async-signal-safe work, as required between fork and exec.
    pub fn new_session() -> std::io::Result<()> {
        // SAFETY: setsid has no memory-safety preconditions.
        if unsafe { setsid() } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Whether `pid` is a live process that is (as far as can be told) a gserver:
/// on Linux its command line must mention `gserver`, which guards against a
/// stale pid file whose pid was reused by another program.
fn is_live_gserver(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if !sys::alive(pid) {
            return false;
        }
        match fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(cmdline) => String::from_utf8_lossy(&cmdline).contains("gserver"),
            Err(_) => true, // no procfs: trust the pid
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Wait until neither `pids` is alive, at most `timeout`.
fn wait_gone(pids: &[u32], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pids.iter().any(|&p| p != 0 && is_live_gserver(p)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A connectable address for the configured bind address and `port`.
fn connect_addr(cfg: &GStoreConfig, port: u16) -> Option<SocketAddr> {
    let host = match cfg.bind_addr.as_str() {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" | "[::]" => "::1",
        h => h,
    };
    (host, port).to_socket_addrs().ok()?.next()
}

fn port_answers(cfg: &GStoreConfig, port: u16) -> bool {
    port != 0
        && connect_addr(cfg, port)
            .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `-s`: start in the foreground (under a supervisor when `deamon=on`).
fn cmd_start(opts: &Opts, cfg: &GStoreConfig, args: &[String]) -> u8 {
    let supervised = std::env::var(ENV_SUPERVISOR).is_ok();
    if !supervised {
        if let Some(info) = running_server(cfg) {
            println!(
                "the server already running. (pid {}, port {})",
                info.pid, info.port
            );
            return 1;
        }
        if cfg.deamon && cfg!(unix) {
            return supervise(cfg, args);
        }
    }
    run_multi(opts, cfg)
}

/// `-b`: re-run this program detached (new session, output to the log file)
/// and return once its port answers.
fn cmd_background(cfg: &GStoreConfig, args: &[String]) -> u8 {
    if !cfg!(unix) {
        eprintln!("gserver: -b/--background is only supported on unix");
        return 2;
    }
    if let Some(info) = running_server(cfg) {
        println!(
            "the server already running. (pid {}, port {})",
            info.pid, info.port
        );
        return 1;
    }
    if let Err(e) = fs::create_dir_all(&cfg.db_home) {
        eprintln!("gserver: cannot create '{}': {e}", cfg.db_home.display());
        return 1;
    }
    let log_path = cfg.server_log_file();
    let log = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gserver: cannot open log '{}': {e}", log_path.display());
            return 1;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gserver: cannot locate the gserver executable: {e}");
            return 1;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.args(with_command(args, "-s"))
        .env(ENV_BACKGROUND, "1")
        .env_remove(ENV_SUPERVISOR)
        .stdin(std::process::Stdio::null())
        .stdout(
            log.try_clone()
                .map(Into::into)
                .unwrap_or(std::process::Stdio::null()),
        )
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure only calls setsid(2), which is async-signal-safe.
        unsafe {
            cmd.pre_exec(sys::new_session);
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gserver: failed to start the background server: {e}");
            return 1;
        }
    };
    let child_pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            eprintln!(
                "gserver: the background server exited during start-up ({status}); see '{}':",
                log_path.display()
            );
            eprint!("{}", log_tail(&log_path, 20));
            return 1;
        }
        if let Some(info) = read_pid_file(&cfg.pid_file()) {
            if (info.pid == child_pid || info.supervisor == child_pid)
                && port_answers(cfg, info.port)
            {
                println!(
                    "gserver started in the background (pid {}): http://{}  (log: {})",
                    info.pid,
                    connect_addr(cfg, info.port)
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| format!("{}:{}", cfg.bind_addr, info.port)),
                    log_path.display()
                );
                return 0;
            }
        }
        if Instant::now() >= deadline {
            eprintln!(
                "gserver: the background server (pid {child_pid}) did not answer within 60 s; \
                 see '{}'",
                log_path.display()
            );
            return 1;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// `-t`: graceful stop through the pid file.
fn cmd_stop(cfg: &GStoreConfig) -> u8 {
    let Some(info) = running_server(cfg) else {
        let _ = fs::remove_file(cfg.pid_file()); // stale, if any
        println!("server is inactive (dead).");
        return 0;
    };
    #[cfg(unix)]
    {
        // Stop the supervisor (it forwards the stop once and does not
        // restart), else the server itself.
        let target = if info.supervisor != 0 && is_live_gserver(info.supervisor) {
            info.supervisor
        } else {
            info.pid
        };
        if !sys::signal(target, sys::SIGTERM) {
            eprintln!(
                "gserver: cannot signal process {target}: {}",
                io::Error::last_os_error()
            );
            return 1;
        }
        let grace = Duration::from_secs(cfg.shutdown_grace.saturating_add(60));
        if !wait_gone(&[info.pid, info.supervisor], grace) {
            eprintln!(
                "gserver: the server [{}] did not stop within {} s; use `gserver -k` to force it",
                info.pid,
                grace.as_secs()
            );
            return 1;
        }
        remove_pid_file_of(&cfg.pid_file(), info.pid);
        println!("the Server [{}] stop successfully.", info.pid);
        0
    }
    #[cfg(not(unix))]
    {
        let _ = info;
        eprintln!("gserver: -t/--stop is only supported on unix (POST /shutdown instead)");
        2
    }
}

/// `-k`: SIGKILL the supervisor (first, so it cannot restart) and the server.
fn cmd_kill(cfg: &GStoreConfig) -> u8 {
    let Some(info) = running_server(cfg) else {
        let _ = fs::remove_file(cfg.pid_file());
        println!("server is inactive (dead).");
        return 0;
    };
    #[cfg(unix)]
    {
        println!("The service will be forcibly stopped!");
        for pid in [info.supervisor, info.pid] {
            if pid != 0 && is_live_gserver(pid) {
                sys::signal(pid, sys::SIGKILL);
            }
        }
        if !wait_gone(&[info.pid, info.supervisor], Duration::from_secs(10)) {
            eprintln!("gserver: the server [{}] is still alive", info.pid);
            return 1;
        }
        let _ = fs::remove_file(cfg.pid_file());
        0
    }
    #[cfg(not(unix))]
    {
        let _ = info;
        eprintln!("gserver: -k/--kill is only supported on unix");
        2
    }
}

/// `-S`: upstream's status report.
fn cmd_status(cfg: &GStoreConfig) -> u8 {
    println!("gStore API Server(gserver)");
    match running_server(cfg) {
        Some(info) => {
            println!("\tActive: active (running)");
            println!("\tMain PID: {}", info.pid);
            println!("\tPort: {}", info.port);
            if info.supervisor != 0 {
                println!("\tSupervisor PID: {}", info.supervisor);
            }
            println!(
                "\tMode: {}",
                if info.background {
                    "background"
                } else {
                    "foreground"
                }
            );
            println!("\tPid file: {}", cfg.pid_file().display());
        }
        None => {
            println!("\tActive: inactive (dead)");
            println!("\tProcess: gserver");
        }
    }
    println!("\tDocs: {DOCS}");
    0
}

/// `-r`: stop if running, then start like the stopped server (background or
/// foreground; foreground when nothing was running, as upstream).
fn cmd_restart(opts: &Opts, cfg: &GStoreConfig, args: &[String]) -> u8 {
    let mut background = false;
    if let Some(info) = running_server(cfg) {
        background = info.background;
        println!("server is active (running).");
        println!("stop server...");
        if cmd_stop(cfg) != 0 {
            return 1;
        }
    }
    println!("start server...");
    if background {
        cmd_background(cfg, args)
    } else {
        cmd_start(opts, cfg, &with_command(args, "-s"))
    }
}

/// `args` with every command flag replaced by one `cmd`.
fn with_command(args: &[String], cmd: &str) -> Vec<String> {
    let mut out = vec![cmd.to_string()];
    out.extend(
        args.iter()
            .filter(|a| {
                !matches!(
                    a.as_str(),
                    "-s" | "--start"
                        | "-b"
                        | "--background"
                        | "-t"
                        | "--stop"
                        | "-r"
                        | "--restart"
                        | "-k"
                        | "--kill"
                        | "-S"
                        | "--status"
                )
            })
            .cloned(),
    );
    out
}

/// The last `n` lines of `path`.
fn log_tail(path: &Path, n: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[lines.len().saturating_sub(n)..].join("\n");
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// The supervisor (`deamon=on`)
// ---------------------------------------------------------------------------

/// Keep the server running in a child process: restart it when it dies from a
/// signal or a panic (exit code 101), with exponential backoff (1 s → 30 s);
/// stop restarting after 10 consecutive runs shorter than 10 s. A termination
/// signal is forwarded to the child once and ends the supervision.
fn supervise(cfg: &GStoreConfig, args: &[String]) -> u8 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gserver: cannot locate the gserver executable: {e}");
            return 1;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(AtomicU32::new(0));
    {
        let (stop, child_pid) = (Arc::clone(&stop), Arc::clone(&child_pid));
        on_termination_signal(move || {
            stop.store(true, Ordering::SeqCst);
            #[cfg(unix)]
            sys::signal(child_pid.load(Ordering::SeqCst), sys::SIGTERM);
            #[cfg(not(unix))]
            let _ = child_pid;
        });
    }
    let me = std::process::id().to_string();
    let child_args = with_command(args, "-s");
    let mut backoff = Duration::from_secs(1);
    let mut quick_failures = 0;
    println!("gserver: supervisor (pid {me}) started (deamon=on)");
    loop {
        let started = Instant::now();
        let mut cmd = Command::new(&exe);
        cmd.args(&child_args).env(ENV_SUPERVISOR, &me);
        #[cfg(unix)]
        {
            // Its own process group: a terminal Ctrl-C reaches only the
            // supervisor, which forwards exactly one SIGTERM.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gserver: supervisor cannot start the server: {e}");
                return 1;
            }
        };
        child_pid.store(child.id(), Ordering::SeqCst);
        if stop.load(Ordering::SeqCst) {
            #[cfg(unix)]
            sys::signal(child.id(), sys::SIGTERM);
        }
        let status = match child.wait() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gserver: supervisor lost the server process: {e}");
                return 1;
            }
        };
        let crashed = status.code().is_none_or(|c| c == 101);
        if stop.load(Ordering::SeqCst) || !crashed {
            remove_pid_file_of(&cfg.pid_file(), child.id());
            println!("gserver: server stopped ({status}); supervisor exits");
            return status.code().map(|c| c.clamp(0, 255) as u8).unwrap_or(0);
        }
        if started.elapsed() > Duration::from_secs(10) {
            quick_failures = 0;
            backoff = Duration::from_secs(1);
        } else {
            quick_failures += 1;
            if quick_failures >= 10 {
                eprintln!(
                    "gserver: server failed {quick_failures} times in a row; supervisor gives up"
                );
                remove_pid_file_of(&cfg.pid_file(), child.id());
                return 1;
            }
        }
        eprintln!("gserver: server stopped abnormally ({status}); restarting in {backoff:?}");
        let resume = Instant::now() + backoff;
        while Instant::now() < resume && !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
        }
        if stop.load(Ordering::SeqCst) {
            remove_pid_file_of(&cfg.pid_file(), child.id());
            return 0;
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// A random hex token for the `system` account (upstream: a random number).
fn random_token() -> String {
    let mut bytes = [0u8; 16];
    let from_os = fs::File::open("/dev/urandom")
        .and_then(|mut f| io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !from_os {
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            h.write_u32(std::process::id());
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Serve the multi-database API rooted at the database home, keeping the pid
/// file for the lifetime of the server.
fn run_multi(opts: &Opts, cfg: &GStoreConfig) -> u8 {
    let root = cfg.db_home.clone();
    let host = cfg.bind_addr.clone();
    let load_policy = match load_policy_for(&opts.load_dirs) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let tls = match load_tls(opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gserver: {e}");
            return 1;
        }
    };
    if cfg.root_password == "123456" {
        eprintln!(
            "warning: root is using the default password '123456'; set root_password in \
             conf.ini (or pass --password) before exposing this server"
        );
    }
    let system_password = random_token();
    let server = match ApiServer::bind(&root, &cfg.root_password, (host.as_str(), cfg.port)) {
        Ok(s) => s
            .with_config(cfg.clone())
            .with_load_policy(load_policy)
            .with_system_password(&system_password),
        Err(e) => {
            eprintln!("failed to start API server at {host}:{}: {e}", cfg.port);
            return 1;
        }
    };
    let addr = server.local_addr().ok();
    let me = std::process::id();
    let info = PidInfo {
        pid: me,
        system_password,
        background: std::env::var(ENV_BACKGROUND).is_ok_and(|v| v == "1"),
        port: addr.map(|a| a.port()).unwrap_or(cfg.port),
        supervisor: std::env::var(ENV_SUPERVISOR)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    let pid_path = cfg.pid_file();
    if let Err(e) = create_pid_file(&pid_path, &info) {
        if e.kind() == io::ErrorKind::AlreadyExists {
            println!(
                "the server already running. (pid file '{}')",
                pid_path.display()
            );
        } else {
            eprintln!(
                "gserver: cannot write pid file '{}': {e}",
                pid_path.display()
            );
        }
        return 1;
    }
    if cfg.check_point {
        println!(
            "gserver: databases are stored under '{}' (check_point on: every write is saved)",
            root.display()
        );
    } else {
        println!(
            "gserver: databases are stored under '{}' (check_point off: changes are saved on \
             /checkpoint, /unload and shutdown)",
            root.display()
        );
    }
    println!(
        "gserver: pid {me}, pid file '{}', {} worker threads",
        pid_path.display(),
        cfg.thread_num
    );
    if shutdown_on_signals(server.shutdown_handle()) {
        println!("gserver: Ctrl-C / SIGTERM shuts down gracefully (saving unsaved changes)");
    }
    let report = serve_api(&server, tls, &root, &host, cfg.port);
    drop(server);
    remove_pid_file_of(&pid_path, me);
    let ok = print_report(&report);
    if report.restart {
        println!("gserver: restarting");
        let _ = io::stdout().flush();
        return reexec();
    }
    if ok {
        0
    } else {
        1
    }
}

/// The TLS configuration from `--tls-cert`/`--tls-key` (feature `tls`).
#[cfg(feature = "tls")]
type Tls = Option<Arc<rustls::ServerConfig>>;
#[cfg(not(feature = "tls"))]
type Tls = Option<()>;

fn load_tls(opts: &Opts) -> Result<Tls, String> {
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (opts.tls_cert.as_deref(), opts.tls_key.as_deref()) {
        return gstore::server::load_tls_config(cert, key)
            .map(Some)
            .map_err(|e| format!("failed to load TLS cert/key: {e}"));
    }
    let _ = opts;
    Ok(None)
}

/// Serve `server` (over TLS when configured) until it stops.
fn serve_api(server: &ApiServer, tls: Tls, root: &Path, host: &str, port: u16) -> ShutdownReport {
    let root = root.display();
    #[cfg(feature = "tls")]
    if let Some(cfg) = tls {
        match server.local_addr() {
            Ok(a) => println!(
                "gserver (API mode, TLS) on https://{a}  root={root}  user=root  \
                 (web console at /, POST /login /sparql /update /upload …; see /status)"
            ),
            Err(_) => println!("gserver (API mode, TLS) on {host}:{port}  root={root}"),
        }
        let _ = io::stdout().flush();
        return server.serve_forever_tls(cfg);
    }
    #[cfg(not(feature = "tls"))]
    let _ = tls;
    match server.local_addr() {
        Ok(a) => println!(
            "gserver (API mode) on http://{a}  root={root}  user=root  \
             (web console at /, POST /login /sparql /update /upload …; see /status)"
        ),
        Err(_) => println!("gserver (API mode) on {host}:{port}  root={root}"),
    }
    let _ = io::stdout().flush();
    server.serve_forever()
}

/// Replace this process with a fresh `gserver` run of the same arguments
/// (`/restart`). The pid stays the same on unix, so a supervisor keeps it.
fn reexec() -> u8 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gserver: cannot restart: {e}");
            return 1;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.args(with_command(&args, "-s"));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("gserver: cannot restart: {err}");
        1
    }
    #[cfg(not(unix))]
    {
        match cmd.spawn() {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("gserver: cannot restart: {e}");
                1
            }
        }
    }
}

/// The legacy single-database server: `gserver <db_dir>`.
fn run_single(opts: &Opts, cfg: &GStoreConfig, dir: &str) -> ExitCode {
    let load_policy = match load_policy_for(&opts.load_dirs) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let db = match open_db(dir, opts.rocksdb) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("gserver: {e}");
            return ExitCode::from(1);
        }
    };
    let host = cfg.bind_addr.as_str();
    let server = match Server::bind(db, (host, cfg.port)) {
        Ok(s) => s
            .with_config(cfg)
            .with_load_policy(load_policy)
            .with_db_dir(dir),
        Err(e) => {
            eprintln!("failed to bind {host}:{}: {e}", cfg.port);
            return ExitCode::from(1);
        }
    };
    if cfg.check_point {
        println!(
            "gserver: database directory '{dir}' (check_point on: every update is saved there)"
        );
    } else {
        println!("gserver: database directory '{dir}' (check_point off: changes are saved there on shutdown)");
    }
    if shutdown_on_signals(server.shutdown_handle()) {
        println!("gserver: Ctrl-C / SIGTERM shuts down gracefully (saving unsaved changes)");
    }
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (opts.tls_cert.as_deref(), opts.tls_key.as_deref()) {
        match gstore::server::load_tls_config(cert, key) {
            Ok(tls) => {
                match server.local_addr() {
                    Ok(a) => println!("gserver listening on https://{a}  (TLS; web console at /, POST /sparql, /update; GET /status)"),
                    Err(_) => println!("gserver (TLS) listening on {host}:{}", cfg.port),
                }
                let ok = print_report(&server.serve_forever_tls(tls));
                return if ok {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                };
            }
            Err(e) => {
                eprintln!("failed to load TLS cert/key: {e}");
                return ExitCode::from(1);
            }
        }
    }
    match server.local_addr() {
        Ok(a) => println!("gserver listening on http://{a}  (web console at /, POST /sparql, /update; GET /status)"),
        Err(_) => println!("gserver listening on {host}:{}", cfg.port),
    }
    let _ = io::stdout().flush();
    if print_report(&server.serve_forever()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The name for a fresh database at `dir`: its last path component without
/// gStore's `.db` suffix.
fn db_name_for(dir: &str) -> String {
    let base = Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = base.strip_suffix(gstore::DB_SUFFIX).unwrap_or(&base);
    if name.is_empty() {
        "gserver".to_string()
    } else {
        name.to_string()
    }
}

/// Print what the graceful shutdown did; `false` if a save failed.
fn print_report(report: &ShutdownReport) -> bool {
    if !report.drained {
        eprintln!("gserver: some requests were still running when the shutdown grace period ended");
    }
    if report.rolled_back > 0 {
        println!(
            "gserver: rolled back {} open transaction(s)",
            report.rolled_back
        );
    }
    for dir in &report.saved {
        println!("gserver: saved database to '{}'", dir.display());
    }
    for (dir, e) in &report.failed {
        eprintln!(
            "gserver: FAILED to save database to '{}': {e}",
            dir.display()
        );
    }
    println!("gserver: shut down");
    report.failed.is_empty()
}

/// The `LOAD` policy for the served databases: nothing may be loaded, except
/// files inside the `--load-dir` directories.
fn load_policy_for(dirs: &[String]) -> Result<LoadPolicy, String> {
    let mut policy = LoadPolicy::restricted();
    for d in dirs {
        if !Path::new(d).is_dir() {
            return Err(format!("--load-dir '{d}' is not a directory"));
        }
        policy = policy.with_allowed_dir(d);
        println!("gserver: SPARQL LOAD may read files under '{d}'");
    }
    Ok(policy)
}

/// Open the single database at `dir`, whatever its layout. A directory that does
/// not exist yet (or is empty) gives a fresh database that is saved there on the
/// first update. Anything else that cannot be opened is fatal: serving an empty
/// stand-in could later be saved over the real database.
fn open_db(dir: &str, expect_rocksdb: bool) -> Result<Database, String> {
    let path = Path::new(dir);
    let is_empty_dir =
        path.is_dir() && fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
    if !path.exists() || is_empty_dir {
        if expect_rocksdb {
            return Err(format!("--rocksdb given but '{dir}' holds no database"));
        }
        eprintln!("'{dir}' holds no database yet; serving a fresh empty database that will be saved there");
        return Ok(Database::new(db_name_for(dir)));
    }
    if expect_rocksdb && Database::storage_kind(path) != Some(StorageKind::RocksDb) {
        return Err(format!(
            "--rocksdb given but '{dir}' is not a RocksDB-backed database"
        ));
    }
    match Database::open(path) {
        Ok(db) => {
            println!("gserver: opened '{dir}' ({} layout)", db.storage());
            Ok(db)
        }
        Err(e) => Err(format!("could not open database '{dir}': {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_commands_and_options() {
        let o = parse_args(&args("-b -c conf.ini -p 9100 --db-home /h --thread-num 4")).unwrap();
        assert_eq!(o.cmd, Some(Cmd::Background));
        assert_eq!(o.config, Some(PathBuf::from("conf.ini")));
        assert_eq!(o.port, Some(9100));
        assert_eq!(o.db_home, Some(PathBuf::from("/h")));
        assert_eq!(o.thread_num, Some(4));
        let o = parse_args(&args("x.db --port 0 --no-checkpoint")).unwrap();
        assert_eq!(o.cmd, None);
        assert_eq!(o.positional.as_deref(), Some("x.db"));
        assert!(o.no_checkpoint);
        assert!(parse_args(&args("-t -k")).is_err(), "two commands");
        assert!(
            parse_args(&args("-t -t")).is_ok(),
            "a repeated command is fine"
        );
        assert!(parse_args(&args("--port")).is_err());
        assert!(parse_args(&args("--bogus")).is_err());
        assert!(parse_args(&args("a b")).is_err());
        assert!(parse_args(&args("--thread-num 0")).is_err());
    }

    #[test]
    fn with_command_replaces_the_command() {
        let a = args("-b --port 0 -c x.ini");
        assert_eq!(with_command(&a, "-s"), args("-s --port 0 -c x.ini"));
        assert_eq!(with_command(&args("--port 1"), "-s"), args("-s --port 1"));
    }

    #[test]
    fn pid_file_round_trips_and_keeps_upstream_line_order() {
        let info = PidInfo {
            pid: 42,
            system_password: "abc".into(),
            background: true,
            port: 9000,
            supervisor: 7,
        };
        let text = info.render();
        assert!(
            text.starts_with("42\nabc\n1\n"),
            "upstream's first three lines: {text}"
        );
        assert_eq!(PidInfo::parse(&text), Some(info));
        // Upstream's own (3-line) file parses too.
        let up = PidInfo::parse("1234\n5678\n0\n").unwrap();
        assert_eq!(
            (up.pid, up.port, up.supervisor, up.background),
            (1234, 0, 0, false)
        );
        assert_eq!(PidInfo::parse("garbage"), None);
    }

    #[test]
    fn precedence_cli_over_conf_over_default() {
        let dir = std::env::temp_dir().join(format!("gserver_cfg_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("conf.ini");
        fs::write(
            &ini,
            "[server]\nport=9555\nthread_num=7\n[system]\ndb_home=/conf/home\n",
        )
        .unwrap();
        let ini_s = ini.to_string_lossy().into_owned();
        let cfg = load_config(&parse_args(&args(&format!("-c {ini_s}"))).unwrap()).unwrap();
        assert_eq!((cfg.port, cfg.thread_num), (9555, 7));
        assert_eq!(cfg.db_home, PathBuf::from("/conf/home"));
        assert_eq!(cfg.max_output_size, 100_000, "default");
        let cfg = load_config(
            &parse_args(&args(&format!(
                "-c {ini_s} -p 9600 --api /cli/root --no-checkpoint"
            )))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cfg.port, 9600);
        assert_eq!(
            cfg.db_home,
            PathBuf::from("/cli/root"),
            "--api root is the db home"
        );
        assert!(!cfg.check_point);
        assert!(load_config(&parse_args(&args("-c /no/such/conf.ini")).unwrap()).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn random_tokens_differ() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
