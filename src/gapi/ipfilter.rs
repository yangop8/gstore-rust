//! IP access control of the `/api` protocol: the allow / deny lists and the
//! failed-login lockout (upstream `APIUtil::check_access_ip`,
//! `src/Util/IPWhiteList.cpp`, `src/Util/IPBlackList.cpp`), and the
//! `ipmanage` operation.
//!
//! # The lists
//!
//! `conf.ini`'s `ip_allow_path` and `ip_deny_path` name the list files
//! (relative paths are relative to the working directory, like every
//! `conf.ini` path). When an allow list is configured it alone decides — only
//! the addresses it lists get in (an empty or missing file lets nobody in, as
//! upstream); otherwise a deny list keeps out the addresses it lists. With
//! neither configured (`ip_deny_path=off`) every address is let in. A blocked
//! address is answered `1101 "IP Blocked!"` before anything else is looked at
//! (upstream's level-1 check).
//!
//! The file format is upstream's (`conf/ipAllow.config`, `conf/ipDeny.config`):
//! one entry per line; a line containing `#` is a comment, as is any line
//! shorter than 7 characters. An entry is
//!
//! * an address — `192.168.1.10` (an IPv6 address works too);
//! * a closed range `first-last` — `192.168.1.120-192.168.1.129`. Upstream
//!   compares the two addresses octet by octet (each octet of the client must
//!   lie between the corresponding octets), which coincides with the numeric
//!   range for the usual same-prefix ranges; this implementation uses the
//!   numeric range, so a range across an octet boundary
//!   (`10.0.0.250-10.0.1.5`) means what it says;
//! * *(extension)* an IPv4 pattern with `*` octets — `192.168.1.*`.
//!
//! # The lockout
//!
//! Every failed authentication from an address counts against it (upstream
//! `update_access_ip_error_num`). With more than [`MAX_AUTH_FAILURES`] failures
//! the address is locked out: every operation except `login` and
//! `testConnect` is answered `1101` (upstream's level-2 check) — until a
//! successful `login` from it resets the count (upstream
//! `reset_access_ip_error_num`) or the server restarts (the counts are kept
//! in memory only, as upstream).
//!
//! # `ipmanage`
//!
//! `type=1` lists the list in effect (`ip_type` `"2"` the allow list, `"1"`
//! the deny list, with the entries as `ips`); `type=2` replaces a list file
//! (`ip_type` `1` deny / `2` allow, `ips` comma-separated — or a JSON array)
//! and reloads it. Root only.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::Ctx;
use crate::config::GStoreConfig;
use crate::json::Json;

/// An address with more failed authentications than this is locked out
/// (upstream: `errornum > 4`).
pub const MAX_AUTH_FAILURES: u32 = 4;

/// The most addresses whose failures are tracked at once (bounds the memory a
/// client spraying source addresses can make the server use).
const MAX_TRACKED: usize = 65_536;

/// Upstream's message for an address the lists keep out.
pub const BLOCKED_MSG: &str = "IP Blocked!";

/// Upstream's message for a locked-out address.
pub const LOCKED_MSG: &str = "The ip has too many error during accessing server, the ip has \
                              been locked until the server restart!";

/// The comment line `ipmanage` writes at the top of a list file it saves.
const SAVED_HEADER: &str = "#update by ipmanage";

/// Which list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// The deny (black) list — upstream `ip_type` `1`.
    Deny,
    /// The allow (white) list — upstream `ip_type` `2`.
    Allow,
}

impl ListKind {
    /// Upstream's `ip_type` code.
    pub fn code(self) -> &'static str {
        match self {
            ListKind::Deny => "1",
            ListKind::Allow => "2",
        }
    }
}

/// One parsed list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    Exact(IpAddr),
    /// An inclusive numeric range of IPv4 or IPv6 addresses.
    Range(u128, u128, bool),
    /// IPv4 octets, `None` = `*`.
    Pattern([Option<u8>; 4]),
}

