//! `gserver` — gStore's HTTP server, with upstream gStore 1.4's lifecycle
//! commands (`bin/gserver -s|-b|-t|-r|-k|-S`, see upstream `src/Main/grpc.cpp`).
//!
//! ```text
//! gserver [MODE] [OPTIONS] [DIR]
//! ```
//!
//! **Modes** (one at most; upstream's flags):
//!
//! - `-s`, `--start` *(default)*: serve in the foreground. Refuses ("the server
//!   already running.", exit 1) when the pid file names a live gserver.
//! - `-b`, `--background`: start detached — the server is re-executed as a new
//!   session leader (`setsid`) with stdin closed and stdout/stderr appended to
//!   the log file; this command returns 0 once the server answers on its port
//!   (non-zero, with the log's tail, if it fails to start).
//! - `-t`, `--stop`: stop the running server gracefully (`SIGTERM` to the pid in
//!   the pid file: in-flight requests finish, open transactions roll back,
//!   unsaved changes are saved), wait for it to exit, remove the pid file.
//! - `-r`, `--restart`: stop the running server (if any), then start it again —
//!   in the background if it had been started with `-b`, as upstream does.
//! - `-k`, `--kill`: force-stop (`SIGKILL`) the server and its supervisor, remove
//!   the pid file. Unsaved changes are lost.
//! - `-S`, `--status`: report whether the server is running (pid, address).
//! - `-h`, `--help`.
//!
//! **What is served:**
//!
//! - no `DIR` *(default)*: the multi-database API ([`ApiServer`]) rooted at
//!   `conf.ini`'s `db_home` (default `./dbhome/`) — gStore 1.4's gserver is
//!   multi-database. Users/RBAC (user `root`, password `root_password`,
//!   default `123456`), `/login`, `/build`, `/load`, `/upload`, transactions,
//!   `/shutdown`, `/restart`, … and the web console at `/`.
//! - `--api DIR`: the same, rooted at `DIR` (the legacy spelling; `--api`
//!   without a `DIR` roots at `.` unless `conf.ini` sets `db_home`).
//! - `DIR` without `--api` (legacy): serve the single database in `DIR` over the
//!   SPARQL-protocol subset — `GET/POST /sparql`, `POST /update`, `GET /status`,
//!   the console at `/`. `DIR` is opened whatever its layout (bincode snapshot,
//!   `gbuild --disk` `kvstore.kv`, or RocksDB with `--features rocksdb`) and
//!   updates are persisted back to it. A `DIR` that does not exist yet (or is an
//!   empty directory) starts empty; one holding a database that cannot be opened
//!   is an error. `--rocksdb` insists `DIR` holds a RocksDB database.
//!
//! **Options** (precedence: flag > `conf.ini` > built-in default):
//!
//! - `-c`, `--config FILE`: the `conf.ini` to read (default: `./conf/conf.ini`
//!   when it exists, else built-in defaults; see `conf/conf.ini.example` and
//!   [`gstore::config`]). Unknown keys only warn.
//! - `-p`, `--port N`: port (`conf.ini` `port`, default 9000; `0` picks a free
//!   one).
//! - `--addr HOST`: bind address (`listen_addr`, default 127.0.0.1).
//! - `--password PW`: root's password when the user store is first created
//!   (`root_password`).
//! - `--no-checkpoint`: turn off `check_point` (save every write before
//!   acknowledging it); changes are then saved on graceful shutdown (and in API
//!   mode on `/checkpoint` and `/unload`).
//! - `--load-dir DIR` (repeatable): let SPARQL `LOAD` (and, in API mode,
//!   `/build?db_path=` and `/restore`) read files inside `DIR`.
//! - `--allow-service`: let federated SPARQL `SERVICE` patterns contact remote
//!   endpoints (off by default: a client could otherwise make the server send
//!   requests to internal hosts).
//! - `--pid-file FILE`, `--log-file FILE`: override the derived locations.
//! - `--tls-cert FILE --tls-key FILE` (feature `tls`): serve HTTPS.
//!
//! **Pid file.** `<db_home>/gserver.pid` (API mode: the root directory), or
//! `<DIR>.pid` next to a single database; the `-b` log is the same path with a
//! `.log` extension (`<db_home>/gserver.log`). The lifecycle commands find the
//! server through it, so give them the same `-c`/`DIR`/`--api` arguments as the
//! start. Format (the first three lines are upstream's `conf/gstore.pid`):
//!
//! ```text
//! <server pid>
//! <system password for /shutdown and /restart (API mode)>
//! <1 if started with -b, else 0>
//! <listening address host:port>
//! <supervisor pid, or 0>
//! ```
//!
//! **`deamon=on`** (`conf.ini`, upstream's spelling): a supervisor process runs
//! the server as its child and restarts it, with a bounded exponential backoff
//! (1 s … 30 s), when it dies abnormally (killed by a signal, or a panic); a
//! normal exit — including a graceful stop — ends the supervisor too.
//!
//! **Signals.** `SIGINT` (Ctrl-C) and `SIGTERM` trigger a graceful shutdown; a
//! second Ctrl-C terminates immediately. `POST /restart` (root or system
//! credentials) shuts down gracefully and re-executes the server in place (same
//! pid).

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::server::{shutdown_on_signals, Server, ShutdownHandle, ShutdownReport};
use gstore::{Database, LoadPolicy, StorageKind};

/// Set in the environment of a server started by `-b` (recorded in the pid
/// file, so `-r` restarts it in the background).
const ENV_BACKGROUND: &str = "GSTORE_GSERVER_BACKGROUND";
/// Set by the `deamon=on` supervisor in its server child's environment: the
/// supervisor's pid.
const ENV_SUPERVISOR: &str = "GSTORE_GSERVER_SUPERVISOR";

const DOCS: &str = "https://www.gstore.cn";

