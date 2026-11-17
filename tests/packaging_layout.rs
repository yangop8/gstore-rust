//! What the deb/rpm packages, the container image and the systemd unit are
//! made of, checked against the sources they must agree with (the Cargo
//! targets, the workspace's MSRV, the image's feature set) and against what
//! they must guarantee (a crash loop ends, the unit's hardening is in place).
//!
//! * review infra#7: the binary list was kept by hand in five places
//!   (Cargo.toml's deb and rpm assets, the Dockerfile, docs/DEPLOY.md twice)
//!   and had already diverged (DEPLOY.md left out gcompact). The Dockerfile
//!   now installs every binary target (`cargo install --path .`), the deb
//!   takes them from Cargo ("$auto"); the rpm list (cargo-generate-rpm has no
//!   automatic assets) and DEPLOY.md's list are checked here.
//! * review infra#19: one MSRV for the workspace (gnlqa declared 1.82 while
//!   depending on gstore's 1.89) and the image's builder; the packages are
//!   built with the image's features (`pfn-wasm`) and say so.
//! * review infra#15: the rpm installs conf.ini with the group gstore, the
//!   user being created by %pre.
//! * review infra#12: the unit's restart policy reaches the start limit.
//! * review infra#14: the unit's hardening; `systemd-analyze verify` (where
//!   installed) accepts the unit without a warning.
//! * review infra#17: the image's health check is the port-independent
//!   packaging/docker/gstore-healthcheck (tests/container_healthcheck.rs runs
//!   it).
//!
//! Cargo.toml is read line by line (the crate has no TOML parser): the
//! checks rely on its one-asset-per-line layout.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The lines of the TOML table `[header]` (up to the next table header).
fn table<'a>(toml: &'a str, header: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut on = false;
    for line in toml.lines() {
        if line.starts_with('[') {
            on = line.trim() == header;
            continue;
        }
        if on {
            out.push(line);
        }
    }
    out
}

/// The string value of `key = "…"` in `lines`.
fn value<'a>(lines: &[&'a str], key: &str) -> Option<&'a str> {
    lines.iter().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"'))
    })
}

/// The gstore package's binary targets: its `[[bin]]` names, and the files
/// directly in src/bin, which Cargo would add on its own (autobins).
fn bin_targets() -> BTreeSet<String> {
    let toml = read("Cargo.toml");
    let mut names = BTreeSet::new();
    let mut in_bin = false;
    for line in toml.lines() {
        if line.starts_with('[') {
            in_bin = line.trim() == "[[bin]]";
            continue;
        }
        if in_bin {
            if let Some(n) = value(&[line], "name") {
                names.insert(n.to_string());
            }
        }
    }
    for e in fs::read_dir(repo().join("src/bin")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            names.insert(p.file_stem().unwrap().to_string_lossy().into_owned());
        }
    }
    assert!(names.contains("gserver") && names.len() > 10, "{names:?}");
    names
}

#[test]
fn every_package_installs_exactly_the_binary_targets() {
    let targets = bin_targets();
    let toml = read("Cargo.toml");

    // The deb: cargo-deb's "$auto" assets, no hand-kept binary entry.
    let deb = table(&toml, "[package.metadata.deb]");
    assert!(
        deb.iter()
            .any(|l| l.trim().trim_end_matches(',') == "\"$auto\""),
        "the deb takes the binaries from Cargo (\"$auto\")"
    );
    let listed: Vec<_> = deb
        .iter()
        .filter(|l| l.contains("target/release/"))
        .collect();
    assert!(listed.is_empty(), "a hand-kept deb binary list: {listed:?}");

    // The rpm: one /usr/bin asset per binary target, nothing else there.
    let rpm = table(&toml, "[package.metadata.generate-rpm]");
    let rpm_bins: BTreeSet<String> = rpm
        .iter()
        .filter_map(|l| l.split("dest = \"/usr/bin/").nth(1))
        .map(|rest| rest.split('"').next().unwrap().to_string())
        .collect();
    assert_eq!(rpm_bins, targets, "the rpm's /usr/bin assets");
    for b in &targets {
        assert!(
            rpm.iter().any(|l| l.contains(&format!(
                "source = \"target/release/{b}\", dest = \"/usr/bin/{b}\", mode = \"755\""
            ))),
            "rpm asset of {b}"
        );
    }

    // The image: `cargo install` of the package, no list.
    let docker = read("Dockerfile");
    assert!(
        docker.contains("cargo install --path . --locked"),
        "{docker}"
    );
    assert!(
        !docker.contains("target/release/"),
        "a hand-kept image list"
    );
    assert!(docker.contains("COPY --from=builder /out/bin/ /usr/local/bin/"));

    // docs/DEPLOY.md's list of what the packages install.
    let deploy = read("docs/DEPLOY.md");
    let bullet = deploy
        .split("* the binaries ")
        .nth(1)
        .and_then(|rest| rest.split(" in `/usr/bin`").next())
        .expect("DEPLOY.md's list of the packaged binaries");
    let documented: BTreeSet<String> = bullet
        .split(|c: char| c.is_whitespace() || c == '`' || c == ',')
        .filter(|w| !w.is_empty() && *w != "and")
        .map(str::to_string)
        .collect();
    assert_eq!(documented, targets, "DEPLOY.md's binary list");
}