impl Rule {
    fn parse(entry: &str) -> Option<Rule> {
        let entry = entry.trim();
        if let Some((a, b)) = entry.split_once('-') {
            let (a, b) = (
                a.trim().parse::<IpAddr>().ok()?,
                b.trim().parse::<IpAddr>().ok()?,
            );
            let (lo, hi, v4) = match (normalize(a), normalize(b)) {
                (IpAddr::V4(a), IpAddr::V4(b)) => {
                    (u128::from(u32::from(a)), u128::from(u32::from(b)), true)
                }
                (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a), u128::from(b), false),
                _ => return None,
            };
            return Some(Rule::Range(lo.min(hi), lo.max(hi), v4));
        }
        if entry.contains('*') {
            let parts: Vec<&str> = entry.split('.').collect();
            if parts.len() != 4 {
                return None;
            }
            let mut octets = [None; 4];
            for (slot, part) in octets.iter_mut().zip(parts) {
                *slot = match part {
                    "*" => None,
                    p => Some(p.parse::<u8>().ok()?),
                };
            }
            return Some(Rule::Pattern(octets));
        }
        entry
            .parse::<IpAddr>()
            .ok()
            .map(|a| Rule::Exact(normalize(a)))
    }

    fn matches(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Rule::Exact(a), ip) => *a == ip,
            (Rule::Range(lo, hi, true), IpAddr::V4(v4)) => {
                (*lo..=*hi).contains(&u128::from(u32::from(v4)))
            }
            (Rule::Range(lo, hi, false), IpAddr::V6(v6)) => (*lo..=*hi).contains(&u128::from(v6)),
            (Rule::Pattern(octets), IpAddr::V4(v4)) => octets
                .iter()
                .zip(v4.octets())
                .all(|(want, got)| want.is_none_or(|w| w == got)),
            _ => false,
        }
    }
}

/// An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) as the IPv4 address.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// One loaded list: its raw entries (as `ipmanage` lists them) and the
/// parsed rules.
#[derive(Debug, Clone, Default)]
struct IpList {
    entries: BTreeSet<String>,
    rules: Vec<Rule>,
}

impl IpList {
    /// Parse a list file's text (upstream `ReadIPFromFile`): lines containing
    /// `#` and lines shorter than 7 characters are skipped. Entries that do
    /// not parse are listed but match nothing.
    fn parse(text: &str) -> IpList {
        let mut list = IpList::default();
        for line in text.lines() {
            let line = line.trim();
            if line.len() < 7 || line.contains('#') {
                continue;
            }
            if list.entries.insert(line.to_string()) {
                if let Some(rule) = Rule::parse(line) {
                    list.rules.push(rule);
                }
            }
        }
        list
    }

    /// The list in `path` (a missing or unreadable file is an empty list, as
    /// upstream).
    fn load(path: &Path) -> IpList {
        IpList::parse(&std::fs::read_to_string(path).unwrap_or_default())
    }

    fn matches(&self, ip: Option<IpAddr>) -> bool {
        ip.is_some_and(|ip| self.rules.iter().any(|r| r.matches(ip)))
    }
}

#[derive(Debug, Default)]
struct State {
    allow: Option<(PathBuf, IpList)>,
    deny: Option<(PathBuf, IpList)>,
    /// Failed authentications per address.
    failures: HashMap<String, u32>,
}

/// The server's IP access control (see the module docs). Thread-safe.
#[derive(Debug, Default)]
pub struct IpFilter {
    state: Mutex<State>,
}

/// Why `ipmanage` could not save a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// The list's file is not configured (`ip_allow_path` / `ip_deny_path`).
    NotConfigured(ListKind),
    /// An entry is not an address, a range or a pattern.
    Invalid(String),
    /// Writing the file failed.
    Io(String),
}

impl IpFilter {
    /// A filter with the given list files (loaded now).
    pub fn new(allow_path: Option<PathBuf>, deny_path: Option<PathBuf>) -> IpFilter {
        let load = |p: Option<PathBuf>| {
            p.map(|p| {
                let list = IpList::load(&p);
                (p, list)
            })
        };
        IpFilter {
            state: Mutex::new(State {
                allow: load(allow_path),
                deny: load(deny_path),
                failures: HashMap::new(),
            }),
        }
    }

