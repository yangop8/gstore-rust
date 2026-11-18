//! The container image's health check, packaging/docker/gstore-healthcheck
//! (review infra#17), run against a real gserver outside Docker: the
//! script's test hooks point it at this server's process instead of PID 1
//! and at this build's gserver.
//!
//! The image used to probe `curl http://127.0.0.1:9000/api?operation=check`:
//! a server on another port (conf.ini, `-p`) or serving HTTPS was reported
//! unhealthy, an IP allow list without 127.0.0.1 refused the probe, and every
//! probe was an access-log record. The script asks `gserver -S` with the
//! server's own arguments, which connects to the address in the pid file
//! without sending a request.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "container_healthcheck_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("work")).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `gserver -s -c CONF -p 0` in `dir/work`, root's password from a file
/// (an explicit one: gserver refuses the default). Killed when dropped.
struct Server {
    child: Child,
    addr: SocketAddr,
}

impl Server {
    fn start(dir: &TmpDir) -> Server {
        let conf = dir.0.join("conf.ini");
        fs::write(&conf, include_str!("../conf/conf.ini.example")).unwrap();
        let pw = dir.0.join("root_password");
        fs::write(&pw, "healthcheck-test-password\n").unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_gserver"))
            .args(["-s", "-c"])
            .arg(&conf)
            .args(["-p", "0"])
            .env("GSTORE_ROOT_PASSWORD_FILE", &pw)
            .env_remove("GSTORE_GSERVER_BACKGROUND")
            .env_remove("GSTORE_GSERVER_SUPERVISOR")
            .current_dir(dir.0.join("work"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gserver");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.split("http://").nth(1) {
                    let a = rest.split_whitespace().next().unwrap_or("").to_string();
                    let _ = tx.send(a);
                }
            }
        });
        // A hang guard only: the line arrives once the server listens.
        let addr = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(a) => a.parse().expect("socket address"),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("gserver did not start");
            }
        };
        Server { child, addr }
    }

    /// SIGTERM (a graceful stop: in-flight requests and their log records
    /// complete), then wait for the exit.
    fn stop(mut self) {
        let st = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
        let exit = self.child.wait().unwrap();
        assert!(exit.success(), "{exit}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run the health check against process `pid`: its exit status and output.
fn health(pid: u32) -> (bool, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/docker/gstore-healthcheck");
    let out = Command::new("/bin/sh")
        .arg(&script)
        .env("GSTORE_HEALTHCHECK_PID", pid.to_string())
        .env("GSTORE_GSERVER", env!("CARGO_BIN_EXE_gserver"))
        .output()
        .expect("run the health check");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn the_health_check_follows_the_server_and_is_not_logged() {
    let dir = TmpDir::new("follow");
    let server = Server::start(&dir);
    let pid = server.child.id();
    // Any free port: not the image's 9000 the old probe hard-coded.
    assert_ne!(server.addr.port(), 9000);
    for _ in 0..3 {
        let (ok, said) = health(pid);
        assert!(ok, "the running server is unhealthy: {said}");
    }
    let access_log = dir.0.join("work/dbhome/access.log");
    assert!(
        !access_log.exists(),
        "a probe was logged as client traffic: {:?}",
        fs::read_to_string(&access_log)
    );
    // One real request, so that the log's absence above means something.
    let mut s = TcpStream::connect(server.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(
        s,
        "GET /api?operation=check HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("\"StatusCode\":0"), "{resp}");
    server.stop();
    let log = fs::read_to_string(&access_log).expect("the request's access-log record");
    assert_eq!(log.lines().count(), 1, "{log}");
    // The server is gone: unhealthy.
    let (ok, said) = health(pid);
    assert!(!ok, "{said}");
}
