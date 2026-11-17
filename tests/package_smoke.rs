//! scripts/package-smoke.sh in its `stage` mode, against the binaries of
//! this test build (review infra#7: the packages and the image were never
//! installed and run anywhere; .github/workflows/packaging.yml runs the same
//! script against the built .deb, .rpm and image).
//!
//! The stage mode lays out the .deb's root in a temporary directory (the
//! [package.metadata.deb] assets, tmpfiles.d's working directory and conf
//! link), runs postinst's first-start password block against it, starts
//! gserver as the systemd unit would (its working directory, environment
//! and ExecStart arguments), checks the container health check, the first
//! start's random root password (the default one is refused), a build, an
//! update and a query, a graceful stop and a second start without the
//! password file. The script needs `curl`; without it this test is skipped.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct TmpDir(PathBuf);

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

/// The gstore package's binary targets: src/bin's files and the [[bin]]
/// names of Cargo.toml.
fn bin_targets(repo: &Path) -> String {
    let mut names = BTreeSet::new();
    for e in fs::read_dir(repo.join("src/bin")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            names.insert(p.file_stem().unwrap().to_string_lossy().into_owned());
        }
    }
    let toml = fs::read_to_string(repo.join("Cargo.toml")).unwrap();
    let mut in_bin = false;
    for line in toml.lines() {
        if line.starts_with('[') {
            in_bin = line.trim() == "[[bin]]";
        } else if in_bin {
            if let Some(v) = line.strip_prefix("name = ") {
                names.insert(v.trim_matches('"').to_string());
            }
        }
    }
    names.into_iter().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_staged_deb_layout_passes_the_package_smoke() {
    if which("curl").is_none() {
        eprintln!("curl is not installed: skipped");
        return;
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmp = TmpDir(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("package_smoke_{}", std::process::id())),
    );
    let _ = fs::remove_dir_all(&tmp.0);
    fs::create_dir_all(&tmp.0).unwrap();
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_gserver")).parent().unwrap();
    let out = Command::new("bash")
        .arg(repo.join("scripts/package-smoke.sh"))
        .arg("stage")
        .arg("--bin-dir")
        .arg(bin_dir)
        .arg("--bins")
        .arg(bin_targets(&repo))
        .env("TMPDIR", &tmp.0)
        .env_remove("GSTORE_GSERVER_BACKGROUND")
        .env_remove("GSTORE_GSERVER_SUPERVISOR")
        .env_remove("GSTORE_ROOT_PASSWORD_FILE")
        .stdin(Stdio::null())
        .output()
        .expect("run bash scripts/package-smoke.sh");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    assert!(said.contains("package smoke: passed (stage)"), "{said}");
    for step in [
        "health check: healthy, no access-log record",
        "requests: check, login (default password refused)",
        "graceful stop",
        "second start without the password file",
    ] {
        assert!(said.contains(step), "{step}: {said}");
    }
    // The script removed its temporary directory.
    assert_eq!(fs::read_dir(&tmp.0).unwrap().count(), 0, "{said}");
}
