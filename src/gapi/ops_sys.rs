//! System operations: `check`, `login`, `testConnect`, `getCoreVersion`,
//! `stat` (upstream `grpc.cpp` `check_task` … `core_version_task`, `stat_task`).

use super::auth;
use super::envelope::GResp;
use super::params::Params;
use super::{Ctx, OpSpec, Registry};
use crate::json::Json;

pub(super) fn register(reg: &mut Registry) {
    reg.add(OpSpec::new("check", check).public());
    reg.add(OpSpec::new("login", login));
    reg.add(OpSpec::new("testConnect", test_connect));
    reg.add(OpSpec::new("getCoreVersion", core_version));
    reg.add(OpSpec::new("stat", stat));
}

/// `check` — the heartbeat; no credentials. Upstream answers with its process
/// id as the `StatusMsg` (its docs show a fixed text; the code, which the
/// `gserver` start/stop logic relies on, sends the pid).
fn check(_ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    GResp::success(&std::process::id().to_string())
}

/// The fields `login` / `testConnect` / `getCoreVersion` share.
fn version_fields(ctx: &Ctx<'_>, msg: &str) -> GResp {
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", msg)
            .with("licensetype", ctx.config.license_type.as_str())
            .with("CoreVersion", ctx.config.core_version.as_str())
            .with("type", ctx.config.http_type.as_str()),
    )
}

/// `login` — verify the credentials (done by the dispatcher) and report the
/// server version. Upstream's `RootPath` field is commented out in its code
/// (only its docs mention it), so it is not sent.
fn login(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    auth::note_login_success(ctx);
    version_fields(ctx, "login successfully")
}

/// `testConnect` — whether the server is reachable with these credentials.
fn test_connect(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    version_fields(ctx, "success")
}

/// `getCoreVersion` — the server version.
fn core_version(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    version_fields(ctx, "success")
}

/// `stat` — this process's CPU usage (percent of one core, sampled over
/// 200 ms like upstream), its resident memory (MB) and the free disk space
/// (MB) under the server's root directory, as `%f` / integer strings.
///
/// Upstream's code names the CPU field `cup_usage` (a typo its docs correct
/// to `cpu_usage`); both names are sent so clients following either work.
fn stat(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let cpu = format!("{:.6}", resources::cpu_usage_percent());
    let mem = format!("{:.6}", resources::rss_mb());
    let disk = resources::disk_available_mb(ctx.root_dir()).to_string();
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", "success")
            .with("cup_usage", cpu.as_str())
            .with("cpu_usage", cpu)
            .with("mem_usage", mem)
            .with("disk_available", disk),
    )
}

/// Process resource figures (upstream `src/Util/ResourceUtil.cpp`), read from
/// `/proc` and `statvfs` on Linux; zeros elsewhere.
mod resources {
    use std::path::Path;
    use std::time::Duration;

    /// The process's CPU time and the machine's total CPU time, in clock
    /// ticks (`/proc/self/stat` utime+stime, the `cpu` line of `/proc/stat`).
    fn cpu_times() -> Option<(u64, u64)> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Fields after the parenthesized command name (which may contain
        // spaces): state is field 3, utime/stime are fields 14/15.
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let proc_time = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
        let total = std::fs::read_to_string("/proc/stat").ok()?;
        let line = total.lines().find(|l| l.starts_with("cpu "))?;
        let total_time = line
            .split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse::<u64>().ok())
            .sum();
        Some((proc_time, total_time))
    }

    /// CPU usage in percent of one core over a 200 ms sample (upstream:
    /// process-time delta / total-time delta × number of CPUs × 100).
    pub(super) fn cpu_usage_percent() -> f64 {
        let Some((p1, t1)) = cpu_times() else {
            return 0.0;
        };
        std::thread::sleep(Duration::from_millis(200));
        let Some((p2, t2)) = cpu_times() else {
            return 0.0;
        };
        if t2 <= t1 {
            return 0.0;
        }
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        (p2.saturating_sub(p1)) as f64 / (t2 - t1) as f64 * cpus * 100.0
    }

    /// Resident set size in whole MB (upstream: `VmRSS` kB `>> 10`).
    pub(super) fn rss_mb() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
                line.split_whitespace().nth(1)?.parse::<u64>().ok()
            })
            .map_or(0.0, |kb| (kb >> 10) as f64)
    }

    /// Free disk space available to unprivileged users under `path`, in MB.
    pub(super) fn disk_available_mb(path: &Path) -> u64 {
        statvfs_available_bytes(path).map_or(0, |b| b >> 20)
    }

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    fn statvfs_available_bytes(path: &Path) -> Option<u64> {
        use std::ffi::CString;
        use std::os::raw::{c_char, c_int};
        use std::os::unix::ffi::OsStrExt;

        // `struct statvfs` on 64-bit Linux (glibc and musl) begins with
        // `unsigned long f_bsize, f_frsize; fsblkcnt_t f_blocks, f_bfree,
        // f_bavail; …` — all 64-bit. Only those leading fields are read; the
        // buffer is larger than the whole struct (112 bytes).
        #[repr(C)]
        struct StatVfsPrefix {
            f_bsize: u64,
            f_frsize: u64,
            f_blocks: u64,
            f_bfree: u64,
            f_bavail: u64,
            _rest: [u64; 27],
        }
        extern "C" {
            fn statvfs(path: *const c_char, buf: *mut StatVfsPrefix) -> c_int;
        }
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf = StatVfsPrefix {
            f_bsize: 0,
            f_frsize: 0,
            f_blocks: 0,
            f_bfree: 0,
            f_bavail: 0,
            _rest: [0; 27],
        };
        // SAFETY: `c_path` is a valid NUL-terminated string and `buf` is a
        // writable buffer larger than `struct statvfs` on this target.
        let rc = unsafe { statvfs(c_path.as_ptr(), &mut buf) };
        if rc != 0 {
            return None;
        }
        let unit = if buf.f_frsize != 0 { buf.f_frsize } else { buf.f_bsize };
        let _ = (buf.f_blocks, buf.f_bfree);
        buf.f_bavail.checked_mul(unit)
    }

    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    fn statvfs_available_bytes(_path: &Path) -> Option<u64> {
        None
    }

    #[cfg(test)]
    mod tests {
        #[cfg(target_os = "linux")]
        #[test]
        fn linux_figures_are_plausible() {
            assert!(super::rss_mb() >= 0.0);
            assert!(super::cpu_times().is_some());
            #[cfg(target_pointer_width = "64")]
            assert!(super::disk_available_mb(&std::env::temp_dir()) > 0);
            assert_eq!(super::disk_available_mb(std::path::Path::new("/nonexistent/x/y")), 0);
        }
    }
}