/// review infra#12: the documented Compose service listened on 0.0.0.0
/// without a root password, so gserver refused to start and Compose
/// restarted it forever.
#[test]
fn the_documented_compose_service_sets_a_root_password() {
    let deploy = read("docs/DEPLOY.md");
    let yaml = deploy
        .split("A Compose service.")
        .nth(1)
        .and_then(|rest| rest.split("```yaml\n").nth(1))
        .and_then(|rest| rest.split("```").next())
        .expect("DEPLOY.md's Compose example");
    for want in [
        "GSTORE_ROOT_PASSWORD_FILE: /run/secrets/gstore_root",
        "secrets: [gstore_root]",
        "  gstore_root:\n",
        "file: ./gstore_root",
        // Published deliberately: the host's loopback only.
        "ports: [\"127.0.0.1:9000:9000\"]",
    ] {
        assert!(yaml.contains(want), "{want}: {yaml}");
    }
    assert!(
        !yaml.contains("\t"),
        "YAML indentation must not use tabs: {yaml}"
    );
}

#[test]
fn one_msrv_for_the_workspace_and_the_image() {
    let toml = read("Cargo.toml");
    let root = table(&toml, "[package]");
    let msrv = value(&root, "rust-version")
        .expect("rust-version")
        .to_string();
    for member in ["gnlqa/Cargo.toml", "bench/Cargo.toml"] {
        let text = read(member);
        let pkg = table(&text, "[package]");
        assert_eq!(value(&pkg, "rust-version"), Some(msrv.as_str()), "{member}");
    }
    let docker = read("Dockerfile");
    assert!(
        docker
            .lines()
            .any(|l| l.trim() == format!("ARG RUST_VERSION={msrv}")),
        "the image builds with the MSRV toolchain {msrv}"
    );
}

#[test]
fn the_packages_are_built_with_the_image_features() {
    let docker = read("Dockerfile");
    let features = docker
        .lines()
        .find_map(|l| l.trim().strip_prefix("ARG FEATURES="))
        .expect("the image's FEATURES")
        .trim_matches('"')
        .to_string();
    assert_eq!(features, "pfn-wasm");
    let toml = read("Cargo.toml");
    let deb = table(&toml, "[package.metadata.deb]");
    assert_eq!(
        value(&deb, "features"),
        Some("[\"pfn-wasm\"]"),
        "cargo deb builds with the image's features"
    );
    let description = deb.join("\n");
    assert!(
        description.contains("Built with the pfn-wasm feature"),
        "{description}"
    );
    let rpm = table(&toml, "[package.metadata.generate-rpm]");
    assert!(
        value(&rpm, "summary").is_some_and(|s| s.contains("pfn-wasm")),
        "{rpm:?}"
    );
}

#[test]
fn the_rpm_installs_conf_ini_with_the_gstore_group() {
    let toml = read("Cargo.toml");
    let rpm = table(&toml, "[package.metadata.generate-rpm]");
    let conf = rpm
        .iter()
        .find(|l| l.contains("dest = \"/etc/gstore/conf.ini\""))
        .expect("the rpm's conf.ini asset");
    for want in [
        "mode = \"640\"",
        "group = \"gstore\"",
        "config = \"noreplace\"",
    ] {
        assert!(conf.contains(want), "{want}: {conf}");
    }
    // The group exists before the files are laid down.
    assert_eq!(
        value(&rpm, "pre_install_script"),
        Some("packaging/rpm/pre_install.sh")
    );
    assert!(repo().join("packaging/rpm/pre_install.sh").is_file());
    // %post no longer re-groups conf.ini (the header does).
    let post = read("packaging/rpm/post_install.sh");
    assert!(
        !post
            .lines()
            .any(|l| l.contains("chgrp") && l.contains("conf.ini")),
        "{post}"
    );
}

/// The `[Service]` / `[Unit]` value of `key` in the unit (the first one).
fn unit_value<'a>(unit: &'a str, key: &str) -> Option<&'a str> {
    unit.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

fn unit() -> String {
    read("packaging/systemd/gserver.service")
}

/// Seconds of a systemd time span such as `5s`, `5min` or a bare `300`.
fn seconds(span: &str) -> u64 {
    let span = span.trim();
    if let Some(m) = span.strip_suffix("min") {
        return m.parse::<u64>().unwrap() * 60;
    }
    span.trim_end_matches('s').parse().unwrap()
}

