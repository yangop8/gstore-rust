//! The packages' first-start root password (review H-10 / infra#3: a fresh
//! deb/rpm install used to serve root/123456 on 127.0.0.1:9000, and gserver
//! now refuses upstream's default password on every address).
//!
//! `packaging/debian/postinst` and `packaging/rpm/post_install.sh` each hold
//! a block between `# >>> first-start root password` and `# <<< …` that, on
//! a fresh install with no user store yet (and `conf.ini` still naming the
//! default password), writes a random password to its own file,
//! `/etc/gstore/root_password` (mode 0640, group gstore). `conf.ini` is never
//! changed: it is a dpkg conffile (Debian Policy 10.7.3; a changed conffile
//! prompts on every later upgrade). The systemd unit names the file in
//! `GSTORE_ROOT_PASSWORD_FILE`, and gserver seeds the user store with it at
//! its first start; a file the variable names but that is gone (an upgrade,
//! or deleted after the password was changed) is skipped. An upgrade, an
//! existing user store or a password the administrator set is left alone.
//! The block takes its paths and group from `GSTORE_PKG_CONF` /
//! `GSTORE_PKG_DATA` / `GSTORE_PKG_PWFILE` / `GSTORE_PKG_GROUP`, so this test
//! runs the very text the packages ship against a temp directory (removed
//! afterwards, also on failure).
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gstore::config::DEFAULT_ROOT_PASSWORD;
use gstore::http_api::ApiServer;

const DEB_POSTINST: &str = include_str!("../packaging/debian/postinst");
const DEB_POSTRM: &str = include_str!("../packaging/debian/postrm");
const RPM_POST: &str = include_str!("../packaging/rpm/post_install.sh");
const UNIT: &str = include_str!("../packaging/systemd/gserver.service");
const EXAMPLE: &str = include_str!("../conf/conf.ini.example");
/// Where the packages put root's first-start password.
const PWFILE: &str = "/etc/gstore/root_password";

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pkg_first_start_{tag}_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("data")).unwrap();
        TmpDir(p)
    }

    fn conf(&self) -> PathBuf {
        self.0.join("conf.ini")
    }

    fn data(&self) -> PathBuf {
        self.0.join("data")
    }

    fn pwfile(&self) -> PathBuf {
        self.0.join("root_password")
    }

    /// An installed `conf.ini` (the shipped example, mode 0640).
    fn install_conf(&self, text: &str) {
        fs::write(self.conf(), text).unwrap();
        fs::set_permissions(self.conf(), fs::Permissions::from_mode(0o640)).unwrap();
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The first-start block of a maintainer script.
fn block(script: &str) -> String {
    let start = script
        .find("# >>> first-start root password")
        .expect("the block's start marker");
    let end = script[start..]
        .find("# <<< first-start root password")
        .expect("the block's end marker")
        + start;
    script[start..end].to_string()
}

/// Run `block` with `sh` and the script arguments `args` against `dir`.
fn run(block: &str, args: &[&str], dir: &TmpDir) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(block)
        .arg("postinst")
        .args(args)
        .env("GSTORE_PKG_CONF", dir.conf())
        .env("GSTORE_PKG_DATA", dir.data())
        .env("GSTORE_PKG_PWFILE", dir.pwfile())
        .env("GSTORE_PKG_GROUP", own_group())
        .output()
        .expect("run sh");
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// The test process's primary group (the block's `chgrp` target here; the
/// packages use `gstore`).
fn own_group() -> String {
    let out = Command::new("id").arg("-gn").output().expect("run id");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The scripts' "fresh install" and "upgrade" arguments.
struct Script {
    name: &'static str,
    text: &'static str,
    fresh: &'static [&'static str],
    upgrade: &'static [&'static str],
}

const SCRIPTS: [Script; 2] = [
    Script {
        name: "deb",
        text: DEB_POSTINST,
        fresh: &["configure", ""],
        upgrade: &["configure", "0.1.0"],
    },
    Script {
        name: "rpm",
        text: RPM_POST,
        fresh: &["1"],
        upgrade: &["2"],
    },
];

