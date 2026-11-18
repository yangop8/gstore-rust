//! The output files of `gquery -f` and `gexport` (round-4 review of
//! bulkcli#10/#11's fix).
//!
//! * The temporary file that replaces an existing file is created private
//!   (`0600`) and given the replaced file's mode only once its content is
//!   complete. It was created with the default mode (`0644` under the usual
//!   umask) and its mode copied afterwards, so output meant for a private
//!   `0600` file could be opened by another user in between — and a
//!   descriptor opened then reads all of it later.
//! * `gexport -z` writes its archive like a plain export: it went through a
//!   fixed `<archive>.part`, opened truncating and following a symbolic
//!   link (a link planted there had its target overwritten), and was
//!   renamed over the unresolved destination (a link to the archive was
//!   replaced by a regular file with the default mode).

#![cfg(unix)]

#[path = "../src/bin/common/cli.rs"]
#[allow(unused_imports)]
mod cli;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_cli_output_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(fs::canonicalize(&p).unwrap())
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// The temporary file replacing an existing file is private from its
/// creation to its completion, whatever the replaced file's mode; the
/// result then has that mode.
#[test]
fn the_temporary_file_is_private_until_its_content_is_complete() {
    let base = TmpDir::new("private");
    for dest_mode in [0o600, 0o644, 0o604] {
        let dest = base.0.join(format!("out-{dest_mode:o}.txt"));
        fs::write(&dest, "old\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(dest_mode)).unwrap();
        let mut out = cli::OutputFile::create(&dest).unwrap();
        let tmp = out.path().to_path_buf();
        assert_ne!(
            tmp, dest,
            "an existing file is replaced, not written in place"
        );
        assert_eq!(
            mode(&tmp) & 0o077,
            0,
            "the temporary file for a {dest_mode:o} file was created with mode {:o}",
            mode(&tmp)
        );
        out.writer().unwrap().write_all(b"secret\n").unwrap();
        out.writer().unwrap().flush().unwrap();
        assert_eq!(
            mode(&tmp) & 0o077,
            0,
            "the incomplete output for a {dest_mode:o} file has mode {:o}",
            mode(&tmp)
        );
        out.commit().unwrap();
        assert_eq!(mode(&dest), dest_mode);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "secret\n");
        assert!(!tmp.exists());
    }
    // A new file gets the default mode of a new file, as before.
    let new = base.0.join("new.txt");
    let mut out = cli::OutputFile::create(&new).unwrap();
    out.writer().unwrap().write_all(b"x\n").unwrap();
    out.commit().unwrap();
    let probe = base.0.join("probe.txt");
    fs::write(&probe, "").unwrap();
    assert_eq!(mode(&new), mode(&probe));
}

#[cfg(feature = "zip")]
mod zipped {
    use super::*;
    use std::process::{Command, Output, Stdio};

    fn run(name: &str, args: &[&str], cwd: &Path) -> Output {
        let exe = match name {
            "gbuild" => env!("CARGO_BIN_EXE_gbuild"),
            "gexport" => env!("CARGO_BIN_EXE_gexport"),
            other => panic!("no tool {other}"),
        };
        Command::new(exe)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn text(out: &Output) -> String {
        format!(
            "status {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn built(tag: &str) -> TmpDir {
        let base = TmpDir::new(tag);
        fs::write(
            base.0.join("t.nt"),
            "<http://e/s> <http://e/p> <http://e/o1> .\n<http://e/s> <http://e/p> <http://e/o2> .\n",
        )
        .unwrap();
        let out = run("gbuild", &["t1", "t.nt"], &base.0);
        assert!(out.status.success(), "{}", text(&out));
        base
    }

    /// A symbolic link to the archive stays a link; the file it names gets
    /// the archive and keeps its mode.
    #[test]
    fn a_zipped_export_through_a_link_replaces_the_file_it_names() {
        let base = built("zip_link");
        let real = base.0.join("real.zip");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o604)).unwrap();
        std::os::unix::fs::symlink("real.zip", base.0.join("link.zip")).unwrap();
        let out = run("gexport", &["-z", "1", "t1", "link.nt"], &base.0);
        assert!(out.status.success(), "{}", text(&out));
        let link = fs::symlink_metadata(base.0.join("link.zip")).unwrap();
        assert!(
            link.file_type().is_symlink(),
            "the link was replaced by a regular file: {}",
            text(&out)
        );
        assert!(fs::read(&real).unwrap().starts_with(b"PK"), "real.zip");
        assert_eq!(mode(&real), 0o604);
    }

    /// What another writer (or another user) put at the old fixed
    /// temporary name is never written through or truncated.
    #[test]
    fn a_zipped_export_never_writes_through_a_planted_temporary_name() {
        let base = built("zip_planted");
        let victim = base.0.join("victim.txt");
        fs::write(&victim, "keep me").unwrap();
        std::os::unix::fs::symlink(&victim, base.0.join("out.zip.part")).unwrap();
        let busy = base.0.join("busy.zip.part");
        fs::write(&busy, "another writer's archive").unwrap();
        for out_name in ["out.nt", "busy.nt"] {
            let out = run("gexport", &["-z", "1", "t1", out_name], &base.0);
            assert!(out.status.success(), "{}", text(&out));
        }
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
        assert_eq!(
            fs::read_to_string(&busy).unwrap(),
            "another writer's archive"
        );
        assert!(fs::read(base.0.join("out.zip")).unwrap().starts_with(b"PK"));
        assert!(fs::read(base.0.join("busy.zip"))
            .unwrap()
            .starts_with(b"PK"));
    }
}