/// How long `-t` waits for the server to exit after `SIGTERM` (it lets
/// in-flight requests finish for up to 10 s, then saves).
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `-b` waits for the detached server to answer.
const START_TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let cli = match Cli::parse(std::env::args_os().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("gserver: {msg}");
            eprintln!("Invalid arguments! Input \"gserver -h\" for help.");
            return ExitCode::from(1);
        }
    };
    if cli.mode() == Mode::Help {
        print_help();
        return ExitCode::SUCCESS;
    }
    let cfg = load_config(&cli);
    let target = Target::resolve(&cli, &cfg);
    let code = match cli.mode() {
        Mode::Start => start_foreground(&cli, &cfg, &target),
        Mode::Background => start_background(&cli, &target),
        Mode::Stop => stop(&target),
        Mode::Restart => restart(&cli, &cfg, &target),
        Mode::Kill => kill(&target),
        Mode::Status => status(&target),
        Mode::Help => 0,
    };
    ExitCode::from(code)
}

fn print_help() {
    println!(
        "\ngStore API Server(gserver)\n\n\
         Usage:\tgserver [option] [DIR]\n\n\
         Options:\n\
         \t-h,--help\t\tDisplay this message.\n\
         \t-s,--start[default]\tStart gServer.\n\
         \t-t,--stop\t\tSafe shutdown gServer.\n\
         \t-r,--restart\t\tRestart gServer.\n\
         \t-k,--kill\t\tForce shutdown gServer.\n\
         \t-b,--background\t\tStart gServer in the background.\n\
         \t-S,--status\t\tShow gServer status.\n\
         \t-c,--config FILE\tconf.ini to use (default ./conf/conf.ini if present).\n\
         \t-p,--port N\t\tPort (default: conf.ini port, 9000).\n\
         \t--addr HOST\t\tBind address (default: conf.ini listen_addr, 127.0.0.1).\n\
         \t--password PW\t\tInitial root password (default: conf.ini root_password).\n\
         \t--no-checkpoint\t\tDo not save every write before acknowledging it.\n\
         \t--load-dir DIR\t\tLet LOAD, /build, /restore read files under DIR (repeatable).\n\
         \t--allow-service\t\tLet SPARQL SERVICE contact remote endpoints.\n\
         \t--pid-file FILE\t\tPid file (default <db_home>/gserver.pid).\n\
         \t--log-file FILE\t\tLog of a background start (default <db_home>/gserver.log).\n\
         \t--api [DIR]\t\tServe the multi-database API rooted at DIR (the default mode).\n\
         \tDIR\t\t\tWithout --api: serve the single database in DIR.\n\
         \t--rocksdb\t\tWith a single DIR: insist it is a RocksDB database."
    );
    #[cfg(feature = "tls")]
    println!("\t--tls-cert FILE --tls-key FILE\tServe HTTPS.");
    println!("\nDocs: {DOCS}\n");
}

// ---------------------------------------------------------------------------
// command line
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Start,
    Background,
    Stop,
    Restart,
    Kill,
    Status,
    Help,
}

#[derive(Debug, Default)]
struct Cli {
    mode: Option<Mode>,
    config: Option<PathBuf>,
    port: Option<u16>,
    addr: Option<String>,
    password: Option<String>,
    api: bool,
    rocksdb: bool,
    no_checkpoint: bool,
    load_dirs: Vec<String>,
    allow_service: bool,
    pid_file: Option<PathBuf>,
    log_file: Option<PathBuf>,
    dir: Option<PathBuf>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    /// Every argument except the mode flag: re-used to start the server again
    /// (`-b`, `-r`, the supervisor, `/restart`), with `-s` appended.
    passthrough: Vec<OsString>,
}

impl Cli {
    /// The requested mode (`-s` when none was given).
    fn mode(&self) -> Mode {
        self.mode.unwrap_or(Mode::Start)
    }

    fn parse(args: impl Iterator<Item = OsString>) -> Result<Cli, String> {
        let args: Vec<OsString> = args.collect();
        let mut cli = Cli::default();
        let mut i = 0;
        let value = |i: &mut usize, flag: &str| -> Result<OsString, String> {
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| format!("{flag} needs a value"))
        };
        while i < args.len() {
            let arg = args[i].to_string_lossy().into_owned();
            let mode = match arg.as_str() {
                "-s" | "--start" => Some(Mode::Start),
                "-b" | "--background" => Some(Mode::Background),
                "-t" | "--stop" => Some(Mode::Stop),
                "-r" | "--restart" => Some(Mode::Restart),
                "-k" | "--kill" => Some(Mode::Kill),
                "-S" | "--status" => Some(Mode::Status),
                "-h" | "--help" => Some(Mode::Help),
                _ => None,
            };
            if let Some(m) = mode {
                if cli.mode.is_some_and(|old| old != m) {
                    return Err(format!("only one of -s/-b/-t/-r/-k/-S may be given (got {arg} too)"));
                }
                cli.mode = Some(m);
                i += 1;
                continue;
            }
            let start = i;
            match arg.as_str() {
                "-c" | "--config" => cli.config = Some(value(&mut i, &arg)?.into()),
                "-p" | "--port" => {
                    let v = value(&mut i, &arg)?;
                    let v = v.to_string_lossy();
                    cli.port = Some(v.parse().map_err(|_| format!("invalid port '{v}'"))?);
                }
                "--addr" => cli.addr = Some(value(&mut i, &arg)?.to_string_lossy().into_owned()),
                "--password" => {
                    cli.password = Some(value(&mut i, &arg)?.to_string_lossy().into_owned())
                }
                "--api" => cli.api = true,
                "--rocksdb" => cli.rocksdb = true,
                "--no-checkpoint" => cli.no_checkpoint = true,
                "--load-dir" => cli
                    .load_dirs
                    .push(value(&mut i, &arg)?.to_string_lossy().into_owned()),
                "--allow-service" => cli.allow_service = true,
                "--pid-file" => cli.pid_file = Some(value(&mut i, &arg)?.into()),
                "--log-file" => cli.log_file = Some(value(&mut i, &arg)?.into()),
                "--tls-cert" => cli.tls_cert = Some(value(&mut i, &arg)?.to_string_lossy().into_owned()),
                "--tls-key" => cli.tls_key = Some(value(&mut i, &arg)?.to_string_lossy().into_owned()),
                s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option '{s}'")),
                _ => {
                    if cli.dir.is_some() {
                        return Err(format!("more than one directory given ('{arg}')"));
                    }
                    cli.dir = Some(args[i].clone().into());
                }
            }
            cli.passthrough.extend(args[start..=i].iter().cloned());
            i += 1;
        }
        #[cfg(not(feature = "tls"))]
        if cli.tls_cert.is_some() || cli.tls_key.is_some() {
            eprintln!(
                "warning: --tls-cert/--tls-key ignored — gserver was built without the `tls` feature \
                 (rebuild with `--features tls` to serve HTTPS)"
            );
        }
        // HTTPS needs BOTH a cert and a key; one without the other would
        // otherwise silently fall back to plaintext HTTP.
        #[cfg(feature = "tls")]
        if cli.tls_cert.is_some() != cli.tls_key.is_some() {
            return Err("--tls-cert and --tls-key must be supplied together to serve HTTPS".into());
        }
        Ok(cli)
    }

    /// The arguments that start this same server in the foreground.
    fn start_args(&self) -> Vec<OsString> {
        let mut a = self.passthrough.clone();
        a.push("-s".into());
        a
    }
}

