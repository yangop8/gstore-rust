//! The deb/rpm maintainer scripts around the systemd unit and the gstore
//! user (review infra#15), run against temporary directories.
//!
//! * Debian: `apt remove gstore` followed by `apt install gstore` used to
//!   leave a service the administrator had enabled disabled (prerm ran
//!   `systemctl disable` on remove). The scripts now follow
//!   dh_installsystemd's protocol for a unit the package does not enable
//!   itself: prerm only stops the service, postrm masks the unit on remove
//!   and forgets it on purge, postinst unmasks it and enables it again if
//!   its enable links were still there. The test runs prerm, postrm and
//!   postinst's enablement block with the real `deb-systemd-helper`
//!   (init-system-helpers) in a `DPKG_ROOT` below the test's temp
//!   directory; where the helper is not installed (not Debian) it is
//!   skipped. `systemctl` / `deb-systemd-invoke` are shims that record their
//!   calls, and the scripts see a running systemd (their
//!   `/run/systemd/system` test points into the temp directory), so the old
//!   prerm's `systemctl disable` would show.
//! * RPM: %pre (packaging/rpm/pre_install.sh) creates the gstore user and
//!   group before the files are laid down, so conf.ini gets the group gstore
//!   of the package header; run here with shims of getent and the user tools.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "packaging_scripts_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("shim")).unwrap();
        TmpDir(p)
    }

    /// An executable `shim/<name>` that appends its arguments (and its
    /// standard input, if `stdin`) to `calls.log`, then runs `then`.
    fn shim(&self, name: &str, then: &str) {
        let log = self.0.join("calls.log");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"{name} $*\" >>'{}'\n{then}\n",
            log.display()
        );
        let p = self.0.join("shim").join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.0.join("calls.log")).unwrap_or_default()
    }

    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.0.join("shim").display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    })
}

/// The enablement block of the deb's postinst.
fn enablement_block() -> String {
    let postinst = read("packaging/debian/postinst");
    let start = postinst
        .find("# >>> unit enablement")
        .expect("postinst's unit enablement block");
    let end = postinst[start..]
        .find("# <<< unit enablement")
        .expect("the block's end marker")
        + start;
    postinst[start..end].to_string()
}

/// A dpkg root with the unit installed, systemd "running" (for the
/// scripts' `[ -d /run/systemd/system ]`) and shims for the commands that
/// would act on the real system.
struct DebRoot {
    tmp: TmpDir,
}

impl DebRoot {
    fn new(tag: &str) -> DebRoot {
        let tmp = TmpDir::new(tag);
        fs::create_dir_all(tmp.0.join("root/run/systemd/system")).unwrap();
        // Shims: systemctl disable drops the enable link, as the real one.
        let link = tmp
            .0
            .join("root/etc/systemd/system/multi-user.target.wants/gserver.service");
        tmp.shim(
            "systemctl",
            &format!(
                "case \"$*\" in *disable*) rm -f '{}' ;; esac\nexit 0",
                link.display()
            ),
        );
        tmp.shim("deb-systemd-invoke", "exit 0");
        let root = DebRoot { tmp };
        root.install_unit();
        root
    }

    fn root(&self) -> PathBuf {
        self.tmp.0.join("root")
    }