    /// A filter from `conf.ini`'s `ip_allow_path` / `ip_deny_path`.
    pub fn from_config(config: &GStoreConfig) -> IpFilter {
        IpFilter::new(config.ip_allow_path.clone(), config.ip_deny_path.clone())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The list in effect, as upstream's `ip_type`: `"2"` the allow list,
    /// `"1"` the deny list, `"3"` none.
    pub fn mode(&self) -> &'static str {
        let st = self.state();
        if st.allow.is_some() {
            "2"
        } else if st.deny.is_some() {
            "1"
        } else {
            "3"
        }
    }

    /// Whether the lists let `ip` in (upstream's level-1 check).
    pub fn permits(&self, ip: &str) -> bool {
        let addr = ip.trim().parse::<IpAddr>().ok().map(normalize);
        let st = self.state();
        if let Some((_, allow)) = &st.allow {
            return allow.matches(addr);
        }
        match &st.deny {
            Some((_, deny)) => !deny.matches(addr),
            None => true,
        }
    }

    /// Whether `ip` is locked out after too many failed authentications
    /// (upstream's level-2 check).
    pub fn locked_out(&self, ip: &str) -> bool {
        self.failures(ip) > MAX_AUTH_FAILURES
    }

    /// The failed authentications counted against `ip`.
    pub fn failures(&self, ip: &str) -> u32 {
        self.state().failures.get(ip).copied().unwrap_or(0)
    }

    /// Count a failed authentication from `ip`.
    pub fn note_failure(&self, ip: &str) {
        let mut st = self.state();
        if !st.failures.contains_key(ip) && st.failures.len() >= MAX_TRACKED {
            // Make room by forgetting an address that is not locked out; if
            // every tracked address is locked out, this one is not tracked.
            let Some(victim) = st
                .failures
                .iter()
                .find(|(_, n)| **n <= MAX_AUTH_FAILURES)
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            st.failures.remove(&victim);
        }
        let n = st.failures.entry(ip.to_string()).or_insert(0);
        *n = n.saturating_add(1);
    }

    /// Clear `ip`'s failed authentications (a successful `login`).
    pub fn reset(&self, ip: &str) {
        self.state().failures.remove(ip);
    }

    /// The entries of list `kind`, sorted (`None` when it is not configured).
    pub fn entries(&self, kind: ListKind) -> Option<Vec<String>> {
        let st = self.state();
        let list = match kind {
            ListKind::Allow => &st.allow,
            ListKind::Deny => &st.deny,
        };
        list.as_ref()
            .map(|(_, l)| l.entries.iter().cloned().collect())
    }

    /// Use the list files `allow_path` / `deny_path` from now on (loaded
    /// now; a configuration reload). The failed-login counts are kept.
    pub fn reconfigure(&self, allow_path: Option<PathBuf>, deny_path: Option<PathBuf>) {
        let fresh = IpFilter::new(allow_path, deny_path);
        let fresh = fresh.state.into_inner().unwrap_or_else(|e| e.into_inner());
        let mut st = self.state();
        st.allow = fresh.allow;
        st.deny = fresh.deny;
    }

    /// Re-read both list files.
    pub fn reload(&self) {
        let mut guard = self.state();
        let st = &mut *guard;
        for (path, list) in [&mut st.allow, &mut st.deny].into_iter().flatten() {
            *list = IpList::load(path);
        }
    }

    /// Replace list `kind`'s file with `ips` (upstream `UpdateIPToFile`,
    /// written atomically) and reload it. Every entry must be an address, a
    /// range or a pattern.
    pub fn save(&self, kind: ListKind, ips: &[String]) -> Result<(), SaveError> {
        for ip in ips {
            if ip.contains('#') || Rule::parse(ip).is_none() {
                return Err(SaveError::Invalid(ip.clone()));
            }
        }
        let mut st = self.state();
        let slot = match kind {
            ListKind::Allow => &mut st.allow,
            ListKind::Deny => &mut st.deny,
        };
        let Some((path, list)) = slot else {
            return Err(SaveError::NotConfigured(kind));
        };
        let mut text = format!("{SAVED_HEADER}\n");
        for ip in ips {
            text.push_str(ip.trim());
            text.push('\n');
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| SaveError::Io(e.to_string()))?;
        }
        crate::db::fsutil::atomic_write(path, text.as_bytes())
            .map_err(|e| SaveError::Io(e.to_string()))?;
        *list = IpList::parse(&text);
        Ok(())
    }
}