/// Read `conf.ini` (the `-c` file, else `./conf/conf.ini` when present) and
/// apply the command-line overrides.
fn load_config(cli: &Cli) -> GStoreConfig {
    let quiet = cli.mode() == Mode::Status;
    let mut cfg = match &cli.config {
        Some(path) => match GStoreConfig::load(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gserver: cannot read configuration file '{}': {e}", path.display());
                std::process::exit(1);
            }
        },
        None => {
            let default = Path::new("conf/conf.ini");
            if default.is_file() {
                GStoreConfig::load(default).unwrap_or_else(|e| {
                    eprintln!("gserver: cannot read '{}': {e}; using defaults", default.display());
                    GStoreConfig::default()
                })
            } else {
                GStoreConfig::default()
            }
        }
    };
    if !quiet {
        if let Some(src) = cfg.source() {
            println!("gserver: configuration from '{}'", src.display());
        }
        for w in cfg.warnings() {
            eprintln!("gserver: conf.ini warning: {w}");
        }
        if cfg.db_suffix != gstore::DB_SUFFIX {
            eprintln!(
                "gserver: conf.ini warning: db_suffix '{}' is not supported yet; databases use '{}'",
                cfg.db_suffix,
                gstore::DB_SUFFIX
            );
        }
        if cfg.root_username != gstore::http_users::ROOT_USER {
            eprintln!(
                "gserver: conf.ini warning: root_username '{}' is not supported yet; the root user is '{}'",
                cfg.root_username,
                gstore::http_users::ROOT_USER
            );
        }
    }
    if let Some(p) = cli.port {
        cfg.port = p;
    }
    if let Some(a) = &cli.addr {
        cfg.listen_addr = a.clone();
    }
    if let Some(pw) = &cli.password {
        cfg.root_password = pw.clone();
    }
    if cli.no_checkpoint {
        cfg.check_point = false;
    }
    cfg
}

/// What a gserver invocation serves, and where its pid file and log live.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// The multi-database API rooted at this directory.
    Api(PathBuf),
    /// One database in this directory (legacy mode).
    Single(PathBuf),
}

#[derive(Debug, Clone)]
struct Target {
    kind: Kind,
    pid_file: PathBuf,
    log_file: PathBuf,
}

impl Target {
    fn resolve(cli: &Cli, cfg: &GStoreConfig) -> Target {
        let kind = match (&cli.dir, cli.api) {
            (Some(d), false) => Kind::Single(d.clone()),
            (Some(d), true) => Kind::Api(d.clone()),
            // The legacy `--api` without a directory served `.`.
            (None, true) if !cfg.is_set("db_home") => Kind::Api(PathBuf::from(".")),
            (None, _) => Kind::Api(cfg.db_home.clone()),
        };
        let pid_file = cli.pid_file.clone().unwrap_or_else(|| match &kind {
            Kind::Api(root) => root.join("gserver.pid"),
            Kind::Single(dir) => {
                let mut s = dir.as_os_str().to_owned();
                // `lubm.db/` and `lubm.db` name the same sibling file.
                while s.to_string_lossy().ends_with('/') && s.len() > 1 {
                    let t = s.to_string_lossy();
                    s = OsString::from(&t[..t.len() - 1]);
                }
                s.push(".pid");
                PathBuf::from(s)
            }
        });
        let log_file = cli
            .log_file
            .clone()
            .unwrap_or_else(|| pid_file.with_extension("log"));
        Target {
            kind,
            pid_file,
            log_file,
        }
    }
}

// ---------------------------------------------------------------------------
// pid file
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct PidInfo {
    pid: u32,
    system_password: String,
    background: bool,
    addr: Option<SocketAddr>,
    supervisor: u32,
}

impl PidInfo {
    fn render(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n",
            self.pid,
            self.system_password,
            u8::from(self.background),
            self.addr.map(|a| a.to_string()).unwrap_or_default(),
            self.supervisor
        )
    }

    fn parse(text: &str) -> Option<PidInfo> {
        let mut lines = text.lines();
        let pid: u32 = lines.next()?.trim().parse().ok()?;
        if pid == 0 || pid > i32::MAX as u32 {
            return None;
        }
        let system_password = lines.next().unwrap_or("").trim().to_string();
        let background = lines.next().is_some_and(|l| l.trim() == "1");
        let addr = lines.next().and_then(|l| l.trim().parse().ok());
        let supervisor = lines
            .next()
            .and_then(|l| l.trim().parse().ok())
            .filter(|p| *p <= i32::MAX as u32)
            .unwrap_or(0);
        Some(PidInfo {
            pid,
            system_password,
            background,
            addr,
            supervisor,
        })
    }

    /// A connectable form of the listening address (an unspecified bind
    /// address is reached through loopback).
    fn connect_addr(&self) -> Option<SocketAddr> {
        let mut a = self.addr?;
        if a.ip().is_unspecified() {
            let lo = match a {
                SocketAddr::V4(_) => "127.0.0.1".parse().ok()?,
                SocketAddr::V6(_) => "::1".parse().ok()?,
            };
            a.set_ip(lo);
        }
        Some(a)
    }

    fn answers(&self) -> bool {
        self.connect_addr()
            .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
    }
}

