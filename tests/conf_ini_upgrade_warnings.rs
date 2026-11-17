//! Round-4 review of bulkcli#9's fix: `conf.ini` values that earlier
//! gstore-rust versions cut at a glued inline comment (`key=value #note`)
//! are now kept whole, as upstream reads them. A path that changes meaning
//! on upgrade must not change silently — least of all `ip_deny_path`, whose
//! list then names no file and reads as empty, letting every address in.
//! `gserver` reports both: the value it now keeps, and a configured IP list
//! file it cannot read.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::config::GStoreConfig;
use gstore::gapi::ipfilter::IpFilter;

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_conf_upgrade_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_glued_note_after_ip_deny_path_is_reported_and_so_is_the_missing_list() {
    let dir = TmpDir::new("lib");
    let deny = dir.0.join("ipDeny.config");
    std::fs::write(&deny, "10.1.2.3\n").unwrap();
    let text = format!("[server]\nip_deny_path={} #blocked hosts\n", deny.display());
    let cfg = GStoreConfig::from_ini_str(&text);
    let kept = PathBuf::from(format!("{} #blocked hosts", deny.display()));
    assert_eq!(cfg.ip_deny_path.as_deref(), Some(kept.as_path()));
    assert_eq!(cfg.warnings().len(), 1, "{:?}", cfg.warnings());
    assert!(
        cfg.warnings()[0].starts_with("line 2: '#blocked hosts' after the value of 'ip_deny_path'"),
        "{:?}",
        cfg.warnings()
    );
    // The list as configured names no file: an empty deny list.
    assert!(IpFilter::from_config(&cfg).permits("10.1.2.3"));
    let w = IpFilter::list_file_warnings(&cfg);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].starts_with("ip_deny_path '"), "{w:?}");
    assert!(
        w[0].contains("does not exist; the deny list is empty"),
        "{w:?}"
    );

    // A readable list, or none configured: nothing to say.
    let mut ok = GStoreConfig::default();
    ok.ip_deny_path = Some(deny.clone());
    assert!(IpFilter::list_file_warnings(&ok).is_empty());
    assert!(!IpFilter::from_config(&ok).permits("10.1.2.3"));
    ok.ip_deny_path = None;
    assert!(IpFilter::list_file_warnings(&ok).is_empty());

    // The built-in deny list missing at its place (`./conf/ipDeny.config`,
    // which the shipped example names too) is the out-of-the-box state:
    // nothing to say.
    assert!(IpFilter::list_file_warnings(&GStoreConfig::default()).is_empty());
    let shipped = GStoreConfig::from_ini_str(include_str!("../conf/conf.ini.example"));
    assert!(IpFilter::list_file_warnings(&shipped).is_empty());

    // An allow list that cannot be read lets nobody in: also said.
    let mut allow = GStoreConfig::default();
    allow.ip_deny_path = None;
    allow.ip_allow_path = Some(dir.0.clone());
    let w = IpFilter::list_file_warnings(&allow);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(
        w[0].contains("is a directory; the allow list is empty"),
        "{w:?}"
    );
}

#[cfg(unix)]
fn gserver(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    use std::process::{Command, Stdio};
    let out = Command::new(env!("CARGO_BIN_EXE_gserver"))
        .args(args)
        .current_dir(cwd)
        .env_remove("GSTORE_GSERVER_BACKGROUND")
        .env_remove("GSTORE_GSERVER_SUPERVISOR")
        .env_remove("GSTORE_ROOT_PASSWORD_FILE")
        .stdin(Stdio::null())
        .output()
        .expect("run gserver");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `gserver` reports a configuration it reads (here through `-t`, which
/// reports it and stops nothing): the kept note and the list file it names
/// no longer exists.
#[cfg(unix)]
#[test]
fn gserver_reports_the_kept_note_and_the_unreadable_deny_list() {
    let dir = TmpDir::new("gserver");
    let home = dir.0.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let home_s = home.to_string_lossy().into_owned();
    std::fs::write(dir.0.join("ipDeny.config"), "10.1.2.3\n").unwrap();
    let conf = dir.0.join("upgrade.ini");
    std::fs::write(
        &conf,
        "[server]\nip_deny_path=./ipDeny.config #blocked hosts\n\
         [system]\ndb_home=./home #primary store\n",
    )
    .unwrap();
    let conf_s = conf.to_string_lossy().into_owned();
    let (_, out, err) = gserver(&dir.0, &["-t", "--api", &home_s, "-c", &conf_s]);
    let all = format!("{out}\n{err}");
    for expected in [
        "conf.ini warning: line 2: '#blocked hosts' after the value of 'ip_deny_path' is part \
         of the value",
        "conf.ini warning: line 4: '#primary store' after the value of 'db_home' is part of \
         the value",
        "conf.ini warning: ip_deny_path './ipDeny.config #blocked hosts' does not exist; the \
         deny list is empty",
    ] {
        assert!(all.contains(expected), "missing {expected:?} in:\n{all}");
    }
}
