//! review cluster-nlqa#15: a `gnlqa.conf` in the current directory is not
//! read unless explicitly configured, so an untrusted checkout cannot point
//! the LLM / gStore endpoints (and the credentials the environment supplies)
//! at another host. Runs the real `gnlqa` binary in a scratch directory.

mod common;

use std::process::Command;

use common::TempDir;

/// The planted config of an untrusted checkout.
const PLANTED: &str = "\
GNLQA_LLM_PROVIDER=openai
OPENAI_BASE_URL=http://collector.invalid/v1
GSTORE_ENDPOINT=http://collector.invalid/sparql
";

/// Run `gnlqa` (no arguments: it prints its configuration) in `cwd` with a
/// clean environment plus `env`; returns stderr.
fn run_gnlqa(cwd: &std::path::Path, env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gnlqa"));
    cmd.current_dir(cwd).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run the gnlqa binary");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn config_in_the_current_directory_is_ignored() {
    let work = TempDir::new("cwd-conf");
    let home = TempDir::new("cwd-home");
    std::fs::write(work.path().join("gnlqa.conf"), PLANTED).unwrap();
    let err = run_gnlqa(
        work.path(),
        &[
            ("HOME", home.path().to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-USER-REAL-SECRET"),
        ],
    );
    assert!(
        !err.contains("collector.invalid"),
        "a gnlqa.conf in the working directory was applied:\n{err}"
    );
    assert!(err.contains("provider=anthropic"), "{err}");
    assert!(
        err.contains("ignoring ./gnlqa.conf"),
        "the ignored file should be pointed out:\n{err}"
    );
    assert!(!err.contains("sk-USER-REAL-SECRET"), "{err}");
}

#[test]
fn an_explicit_gnlqa_config_path_is_read() {
    let work = TempDir::new("explicit-conf");
    let home = TempDir::new("explicit-home");
    std::fs::write(work.path().join("gnlqa.conf"), PLANTED).unwrap();
    let err = run_gnlqa(
        work.path(),
        &[
            ("HOME", home.path().to_str().unwrap()),
            ("GNLQA_CONFIG", "./gnlqa.conf"),
        ],
    );
    assert!(
        err.contains("gstore=http://collector.invalid/sparql"),
        "{err}"
    );
    assert!(err.contains("provider=openai"), "{err}");
    assert!(!err.contains("ignoring"), "{err}");
}

#[test]
fn the_per_user_config_directory_is_read() {
    let work = TempDir::new("xdg-work");
    let xdg = TempDir::new("xdg-config");
    std::fs::create_dir_all(xdg.path().join("gnlqa")).unwrap();
    std::fs::write(
        xdg.path().join("gnlqa").join("gnlqa.conf"),
        "GSTORE_ENDPOINT=http://127.0.0.1:9/from-xdg\n",
    )
    .unwrap();
    let err = run_gnlqa(
        work.path(),
        &[
            ("HOME", work.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", xdg.path().to_str().unwrap()),
        ],
    );
    assert!(err.contains("gstore=http://127.0.0.1:9/from-xdg"), "{err}");

    // $HOME/.config/gnlqa/gnlqa.conf when XDG_CONFIG_HOME is unset
    let home = TempDir::new("home-config");
    std::fs::create_dir_all(home.path().join(".config").join("gnlqa")).unwrap();
    std::fs::write(
        home.path().join(".config").join("gnlqa").join("gnlqa.conf"),
        "GSTORE_ENDPOINT=http://127.0.0.1:9/from-home\n",
    )
    .unwrap();
    let err = run_gnlqa(work.path(), &[("HOME", home.path().to_str().unwrap())]);
    assert!(err.contains("gstore=http://127.0.0.1:9/from-home"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_world_writable_config_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new("ww-work");
    let conf = work.path().join("shared.conf");
    std::fs::write(&conf, "GSTORE_ENDPOINT=http://collector.invalid/sparql\n").unwrap();
    std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o666)).unwrap();
    let err = run_gnlqa(
        work.path(),
        &[
            ("HOME", work.path().to_str().unwrap()),
            ("GNLQA_CONFIG", conf.to_str().unwrap()),
        ],
    );
    assert!(!err.contains("gstore=http://collector.invalid"), "{err}");
    assert!(err.contains("world-writable"), "{err}");
}

#[test]
fn library_lookup_never_reads_the_working_directory() {
    let work = TempDir::new("lib-cwd");
    std::fs::write(work.path().join("gnlqa.conf"), PLANTED).unwrap();
    let cfg = gnlqa::Config::from_lookup(|_| None, Some(work.path()));
    assert_eq!(cfg.llm_provider, "anthropic");
    assert_eq!(cfg.gstore_endpoint, "http://127.0.0.1:9000/sparql");
}
