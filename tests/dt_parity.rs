//! Data test: the parity CLI tools (gadd / gsub / gshow / gexport / gbackup /
//! grestore / gdrop / gmonitor) plus the new RDF input formats reachable through
//! them. Runs the actual compiled binaries via Cargo's `CARGO_BIN_EXE_*` paths.

use std::path::{Path, PathBuf};
use std::process::Command;

fn small_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/small/small.nt")
}

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("gstore_dt_parity_{tag}"))
}

fn run(bin: &str, args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {bin}: {e}"))
}

fn osstr(s: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(s)
}

/// Build a fresh db from small.nt at `<name>.db`, returning the db dir.
fn build_db(name: &Path) -> String {
    let dir = format!("{}.db", name.display());
    let _ = std::fs::remove_dir_all(&dir);
    let out = run(
        env!("CARGO_BIN_EXE_gbuild"),
        &[name.as_os_str(), small_nt().as_os_str()],
    );
    assert!(out.status.success(), "gbuild failed: {out:?}");
    dir
}

#[test]
fn full_cli_lifecycle() {
    let name = tmp("life");
    let dir = build_db(&name);

    // gshow reports the 25 triples small.nt holds.
    let show = run(env!("CARGO_BIN_EXE_gshow"), &[name.as_os_str()]);
    assert!(show.status.success(), "gshow failed: {show:?}");
    let show_out = String::from_utf8_lossy(&show.stdout);
    assert!(show_out.contains("triples   : 25"), "gshow: {show_out}");

    // gmonitor prints monitoring info.
    let mon = run(env!("CARGO_BIN_EXE_gmonitor"), &[name.as_os_str()]);
    assert!(mon.status.success(), "gmonitor failed: {mon:?}");
    let mon_out = String::from_utf8_lossy(&mon.stdout);
    assert!(mon_out.contains("triple_num"), "gmonitor: {mon_out}");
    assert!(mon_out.contains("disk_used"), "gmonitor: {mon_out}");

    // gadd: insert two new triples from an N-Triples file.
    let add_file = tmp("add.nt");
    std::fs::write(&add_file, "<root> <contain> <nodeADD> .\n<nodeADD> <own> <pZ> .\n").unwrap();
    let add = run(
        env!("CARGO_BIN_EXE_gadd"),
        &[name.as_os_str(), add_file.as_os_str()],
    );
    assert!(add.status.success(), "gadd failed: {add:?}");
    let add_out = String::from_utf8_lossy(&add.stdout);
    assert!(add_out.contains("Added 2 new triple(s)"), "gadd: {add_out}");

    // The count is now 27 (a fresh process must see the persisted change).
    let show2 = run(env!("CARGO_BIN_EXE_gshow"), &[name.as_os_str()]);
    let show2_out = String::from_utf8_lossy(&show2.stdout);
    assert!(show2_out.contains("triples   : 27"), "gshow2: {show2_out}");

    // gsub: remove the two triples again -> back to 25.
    let sub = run(
        env!("CARGO_BIN_EXE_gsub"),
        &[name.as_os_str(), add_file.as_os_str()],
    );
    assert!(sub.status.success(), "gsub failed: {sub:?}");
    let sub_out = String::from_utf8_lossy(&sub.stdout);
    assert!(sub_out.contains("Removed 2 triple(s)"), "gsub: {sub_out}");

    // gexport: dump to .nt and confirm a known triple appears.
    let export = tmp("export.nt");
    let exp = run(
        env!("CARGO_BIN_EXE_gexport"),
        &[name.as_os_str(), export.as_os_str()],
    );
    assert!(exp.status.success(), "gexport failed: {exp:?}");
    let dumped = std::fs::read_to_string(&export).unwrap();
    assert_eq!(dumped.lines().count(), 25, "export line count");
    assert!(dumped.contains("<root> <contain> <node0> ."), "export: {dumped}");

    // gbackup then grestore into a new db, which must hold the same 25 triples.
    let backup = tmp("backup_dir");
    let _ = std::fs::remove_dir_all(&backup);
    let bk = run(
        env!("CARGO_BIN_EXE_gbackup"),
        &[name.as_os_str(), backup.as_os_str()],
    );
    assert!(bk.status.success(), "gbackup failed: {bk:?}");

    let restored = tmp("restored");
    let restored_dir = format!("{}.db", restored.display());
    let _ = std::fs::remove_dir_all(&restored_dir);
    let rs = run(
        env!("CARGO_BIN_EXE_grestore"),
        &[backup.as_os_str(), restored.as_os_str()],
    );
    assert!(rs.status.success(), "grestore failed: {rs:?}");
    let show3 = run(env!("CARGO_BIN_EXE_gshow"), &[restored.as_os_str()]);
    let show3_out = String::from_utf8_lossy(&show3.stdout);
    assert!(show3_out.contains("triples   : 25"), "restored: {show3_out}");

    // gdrop without confirmation refuses; with --yes it deletes.
    let nodrop = run(env!("CARGO_BIN_EXE_gdrop"), &[name.as_os_str()]);
    assert!(!nodrop.status.success(), "gdrop should refuse without --yes");
    assert!(std::path::Path::new(&dir).is_dir(), "db must still exist");

    let drop = run(
        env!("CARGO_BIN_EXE_gdrop"),
        &[name.as_os_str(), osstr("--yes")],
    );
    assert!(drop.status.success(), "gdrop --yes failed: {drop:?}");
    assert!(!std::path::Path::new(&dir).is_dir(), "db must be gone");

    // Cleanup.
    let _ = std::fs::remove_dir_all(&restored_dir);
    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_file(&add_file);
    let _ = std::fs::remove_file(&export);
}

