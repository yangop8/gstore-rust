//! gserver lifecycle and server-infrastructure tests:
//!
//! - the `/shutdown` and `/restart` path routes of [`ApiServer`] (credentials,
//!   gStore status envelope, graceful shutdown with final saves, restart flag);
//! - the real `gserver` binary: `-b` (background) / `-S` (status) / `-t` (stop)
//!   / `-k` (kill) / `-r` (restart) through its pid file, `-s` refusing a second
//!   server, `conf.ini` + command-line precedence, `/restart` re-executing in
//!   place, the `deamon=on` supervisor restarting a killed server, and the legacy
//!   single-database invocation.
//!
//! Every server runs in its own temporary directory (also its working
//! directory) on a free port; a guard kills whatever a failing test left behind.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::query::hash::md5_hex;
use gstore::server::ShutdownReport;

// ---- helpers ----------------------------------------------------------------

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_lifecycle_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One HTTP/1.1 request (`Connection: close`) with optional extra headers;
/// returns `(status, body)`.
fn request_h(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    request_h(addr, method, target, &[], body)
}

/// `"StatusCode":N` of a gStore envelope.
fn status_code(body: &str) -> i64 {
    let i = body.find("\"StatusCode\":").expect("StatusCode") + "\"StatusCode\":".len();
    body[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect::<String>()
        .parse()
        .unwrap()
}

/// Start an [`ApiServer`] (root password `root`) on a background thread.
fn start_api(
    root: &Path,
    configure: impl FnOnce(ApiServer) -> ApiServer,
) -> (Arc<ApiServer>, SocketAddr, thread::JoinHandle<ShutdownReport>) {
    let server = Arc::new(configure(ApiServer::bind(root, "root", "127.0.0.1:0").unwrap()));
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    (server, addr, serving)
}

/// Join a serving thread, failing the test if it does not finish in time.
fn join_within(serving: thread::JoinHandle<ShutdownReport>, limit: Duration) -> ShutdownReport {
    let t0 = Instant::now();
    while !serving.is_finished() {
        assert!(t0.elapsed() < limit, "the server did not shut down within {limit:?}");
        thread::sleep(Duration::from_millis(20));
    }
    serving.join().unwrap()
}

// ---- /shutdown and /restart routes ------------------------------------------

#[test]
fn shutdown_route_requires_root_or_system_credentials() {
    let root = tmp_dir("shutdown_creds");
    let (server, addr, serving) = start_api(&root, |s| s.with_system_password("s3cret"));

    // Missing credentials: StatusParamIsIllegal (1003), still HTTP 200.
    let (s, body) = request(&addr, "POST", "/shutdown", "");
    assert_eq!(s, 200, "{body}");
    assert_eq!(status_code(&body), 1003, "{body}");
    let (_, body) = request(&addr, "GET", "/shutdown?username=root", "");
    assert_eq!(status_code(&body), 1003, "{body}");
    // Wrong root password, wrong system password: AuthenticationFailed (1001).
    let (s, body) = request(&addr, "GET", "/shutdown?username=root&password=nope", "");
    assert_eq!((s, status_code(&body)), (200, 1001), "{body}");
    let (_, body) = request(
        &addr,
        "POST",
        "/shutdown",
        r#"{"username":"system","password":"wrong"}"#,
    );
    assert_eq!(status_code(&body), 1001, "{body}");
    // Any other user has no right to stop the server.
    let (_, body) = request(&addr, "POST", "/shutdown", "username=joe&password=x");
    assert_eq!(status_code(&body), 1001, "{body}");
    assert!(body.contains("no rights"), "{body}");
    // `encryption=1` with the MD5 digest of a wrong password.
    let digest = md5_hex("nope");
    let (_, body) = request(
        &addr,
        "GET",
        &format!("/shutdown?username=root&password={digest}&encryption=1"),
        "",
    );
    assert_eq!(status_code(&body), 1001, "{body}");

    // None of that stopped the server.
    assert!(!server.shutdown_handle().is_shutdown());
    let (s, _) = request(&addr, "GET", "/", "");
    assert_eq!(s, 200);

    // Root with a JSON body (what upstream's SDKs send) stops it.
    let (s, body) = request(
        &addr,
        "POST",
        "/shutdown",
        r#"{"username":"root","password":"root"}"#,
    );
    assert_eq!(s, 200, "{body}");
    assert_eq!(status_code(&body), 0, "{body}");
    assert!(body.contains("Server stopped successfully."), "{body}");
    let report = join_within(serving, Duration::from_secs(20));
    assert!(report.drained, "{report:?}");
    assert!(!server.shutdown_handle().restart_requested());
    assert!(TcpStream::connect(addr).is_err(), "the listener is closed");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shutdown_route_accepts_the_system_user_from_headers_and_md5() {
    let root = tmp_dir("shutdown_system");
    let (server, addr, serving) = start_api(&root, |s| s.with_system_password("4711"));
    assert_eq!(server.system_password(), "4711");
    // Upstream's `/shutdown` reads `username`/`password` from headers;
    // `encryption=1` means the password is its MD5 hex digest.
    let digest = md5_hex("4711");
    let (s, body) = request_h(
        &addr,
        "GET",
        "/grpc/shutdown",
        &[("username", "system"), ("password", &digest), ("encryption", "1")],
        "",
    );
    assert_eq!((s, status_code(&body)), (200, 0), "{body}");
    join_within(serving, Duration::from_secs(20));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shutdown_route_checks_root_through_the_api_credential_check() {
    // `/shutdown` verifies root with the `/api` layer's credential check, so
    // root's MD5 digest (`encryption=1`) works there exactly as for any
    // `/api` operation.
    let root = tmp_dir("shutdown_root_md5");
    let (server, addr, serving) = start_api(&root, |s| s);
    let digest = md5_hex("root");
    let (_, body) = request(
        &addr,
        "POST",
        "/api",
        &format!(r#"{{"operation":"login","username":"root","password":"{digest}","encryption":"1"}}"#),
    );
    assert_eq!(status_code(&body), 0, "{body}");
    let (s, body) = request(
        &addr,
        "GET",
        &format!("/shutdown?username=root&password={digest}&encryption=1"),
        "",
    );
    assert_eq!((s, status_code(&body)), (200, 0), "{body}");
    join_within(serving, Duration::from_secs(20));
    assert!(!server.shutdown_handle().restart_requested());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shutdown_route_saves_unsaved_changes_like_sigterm() {
    let root = tmp_dir("shutdown_saves");
    let (_server, addr, serving) = start_api(&root, |s| s.with_check_point(false));
    let auth = [("Authorization", "Basic cm9vdDpyb290")]; // root:root
    let (s, body) = request_h(
        &addr,
        "POST",
        "/build?db_name=g",
        &auth,
        "<http://ex/a> <http://ex/p> <http://ex/b> .",
    );
    assert_eq!(s, 200, "{body}");
    let (s, body) = request_h(
        &addr,
        "POST",
        "/update?db_name=g",
        &auth,
        "INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/o> }",
    );
    assert_eq!(s, 200, "{body}");
    let (_, body) = request(&addr, "GET", "/shutdown?username=root&password=root", "");
    assert_eq!(status_code(&body), 0, "{body}");
    let report = join_within(serving, Duration::from_secs(20));
    assert_eq!(report.saved, vec![root.join("g.db")], "{report:?}");
    let mut db = gstore::Database::open(root.join("g.db")).unwrap();
    let r = db
        .query("ASK { <http://ex/new> <http://ex/p> <http://ex/o> }")
        .unwrap();
    assert_eq!(r, gstore::QueryResult::Ask(true), "the update survived /shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restart_route_requests_a_restart() {
    let root = tmp_dir("restart_route");
    let (server, addr, serving) = start_api(&root, |s| s);
    let (_, body) = request(&addr, "POST", "/restart", "username=root&password=bad");
    assert_eq!(status_code(&body), 1001, "{body}");
    let (s, body) = request(&addr, "POST", "/restart", "username=root&password=root");
    assert_eq!((s, status_code(&body)), (200, 0), "{body}");
    assert!(body.contains("Server restarted successfully."), "{body}");
    join_within(serving, Duration::from_secs(20));
    let handle = server.shutdown_handle();
    assert!(handle.is_shutdown() && handle.restart_requested());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_api_server_built_from_conf_ini_uses_its_settings() {
    let root = tmp_dir("bind_with_config");
    let cfg = gstore::config::GStoreConfig::from_ini_str(&format!(
        "[server]\nport=0\nthread_num=2\nkeep_alive_timeout=0\nmax_output_size=7\ncors=off\n\
         [system]\ndb_home={}\nroot_password=cfgpw\nsystem_username=sys\ncheck_point=off\n\
         licensetype=trial\n\
         [upload]\nupload_max_body_size=64\n",
        root.display()
    ));
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    let server = Arc::new(ApiServer::bind_with_config(cfg).unwrap());
    assert_eq!(server.root_dir(), root.as_path());
    assert_eq!(server.http_options().threads, 2);
    assert_eq!(server.http_options().max_body, 64);
    assert!(!server.check_point());
    assert_eq!(server.config().max_output_size, 7);
    // The `/api` layer runs with the file's max_output_size and licensetype.
    assert_eq!(server.gapi_config().max_output_size, 7);
    assert_eq!(server.gapi_config().license_type, "trial");
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());

    // root_password seeds the user store.
    let (s, body) = request(&addr, "POST", "/login", "username=root&password=cfgpw");
    assert_eq!(s, 200, "{body}");
    // The upstream `/api` protocol is served too, reporting the licensetype.
    let (s, body) = request(&addr, "GET", "/api?operation=login&username=root&password=cfgpw", "");
    assert_eq!((s, status_code(&body)), (200, 0), "{body}");
    assert!(body.contains(r#""licensetype":"trial""#), "{body}");
    // keep_alive_timeout=0 ⇒ every response closes; cors=off ⇒ no CORS header.
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut resp = String::new();
    sock.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("Connection: close"), "{resp}");
    assert!(!resp.contains("Access-Control-Allow-Origin"), "{resp}");
    // upload_max_body_size caps request bodies.
    let (s, _) = request(&addr, "POST", "/login", &"x".repeat(100));
    assert_eq!(s, 413);
    // system_username names the user that may stop the server.
    let pw = server.system_password().to_string();
    let (_, body) = request(&addr, "GET", &format!("/shutdown?username=system&password={pw}"), "");
    assert_eq!(status_code(&body), 1001, "only `sys` is the system user here: {body}");
    let (_, body) = request(&addr, "GET", &format!("/shutdown?username=sys&password={pw}"), "");
    assert_eq!(status_code(&body), 0, "{body}");
    join_within(serving, Duration::from_secs(20));
    let _ = std::fs::remove_dir_all(&root);
}

// ---- the gserver binary ------------------------------------------------------

#[cfg(unix)]
mod binary {
    use super::*;

    /// A port that was free a moment ago.
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Run `gserver args…` in `cwd` to completion (bounded); returns
    /// `(exit code, stdout, stderr)`.
    fn gserver(cwd: &Path, args: &[&str]) -> (i32, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gserver"))
            .args(args)
            .current_dir(cwd)
            .env_remove("GSTORE_GSERVER_BACKGROUND")
            .env_remove("GSTORE_GSERVER_SUPERVISOR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gserver");
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let out_t = thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let err_t = thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let t0 = Instant::now();
        let code = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st.code().unwrap_or(-1);
            }
            if t0.elapsed() > Duration::from_secs(150) {
                let _ = child.kill();
                panic!("gserver {args:?} did not finish");
            }
            thread::sleep(Duration::from_millis(20));
        };
        (code, out_t.join().unwrap(), err_t.join().unwrap())
    }

    /// The pid file's lines: pid, system password, background, address,
    /// supervisor.
    fn pid_lines(pid_file: &Path) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(pid_file).ok()?;
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        (lines.len() >= 5).then_some(lines)
    }

    fn alive(pid: &str) -> bool {
        let probe = Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rfind(')').map(|i| s[i + 1..].trim_start().starts_with('Z')))
            .unwrap_or(false);
        probe && !zombie
    }

    fn wait_dead(pid: &str, limit: Duration) -> bool {
        let t0 = Instant::now();
        while alive(pid) {
            if t0.elapsed() > limit {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn sigkill(pid: &str) {
        let _ = Command::new("kill").args(["-9", pid]).stderr(Stdio::null()).status();
    }

    /// Kills whatever the pid file names when dropped (test failure cleanup),
    /// then removes the directory.
    struct Cleanup {
        dir: PathBuf,
        pid_file: PathBuf,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(lines) = pid_lines(&self.pid_file) {
                for pid in [&lines[4], &lines[0]] {
                    if pid != "0" && !pid.is_empty() {
                        sigkill(pid);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The `http://ADDR` a gserver invocation printed.
    fn printed_addr(out: &str) -> SocketAddr {
        out.split("http://")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|a| a.trim_end_matches(',').parse().ok())
            .unwrap_or_else(|| panic!("no address in: {out}"))
    }

    fn addr_of(pid_file: &Path) -> SocketAddr {
        pid_lines(pid_file).unwrap()[3].parse().unwrap()
    }

    #[test]
    fn background_start_status_refusal_and_stop() {
        let dir = tmp_dir("bin_bg");
        let home = dir.join("home");
        let home_s = home.to_string_lossy().into_owned();
        let pid_file = home.join("gserver.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };

        let (code, out, err) = gserver(&dir, &["-b", "--api", &home_s, "-p", "0", "--password", "pw"]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        let addr = printed_addr(&out);
        let lines = pid_lines(&pid_file).expect("pid file written");
        let pid = lines[0].clone();
        assert!(alive(&pid));
        assert!(!lines[1].is_empty(), "the system password is recorded");
        assert_eq!(lines[2], "1", "started in the background");
        assert_eq!(lines[3], addr.to_string());
        assert_eq!(lines[4], "0", "no supervisor");
        assert!(home.join("gserver.log").exists(), "output goes to the log file");

        // It serves the multi-database API.
        let (s, body) = request_h(&addr, "POST", "/login", &[], "username=root&password=pw");
        assert_eq!(s, 200, "{body}");

        // -S reports it.
        let (code, out, _) = gserver(&dir, &["-S", "--api", &home_s]);
        assert_eq!(code, 0);
        assert!(out.contains("Active: active (running)"), "{out}");
        assert!(out.contains(&format!("Main PID: {pid}")), "{out}");
        assert!(out.contains(&addr.to_string()), "{out}");

        // A second start — background or foreground — is refused.
        let (code, out, _) = gserver(&dir, &["-b", "--api", &home_s, "-p", "0"]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("already running"), "{out}");
        let (code, out, _) = gserver(&dir, &["--api", &home_s, "-p", "0"]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("the server already running."), "{out}");

        // -t stops it gracefully and removes the pid file.
        let (code, out, err) = gserver(&dir, &["-t", "--api", &home_s]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert!(out.contains("stop successfully"), "{out}");
        assert!(wait_dead(&pid, Duration::from_secs(5)));
        assert!(!pid_file.exists());
        let log = std::fs::read_to_string(home.join("gserver.log")).unwrap();
        assert!(log.contains("gserver: shut down"), "{log}");

        let (code, out, _) = gserver(&dir, &["-S", "--api", &home_s]);
        assert_eq!(code, 0);
        assert!(out.contains("Active: inactive (dead)"), "{out}");
        let (code, out, _) = gserver(&dir, &["-t", "--api", &home_s]);
        assert_eq!(code, 0);
        assert!(out.contains("server is inactive (dead)."), "{out}");
    }

    #[test]
    fn a_stale_pid_file_does_not_block_a_start() {
        let dir = tmp_dir("bin_stale");
        let home = dir.join("home");
        let home_s = home.to_string_lossy().into_owned();
        let pid_file = home.join("gserver.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };
        std::fs::create_dir_all(&home).unwrap();
        // A pid that is certainly not a running gserver.
        let mut child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(&pid_file, format!("{dead}\nx\n0\n127.0.0.1:1\n0\n")).unwrap();
        let (code, out, _) = gserver(&dir, &["-S", "--api", &home_s]);
        assert!(code == 0 && out.contains("inactive (dead)"), "{out}");
        let (code, out, err) = gserver(&dir, &["-b", "--api", &home_s, "-p", "0"]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        let (code, _, _) = gserver(&dir, &["-t", "--api", &home_s]);
        assert_eq!(code, 0);
    }

    #[test]
    fn conf_ini_configures_the_server_and_flags_override_it() {
        let dir = tmp_dir("bin_conf");
        let home = dir.join("dbhome");
        let pid_file = home.join("gserver.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };
        let (p1, p2) = (free_port(), free_port());
        let ini = dir.join("my.ini");
        std::fs::write(
            &ini,
            format!(
                "[server]\nport={p1}\nthread_num=4\nunknown_key=1\n\
                 [system]\ndb_home=\"{}\"\nroot_password=\"frominit\"\n",
                home.display()
            ),
        )
        .unwrap();
        let ini_s = ini.to_string_lossy().into_owned();

        // conf.ini supplies port, db_home (hence the pid file) and root's password.
        let (code, out, err) = gserver(&dir, &["-b", "-c", &ini_s]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert!(err.contains("unknown key 'unknown_key'"), "unknown keys warn: {err}");
        assert_eq!(addr_of(&pid_file).port(), p1);
        let addr: SocketAddr = format!("127.0.0.1:{p1}").parse().unwrap();
        let (s, body) = request(&addr, "POST", "/login", "username=root&password=frominit");
        assert_eq!(s, 200, "root_password from conf.ini: {body}");
        // The default mode serves the upstream `/api` protocol on the same
        // users (per-request credentials, the gStore envelope).
        let (s, body) = request(&addr, "GET", "/api?operation=login&username=root&password=frominit", "");
        assert_eq!((s, status_code(&body)), (200, 0), "{body}");
        assert!(body.contains("\"CoreVersion\""), "{body}");
        let (s, body) = request(&addr, "POST", "/", r#"{"operation":"check"}"#);
        assert_eq!((s, status_code(&body)), (200, 0), "{body}");
        let log = std::fs::read_to_string(home.join("gserver.log")).unwrap();
        assert!(log.contains("4 worker threads"), "thread_num from conf.ini: {log}");
        let (code, _, _) = gserver(&dir, &["-t", "-c", &ini_s]);
        assert_eq!(code, 0);

        // A command-line flag beats conf.ini.
        let (code, out, err) = gserver(&dir, &["-b", "-c", &ini_s, "-p", &p2.to_string()]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert_eq!(addr_of(&pid_file).port(), p2);
        let (code, _, _) = gserver(&dir, &["-t", "-c", &ini_s]);
        assert_eq!(code, 0);

        // `./conf/conf.ini` in the working directory is read by default.
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        std::fs::copy(&ini, dir.join("conf/conf.ini")).unwrap();
        let (code, out, err) = gserver(&dir, &["-b"]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert_eq!(addr_of(&pid_file).port(), p1);
        let (code, _, _) = gserver(&dir, &["-t"]);
        assert_eq!(code, 0);

        // A -c file that cannot be read is an error.
        let (code, _, err) = gserver(&dir, &["-S", "-c", "missing.ini"]);
        assert_eq!(code, 1);
        assert!(err.contains("cannot read configuration file"), "{err}");
    }

    #[test]
    fn kill_and_restart_commands() {
        let dir = tmp_dir("bin_kill");
        let home = dir.join("home");
        let home_s = home.to_string_lossy().into_owned();
        let pid_file = home.join("gserver.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };
        let (code, out, err) = gserver(&dir, &["-b", "--api", &home_s, "-p", "0"]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        let first = pid_lines(&pid_file).unwrap()[0].clone();

        // -r stops the background server and starts it again in the background.
        let (code, out, err) = gserver(&dir, &["-r", "--api", &home_s, "-p", "0"]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert!(out.contains("stop server...") && out.contains("start server..."), "{out}");
        assert!(wait_dead(&first, Duration::from_secs(5)));
        let lines = pid_lines(&pid_file).unwrap();
        let second = lines[0].clone();
        assert_ne!(first, second);
        assert!(alive(&second));
        assert_eq!(lines[2], "1", "restarted in the background");
        let (s, _) = request(&addr_of(&pid_file), "GET", "/", "");
        assert_eq!(s, 200);

        // -k force-stops it.
        let (code, out, _) = gserver(&dir, &["-k", "--api", &home_s]);
        assert_eq!(code, 0);
        assert!(out.contains("forcibly stopped"), "{out}");
        assert!(wait_dead(&second, Duration::from_secs(5)));
        assert!(!pid_file.exists());
    }

    #[test]
    fn the_restart_route_reexecutes_the_server_in_place() {
        let dir = tmp_dir("bin_restart_route");
        let home = dir.join("home");
        let home_s = home.to_string_lossy().into_owned();
        let pid_file = home.join("gserver.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };
        let port = free_port().to_string();
        let (code, out, err) = gserver(
            &dir,
            &["-b", "--api", &home_s, "-p", &port, "--password", "pw", "--no-checkpoint"],
        );
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        let addr = printed_addr(&out);
        let before = pid_lines(&pid_file).unwrap();
        let auth = [("Authorization", "Basic cm9vdDpwdw==")]; // root:pw
        let (s, body) = request_h(
            &addr,
            "POST",
            "/build?db_name=r",
            &auth,
            "<http://ex/a> <http://ex/p> <http://ex/b> .",
        );
        assert_eq!(s, 200, "{body}");
        let (s, body) = request_h(
            &addr,
            "POST",
            "/update?db_name=r",
            &auth,
            "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        );
        assert_eq!(s, 200, "{body}");

        let (s, body) = request(&addr, "POST", "/restart", r#"{"username":"root","password":"pw"}"#);
        assert_eq!((s, status_code(&body)), (200, 0), "{body}");

        // Same pid, a fresh system password, and serving again.
        let t0 = Instant::now();
        let after = loop {
            if let Some(l) = pid_lines(&pid_file) {
                if l[1] != before[1] && TcpStream::connect(addr).is_ok() {
                    break l;
                }
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "the server did not come back");
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(after[0], before[0], "re-executed in place");
        assert_eq!(after[2], "1", "still a background server");
        let log = std::fs::read_to_string(home.join("gserver.log")).unwrap();
        assert!(log.contains("saved database to"), "unsaved changes were saved: {log}");
        assert!(log.contains("gserver: restarting"), "{log}");
        // The update made before the restart is there after it.
        let (s, body) = request_h(&addr, "POST", "/load?db_name=r", &auth, "");
        assert_eq!(s, 200, "{body}");
        let (s, body) = request_h(
            &addr,
            "POST",
            "/sparql?db_name=r",
            &auth,
            "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }",
        );
        assert_eq!(s, 200, "{body}");
        assert!(body.contains("true"), "{body}");

        // Upstream's `gserver -t` path: the system user with the pid file's
        // password.
        let (s, body) = request(
            &addr,
            "GET",
            &format!("/shutdown?username=system&password={}", after[1]),
            "",
        );
        assert_eq!((s, status_code(&body)), (200, 0), "{body}");
        assert!(wait_dead(&after[0], Duration::from_secs(30)));
        assert!(!pid_file.exists(), "a stopped server removes its pid file");
    }

    #[test]
    fn the_deamon_supervisor_restarts_a_killed_server() {
        let dir = tmp_dir("bin_deamon");
        let home = dir.join("home");
        let pid_file = home.join("gserver.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };
        let ini = dir.join("d.ini");
        std::fs::write(
            &ini,
            format!(
                "[server]\nport={}\ndeamon=on\n[system]\ndb_home={}\n",
                free_port(),
                home.display()
            ),
        )
        .unwrap();
        let ini_s = ini.to_string_lossy().into_owned();
        let (code, out, err) = gserver(&dir, &["-b", "-c", &ini_s]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        let lines = pid_lines(&pid_file).unwrap();
        let (server, supervisor) = (lines[0].clone(), lines[4].clone());
        assert_ne!(supervisor, "0", "a supervisor watches the server");
        assert!(alive(&supervisor));
        let (code, out, _) = gserver(&dir, &["-S", "-c", &ini_s]);
        assert!(code == 0 && out.contains("Supervisor PID"), "{out}");

        // Crash the server: the supervisor starts a new one.
        sigkill(&server);
        let t0 = Instant::now();
        let restarted = loop {
            if let Some(l) = pid_lines(&pid_file) {
                if l[0] != server && alive(&l[0]) && TcpStream::connect(addr_of(&pid_file)).is_ok() {
                    break l;
                }
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "not restarted");
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(restarted[4], supervisor);

        // A graceful stop ends the server and its supervisor.
        let (code, out, err) = gserver(&dir, &["-t", "-c", &ini_s]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert!(wait_dead(&restarted[0], Duration::from_secs(10)));
        assert!(wait_dead(&supervisor, Duration::from_secs(10)), "the supervisor exited");
    }

    #[test]
    fn the_legacy_single_database_invocation_works_with_the_lifecycle() {
        let dir = tmp_dir("bin_single");
        let db_dir = dir.join("lubm.db");
        gstore::Database::build_from_str("lubm", "<http://ex/a> <http://ex/p> <http://ex/b> .")
            .unwrap()
            .save(&db_dir)
            .unwrap();
        let db_s = db_dir.to_string_lossy().into_owned();
        let pid_file = dir.join("lubm.db.pid");
        let _cleanup = Cleanup {
            dir: dir.clone(),
            pid_file: pid_file.clone(),
        };
        let (code, out, err) = gserver(&dir, &["-b", &db_s, "-p", "0", "--no-checkpoint"]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        let addr = printed_addr(&out);
        assert!(pid_file.exists(), "the pid file sits next to the database");
        let (s, body) = request(
            &addr,
            "POST",
            "/update",
            "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }",
        );
        assert_eq!(s, 200, "{body}");
        // Keep-alive: two requests on one connection.
        let sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = sock.try_clone().unwrap();
        let mut r = BufReader::new(sock);
        for _ in 0..2 {
            w.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut head = String::new();
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            assert!(head.contains("Connection: keep-alive"), "{head}");
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).unwrap();
            assert!(String::from_utf8_lossy(&body).contains("\"triples\":2"));
        }
        drop((w, r));
        let (code, out, err) = gserver(&dir, &["-t", &db_s]);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        assert!(!pid_file.exists());
        let mut db = gstore::Database::open(&db_dir).unwrap();
        let r = db
            .query("ASK { <http://ex/x> <http://ex/p> <http://ex/y> }")
            .unwrap();
        assert_eq!(r, gstore::QueryResult::Ask(true), "saved on the graceful stop");
    }
}