#[test]
fn a_fresh_install_gets_a_random_root_password() {
    for s in &SCRIPTS {
        let blk = block(s.text);
        let mut seen = Vec::new();
        for _ in 0..2 {
            let dir = TmpDir::new(s.name);
            dir.install_conf(EXAMPLE);
            let said = run(&blk, s.fresh, &dir);
            assert!(
                said.contains(&dir.pwfile().to_string_lossy().into_owned()),
                "{}: {said}",
                s.name
            );
            // review H-10 / infra#3 (round-1 review): conf.ini is a dpkg
            // conffile and stays byte-identical; the password has a file of
            // its own (the block used to rewrite conf.ini in place).
            assert_eq!(
                fs::read_to_string(dir.conf()).unwrap(),
                EXAMPLE,
                "{}",
                s.name
            );
            assert_eq!(mode(&dir.conf()), 0o640, "{}", s.name);
            let text = fs::read_to_string(dir.pwfile()).unwrap();
            let pw = text.strip_suffix('\n').expect("one line").to_string();
            assert_ne!(pw, DEFAULT_ROOT_PASSWORD, "{}", s.name);
            assert_eq!(pw.len(), 32, "{}: {pw}", s.name);
            assert!(pw.bytes().all(|b| b.is_ascii_hexdigit()), "{pw}");
            assert!(
                !said.contains(&pw),
                "{}: the password is not printed",
                s.name
            );
            assert_eq!(mode(&dir.pwfile()), 0o640, "{}", s.name);
            assert!(!dir.0.join("root_password.gstore-new").exists());
            assert!(!dir.0.join("conf.ini.gstore-new").exists());
            // The first start seeds the user store with it.
            let root = dir.data().join("dbhome");
            let server = ApiServer::bind(&root, &pw, "127.0.0.1:0").unwrap();
            assert!(!server.root_has_default_password(), "{}", s.name);
            seen.push(pw);
        }
        assert_ne!(seen[0], seen[1], "{}: a new password per install", s.name);
    }
}

#[test]
fn upgrades_existing_stores_and_set_passwords_are_left_alone() {
    for s in &SCRIPTS {
        let blk = block(s.text);
        // An upgrade: conf.ini is the administrator's, whatever it says.
        let dir = TmpDir::new(s.name);
        dir.install_conf(EXAMPLE);
        run(&blk, s.upgrade, &dir);
        assert_eq!(
            fs::read_to_string(dir.conf()).unwrap(),
            EXAMPLE,
            "{}",
            s.name
        );
        assert!(!dir.pwfile().exists(), "{}", s.name);

        // A fresh install over an existing user store (a reinstall): the
        // store decides root's password, conf.ini is not touched.
        let dir = TmpDir::new(s.name);
        dir.install_conf(EXAMPLE);
        fs::create_dir_all(dir.data().join("dbhome")).unwrap();
        fs::write(dir.data().join("dbhome/users.db"), "").unwrap();
        run(&blk, s.fresh, &dir);
        assert_eq!(
            fs::read_to_string(dir.conf()).unwrap(),
            EXAMPLE,
            "{}",
            s.name
        );
        assert!(!dir.pwfile().exists(), "{}", s.name);

        // A password already set (e.g. a preseeded conf.ini).
        let dir = TmpDir::new(s.name);
        let custom = EXAMPLE.replace("root_password=\"123456\"", "root_password=\"mine\"");
        assert_ne!(custom, EXAMPLE);
        dir.install_conf(&custom);
        let said = run(&blk, s.fresh, &dir);
        assert!(said.is_empty(), "{said}");
        assert_eq!(
            fs::read_to_string(dir.conf()).unwrap(),
            custom,
            "{}",
            s.name
        );
        assert_eq!(mode(&dir.conf()), 0o640);
        assert!(!dir.pwfile().exists(), "{}", s.name);

        // A password file already there (a second configure run) is kept.
        let dir = TmpDir::new(s.name);
        dir.install_conf(EXAMPLE);
        fs::write(dir.pwfile(), "kept\n").unwrap();
        let said = run(&blk, s.fresh, &dir);
        assert!(said.is_empty(), "{said}");
        assert_eq!(fs::read_to_string(dir.pwfile()).unwrap(), "kept\n");
    }
}

