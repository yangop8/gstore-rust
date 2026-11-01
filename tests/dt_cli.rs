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

/// Run `cmd` with its standard output a pipe whose reader is already gone
/// (what a tool sees after `| head -1` exited), returning its status and
/// standard error.
fn run_with_closed_stdout(cmd: &mut Command) -> (std::process::ExitStatus, String) {
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    let out = cmd
        .stdout(writer)
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the tool");
    (
        out.status,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A tool whose standard output is a closed pipe (`gserver -S | head -1`,
/// `gquery … | head`) stops writing and exits with its usual status — no
/// "failed printing to stdout: Broken pipe" panic, no error message.
#[test]
fn a_closed_stdout_pipe_ends_the_output_quietly() {
    let work = std::env::temp_dir().join(format!("gstore_dt_cli_epipe_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let name = work.join("epipe");
    let build = Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .arg(&name)
        .arg(small_nt())
        .output()
        .expect("run gbuild");
    assert!(build.status.success(), "gbuild failed: {build:?}");
    let db = format!("{}.db", name.display());

    let quiet = |what: &str, (status, stderr): (std::process::ExitStatus, String)| {
        assert!(!stderr.contains("panicked"), "{what} panicked: {stderr}");
        assert!(
            !stderr.contains("Broken pipe"),
            "{what} reported the pipe: {stderr}"
        );
        assert!(status.success(), "{what} exited with {status}: {stderr}");
    };
    // Every tool's help.
    for exe in [
        env!("CARGO_BIN_EXE_gbuild"),
        env!("CARGO_BIN_EXE_gquery"),
        env!("CARGO_BIN_EXE_gadd"),
        env!("CARGO_BIN_EXE_gsub"),
        env!("CARGO_BIN_EXE_gdrop"),
        env!("CARGO_BIN_EXE_gexport"),
        env!("CARGO_BIN_EXE_gbackup"),
        env!("CARGO_BIN_EXE_grestore"),
        env!("CARGO_BIN_EXE_gshow"),
        env!("CARGO_BIN_EXE_gmonitor"),
        env!("CARGO_BIN_EXE_gserver"),
        env!("CARGO_BIN_EXE_gconsole"),
    ] {
        quiet(exe, run_with_closed_stdout(Command::new(exe).arg("--help")));
    }
    // Status and listing output.
    quiet(
        "gserver -S",
        run_with_closed_stdout(
            Command::new(env!("CARGO_BIN_EXE_gserver"))
                .arg("-S")
                .current_dir(&work),
        ),
    );
    quiet(
        "gshow",
        run_with_closed_stdout(Command::new(env!("CARGO_BIN_EXE_gshow")).current_dir(&work)),
    );
    quiet(
        "gshow <db>",
        run_with_closed_stdout(Command::new(env!("CARGO_BIN_EXE_gshow")).arg(&name)),
    );
    quiet(
        "gmonitor <db>",
        run_with_closed_stdout(Command::new(env!("CARGO_BIN_EXE_gmonitor")).arg(&name)),
    );
    // A query result streamed to the closed pipe.
    quiet(
        "gquery -e",
        run_with_closed_stdout(
            Command::new(env!("CARGO_BIN_EXE_gquery"))
                .arg(&name)
                .arg("-e")
                .arg("SELECT ?s ?p ?o WHERE { ?s ?p ?o }"),
        ),
    );
    // The embedded console reading its commands from standard input.
    let script = work.join("script.txt");
    std::fs::write(&script, "SELECT ?s WHERE { ?s ?p ?o };\nquit\n").unwrap();
    quiet(
        "gconsole --local",
        run_with_closed_stdout(
            Command::new(env!("CARGO_BIN_EXE_gconsole"))
                .arg("--local")
                .arg(&db)
                .stdin(std::fs::File::open(&script).unwrap()),
        ),
    );
    // The output is gone, but the work was done: a mutating tool finished.
    quiet(
        "gadd",
        run_with_closed_stdout(
            Command::new(env!("CARGO_BIN_EXE_gadd"))
                .arg(&name)
                .arg(small_nt()),
        ),
    );
    let _ = std::fs::remove_dir_all(&work);
}