fn read_pid_file(path: &Path) -> Option<PidInfo> {
    PidInfo::parse(&fs::read_to_string(path).ok()?)
}

/// Write the pid file atomically (temp file + rename), readable only by its
/// owner on unix — it holds the system password.
fn write_pid_file(path: &Path, info: &PidInfo) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("pid.tmp{}", std::process::id()));
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
    fs::rename(&tmp, path)
}

/// Remove the pid file if it still names `pid`.
fn remove_pid_file_of(path: &Path, pid: u32) {
    if read_pid_file(path).is_some_and(|i| i.pid == pid) {
        let _ = fs::remove_file(path);
    }
}

/// The server the pid file names, if it is alive (a gserver process other than
/// this one). A stale pid file — dead process, reused pid, garbage — yields
/// `None`.
fn live_server(target: &Target) -> Option<PidInfo> {
    let info = read_pid_file(&target.pid_file)?;
    if info.pid == std::process::id() {
        return None; // our own, e.g. after `/restart` re-executed this process
    }
    (proc_util::alive(info.pid) && proc_util::is_gserver(info.pid)).then_some(info)
}

// ---------------------------------------------------------------------------
// modes
// ---------------------------------------------------------------------------

fn start_foreground(cli: &Cli, cfg: &GStoreConfig, target: &Target) -> u8 {
    if live_server(target).is_some() {
        println!("the server already running.");
        return 1;
    }
    let supervised = std::env::var(ENV_SUPERVISOR)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if cfg.deamon && supervised == 0 {
        return supervise(cli);
    }
    serve(cli, cfg, target, supervised)
}

