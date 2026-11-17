//! Review bulkcli#5: the zip entry limit (`unzip_max_entries`) is enforced
//! before the zip reader decodes the central directory. The reader decodes
//! every record into memory before it returns (about 190 bytes per entry),
//! and the limit used to be compared with the entry count only afterwards:
//! a package of a few hundred thousand empty entries cost tens of megabytes
//! — a 1 GiB upload several gigabytes — before it was refused.
//!
//! One test in its own binary: it measures the process's peak resident set
//! (`VmHWM`), which other tests running alongside would disturb. The
//! package is written by hand, streaming, so making it costs no memory.
#![cfg(all(feature = "zip", target_os = "linux"))]

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use gstore::zipio::{self, ZipCode, ZipLimits};

/// A temp directory removed when dropped, also when the test fails.
struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The process's peak resident set size, in KiB.
fn vm_hwm_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM in /proc/self/status")
}

/// Write a zip64 package of `n` empty stored entries named by 8 hex digits
/// (local headers, central directory, zip64 end record and locator, end
/// record), streaming.
fn write_many_entries(path: &Path, n: u64) {
    const NAME: u64 = 8;
    let mut w = BufWriter::new(fs::File::create(path).unwrap());
    let u16le = |v: u64| (v as u16).to_le_bytes();
    let u32le = |v: u64| (v as u32).to_le_bytes();
    for i in 0..n {
        w.write_all(b"PK\x03\x04").unwrap();
        w.write_all(&u16le(20)).unwrap(); // version needed
        w.write_all(&[0; 4]).unwrap(); // flags, method (stored)
        w.write_all(&[0, 0, 0x21, 0]).unwrap(); // time, date (1980-01-01)
        w.write_all(&[0; 12]).unwrap(); // crc, sizes
        w.write_all(&u16le(NAME)).unwrap();
        w.write_all(&u16le(0)).unwrap(); // extra
        write!(w, "{i:08x}").unwrap();
    }
    let cd_start = n * (30 + NAME);
    for i in 0..n {
        w.write_all(b"PK\x01\x02").unwrap();
        w.write_all(&u16le(20)).unwrap(); // made by
        w.write_all(&u16le(20)).unwrap(); // needed
        w.write_all(&[0; 4]).unwrap(); // flags, method
        w.write_all(&[0, 0, 0x21, 0]).unwrap(); // time, date
        w.write_all(&[0; 12]).unwrap(); // crc, sizes
        w.write_all(&u16le(NAME)).unwrap();
        w.write_all(&[0; 8]).unwrap(); // extra, comment, disk, internal attrs
        w.write_all(&[0; 4]).unwrap(); // external attrs
        w.write_all(&u32le(i * (30 + NAME))).unwrap(); // local header offset
        write!(w, "{i:08x}").unwrap();
    }
    let cd_size = n * (46 + NAME);
    let eocd64_at = cd_start + cd_size;
    w.write_all(b"PK\x06\x06").unwrap();
    w.write_all(&44u64.to_le_bytes()).unwrap();
    w.write_all(&u16le(45)).unwrap();
    w.write_all(&u16le(45)).unwrap();
    w.write_all(&[0; 8]).unwrap(); // disk, disk with the directory
    w.write_all(&n.to_le_bytes()).unwrap();
    w.write_all(&n.to_le_bytes()).unwrap();
    w.write_all(&cd_size.to_le_bytes()).unwrap();
    w.write_all(&cd_start.to_le_bytes()).unwrap();
    w.write_all(b"PK\x06\x07").unwrap();
    w.write_all(&[0; 4]).unwrap();
    w.write_all(&eocd64_at.to_le_bytes()).unwrap();
    w.write_all(&u32le(1)).unwrap();
    w.write_all(b"PK\x05\x06").unwrap();
    w.write_all(&[0; 4]).unwrap();
    w.write_all(&[0xFF; 4]).unwrap(); // entries: in the zip64 record
    w.write_all(&[0xFF; 8]).unwrap(); // size, offset: in the zip64 record
    w.write_all(&[0; 2]).unwrap();
    w.flush().unwrap();
}

#[test]
fn the_entry_limit_holds_before_the_central_directory_is_read() {
    const N: u64 = 400_000;
    let dir =
        TmpDir(std::env::temp_dir().join(format!("gstore_zip_cd_bound_{}", std::process::id())));
    let _ = fs::remove_dir_all(&dir.0);
    fs::create_dir_all(&dir.0).unwrap();
    let zip = dir.0.join("many.zip");
    write_many_entries(&zip, N);

    let before = vm_hwm_kib();
    let e = zipio::unpack(&zip, &dir.0.join("out"), &ZipLimits::default(), &|_| true)
        .expect_err("over the entry limit");
    let grown = vm_hwm_kib().saturating_sub(before);
    assert_eq!(e.code(), Some(ZipCode::NotSupported), "{e}");
    assert!(e.to_string().contains(&format!("holds {N} entries")), "{e}");
    // The reader would have decoded ~400k records (about 75 MB); the
    // check reads the end of the file only.
    assert!(
        grown < 16 * 1024,
        "the peak resident set grew by {grown} KiB before the package was refused"
    );
    assert!(!dir.0.join("out").exists(), "nothing was unpacked");

    // Under a limit it fits, the same package is read as before (and then
    // refused by the entry filter, before anything is written).
    let limits = ZipLimits {
        max_entries: N as usize,
        ..ZipLimits::default()
    };
    let e = zipio::unpack(&zip, &dir.0.join("out2"), &limits, &|_| false)
        .expect_err("no entry accepted");
    assert!(
        e.to_string().contains("is not allowed"),
        "the limit lets it through, the reader reads it: {e}"
    );
}
