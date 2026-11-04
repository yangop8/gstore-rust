//! Process and machine statistics: resident memory, children's peak RSS,
//! directory sizes, page-cache dropping and a few facts about the host.
//!
//! Everything Linux-specific degrades to `None` elsewhere (the results then
//! carry `null`).

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// A `/proc/self/status` field in KiB (`VmRSS`, `VmHWM`, …).
pub fn status_kib(key: &str) -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// Current resident set size in MiB (`VmRSS`).
pub fn rss_mib() -> Option<f64> {
    status_kib("VmRSS").map(|k| k as f64 / 1024.0)
}

/// Peak resident set size of this process so far in MiB (`VmHWM`).
pub fn hwm_mib() -> Option<f64> {
    status_kib("VmHWM").map(|k| k as f64 / 1024.0)
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod linux {
    /// `struct rusage` on 64-bit Linux: two `timeval`s, then 14 `long`s.
    #[repr(C)]
    pub struct RUsage {
        pub ru_utime: [i64; 2],
        pub ru_stime: [i64; 2],
        pub ru_maxrss: i64,
        pub rest: [i64; 13],
    }

    pub const RUSAGE_CHILDREN: i32 = -1;
    pub const PR_SET_PDEATHSIG: i32 = 1;
    pub const SIGKILL: u64 = 9;
    pub const ESRCH: i32 = 3;

    extern "C" {
        pub fn getrusage(who: i32, usage: *mut RUsage) -> i32;
        pub fn sync();
        pub fn kill(pid: i32, sig: i32) -> i32;
        pub fn prctl(option: i32, ...) -> i32;
        pub fn getppid() -> i32;
    }
}

/// The largest peak RSS (MiB) among the terminated, waited-for children of
/// this process (`getrusage(RUSAGE_CHILDREN)`); `None` off 64-bit Linux.
pub fn children_peak_rss_mib() -> Option<f64> {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        let mut u = linux::RUsage {
            ru_utime: [0; 2],
            ru_stime: [0; 2],
            ru_maxrss: 0,
            rest: [0; 13],
        };
        // SAFETY: `u` is a properly sized and aligned `struct rusage` for
        // this target; getrusage only writes into it.
        let rc = unsafe { linux::getrusage(linux::RUSAGE_CHILDREN, &mut u) };
        if rc == 0 {
            return Some(u.ru_maxrss as f64 / 1024.0);
        }
        None
    }
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    {
        None
    }
}

/// Kill a whole process group (the child was started as its own group
/// leader), so a killed phase leaves no `gbuild` behind. Returns whether the
/// signal was sent.
pub fn kill_process_group(pgid: u32) -> bool {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        // SAFETY: plain syscall wrapper; a negative pid names the group.
        unsafe { linux::kill(-(pgid as i32), 9) == 0 }
    }
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    {
        let _ = pgid;
        false
    }
}

/// For a [`std::os::unix::process::CommandExt::pre_exec`] hook: have the
/// process about to run be killed (SIGKILL) when `parent` — the process
/// spawning it — dies. Phase children and `gbuild` run in their own process
/// group, so neither a terminal's Ctrl-C nor `timeout(1)` reaches them; this
/// keeps them from outliving an interrupted harness (and the `flock` it may
/// have handed down). A parent that died before the hook ran fails the spawn.
/// Only async-signal-safe calls (it runs between fork and exec). A no-op off
/// 64-bit Linux.
pub fn die_with_parent(parent: u32) -> io::Result<()> {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        // SAFETY: prctl(PR_SET_PDEATHSIG) and getppid are plain syscall
        // wrappers that touch no memory of ours.
        unsafe {
            if linux::prctl(linux::PR_SET_PDEATHSIG, linux::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if linux::getppid() as u32 != parent {
                return Err(io::Error::from_raw_os_error(linux::ESRCH));
            }
        }
    }
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    {
        let _ = parent;
    }
    Ok(())
}

/// Flush dirty pages and drop the page cache (needs root on Linux). Returns
/// whether the cache was dropped.
pub fn drop_page_cache() -> bool {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        // SAFETY: sync(2) takes no arguments and cannot fail.
        unsafe { linux::sync() };
        std::fs::write("/proc/sys/vm/drop_caches", b"3\n").is_ok()
    }
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    {
        false
    }
}

/// Total apparent size (bytes) of every file below `dir` (like `du -sb`, not
/// following symlinks), and the size of each top-level entry.
pub fn dir_bytes(dir: &Path) -> io::Result<(u64, BTreeMap<String, u64>)> {
    fn walk(p: &Path) -> io::Result<u64> {
        let md = std::fs::symlink_metadata(p)?;
        if md.is_dir() {
            let mut n = 0;
            for e in std::fs::read_dir(p)? {
                n += walk(&e?.path())?;
            }
            Ok(n)
        } else {
            Ok(md.len())
        }
    }
    let mut top = BTreeMap::new();
    let mut total = 0;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let n = walk(&e.path())?;
        total += n;
        top.insert(e.file_name().to_string_lossy().into_owned(), n);
    }
    Ok((total, top))
}

/// The 1/5/15-minute load averages.
pub fn loadavg() -> Option<[f64; 3]> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut it = s.split_whitespace().map(|x| x.parse::<f64>().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

/// Total memory in MiB (`MemTotal`).
pub fn mem_total_mib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    s.lines().find_map(|l| {
        let rest = l.strip_prefix("MemTotal:")?;
        rest.split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|k| k / 1024)
    })
}

/// The host name, if known.
pub fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .or_else(|| std::env::var("HOSTNAME").ok())
}

/// The CPU model name, if known.
pub fn cpu_model() -> Option<String> {
    let s = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    s.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "model name").then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_bytes_sums_files() {
        let d = std::env::temp_dir().join(format!("gsb-dirbytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a"), b"12345").unwrap();
        std::fs::write(d.join("sub/b"), b"123").unwrap();
        let (n, top) = dir_bytes(&d).unwrap();
        assert_eq!(n, 8);
        assert_eq!(top["a"], 5);
        assert_eq!(top["sub"], 3);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn proc_stats_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(rss_mib().unwrap() > 0.0);
            assert!(hwm_mib().unwrap() >= rss_mib().unwrap() - 1.0);
            assert!(children_peak_rss_mib().is_some());
        }
    }
}