fn start_background(cli: &Cli, target: &Target) -> u8 {
    if let Some(info) = live_server(target) {
        println!("the server already running (pid {}).", info.pid);
        return 1;
    }
    #[cfg(not(unix))]
    {
        let _ = cli;
        eprintln!("gserver: -b/--background is only supported on unix");
        1
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let log = &target.log_file;
        if let Some(parent) = log.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = fs::create_dir_all(parent);
        }
        let mut log_file = match fs::OpenOptions::new().create(true).append(true).open(log) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("gserver: cannot open log file '{}': {e}", log.display());
                return 1;
            }
        };
        let log_start = log_file.seek(SeekFrom::End(0)).unwrap_or(0);
        let _ = writeln!(
            log_file,
            "==== gserver starting in the background (unix time {}) ====",
            unix_secs()
        );
        let (out, err) = match (log_file.try_clone(), log_file.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => {
                eprintln!("gserver: cannot redirect output to '{}'", log.display());
                return 1;
            }
        };
        let mut cmd = Command::new(self_exe());
        cmd.args(cli.start_args())
            .env(ENV_BACKGROUND, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        // SAFETY: `setsid` is async-signal-safe; the closure touches nothing
        // else between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                proc_util::new_session();
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gserver: failed to start the server in the background: {e}");
                return 1;
            }
        };
        let child_pid = child.id();
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(st)) = child.try_wait() {
                eprintln!("gserver: the server failed to start ({st}); log '{}':", log.display());
                eprint!("{}", log_tail(log, log_start));
                return 1;
            }
            if let Some(info) = read_pid_file(&target.pid_file) {
                if (info.pid == child_pid || info.supervisor == child_pid) && info.answers() {
                    let addr = info.addr.map(|a| a.to_string()).unwrap_or_default();
                    println!(
                        "gserver started in the background (pid {}), listening on http://{addr}",
                        info.pid
                    );
                    println!("gserver: log file '{}', pid file '{}'", log.display(), target.pid_file.display());
                    return 0;
                }
            }
            if Instant::now() > deadline {
                eprintln!(
                    "gserver: the background server did not answer within {}s; see '{}'",
                    START_TIMEOUT.as_secs(),
                    log.display()
                );
                return 1;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn stop(target: &Target) -> u8 {
    let Some(info) = live_server(target) else {
        println!("server is inactive (dead).");
        if let Some(stale) = read_pid_file(&target.pid_file) {
            if stale.pid != std::process::id() {
                let _ = fs::remove_file(&target.pid_file);
            }
        }
        return 0;
    };
    println!("stopping gserver (pid {})...", info.pid);
    if let Err(e) = proc_util::signal(info.pid, proc_util::SIGTERM) {
        eprintln!("gserver: cannot signal pid {}: {e}", info.pid);
        return 1;
    }
    if !proc_util::wait_exit(info.pid, STOP_TIMEOUT) {
        eprintln!(
            "gserver: pid {} did not stop within {}s; use -k to force it",
            info.pid,
            STOP_TIMEOUT.as_secs()
        );
        return 1;
    }
    // A supervisor ends by itself once its server exits normally.
    if info.supervisor != 0 && !proc_util::wait_exit(info.supervisor, Duration::from_secs(10)) {
        eprintln!("gserver: supervisor pid {} is still running", info.supervisor);
    }
    remove_pid_file_of(&target.pid_file, info.pid);
    println!("the Server [{}] stop successfully.", info.pid);
    0
}

fn restart(cli: &Cli, cfg: &GStoreConfig, target: &Target) -> u8 {
    let mut background = false;
    if let Some(info) = live_server(target) {
        background = info.background;
        println!("server is active (running).");
        println!("stop server...");
        let code = stop(target);
        if code != 0 {
            return code;
        }
    }
    println!("start server...");
    if background {
        start_background(cli, target)
    } else {
        start_foreground(cli, cfg, target)
    }
}

fn kill(target: &Target) -> u8 {
    println!("The service will be forcibly stopped!");
    let Some(info) = read_pid_file(&target.pid_file) else {
        println!("server is inactive (dead).");
        return 0;
    };
    let me = std::process::id();
    // The supervisor first, so it cannot restart the server it watches.
    for pid in [info.supervisor, info.pid] {
        if pid != 0 && pid != me && proc_util::alive(pid) && proc_util::is_gserver(pid) {
            match proc_util::signal(pid, proc_util::SIGKILL) {
                Ok(()) => {
                    proc_util::wait_exit(pid, Duration::from_secs(10));
                    println!("killed pid {pid}");
                }
                Err(e) => eprintln!("gserver: cannot kill pid {pid}: {e}"),
            }
        }
    }
    if info.pid != me {
        let _ = fs::remove_file(&target.pid_file);
    }
    0
}

fn status(target: &Target) -> u8 {
    println!("gStore API Server(gserver)");
    match live_server(target) {
        Some(info) => {
            println!("\tActive: active (running)");
            println!("\tMain PID: {}", info.pid);
            if let Some(a) = info.addr {
                let answering = if info.answers() { "" } else { " (not answering)" };
                println!("\tAddress: http://{a}{answering}");
            }
            println!(
                "\tStarted: {}",
                if info.background { "in the background (-b)" } else { "in the foreground" }
            );
            if info.supervisor != 0 {
                println!("\tSupervisor PID: {} (deamon=on)", info.supervisor);
            }
        }
        None => {
            println!("\tActive: inactive (dead)");
        }
    }
    println!("\tPID file: {}", target.pid_file.display());
    println!("\tDocs: {DOCS}");
    0
}

// ---------------------------------------------------------------------------
// supervisor (deamon=on)
// ---------------------------------------------------------------------------

/// Run the server as a child and restart it after an abnormal exit, with a
/// bounded exponential backoff. A normal exit (graceful stop, startup error)
/// ends the supervisor with the child's exit code. `SIGINT`/`SIGTERM` are
/// forwarded to the child once (it runs in its own process group, so a
/// terminal's Ctrl-C reaches only the supervisor).
fn supervise(cli: &Cli) -> u8 {
    #[cfg(not(unix))]
    {
        let _ = cli;
        eprintln!("gserver: deamon=on is only supported on unix; set deamon=off");
        1
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::process::Command;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::Arc;

        let child_pid = Arc::new(AtomicU32::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        {
            let (child_pid, stopping) = (Arc::clone(&child_pid), Arc::clone(&stopping));
            gstore::server::on_termination_signal(move || {
                stopping.store(true, Ordering::SeqCst);
                let pid = child_pid.load(Ordering::SeqCst);
                if pid != 0 {
                    let _ = proc_util::signal(pid, proc_util::SIGTERM);
                }
            });
        }
        let me = std::process::id();
        println!("gserver: supervisor (pid {me}) started (deamon=on)");
        let mut backoff = Duration::from_secs(1);
        loop {
            let mut cmd = Command::new(self_exe());
            cmd.args(cli.start_args())
                .env(ENV_SUPERVISOR, me.to_string())
                .process_group(0);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("gserver: supervisor cannot start the server: {e}");
                    return 1;
                }
            };
            child_pid.store(child.id(), Ordering::SeqCst);
            if stopping.load(Ordering::SeqCst) {
                let _ = proc_util::signal(child.id(), proc_util::SIGTERM);
            }
            let started = Instant::now();
            let status = child.wait();
            child_pid.store(0, Ordering::SeqCst);
            let status = match status {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("gserver: supervisor lost its server: {e}");
                    return 1;
                }
            };
            // Abnormal: killed by a signal, or a Rust panic (exit code 101).
            let abnormal = status.signal().is_some() || status.code() == Some(101);
            if !abnormal || stopping.load(Ordering::SeqCst) {
                println!("gserver: server stopped ({status}); supervisor exiting");
                return status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1);
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            eprintln!(
                "gserver: server stopped abnormally ({status}); restarting in {}s",
                backoff.as_secs()
            );
            let until = Instant::now() + backoff;
            while Instant::now() < until {
                if stopping.load(Ordering::SeqCst) {
                    return 0;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }
}

// ---------------------------------------------------------------------------
// serving
// ---------------------------------------------------------------------------

/// Serve in this process until a graceful shutdown; then remove the pid file
/// (or, for `/restart`, re-execute in place) and exit.
fn serve(cli: &Cli, cfg: &GStoreConfig, target: &Target, supervisor: u32) -> u8 {
    let load_policy = load_policy_for(&cli.load_dirs, cli.allow_service);
    let background = std::env::var_os(ENV_BACKGROUND).is_some();
    let addr = (cfg.listen_addr.as_str(), cfg.port);
    let (report, handle) = match &target.kind {
        Kind::Api(root) => {
            if cli.rocksdb {
                eprintln!("gserver: --rocksdb is ignored in API mode (the API manages its own databases)");
            }
            if cfg.root_password == "123456" {
                eprintln!(
                    "warning: root is using the default password '123456'; \
                     pass --password (or set root_password) before exposing this server"
                );
            }
            // The served root becomes the configuration's db_home, so request
            // handlers reading the config see where the databases really are.
            let mut served = cfg.clone();
            served.db_home = root.clone();
            let server = match ApiServer::bind_with_config(served) {
                Ok(s) => s.with_load_policy(load_policy),
                Err(e) => {
                    eprintln!("failed to start API server at {}:{}: {e}", cfg.listen_addr, cfg.port);
                    return 1;
                }
            };
            let root_s = root.display();
            if cfg.check_point {
                println!("gserver: databases are stored under '{root_s}' (check_point on: every write is saved)");
            } else {
                println!(
                    "gserver: databases are stored under '{root_s}' (check_point off: changes are saved \
                     on /checkpoint, /unload and shutdown)"
                );
            }
            let local = server.local_addr().ok();
            announce_pid(target, local, server.system_password(), background, supervisor);
            let handle = server.shutdown_handle();
            install_signals(&handle);
            println!(
                "gserver: {} worker threads, keep-alive {}s",
                server.http_options().threads,
                server.http_options().keep_alive_timeout.as_secs()
            );
            let report = run_api(&server, &root_s.to_string(), cli);
            (report, handle)
        }
        Kind::Single(dir) => {
            let dir_s = dir.to_string_lossy().into_owned();
            let db = open_or_exit(&dir_s, cli.rocksdb);
            let server = match Server::bind(db, addr) {
                Ok(s) => s
                    .with_check_point(cfg.check_point)
                    .with_load_policy(load_policy)
                    .with_http_options(cfg.http_options())
                    .with_db_dir(dir),
                Err(e) => {
                    eprintln!("failed to bind {}:{}: {e}", cfg.listen_addr, cfg.port);
                    return 1;
                }
            };
            if cfg.check_point {
                println!("gserver: database directory '{dir_s}' (check_point on: every update is saved there)");
            } else {
                println!(
                    "gserver: database directory '{dir_s}' (check_point off: changes are saved there on shutdown)"
                );
            }
            let local = server.local_addr().ok();
            announce_pid(target, local, "", background, supervisor);
            let handle = server.shutdown_handle();
            install_signals(&handle);
            let report = run_single(&server, cli);
            (report, handle)
        }
    };
    let failed = print_report(&report);
    if handle.restart_requested() {
        println!("gserver: restarting");
        let _ = io::stdout().flush();
        reexec(cli);
        // Only reached when exec failed.
        remove_pid_file_of(&target.pid_file, std::process::id());
        return 1;
    }
    remove_pid_file_of(&target.pid_file, std::process::id());
    u8::from(failed)
}

fn install_signals(handle: &ShutdownHandle) {
    if shutdown_on_signals(handle.clone()) {
        println!("gserver: Ctrl-C / SIGTERM shuts down gracefully (saving unsaved changes)");
    }
}

/// Record this server in the pid file (warn, but keep serving, on failure).
fn announce_pid(
    target: &Target,
    addr: Option<SocketAddr>,
    system_password: &str,
    background: bool,
    supervisor: u32,
) {
    let info = PidInfo {
        pid: std::process::id(),
        system_password: system_password.to_string(),
        background,
        addr,
        supervisor,
    };
    match write_pid_file(&target.pid_file, &info) {
        Ok(()) => println!("gserver: pid {} (pid file '{}')", info.pid, target.pid_file.display()),
        Err(e) => eprintln!(
            "gserver: warning: cannot write pid file '{}': {e} (-t/-S/-k will not find this server)",
            target.pid_file.display()
        ),
    }
}

/// Serve the multi-database API until it shuts down.
fn run_api(server: &ApiServer, root: &str, cli: &Cli) -> ShutdownReport {
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (cli.tls_cert.as_deref(), cli.tls_key.as_deref()) {
        match gstore::server::load_tls_config(cert, key) {
            Ok(cfg) => {
                match server.local_addr() {
                    Ok(a) => println!(
                        "gserver (API mode, TLS) on https://{a}  root={root}  user=root  \
                         (web console at /, POST /login /sparql /update /upload …; see /status)"
                    ),
                    Err(_) => println!("gserver (API mode, TLS)  root={root}"),
                }
                return server.serve_forever_tls(cfg);
            }
            Err(e) => {
                eprintln!("failed to load TLS cert/key: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(feature = "tls"))]
    let _ = cli;
    match server.local_addr() {
        Ok(a) => println!(
            "gserver (API mode) on http://{a}  root={root}  user=root  \
             (web console at /, POST /login /sparql /update /upload …; see /status)"
        ),
        Err(_) => println!("gserver (API mode)  root={root}"),
    }
    server.serve_forever()
}

/// Serve the single database until it shuts down.
fn run_single(server: &Server, cli: &Cli) -> ShutdownReport {
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (cli.tls_cert.as_deref(), cli.tls_key.as_deref()) {
        match gstore::server::load_tls_config(cert, key) {
            Ok(cfg) => {
                if let Ok(a) = server.local_addr() {
                    println!(
                        "gserver listening on https://{a}  (TLS; web console at /, POST /sparql, /update; GET /status)"
                    );
                }
                return server.serve_forever_tls(cfg);
            }
            Err(e) => {
                eprintln!("failed to load TLS cert/key: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(feature = "tls"))]
    let _ = cli;
    if let Ok(a) = server.local_addr() {
        println!("gserver listening on http://{a}  (web console at /, POST /sparql, /update; GET /status)");
    }
    server.serve_forever()
}

/// Replace this process with a fresh `gserver` started the same way (keeps the
/// pid, so the pid file and a supervisor stay valid). Returns only on failure.
fn reexec(cli: &Cli) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(self_exe()).args(cli.start_args()).exec();
        eprintln!("gserver: restart failed: {err}");
    }
    #[cfg(not(unix))]
    {
        let _ = cli;
        eprintln!("gserver: /restart re-execution is only supported on unix; the server stopped");
    }
}

/// The path of this executable (falls back to `argv[0]`).
fn self_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .filter(|p| p.exists())
        .or_else(|| std::env::args_os().next().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("gserver"))
}

/// The log written since byte `from` (at most its last 4 KiB).
fn log_tail(path: &Path, from: u64) -> String {
    let mut s = String::new();
    if let Ok(mut f) = fs::File::open(path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = from.max(len.saturating_sub(4096));
        if f.seek(SeekFrom::Start(start)).is_ok() {
            let _ = f.read_to_string(&mut s);
        }
    }
    s
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Print what the graceful shutdown did. Returns whether a save failed.
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
        eprintln!("gserver: FAILED to save database to '{}': {e}", dir.display());
    }
    println!("gserver: shut down");
    !report.failed.is_empty()
}

/// The `LOAD` policy for the served databases: nothing may be loaded, except
/// files inside the `--load-dir` directories; `SERVICE` may contact remote
/// endpoints only with `--allow-service`.
fn load_policy_for(dirs: &[String], allow_service: bool) -> LoadPolicy {
    let mut policy = LoadPolicy::restricted();
    if allow_service {
        policy.allow_remote = true;
        println!("gserver: SPARQL SERVICE may contact remote endpoints");
    }
    for d in dirs {
        if !Path::new(d).is_dir() {
            eprintln!("error: --load-dir '{d}' is not a directory");
            std::process::exit(2);
        }
        policy = policy.with_allowed_dir(d);
        println!("gserver: SPARQL LOAD may read files under '{d}'");
    }
    policy
}

/// Open the single database at `dir`, whatever its layout. A directory that does
/// not exist yet (or is empty) gives a fresh database that is saved there on the
/// first update. Anything else that cannot be opened is fatal: serving an empty
/// stand-in could later be saved over the real database.
fn open_or_exit(dir: &str, expect_rocksdb: bool) -> Database {
    let path = Path::new(dir);
    let is_empty_dir = path.is_dir()
        && std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
    if !path.exists() || is_empty_dir {
        if expect_rocksdb {
            eprintln!("gserver: --rocksdb given but '{dir}' holds no database");
            std::process::exit(1);
        }
        eprintln!("'{dir}' holds no database yet; serving a fresh empty database that will be saved there");
        return Database::new(db_name_for(dir));
    }
    if expect_rocksdb && Database::storage_kind(path) != Some(StorageKind::RocksDb) {
        eprintln!("gserver: --rocksdb given but '{dir}' is not a RocksDB-backed database");
        std::process::exit(1);
    }
    match Database::open(path) {
        Ok(db) => {
            println!("gserver: opened '{dir}' ({} layout)", db.storage());
            db
        }
        Err(e) => {
            eprintln!("gserver: could not open database '{dir}': {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// process utilities (tiny unix FFI: kill(2), setsid(2))
// ---------------------------------------------------------------------------

mod proc_util {
    use std::time::{Duration, Instant};

    pub const SIGTERM: i32 = 15;
    pub const SIGKILL: i32 = 9;

    #[cfg(unix)]
    mod ffi {
        use std::os::raw::c_int;
        extern "C" {
            pub fn kill(pid: c_int, sig: c_int) -> c_int;
            pub fn setsid() -> c_int;
        }
    }

    /// Send `sig` to process `pid` (`0` probes for existence). Refuses pid 0 and
    /// anything above `i32::MAX`, which `kill(2)` would treat as process groups.
    #[cfg(unix)]
    pub fn signal(pid: u32, sig: i32) -> std::io::Result<()> {
        let Ok(p) = i32::try_from(pid) else {
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        if p <= 0 {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        // SAFETY: plain syscall wrapper; `p > 0` names a single process.
        if unsafe { ffi::kill(p, sig) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(not(unix))]
    pub fn signal(_pid: u32, _sig: i32) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "signals are only supported on unix",
        ))
    }

    /// Become a session leader (detach from the controlling terminal). Called
    /// between fork and exec: async-signal-safe.
    #[cfg(unix)]
    pub fn new_session() {
        // SAFETY: `setsid` takes no arguments and is async-signal-safe.
        unsafe {
            ffi::setsid();
        }
    }

    /// Whether `pid` is a live (non-zombie) process.
    pub fn alive(pid: u32) -> bool {
        let exists = match signal(pid, 0) {
            Ok(()) => true,
            // EPERM: it exists but belongs to someone else.
            Err(e) => e.raw_os_error() == Some(1),
        };
        exists && !zombie(pid)
    }

    /// A zombie has exited but was not reaped yet (Linux `/proc` only).
    fn zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                // `pid (comm) S ...` — comm may contain spaces/parens.
                let rest = &s[s.rfind(')')? + 1..];
                rest.split_whitespace().next().map(|st| st == "Z" || st == "X")
            })
            .unwrap_or(false)
    }

    /// Whether `pid` runs a gserver binary (guards against signalling an
    /// unrelated process that reused a stale pid). Without `/proc` (non-Linux)
    /// this cannot be checked and is assumed.
    pub fn is_gserver(pid: u32) -> bool {
        match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(cmd) => {
                let argv0 = cmd.split(|b| *b == 0).next().unwrap_or(&[]);
                let argv0 = String::from_utf8_lossy(argv0);
                let base = argv0.rsplit('/').next().unwrap_or("");
                base.contains("gserver")
            }
            Err(_) => !std::path::Path::new("/proc/self").exists(),
        }
    }

    /// Wait (at most `timeout`) for `pid` to exit.
    pub fn wait_exit(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while alive(pid) {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<Cli, String> {
        Cli::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn modes_and_options_parse() {
        let c = cli(&[]).unwrap();
        assert!(c.mode() == Mode::Start && c.dir.is_none() && !c.api);
        let c = cli(&["-b", "-c", "my.ini", "-p", "9100", "--addr", "0.0.0.0"]).unwrap();
        assert!(c.mode() == Mode::Background);
        assert_eq!(c.config.as_deref(), Some(Path::new("my.ini")));
        assert_eq!(c.port, Some(9100));
        assert_eq!(c.addr.as_deref(), Some("0.0.0.0"));
        // The mode flag is dropped from the re-start arguments.
        assert_eq!(
            c.start_args(),
            ["-c", "my.ini", "-p", "9100", "--addr", "0.0.0.0", "-s"].map(OsString::from)
        );
        for (flag, mode) in [
            ("--start", Mode::Start),
            ("--stop", Mode::Stop),
            ("-t", Mode::Stop),
            ("--restart", Mode::Restart),
            ("-r", Mode::Restart),
            ("--kill", Mode::Kill),
            ("-k", Mode::Kill),
            ("--status", Mode::Status),
            ("-S", Mode::Status),
            ("-h", Mode::Help),
        ] {
            assert!(cli(&[flag]).unwrap().mode() == mode, "{flag}");
        }
        // Legacy invocations keep working.
        let c = cli(&["db/lubm.db", "--port", "0", "--no-checkpoint", "--load-dir", "x"]).unwrap();
        assert_eq!(c.dir.as_deref(), Some(Path::new("db/lubm.db")));
        assert!(c.no_checkpoint && c.port == Some(0) && c.load_dirs == ["x"]);
        assert!(!c.allow_service && cli(&["--allow-service"]).unwrap().allow_service);
        let c = cli(&["root", "--api", "--password", "pw"]).unwrap();
        assert!(c.api && c.password.as_deref() == Some("pw"));

        assert!(cli(&["-s", "-t"]).is_err(), "two modes");
        assert!(cli(&["-p", "nope"]).is_err());
        assert!(cli(&["-p"]).is_err());
        assert!(cli(&["--bogus"]).is_err());
        assert!(cli(&["a", "b"]).is_err());
    }

    #[test]
    fn targets_derive_pid_and_log_files() {
        let cfg = GStoreConfig::default();
        let t = Target::resolve(&cli(&[]).unwrap(), &cfg);
        assert_eq!(t.kind, Kind::Api(PathBuf::from("./dbhome/")));
        assert_eq!(t.pid_file, PathBuf::from("./dbhome/gserver.pid"));
        assert_eq!(t.log_file, PathBuf::from("./dbhome/gserver.log"));
        // Legacy `--api` without a directory serves `.` unless db_home is set.
        let t = Target::resolve(&cli(&["--api"]).unwrap(), &cfg);
        assert_eq!(t.kind, Kind::Api(PathBuf::from(".")));
        let set = GStoreConfig::from_ini_str("[system]\ndb_home=/srv/gs\n");
        let t = Target::resolve(&cli(&["--api"]).unwrap(), &set);
        assert_eq!(t.kind, Kind::Api(PathBuf::from("/srv/gs")));
        let t = Target::resolve(&cli(&["/data/x", "--api"]).unwrap(), &set);
        assert_eq!(t.pid_file, PathBuf::from("/data/x/gserver.pid"));
        // A single database: the pid file sits next to it.
        let t = Target::resolve(&cli(&["/data/lubm.db/"]).unwrap(), &cfg);
        assert_eq!(t.kind, Kind::Single(PathBuf::from("/data/lubm.db/")));
        assert_eq!(t.pid_file, PathBuf::from("/data/lubm.db.pid"));
        assert_eq!(t.log_file, PathBuf::from("/data/lubm.db.log"));
        let t = Target::resolve(&cli(&["--pid-file", "/run/g.pid", "--log-file", "/var/g.log"]).unwrap(), &cfg);
        assert_eq!(t.pid_file, PathBuf::from("/run/g.pid"));
        assert_eq!(t.log_file, PathBuf::from("/var/g.log"));
    }

    #[test]
    fn command_line_overrides_conf_ini() {
        let dir = std::env::temp_dir().join(format!("gserver_unit_cfg_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("conf.ini");
        fs::write(&ini, "[server]\nport=9555\nthread_num=3\n[system]\nroot_password=fromfile\ncheck_point=on\n").unwrap();
        let ini_s = ini.to_string_lossy().into_owned();
        let c = load_config(&cli(&["-S", "-c", &ini_s]).unwrap());
        assert_eq!((c.port, c.thread_num, c.root_password.as_str()), (9555, 3, "fromfile"));
        assert!(c.check_point);
        let c = load_config(
            &cli(&["-S", "-c", &ini_s, "-p", "9600", "--password", "cli", "--no-checkpoint"]).unwrap(),
        );
        assert_eq!((c.port, c.thread_num, c.root_password.as_str()), (9600, 3, "cli"));
        assert!(!c.check_point);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_file_round_trips_and_rejects_garbage() {
        let info = PidInfo {
            pid: 4242,
            system_password: "123".into(),
            background: true,
            addr: Some("0.0.0.0:9000".parse().unwrap()),
            supervisor: 7,
        };
        assert_eq!(PidInfo::parse(&info.render()), Some(info.clone()));
        assert_eq!(info.connect_addr(), Some("127.0.0.1:9000".parse().unwrap()));
        // Upstream's three-line format still parses.
        let up = PidInfo::parse("4242\nsecret\n0\n").unwrap();
        assert_eq!((up.pid, up.system_password.as_str(), up.background), (4242, "secret", false));
        assert_eq!((up.addr, up.supervisor), (None, 0));
        // pid 0 / negative / huge would make kill(2) signal process groups.
        assert_eq!(PidInfo::parse("0\n"), None);
        assert_eq!(PidInfo::parse("-1\n"), None);
        assert_eq!(PidInfo::parse("4294967295\n"), None);
        assert_eq!(PidInfo::parse("garbage"), None);

        let dir = std::env::temp_dir().join(format!("gserver_unit_pid_{}", std::process::id()));
        let path = dir.join("sub").join("gserver.pid");
        write_pid_file(&path, &info).unwrap();
        assert_eq!(read_pid_file(&path), Some(info));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the pid file holds the system password");
        }
        remove_pid_file_of(&path, 1);
        assert!(path.exists(), "only removed when it names the given pid");
        remove_pid_file_of(&path, 4242);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn process_probes() {
        let me = std::process::id();
        assert!(proc_util::alive(me));
        assert!(proc_util::signal(0, 0).is_err(), "pid 0 is refused");
        assert!(proc_util::signal(u32::MAX, 0).is_err());
        // A child that exited and was reaped is gone.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(proc_util::wait_exit(pid, Duration::from_secs(5)));
    }
}
