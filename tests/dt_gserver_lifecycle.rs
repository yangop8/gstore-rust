//! Server lifecycle: the `gserver` CLI (`-b`/`-S`/`-t`/`-r`/`-k`, pid file,
//! conf.ini, `deamon` supervisor) driven through the real binary, and the
//! upstream `/shutdown` / `/restart` routes, keep-alive and CORS of
//! [`ApiServer`].
//!
//! Every binary test runs in its own temporary database home with `-p 0`
//! (the port actually bound is read back from the pid file) and kills any
//! process it started when it ends, also on failure.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;

/// `root:root` as an HTTP Basic credential.
const ROOT_BASIC: &str = "Basic cm9vdDpyb290";

fn tmp_home(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gstore_lifecycle_{tag}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Send one request (`Connection: close`) and return `(status, head, body)`.
fn http(addr: &SocketAddr, method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> (u16, String, String) {
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
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status: {resp:?}"));
    (status, head.to_string(), body.to_string())
}

/// The `"StatusCode"` of a gStore JSON envelope.
fn status_code(body: &str) -> i64 {
    let i = body.find("\"StatusCode\":").unwrap_or_else(|| panic!("no StatusCode: {body}"));
    body[i + 13..]
        .trim_start()
        .split(|c: char| !(c == '-' || c.is_ascii_digit()))
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------
// ApiServer: /shutdown, /restart, keep-alive, CORS (in-process)
// ---------------------------------------------------------------------------

fn start_api(home: &Path, tune: impl FnOnce(ApiServer) -> ApiServer) -> (Arc<ApiServer>, SocketAddr, thread::JoinHandle<gstore::server::ShutdownReport>) {
    let server = Arc::new(tune(ApiServer::bind(home, "root", "127.0.0.1:0").unwrap()));
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    (server, addr, serving)
}

#[test]
fn shutdown_route_requires_root_or_system_credentials() {
    let home = tmp_home("shutdown_route");
    let (server, addr, serving) = start_api(&home, |s| s.with_system_password("s3cret"));

    // No credentials: illegal parameter, at HTTP 200 (upstream envelope).
    let (st, _, body) = http(&addr, "POST", "/shutdown", &[], "");
    assert_eq!((st, status_code(&body)), (200, 1003), "{body}");
    // Wrong root password (JSON body), wrong system password (headers), and a
    // user that is neither: authentication failed.
    let (st, _, body) = http(&addr, "POST", "/shutdown", &[("Content-Type", "application/json")], r#"{"username":"root","password":"nope"}"#);
    assert_eq!((st, status_code(&body)), (200, 1001), "{body}");
    let (_, _, body) = http(&addr, "GET", "/shutdown", &[("username", "system"), ("password", "guess")], "");
    assert_eq!(status_code(&body), 1001, "{body}");
    let (_, _, body) = http(&addr, "GET", "/shutdown?username=alice&password=x", &[], "");
    assert_eq!(status_code(&body), 1001, "{body}");
    assert!(!server.shutdown_handle().is_shutdown(), "still serving");
    let (st, _, _) = http(&addr, "GET", "/status", &[("Authorization", ROOT_BASIC)], "");
    assert_eq!(st, 200);

    // Root's credentials (form body) stop it gracefully.
    let (st, head, body) = http(
        &addr,
        "POST",
        "/shutdown",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        "username=root&password=root",
    );
    assert_eq!((st, status_code(&body)), (200, 0), "{body}");
    assert!(body.contains("Server stopped successfully."), "{body}");
    assert!(head.contains("Connection: close"), "{head}");
    let report = serving.join().unwrap();
    assert!(report.drained && !report.restart, "{report:?}");
    assert!(TcpStream::connect(addr).is_err(), "the listener is closed");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn system_account_from_the_pid_file_may_stop_and_restart() {
    let home = tmp_home("system_route");
    let (_server, addr, serving) = start_api(&home, |s| s.with_system_password("s3cret"));
    // Upstream's `gserver -t` sends the system credentials as headers.
    let (_, _, body) = http(&addr, "POST", "/restart", &[("username", "system"), ("password", "s3cret")], "");
    assert_eq!(status_code(&body), 0, "{body}");
    assert!(body.contains("Server restarted successfully."), "{body}");
    let report = serving.join().unwrap();
    assert!(report.restart, "a /restart is reported as such: {report:?}");

    // Without a configured system password the system account cannot log in.
    let (_server, addr, serving) = start_api(&home, |s| s);
    let (_, _, body) = http(&addr, "POST", "/shutdown", &[("username", "system"), ("password", "")], "");
    assert_eq!(status_code(&body), 1003, "{body}");
    let (_, _, body) = http(&addr, "POST", "/shutdown", &[("username", "system"), ("password", "x")], "");
    assert_eq!(status_code(&body), 1001, "{body}");
    let (_, _, body) = http(&addr, "GET", "/shutdown?username=root&password=root", &[], "");
    assert_eq!(status_code(&body), 0, "{body}");
    assert!(!serving.join().unwrap().restart);
    let _ = std::fs::remove_dir_all(&home);
}

/// Read one `Content-Length`-framed response from a kept-alive connection.
fn read_one<R: BufRead>(r: &mut R) -> (String, String) {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "EOF in head: {head}");
        if line == "\r\n" {
            break;
        }
        head.push_str(&line);
    }
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .map(|v| v.trim().parse().unwrap())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    (head, String::from_utf8(body).unwrap())
}

#[test]
fn api_server_keeps_connections_alive_and_answers_preflights() {
    let home = tmp_home("keepalive");
    let (server, addr, serving) = start_api(&home, |s| s.with_keep_alive(Some(Duration::from_secs(5)), 10));
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut w = s.try_clone().unwrap();
    let mut r = BufReader::new(s);
    for target in ["/status", "/show"] {
        w.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: {ROOT_BASIC}\r\n\r\n").as_bytes())
            .unwrap();
        let (head, body) = read_one(&mut r);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Connection: keep-alive"), "{head}");
        assert!(!body.is_empty());
    }
    // A /shutdown on the kept-alive connection closes it after its response.
    w.write_all(b"GET /shutdown?username=root&password=root HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let (head, body) = read_one(&mut r);
    assert!(head.contains("Connection: close"), "{head}");
    assert_eq!(status_code(&body), 0, "{body}");
    let mut rest = String::new();
    r.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "");
    serving.join().unwrap();
    drop(server);

    // CORS: upstream's /file/* preflights, and a configured origin everywhere.
    let (server, addr, serving) = start_api(&home, |s| s);
    let (st, head, body) = http(&addr, "OPTIONS", "/file/upload", &[("Origin", "http://ui")], "");
    assert_eq!(st, 200);
    assert!(head.contains("Access-Control-Allow-Origin: *"), "{head}");
    assert!(head.contains("Access-Control-Allow-Methods: POST"), "{head}");
    assert_eq!(body, "ok");
    server.shutdown();
    serving.join().unwrap();
    let cfg = GStoreConfig::from_ini_str("[server]\ncors_allow_origin=http://ui\n");
    let (server, addr, serving) = start_api(&home, |s| s.with_config(cfg));
    let (_, head, _) = http(&addr, "GET", "/status", &[("Authorization", ROOT_BASIC)], "");
    assert!(head.contains("Access-Control-Allow-Origin: http://ui"), "{head}");
    assert_eq!(server.config().cors_allow_origin.as_deref(), Some("http://ui"));
    assert_eq!(server.config().db_home, home, "db_home is the server's root");
    server.shutdown();
    serving.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn api_server_worker_pool_is_bounded_by_thread_num() {
    let home = tmp_home("pool");
    let (server, addr, serving) = start_api(&home, |s| s.with_thread_num(2));
    // N = 2 connections stuck mid-request pin both workers…
    let head = format!("POST /show HTTP/1.1\r\nHost: x\r\nAuthorization: {ROOT_BASIC}\r\nContent-Length: 1\r\nConnection: close\r\n\r\n");
    let mut slow: Vec<TcpStream> = (0..2)
        .map(|_| {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(head.as_bytes()).unwrap();
            s
        })
        .collect();
    thread::sleep(Duration::from_millis(300));
    // …so the N + 1st waits in the queue…
    let mut extra = TcpStream::connect(addr).unwrap();
    extra
        .write_all(format!("GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: {ROOT_BASIC}\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    extra.set_read_timeout(Some(Duration::from_millis(700))).unwrap();
    let mut b = [0u8; 1];
    let early = extra.read(&mut b);
    assert!(
        matches!(&early, Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "the N+1st connection must wait for a worker: {early:?}"
    );
    // …until one of them completes.
    slow[0].write_all(b" ").unwrap();
    let mut done = String::new();
    slow[0].read_to_string(&mut done).unwrap();
    assert!(done.starts_with("HTTP/1.1 200"), "{done}");
    extra.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut resp = String::new();
    extra.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    slow[1].write_all(b" ").unwrap();
    server.shutdown();
    serving.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// The gserver binary
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod binary {
    use super::*;
    use std::process::{Command, Output};

    /// Kills whatever a test started under `home` (from the pid file) when
    /// dropped, then removes the directory.
    struct Home {
        dir: PathBuf,
        seen: std::cell::RefCell<Vec<u32>>,
    }

    impl Home {
        fn new(tag: &str) -> Home {
            Home {
                dir: tmp_home(tag),
                seen: Default::default(),
            }
        }

        fn pid_file(&self) -> PathBuf {
            self.dir.join("gstore.pid")
        }

        /// `(pid, system password, background, port, supervisor)`.
        fn pid_info(&self) -> Option<(u32, String, bool, u16, u32)> {
            let text = std::fs::read_to_string(self.pid_file()).ok()?;
            let l: Vec<&str> = text.lines().collect();
            let info = (
                l.first()?.parse().ok()?,
                l.get(1)?.to_string(),
                *l.get(2)? == "1",
                l.get(3)?.parse().ok()?,
                l.get(4)?.parse().ok()?,
            );
            let mut seen = self.seen.borrow_mut();
            seen.push(info.0);
            if info.4 != 0 {
                seen.push(info.4);
            }
            Some(info)
        }

        fn addr(&self) -> SocketAddr {
            let (_, _, _, port, _) = self.pid_info().expect("pid file");
            SocketAddr::from(([127, 0, 0, 1], port))
        }

        /// Run `gserver ARGS --db-home <home> -p 0` from inside the home.
        fn run(&self, args: &[&str]) -> Output {
            self.run_raw(&[args, &["--db-home", self.dir.to_str().unwrap(), "-p", "0"]].concat())
        }

        fn run_raw(&self, args: &[&str]) -> Output {
            let out = Command::new(env!("CARGO_BIN_EXE_gserver"))
                .args(args)
                .current_dir(&self.dir)
                .output()
                .expect("run gserver");
            let _ = self.pid_info();
            out
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.join("gserver.log")).unwrap_or_default()
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = self.pid_info();
            for pid in self.seen.borrow().iter() {
                if alive(*pid) {
                    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Whether `pid` is a live (non-zombie) process.
    fn alive(pid: u32) -> bool {
        let proc_dir = Path::new("/proc");
        if proc_dir.is_dir() {
            return std::fs::read(proc_dir.join(pid.to_string()).join("cmdline"))
                .is_ok_and(|c| !c.is_empty());
        }
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn text(o: &Output) -> String {
        format!(
            "status {:?}\n--- stdout\n{}--- stderr\n{}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    }

    #[test]
    fn background_status_stop_lifecycle() {
        let home = Home::new("bg");
        let out = home.run(&["-b", "--password", "root"]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        assert!(text(&out).contains("started in the background"), "{}", text(&out));
        let (pid, syspw, background, port, supervisor) = home.pid_info().expect("pid file");
        assert!(alive(pid) && background && port != 0 && supervisor == 0);
        assert_eq!(syspw.len(), 32, "a generated system password");
        let addr = home.addr();
        let (st, _, body) = http(&addr, "GET", "/status", &[("Authorization", ROOT_BASIC)], "");
        assert_eq!(st, 200, "{body}");

        // -S reports it; a second start is refused (foreground or background).
        let out = home.run(&["-S"]);
        let s = text(&out);
        assert!(out.status.success(), "{s}");
        assert!(s.contains("Active: active (running)"), "{s}");
        assert!(s.contains(&format!("Main PID: {pid}")), "{s}");
        assert!(s.contains(&format!("Port: {port}")), "{s}");
        for cmd in ["-b", "-s"] {
            let out = home.run(&[cmd]);
            assert_eq!(out.status.code(), Some(1), "{}", text(&out));
            assert!(text(&out).contains("already running"), "{}", text(&out));
        }
        assert!(alive(pid), "the refused starts left the server alone");

        // -t stops it gracefully, waits for the exit and clears the pid file.
        let out = home.run(&["-t"]);
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains(&format!("the Server [{pid}] stop successfully.")), "{}", text(&out));
        assert!(!alive(pid), "-t returns after the server exited");
        assert!(!home.pid_file().exists());
        let log = home.log();
        assert!(log.contains("gserver: shut down"), "{log}");
        let out = home.run(&["-S"]);
        assert!(text(&out).contains("Active: inactive (dead)"), "{}", text(&out));
        let out = home.run(&["-t"]);
        assert!(out.status.success() && text(&out).contains("server is inactive (dead)."), "{}", text(&out));
    }

    #[test]
    fn kill_force_stops_and_removes_the_pid_file() {
        let home = Home::new("kill");
        let out = home.run(&["-b", "--password", "root"]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        let (pid, ..) = home.pid_info().unwrap();
        let out = home.run(&["-k"]);
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("forcibly stopped"), "{}", text(&out));
        assert!(!alive(pid));
        assert!(!home.pid_file().exists());
        // A stale pid file (dead pid) does not block the next start.
        std::fs::write(home.pid_file(), format!("{pid}\nx\n1\n1\n0\n")).unwrap();
        let out = home.run(&["-S"]);
        assert!(text(&out).contains("inactive (dead)"), "{}", text(&out));
        let out = home.run(&["-b", "--password", "root"]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        let (pid2, ..) = home.pid_info().unwrap();
        assert_ne!(pid, pid2);
        assert!(home.run(&["-t"]).status.success());
    }

    #[test]
    fn restart_command_and_route() {
        let home = Home::new("restart");
        assert!(home.run(&["-b", "--password", "root"]).status.success(), "{}", home.log());
        let (pid1, ..) = home.pid_info().unwrap();
        // -r of a background server restarts it in the background.
        let out = home.run(&["-r", "--password", "root"]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        let s = text(&out);
        assert!(s.contains("stop server...") && s.contains("start server..."), "{s}");
        let (pid2, _, background, ..) = home.pid_info().unwrap();
        assert!(pid2 != pid1 && background && !alive(pid1));

        // /restart (root's credentials) re-executes the same process: the pid
        // stays, the system password is regenerated, the port answers again.
        let (_, syspw, _, port, _) = home.pid_info().unwrap();
        let (st, _, body) = http(
            &home.addr(),
            "POST",
            "/restart",
            &[("Content-Type", "application/json")],
            r#"{"username": "root", "password": "root"}"#,
        );
        assert_eq!((st, status_code(&body)), (200, 0), "{body}");
        wait_until("the re-executed server", Duration::from_secs(30), || {
            home.pid_info().is_some_and(|(p, pw, _, newport, _)| {
                p == pid2
                    && pw != syspw
                    && TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], newport))).is_ok()
            })
        });
        let _ = port;
        let log = home.log();
        assert!(log.contains("gserver: restarting"), "{log}");
        let (st, _, _) = http(&home.addr(), "GET", "/status", &[("Authorization", ROOT_BASIC)], "");
        assert_eq!(st, 200);

        // /shutdown with the system account from the pid file stops it.
        let (pid, syspw, ..) = home.pid_info().unwrap();
        let (_, _, body) = http(&home.addr(), "POST", "/shutdown", &[("username", "system"), ("password", &syspw)], "");
        assert_eq!(status_code(&body), 0, "{body}");
        wait_until("the server to exit", Duration::from_secs(30), || !alive(pid));
        assert!(!home.pid_file().exists(), "a clean exit removes the pid file");
    }

    #[test]
    fn conf_ini_settings_and_cli_precedence() {
        let home = Home::new("conf");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let conf = home.dir.join("my.ini");
        std::fs::write(
            &conf,
            format!(
                "[server]\nport={port}\nthread_num=3\nbogus_key=1\n[system]\ndb_home={}\nroot_password=\"root\"\n",
                home.dir.display()
            ),
        )
        .unwrap();
        let conf_s = conf.to_str().unwrap();
        // The port comes from the file (no -p here).
        let out = home.run_raw(&["-b", "-c", conf_s]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        let (_, _, _, bound, _) = home.pid_info().unwrap();
        assert_eq!(bound, port, "conf.ini port");
        let log = home.log();
        assert!(log.contains("3 worker threads"), "{log}");
        assert!(log.contains("unknown key 'bogus_key'"), "an unknown key only warns: {log}");
        // root_password from the file seeded the user store.
        let (st, _, _) = http(&home.addr(), "GET", "/status", &[("Authorization", ROOT_BASIC)], "");
        assert_eq!(st, 200);
        // -S / -t find the server through the same conf.ini (db_home).
        assert!(text(&home.run_raw(&["-S", "-c", conf_s])).contains("active (running)"));
        assert!(home.run_raw(&["-t", "-c", conf_s]).status.success());

        // A command-line port overrides the file's.
        let out = home.run_raw(&["-b", "-c", conf_s, "-p", "0"]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        let (_, _, _, bound, _) = home.pid_info().unwrap();
        assert_ne!(bound, port, "-p beats conf.ini");
        assert!(home.run_raw(&["-t", "-c", conf_s]).status.success());

        // A missing explicit config file is an error.
        let out = home.run_raw(&["-S", "-c", "/no/such/conf.ini"]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    }

    #[test]
    fn deamon_supervisor_restarts_a_crashed_server() {
        let home = Home::new("deamon");
        let conf = home.dir.join("conf.ini");
        std::fs::write(&conf, "[server]\ndeamon=on\n[system]\nroot_password=root\n").unwrap();
        let conf_s = conf.to_str().unwrap();
        let out = home.run(&["-b", "-c", conf_s]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        let (pid, _, _, _, supervisor) = home.pid_info().unwrap();
        assert!(supervisor != 0 && alive(supervisor) && alive(pid), "supervised");

        // The server dies from SIGKILL: the supervisor starts a new one.
        assert!(Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
        wait_until("a restarted server", Duration::from_secs(30), || {
            home.pid_info().is_some_and(|(p, _, _, port, sup)| {
                p != pid
                    && sup == supervisor
                    && TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).is_ok()
            })
        });
        let (pid2, ..) = home.pid_info().unwrap();
        assert!(home.log().contains("stopped abnormally"), "{}", home.log());

        // -t stops the supervisor and the server for good.
        let out = home.run(&["-t", "-c", conf_s]);
        assert!(out.status.success(), "{}\nlog:\n{}", text(&out), home.log());
        assert!(!alive(pid2) && !alive(supervisor));
        thread::sleep(Duration::from_millis(1500));
        assert!(!home.pid_file().exists(), "nothing was restarted after the stop");
    }

    #[test]
    fn single_database_mode_stays_foreground_only() {
        let home = Home::new("single");
        let db = home.dir.join("x.db");
        let out = home.run_raw(&[db.to_str().unwrap(), "-b"]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("manage the multi-database server"), "{}", text(&out));
        let out = home.run_raw(&["--help"]);
        assert!(out.status.success());
        assert!(text(&out).contains("-b, --background"), "{}", text(&out));
    }
}