#[test]
fn gadd_reads_each_new_format() {
    // A db whose data arrives via N-Quads, TriG, and RDF/XML files, each routed
    // through the matching new reader by file extension.
    let name = tmp("formats");
    let dir = build_db(&name);
    let base = run(env!("CARGO_BIN_EXE_gshow"), &[name.as_os_str()]);
    assert!(String::from_utf8_lossy(&base.stdout).contains("triples   : 25"));

    // N-Quads (graph term is flattened into the default graph).
    let nq = tmp("d.nq");
    std::fs::write(&nq, "<nqs> <nqp> <nqo> <g1> .\n<nqs> <nqp> \"lit\" .\n").unwrap();
    let a1 = run(
        env!("CARGO_BIN_EXE_gadd"),
        &[name.as_os_str(), nq.as_os_str()],
    );
    assert!(a1.status.success(), "gadd nq: {a1:?}");
    assert!(String::from_utf8_lossy(&a1.stdout).contains("Added 2 new triple(s)"));

    // TriG.
    let tg = tmp("d.trig");
    std::fs::write(
        &tg,
        "@prefix : <http://e/> .\n:ts :tp :to .\n:g { :a :b :c }\n",
    )
    .unwrap();
    let a2 = run(
        env!("CARGO_BIN_EXE_gadd"),
        &[name.as_os_str(), tg.as_os_str()],
    );
    assert!(a2.status.success(), "gadd trig: {a2:?}");
    assert!(String::from_utf8_lossy(&a2.stdout).contains("Added 2 new triple(s)"));

    // RDF/XML.
    let rx = tmp("d.rdf");
    std::fs::write(
        &rx,
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="http://ex/">
  <rdf:Description rdf:about="http://ex/x"><ex:name>X</ex:name></rdf:Description>
</rdf:RDF>"#,
    )
    .unwrap();
    let a3 = run(
        env!("CARGO_BIN_EXE_gadd"),
        &[name.as_os_str(), rx.as_os_str()],
    );
    assert!(a3.status.success(), "gadd rdf: {a3:?}");
    assert!(String::from_utf8_lossy(&a3.stdout).contains("Added 1 new triple(s)"));

    // 25 + 2 + 2 + 1 = 30.
    let show = run(env!("CARGO_BIN_EXE_gshow"), &[name.as_os_str()]);
    assert!(
        String::from_utf8_lossy(&show.stdout).contains("triples   : 30"),
        "final show: {}",
        String::from_utf8_lossy(&show.stdout)
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&nq);
    let _ = std::fs::remove_file(&tg);
    let _ = std::fs::remove_file(&rx);
}

#[test]
fn wrong_args_exit_nonzero() {
    for bin in [
        env!("CARGO_BIN_EXE_gadd"),
        env!("CARGO_BIN_EXE_gsub"),
        env!("CARGO_BIN_EXE_gshow"),
        env!("CARGO_BIN_EXE_gexport"),
        env!("CARGO_BIN_EXE_gbackup"),
        env!("CARGO_BIN_EXE_grestore"),
        env!("CARGO_BIN_EXE_gmonitor"),
        env!("CARGO_BIN_EXE_gdrop"),
    ] {
        let out = Command::new(bin).output().expect("run with no args");
        assert!(!out.status.success(), "{bin} should fail with no args");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("usage:"), "{bin} should print usage: {err}");
    }
}