#[test]
fn a_crash_looping_service_reaches_the_start_limit() {
    let unit = unit();
    assert_eq!(unit_value(&unit, "Restart"), Some("on-failure"));
    let restart_sec = seconds(unit_value(&unit, "RestartSec").unwrap_or("100ms"));
    // systemd's defaults (DefaultStartLimitIntervalSec=10s, Burst=5).
    let interval = seconds(unit_value(&unit, "StartLimitIntervalSec").unwrap_or("10s"));
    let burst: u64 = unit_value(&unit, "StartLimitBurst")
        .unwrap_or("5")
        .parse()
        .unwrap();
    // A server that fails at once is started every `restart_sec`; the
    // start that exceeds `burst` starts within `interval` is refused — it
    // comes `burst * restart_sec` after the first. review infra#12: with
    // RestartSec=5s and the defaults it never did (25 s > 10 s).
    assert!(
        burst * restart_sec < interval,
        "{burst} starts every {restart_sec} s never exceed the limit of {interval} s"
    );
    // gserver's own error exits (status 1, usage 2) are not restarted.
    let prevent = unit_value(&unit, "RestartPreventExitStatus").unwrap_or("");
    let codes: Vec<_> = prevent.split_whitespace().collect();
    assert!(codes.contains(&"1") && codes.contains(&"2"), "{prevent}");
}

#[test]
fn the_unit_is_hardened() {
    let unit = unit();
    let lines: BTreeSet<&str> = unit.lines().map(str::trim).collect();
    // review infra#14: what the unit lacked (and what it already had).
    for want in [
        "UMask=0027",
        "NoNewPrivileges=yes",
        "CapabilityBoundingSet=",
        "PrivateTmp=yes",
        "PrivateDevices=yes",
        "ProtectSystem=strict",
        "ProtectHome=yes",
        "ProtectKernelTunables=yes",
        "ProtectKernelModules=yes",
        "ProtectKernelLogs=yes",
        "ProtectControlGroups=yes",
        "ProtectClock=yes",
        "ProtectHostname=yes",
        "ProtectProc=invisible",
        "ReadWritePaths=/var/lib/gstore",
        "RestrictSUIDSGID=yes",
        "RestrictRealtime=yes",
        "RestrictNamespaces=yes",
        "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
        "LockPersonality=yes",
        "MemoryDenyWriteExecute=yes",
        "SystemCallArchitectures=native",
        "SystemCallFilter=@system-service",
        "SystemCallErrorNumber=EPERM",
    ] {
        assert!(lines.contains(want), "the unit lacks {want}");
    }
    // ProtectHome's consequence is documented for administrators.
    let deploy = read("docs/DEPLOY.md");
    assert!(deploy.contains("`ProtectHome=yes`"), "DEPLOY.md");
}

#[test]
fn systemd_analyze_verifies_the_unit() {
    let Some(analyze) = which("systemd-analyze") else {
        eprintln!("systemd-analyze is not installed: skipped");
        return;
    };
    // verify checks that ExecStart's program exists: point it at this
    // build's gserver.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("packaging_layout_unit_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    struct Rm(PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _rm = Rm(dir.clone());
    let text = unit().replace(
        "ExecStart=/usr/bin/gserver",
        &format!("ExecStart={}", env!("CARGO_BIN_EXE_gserver")),
    );
    let file = dir.join("gserver.service");
    fs::write(&file, text).unwrap();
    let out = std::process::Command::new(analyze)
        .arg("verify")
        .arg(&file)
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    // Unknown or misspelt keys are only warnings: none is allowed.
    assert!(said.trim().is_empty(), "{said}");
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    })
}

#[test]
fn the_image_health_check_does_not_hard_code_the_port() {
    let docker = read("Dockerfile");
    let hc: String = docker
        .split("HEALTHCHECK ")
        .nth(1)
        .expect("a HEALTHCHECK")
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(
        hc.contains("CMD [\"/usr/local/bin/gstore-healthcheck\"]"),
        "{hc}"
    );
    assert!(!hc.contains("9000"), "{hc}");
    assert!(
        docker.contains(
            "COPY --chmod=0755 packaging/docker/gstore-healthcheck /usr/local/bin/gstore-healthcheck"
        ),
        "the image installs the health check"
    );
    // The build context lets it in (.dockerignore drops packaging/).
    let ignore = read(".dockerignore");
    assert!(
        ignore
            .lines()
            .any(|l| l.trim() == "!packaging/docker/gstore-healthcheck"),
        "{ignore}"
    );
    assert!(Path::new(&repo().join("packaging/docker/gstore-healthcheck")).is_file());
}