/// The package wiring: the scripts write the file the systemd unit hands to
/// gserver, and a purge of the deb removes it.
#[test]
fn the_unit_hands_the_password_file_to_gserver() {
    assert!(
        UNIT.lines()
            .any(|l| l.trim() == format!("Environment=GSTORE_ROOT_PASSWORD_FILE={PWFILE}")),
        "{UNIT}"
    );
    for s in &SCRIPTS {
        assert!(
            block(s.text).contains(&format!(
                "GSTORE_PKG_PWFILE=${{GSTORE_PKG_PWFILE:-{PWFILE}}}"
            )),
            "{}",
            s.name
        );
    }
    assert!(DEB_POSTRM.contains(&format!("rm -f {PWFILE}")));
}

/// A `gserver -s` child, killed and reaped when dropped.
struct Gserver(Child, SocketAddr);

impl Gserver {
    /// `gserver -s --api ROOT -p 0 -c CONF` in `cwd` with
    /// `GSTORE_ROOT_PASSWORD_FILE=pwfile`: `Err(stderr)` when it exits
    /// without listening.
    fn start(cwd: &Path, root: &Path, conf: &Path, pwfile: &Path) -> Result<Gserver, String> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gserver"))
            .arg("-s")
            .arg("--api")
            .arg(root)
            .args(["-p", "0", "-c"])
            .arg(conf)
            .env("GSTORE_ROOT_PASSWORD_FILE", pwfile)
            .env_remove("GSTORE_GSERVER_BACKGROUND")
            .env_remove("GSTORE_GSERVER_SUPERVISOR")
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gserver");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.split("http://").nth(1) {
                    let addr = rest.split_whitespace().next().unwrap_or("").to_string();
                    let _ = tx.send(addr);
                }
            }
        });
        // A hang guard only.
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(a) => Ok(Gserver(child, a.parse().expect("socket address"))),
            Err(_) => {
                let _ = child.kill();
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                let _ = child.wait();
                Err(err)
            }
        }
    }

    /// Whether root logs in with `password`.
    fn root_logs_in(&self, password: &str) -> bool {
        let body = format!("username=root&password={password}");
        let mut s = TcpStream::connect(self.1).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        write!(
            s,
            "POST /login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: \
             application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp.split_whitespace().nth(1) == Some("200")
    }
}

impl Drop for Gserver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The installed flow end to end: a fresh install's file seeds root's
/// password at gserver's first start; once the file is gone (deleted after
/// the password was changed, or never there: an upgrade) the unit's
/// `GSTORE_ROOT_PASSWORD_FILE` still names it and gserver starts on the
/// existing user store instead of failing to read it.
#[test]
fn gserver_seeds_root_from_the_file_and_starts_without_it_later() {
    let dir = TmpDir::new("gserver");
    dir.install_conf(EXAMPLE);
    run(&block(DEB_POSTINST), &["configure", ""], &dir);
    let pw = fs::read_to_string(dir.pwfile()).unwrap().trim().to_string();
    let root = dir.data().join("dbhome");
    let first = Gserver::start(&dir.0, &root, &dir.conf(), &dir.pwfile()).expect("first start");
    assert!(first.root_logs_in(&pw), "root's password is the file's");
    assert!(!first.root_logs_in(DEFAULT_ROOT_PASSWORD));
    drop(first);
    assert!(root.join("users.db").exists());
    fs::remove_file(dir.pwfile()).unwrap();
    let again = Gserver::start(&dir.0, &root, &dir.conf(), &dir.pwfile())
        .unwrap_or_else(|err| panic!("a start without the file failed: {err}"));
    assert!(
        again.root_logs_in(&pw),
        "the user store keeps root's password"
    );
}
