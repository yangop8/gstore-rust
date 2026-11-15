//! The packages' first-start root password (review H-10 / infra#3: a fresh
//! deb/rpm install used to serve root/123456 on 127.0.0.1:9000, and gserver
//! now refuses upstream's default password on every address).
//!
//! `packaging/debian/postinst` and `packaging/rpm/post_install.sh` each hold
//! a block between `# >>> first-start root password` and `# <<< …` that, on
//! a fresh install with no user store yet, replaces `root_password="123456"`
//! in `/etc/gstore/conf.ini` with a random password, keeping the file's mode;
//! an upgrade, an existing user store or a password the administrator set is
//! left alone. The block takes its paths from `GSTORE_PKG_CONF` /
//! `GSTORE_PKG_DATA`, so this test runs the very text the packages ship
//! against a temp directory (removed afterwards, also on failure).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::config::{GStoreConfig, DEFAULT_ROOT_PASSWORD};
use gstore::http_api::ApiServer;

const DEB_POSTINST: &str = include_str!("../packaging/debian/postinst");
const RPM_POST: &str = include_str!("../packaging/rpm/post_install.sh");
const EXAMPLE: &str = include_str!("../conf/conf.ini.example");

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
            assert!(said.contains("root_password"), "{}: {said}", s.name);
            let text = fs::read_to_string(dir.conf()).unwrap();
            let cfg = GStoreConfig::from_ini_str(&text);
            assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
            let pw = cfg.root_password.clone();
            assert_ne!(pw, DEFAULT_ROOT_PASSWORD, "{}", s.name);
            assert_eq!(pw.len(), 32, "{}: {pw}", s.name);
            assert!(pw.bytes().all(|b| b.is_ascii_hexdigit()), "{pw}");
            assert!(
                !said.contains(&pw),
                "{}: the password is not printed",
                s.name
            );
            // Only that line changed, the mode is kept, no copy is left.
            let changed: Vec<_> = EXAMPLE
                .lines()
                .zip(text.lines())
                .filter(|(a, b)| a != b)
                .collect();
            assert_eq!(changed.len(), 1, "{}: {changed:?}", s.name);
            assert_eq!(EXAMPLE.lines().count(), text.lines().count());
            assert_eq!(mode(&dir.conf()), 0o640, "{}", s.name);
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
    }
}