    fn install_unit(&self) {
        let dir = self.root().join("lib/systemd/system");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("gserver.service"),
            read("packaging/systemd/gserver.service"),
        )
        .unwrap();
    }

    fn remove_unit(&self) {
        fs::remove_file(self.root().join("lib/systemd/system/gserver.service")).unwrap();
    }

    /// `sh -c TEXT name ARGS` as dpkg runs a maintainer script (with
    /// DPKG_ROOT set: dpkg --root), the scripts' `/run/systemd/system` moved
    /// into the root.
    fn run(&self, name: &str, text: &str, args: &[&str]) {
        self.run_as(name, text, args, true);
    }

    /// `sh -c TEXT name ARGS` as dpkg runs a maintainer script on the live
    /// system (no DPKG_ROOT), so its systemd steps run — against the
    /// shims, which only record the call (prerm acts through systemctl /
    /// deb-systemd-invoke alone; review infra#15: with DPKG_ROOT set, prerm
    /// skips its systemd steps and a `systemctl disable` among them would go
    /// unnoticed).
    fn run_live(&self, name: &str, text: &str, args: &[&str]) {
        self.run_as(name, text, args, false);
    }

    fn run_as(&self, name: &str, text: &str, args: &[&str], dpkg_root: bool) {
        let run_dir = self.root().join("run/systemd/system");
        let text = text.replace("/run/systemd/system", &run_dir.to_string_lossy());
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(&text).arg(name).args(args);
        if dpkg_root {
            cmd.env("DPKG_ROOT", self.root());
        } else {
            cmd.env_remove("DPKG_ROOT");
        }
        let out = cmd
            .env("DPKG_MAINTSCRIPT_PACKAGE", "gstore")
            .env("DPKG_MAINTSCRIPT_NAME", name)
            .env("PATH", self.tmp.path_env())
            .output()
            .expect("run sh");
        assert!(
            out.status.success(),
            "{name} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn enable_link(&self) -> PathBuf {
        self.root()
            .join("etc/systemd/system/multi-user.target.wants/gserver.service")
    }

    fn mask_link(&self) -> PathBuf {
        self.root().join("etc/systemd/system/gserver.service")
    }

    /// What `systemctl enable gserver` creates ([Install] WantedBy=).
    fn admin_enables(&self) {
        let link = self.enable_link();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink("/lib/systemd/system/gserver.service", &link).unwrap();
    }

    fn enabled(&self) -> bool {
        fs::symlink_metadata(self.enable_link()).is_ok()
    }

    fn masked(&self) -> bool {
        fs::read_link(self.mask_link()).is_ok_and(|t| t == Path::new("/dev/null"))
    }
}

fn deb_systemd_helper() -> bool {
    if which("deb-systemd-helper").is_some() {
        return true;
    }
    eprintln!("deb-systemd-helper (Debian's init-system-helpers) is not installed: skipped");
    false
}

#[test]
fn removing_and_reinstalling_the_deb_keeps_an_enabled_unit_enabled() {
    if !deb_systemd_helper() {
        return;
    }
    let prerm = read("packaging/debian/prerm");
    let postrm = read("packaging/debian/postrm");
    let block = enablement_block();
    let r = DebRoot::new("reinstall");

    // A fresh install leaves the unit disabled.
    r.run("postinst", &block, &["configure", ""]);
    assert!(!r.enabled(), "the package enabled the unit itself");
    // The administrator enables it (`systemctl enable gserver`).
    r.admin_enables();
    // `apt remove gstore`: prerm (on the live system: its systemd steps
    // run, against the shims), the files go, postrm.
    r.run_live("prerm", &prerm, &["remove"]);
    // prerm stops the running server ...
    assert!(
        r.tmp.calls().contains("stop gserver.service"),
        "prerm did not stop the server: {}",
        r.tmp.calls()
    );
    // ... and (review infra#15) no longer runs `systemctl disable`.
    assert!(
        !r.tmp.calls().contains("disable"),
        "prerm disabled the unit: {}",
        r.tmp.calls()
    );
    assert!(r.enabled(), "prerm dropped the enable link");
    r.remove_unit();
    r.run("postrm", &postrm, &["remove"]);
    assert!(r.masked(), "postrm masks the removed unit");
    // `apt install gstore` again (dpkg passes the configured version).
    r.install_unit();
    r.run("postinst", &block, &["configure", "0.1.0"]);
    assert!(!r.masked(), "postinst unmasks the unit");
    assert!(
        r.enabled(),
        "the unit the administrator enabled is enabled again"
    );
    // An upgrade of an enabled unit keeps it enabled too.
    r.run_live("prerm", &prerm, &["upgrade", "0.1.0"]);
    r.run("postinst", &block, &["configure", "0.1.0"]);
    assert!(r.enabled());
}

