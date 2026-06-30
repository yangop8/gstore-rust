//! Data test: the `gbuild` → `gquery` command-line pipeline.
//!
//! Runs the actual compiled binaries (via Cargo's `CARGO_BIN_EXE_*` paths) to
//! validate argument handling, persistence on disk, and result printing.

use std::path::PathBuf;
use std::process::Command;

fn small_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/small/small.nt")
}

fn db_name() -> PathBuf {
    std::env::temp_dir().join("gstore_dt_cli_small")
}

#[test]
fn gbuild_then_gquery_roundtrip() {
    let name = db_name();
    let _ = std::fs::remove_dir_all(format!("{}.db", name.display()));

    // gbuild <name> small.nt
    let build = Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .arg(&name)
        .arg(small_nt())
        .output()
        .expect("run gbuild");
    assert!(build.status.success(), "gbuild failed: {build:?}");
    let build_out = String::from_utf8_lossy(&build.stdout);
    assert!(build_out.contains("triples=25"), "stats: {build_out}");

    // gquery <name> -e "SELECT ..."
    let query = Command::new(env!("CARGO_BIN_EXE_gquery"))
        .arg(&name)
        .arg("-e")
        .arg("SELECT ?o WHERE { <root> <contain> ?o }")
        .output()
        .expect("run gquery");
    assert!(query.status.success(), "gquery failed: {query:?}");
    let out = String::from_utf8_lossy(&query.stdout);
    assert!(out.contains("<node0>"), "missing node0 in:\n{out}");
    assert!(out.contains("<node4>"), "missing node4 in:\n{out}");

    // Row count is reported on stderr.
    let err = String::from_utf8_lossy(&query.stderr);
    assert!(err.contains("5 row(s)"), "row count line: {err}");

    let _ = std::fs::remove_dir_all(format!("{}.db", name.display()));
}

#[test]
fn gquery_update_persists_to_disk() {
    let name = std::env::temp_dir().join("gstore_dt_cli_update");
    let dir = format!("{}.db", name.display());
    let _ = std::fs::remove_dir_all(&dir);

    Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .arg(&name)
        .arg(small_nt())
        .output()
        .expect("gbuild");

    // Apply an INSERT DATA update.
    let upd = Command::new(env!("CARGO_BIN_EXE_gquery"))
        .arg(&name)
        .arg("-e")
        .arg("INSERT DATA { <root> <contain> <nodeNEW> }")
        .output()
        .expect("gquery insert");
    assert!(upd.status.success());

    // A fresh gquery process must see the persisted change.
    let q = Command::new(env!("CARGO_BIN_EXE_gquery"))
        .arg(&name)
        .arg("-e")
        .arg("ASK { <root> <contain> <nodeNEW> }")
        .output()
        .expect("gquery ask");
    let out = String::from_utf8_lossy(&q.stdout);
    assert!(out.trim() == "true", "expected true, got: {out}");

    let _ = std::fs::remove_dir_all(&dir);
}