/// `ipmanage` — query (`type=1`) or save (`type=2`) the IP lists (upstream
/// `ip_manage_task`).
pub(super) fn ipmanage(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let filter = &ctx.inner.ipfilter;
    match p.str("type").as_deref().map(str::trim) {
        Some("1") => {
            let mode = filter.mode();
            let kind = match mode {
                "2" => ListKind::Allow,
                "1" => ListKind::Deny,
                _ => {
                    return GResp::error(
                        Status::OperationFailed,
                        "please configure ip_deny_path or ip_allow_path in the conf.ini file \
                         first.",
                    )
                }
            };
            let ips: Vec<Json> = filter
                .entries(kind)
                .unwrap_or_default()
                .into_iter()
                .map(Json::from)
                .collect();
            GResp::json(
                Json::obj()
                    .with("StatusCode", 0)
                    .with("StatusMsg", "success")
                    .with(
                        "ResponseBody",
                        Json::obj().with("ip_type", mode).with("ips", ips),
                    ),
            )
        }
        Some("2") => {
            let ips: Vec<String> = p
                .strings("ips")
                .iter()
                .flat_map(|s| s.split(','))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if ips.is_empty() {
                return GResp::error(Status::ParamIsIllegal, "the ips can't be empty");
            }
            let kind = match p.str("ip_type").as_deref().map(str::trim) {
                Some("1") => ListKind::Deny,
                Some("2") => ListKind::Allow,
                _ => {
                    return GResp::error(
                        Status::ParamIsIllegal,
                        "ip_type is invalid, please look up the api document.",
                    )
                }
            };
            match filter.save(kind, &ips) {
                Ok(()) => GResp::success("success"),
                Err(SaveError::NotConfigured(ListKind::Deny)) => GResp::error(
                    Status::OperationFailed,
                    "ip_deny_path is not configured, please configure it in the conf.ini file \
                     first.",
                ),
                Err(SaveError::NotConfigured(ListKind::Allow)) => GResp::error(
                    Status::OperationFailed,
                    "ip_allow_path is not configured, please configure it in the conf.ini file \
                     first.",
                ),
                Err(SaveError::Invalid(ip)) => GResp::error(
                    Status::ParamIsIllegal,
                    format!(
                        "invalid ip '{ip}': use an address, a range first-last, or a pattern \
                         such as 192.168.1.*"
                    ),
                ),
                Err(SaveError::Io(e)) => {
                    GResp::error(Status::OperationFailed, format!("IP manger fail: {e}"))
                }
            }
        }
        _ => GResp::error(
            Status::ParamIsIllegal,
            "type is invalid, please look up the api document.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gstore_ipfilter_{tag}_{}_{}",
            std::process::id(),
            crate::sysdb::clock::unix_millis()
        ))
    }

    #[test]
    fn upstream_list_files_parse() {
        // Upstream's shipped file is all comments: an empty list.
        let shipped = "#two kinds of ip format\n#1. a single ip address \n#1.2.2.1\n\n\
                       #1.1.1.1-1.1.1.2\n#    3. do not include extra space\n";
        assert!(IpList::parse(shipped).entries.is_empty());
        let l = IpList::parse(
            "10.0.0.1\n192.168.1.120-192.168.1.129\r\n 172.16.*.* \nshort\n1.2.3.4 # c\n::1\nfe80::1:2\n",
        );
        assert_eq!(l.entries.len(), 4, "{:?}", l.entries);
        let m = |ip: &str| l.matches(ip.parse().ok().map(normalize));
        assert!(m("10.0.0.1"));
        assert!(!m("10.0.0.2"));
        assert!(m("192.168.1.120") && m("192.168.1.125") && m("192.168.1.129"));
        assert!(!m("192.168.1.130") && !m("192.168.1.119"));
        assert!(m("172.16.200.3"));
        assert!(!m("172.17.0.1"));
        assert!(m("fe80::1:2"));
        assert!(
            !m("::1"),
            "a line shorter than 7 characters is skipped, as upstream"
        );
        assert!(m("::ffff:10.0.0.1"), "IPv4-mapped peers match IPv4 entries");
        assert!(!m("1.2.3.4"), "a line with # is a comment");
    }

    #[test]
    fn ranges_are_numeric_and_may_be_reversed() {
        let l = IpList::parse("10.0.1.5-10.0.0.250\n");
        let m = |ip: &str| l.matches(ip.parse().ok());
        assert!(m("10.0.0.251") && m("10.0.1.0") && m("10.0.1.5"));
        assert!(!m("10.0.1.6") && !m("10.0.0.249"));
    }

    #[test]
    fn allow_list_wins_and_an_empty_one_admits_nobody() {
        let dir = tmp("modes");
        std::fs::create_dir_all(&dir).unwrap();
        let allow = dir.join("allow");
        let deny = dir.join("deny");
        std::fs::write(&deny, "127.0.0.1\n").unwrap();
        let f = IpFilter::new(None, Some(deny.clone()));
        assert_eq!(f.mode(), "1");
        assert!(!f.permits("127.0.0.1"));
        assert!(f.permits("127.0.0.2"));
        assert!(f.permits("unknown"));
        let f = IpFilter::new(Some(allow.clone()), Some(deny));
        assert_eq!(f.mode(), "2");
        assert!(!f.permits("127.0.0.2"), "missing allow file: nobody");
        f.save(ListKind::Allow, &["127.0.0.2".into()]).unwrap();
        assert!(f.permits("127.0.0.2"));
        assert!(!f.permits("unknown"));
        assert!(std::fs::read_to_string(&allow)
            .unwrap()
            .starts_with(SAVED_HEADER));
        assert_eq!(
            f.save(ListKind::Allow, &["1.2.3.4\n5.6.7.8".into()]),
            Err(SaveError::Invalid("1.2.3.4\n5.6.7.8".into()))
        );
        let none = IpFilter::new(None, None);
        assert_eq!(none.mode(), "3");
        assert!(none.permits("8.8.8.8"));
        assert_eq!(
            none.save(ListKind::Deny, &["1.2.3.4".into()]),
            Err(SaveError::NotConfigured(ListKind::Deny))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_picks_up_edited_list_files() {
        let dir = tmp("reload");
        std::fs::create_dir_all(&dir).unwrap();
        let deny = dir.join("deny");
        std::fs::write(&deny, "10.9.9.9\n").unwrap();
        let f = IpFilter::new(None, Some(deny.clone()));
        assert!(f.permits("10.9.9.8"));
        std::fs::write(&deny, "10.9.9.0-10.9.9.255\n").unwrap();
        assert!(f.permits("10.9.9.8"), "not re-read on its own");
        f.reload();
        assert!(!f.permits("10.9.9.8"));
        assert_eq!(
            f.entries(ListKind::Deny),
            Some(vec!["10.9.9.0-10.9.9.255".to_string()])
        );
        assert_eq!(f.entries(ListKind::Allow), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lockout_after_more_than_four_failures_until_reset() {
        let f = IpFilter::new(None, None);
        for _ in 0..MAX_AUTH_FAILURES {
            f.note_failure("10.1.1.1");
        }
        assert!(!f.locked_out("10.1.1.1"));
        f.note_failure("10.1.1.1");
        assert!(f.locked_out("10.1.1.1"));
        assert!(!f.locked_out("10.1.1.2"), "per address");
        f.reset("10.1.1.1");
        assert!(!f.locked_out("10.1.1.1"));
        assert_eq!(f.failures("10.1.1.1"), 0);
    }
}