#[test]
fn a_disabled_unit_stays_disabled_and_a_purge_forgets_it() {
    if !deb_systemd_helper() {
        return;
    }
    let prerm = read("packaging/debian/prerm");
    let postrm = read("packaging/debian/postrm");
    let block = enablement_block();

    // Never enabled: remove + install leaves it disabled and unmasked.
    let r = DebRoot::new("disabled");
    r.run("postinst", &block, &["configure", ""]);
    r.run_live("prerm", &prerm, &["remove"]);
    r.remove_unit();
    r.run("postrm", &postrm, &["remove"]);
    r.install_unit();
    r.run("postinst", &block, &["configure", "0.1.0"]);
    assert!(!r.enabled() && !r.masked());

    // Enabled, then purged: nothing of it is left; a later install starts
    // over (not enabled). The purge also removes the first-start password
    // file and the ./conf link below the dpkg root.
    let r = DebRoot::new("purge");
    r.run("postinst", &block, &["configure", ""]);
    r.admin_enables();
    let pwfile = r.root().join("etc/gstore/root_password");
    fs::create_dir_all(pwfile.parent().unwrap()).unwrap();
    fs::write(&pwfile, "secret\n").unwrap();
    let conf_link = r.root().join("var/lib/gstore/conf");
    fs::create_dir_all(conf_link.parent().unwrap()).unwrap();
    symlink("/etc/gstore", &conf_link).unwrap();
    r.run_live("prerm", &prerm, &["remove"]);
    r.remove_unit();
    r.run("postrm", &postrm, &["remove"]);
    r.run("postrm", &postrm, &["purge"]);
    assert!(!r.enabled() && !r.masked());
    assert!(!pwfile.exists() && fs::symlink_metadata(&conf_link).is_err());
    r.install_unit();
    r.run("postinst", &block, &["configure", ""]);
    assert!(!r.enabled());
}

/// packaging/sysusers/gstore.conf's `u` line.
fn sysusers_line() -> String {
    read("packaging/sysusers/gstore.conf")
        .lines()
        .find(|l| l.starts_with("u "))
        .expect("the sysusers.d u line")
        .to_string()
}

#[test]
fn the_rpm_pre_script_creates_the_user_before_the_files() {
    let pre = fs::read_to_string(repo().join("packaging/rpm/pre_install.sh"))
        .expect("review infra#15: the rpm has a %pre creating the user");
    assert!(
        pre.contains(&format!("'{}'", sysusers_line())),
        "%pre's sysusers line is packaging/sysusers/gstore.conf's"
    );

    // With systemd-sysusers: the line goes to it (and getent then finds
    // the user and the group).
    let t = TmpDir::new("pre_sysusers");
    let state = t.0.join("created");
    t.shim(
        "getent",
        &format!("[ -e '{}' ] && exit 0\nexit 2", state.display()),
    );
    t.shim(
        "systemd-sysusers",
        &format!(
            "cat >>'{}'\ntouch '{}'\nexit 0",
            t.0.join("calls.log").display(),
            state.display()
        ),
    );
    t.shim("groupadd", "exit 0");
    t.shim("useradd", "exit 0");
    run_pre(&pre, &t);
    let calls = t.calls();
    assert!(
        calls.contains("systemd-sysusers --replace=/usr/lib/sysusers.d/gstore.conf -"),
        "{calls}"
    );
    assert!(calls.contains(&sysusers_line()), "{calls}");
    assert!(
        !calls.contains("groupadd") && !calls.contains("useradd"),
        "{calls}"
    );

    // Without it: groupadd, then useradd into that group.
    let t = TmpDir::new("pre_shadow");
    let group = t.0.join("group");
    let user = t.0.join("user");
    t.shim(
        "getent",
        &format!(
            "case $1 in group) [ -e '{}' ] && exit 0 ;; passwd) [ -e '{}' ] && exit 0 ;; esac\nexit 2",
            group.display(),
            user.display()
        ),
    );
    // Shell builtins only: PATH holds nothing but the shims.
    t.shim("groupadd", &format!(": >'{}'", group.display()));
    t.shim("useradd", &format!(": >'{}'", user.display()));
    // A PATH of the shims alone: no systemd-sysusers (and nothing that
    // could create a real user).
    run_pre_with_path(&pre, &t.0.join("shim").to_string_lossy());
    let calls = t.calls();
    let g = calls.find("groupadd --system gstore").expect(&calls);
    let u = calls.find("useradd --system --gid gstore").expect(&calls);
    assert!(g < u, "{calls}");

    // Both there already: nothing is created.
    let t = TmpDir::new("pre_exists");
    t.shim("getent", "exit 0");
    t.shim("systemd-sysusers", "exit 0");
    t.shim("groupadd", "exit 0");
    t.shim("useradd", "exit 0");
    run_pre(&pre, &t);
    assert_eq!(
        t.calls()
            .lines()
            .filter(|l| !l.starts_with("getent"))
            .count(),
        0
    );
}

fn run_pre(pre: &str, t: &TmpDir) {
    run_pre_with_path(pre, &t.path_env());
}

/// %pre as rpm runs it on a fresh install (`$1` = 1), with `path` as PATH.
fn run_pre_with_path(pre: &str, path: &str) {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(pre)
        .arg("pre_install")
        .arg("1")
        .env("PATH", path)
        .output()
        .expect("run sh");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
