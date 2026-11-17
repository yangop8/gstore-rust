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
//! one entry per line; a line containing `#` is a comment, as is any other
//! line shorter than 7 characters that is not an entry (upstream skips every
//! line shorter than 7 characters; a short IPv6 form such as `::1` is an
//! entry here, so a list `ipmanage` saved loads back as it was saved). An
//! entry is
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
//! **Default (review gapi#5 / sec#7 / http#6).** Failed authentications are
//! counted per *account and address* ([`LoginLockout`]): with more than
//! [`MAX_AUTH_FAILURES`] failures of one account from one address (the
//! next failure after `login_lockout_failures` of them locks), that
//! account is locked for that address — first for one second, each further
//! failure doubling the lock up to 15 minutes ([`LockoutPolicy`];
//! `conf.ini`'s `login_lockout_failures` / `login_lockout_max_seconds`) —
//! and the counts are forgotten after an hour without a failure. The
//! address is the client's IPv4 address, or the /64 network of an IPv6 one
//! (an IPv4-mapped IPv6 address counts as its IPv4 address,
//! [`lockout_address`]): rotating addresses inside one's /64 gets no fresh
//! counts. Every path
//! that checks a password honours it before it runs the (deliberately slow)
//! password hash: `/api` `login`, `testConnect` and every authenticated
//! operation, the lifecycle routes, REST `/login` and Basic credentials, and
//! the pre-body credential check (`crate::preauth`); a refused attempt is
//! answered `1101` (REST: `429` with `Retry-After`) and costs no hash. A
//! successful authentication clears only its own account's count from that
//! address, so one account's login never unlocks another. Failures of every
//! account from one address also lock the address once there are more than
//! [`LockoutPolicy::max_address_failures`] of them (the same back-off), so
//! guessing across many user names gets no more hashes for free — but that
//! address-wide lock refuses only accounts not known good at the address:
//! one that authenticated successfully from it within the last
//! `forget_after` (an hour) keeps working, as do other operations of
//! already logged-in sessions, so clients behind one proxy or NAT are not
//! locked out of the server by someone else's guessing (round-2 review). It
//! is answered "too many failed logins from this address" (an account's own
//! lock: "… for this account from this address"). The key is the TCP peer's
//! address: behind a reverse proxy every client shares the proxy's.
//!
//! **`upstream_login_lockout=on`** (an upstream-compat switch; `gserver`
//! warns at startup) restores upstream's rule: every failed authentication
//! from an address counts against it (upstream `update_access_ip_error_num`).
//! With more than [`MAX_AUTH_FAILURES`] failures the address is locked out:
//! every operation except `login` and `testConnect` (and REST `/login`) is
//! answered `1101` (upstream's level-2 check) — until a successful `login`
//! from it resets the count (upstream `reset_access_ip_error_num`) or the
//! server restarts (the counts are kept in memory only, as upstream). Under
//! it `login` and `testConnect` keep evaluating guesses from a locked-out
//! address, each one a password hash.
//!
//! # `ipmanage`
//!
//! `type=1` lists the list in effect (`ip_type` `"2"` the allow list, `"1"`
//! the deny list, with the entries as `ips`); `type=2` replaces a list file
//! (`ip_type` `1` deny / `2` allow, `ips` comma-separated — or a JSON array)
//! and reloads it. Root only.

use std::collections::{BTreeSet, HashMap};
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::envelope::{GResp, Status};
use super::params::Params;
use super::Ctx;
use crate::config::GStoreConfig;
use crate::json::Json;

/// An address (by default: an account at an address) with more failed
/// authentications than this is locked out (upstream: `errornum > 4`).
pub const MAX_AUTH_FAILURES: u32 = 4;

/// The most addresses whose failures are tracked at once (bounds the memory a
/// client spraying source addresses can make the server use).
const MAX_TRACKED: usize = 65_536;

/// Upstream's message for an address the lists keep out.
pub const BLOCKED_MSG: &str = "IP Blocked!";

/// Upstream's message for a locked-out address (`upstream_login_lockout`).
pub const LOCKED_MSG: &str = "The ip has too many error during accessing server, the ip has \
                              been locked until the server restart!";

/// How a refused attempt of the default lockout ([`LoginLockout`]) starts
/// its message (see [`lockout_msg`]).
pub const LOCKOUT_MSG_PREFIX: &str = "too many failed logins";

/// The message of an authentication refused by the default lockout, for an
/// account locked for `wait` more (rounded up to whole seconds).
pub fn lockout_msg(wait: Duration) -> String {
    format!(
        "{LOCKOUT_MSG_PREFIX} for this account from this address: retry in {} s",
        retry_after_secs(wait)
    )
}

/// The message of an authentication refused by the default lockout's
/// address-wide lock (guessing across many accounts from the address), for
/// `wait` more (rounded up to whole seconds).
pub fn address_lockout_msg(wait: Duration) -> String {
    format!(
        "{LOCKOUT_MSG_PREFIX} from this address: retry in {} s",
        retry_after_secs(wait)
    )
}

/// Why the default lockout refuses a password check, and for how long
/// ([`LoginLockout::refusal_detail`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockRefusal {
    /// How much longer the lock lasts.
    pub wait: Duration,
    /// The address-wide lock (guessing across accounts), not the account's
    /// own.
    pub address_wide: bool,
}

impl LockRefusal {
    /// The refused attempt's message ([`lockout_msg`] /
    /// [`address_lockout_msg`]).
    pub fn message(&self) -> String {
        if self.address_wide {
            address_lockout_msg(self.wait)
        } else {
            lockout_msg(self.wait)
        }
    }
}

/// The address a failed authentication from `ip` counts against: an IPv4
/// address as it is, an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) as its
/// IPv4 address, and any other IPv6 address as its /64 network
/// (`2001:db8:1:2::/64`) — the block one subscriber is usually given, so
/// rotating addresses inside it gets no fresh counts. Anything else (not an
/// address) as given.
pub fn lockout_address(ip: &str) -> String {
    match ip.trim().parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.to_string(),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
        Err(_) => ip.to_string(),
    }
}

/// `wait` in whole seconds, rounded up (at least 1): the `Retry-After` value
/// of a refused attempt.
pub fn retry_after_secs(wait: Duration) -> u64 {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    secs.max(1)
}

/// The failed-login policy of a [`LoginLockout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutPolicy {
    /// Failed authentications of one account from one address that are let
    /// through: the next one locks the account for that address (default
    /// [`MAX_AUTH_FAILURES`], upstream's count — the 5th failure locks, as
    /// upstream's `errornum > 4`; `conf.ini` `login_lockout_failures`). `0`
    /// locks after every failure.
    pub max_failures: u32,
    /// How long the first lock lasts (default 1 s). Every further failure
    /// once locked (an attempt let through after a lock ended, which failed
    /// again) doubles it.
    pub first_lock: Duration,
    /// The longest lock (default 15 min; `conf.ini`
    /// `login_lockout_max_seconds`).
    pub max_lock: Duration,
    /// The counts are forgotten after this long without a failure (default
    /// 1 h).
    pub forget_after: Duration,
    /// Failed authentications of every account from one address that are
    /// let through: the next one locks the whole address, with the same
    /// back-off (default `8 × max_failures`, at least 8) — so guessing across
    /// many user names from one address is bounded too.
    pub max_address_failures: u32,
}

impl Default for LockoutPolicy {
    fn default() -> LockoutPolicy {
        LockoutPolicy::with_limits(MAX_AUTH_FAILURES, Duration::from_secs(15 * 60))
    }
}

impl LockoutPolicy {
    /// The default policy with `max_failures` and `max_lock` (`conf.ini`'s
    /// `login_lockout_failures` / `login_lockout_max_seconds`).
    pub fn with_limits(max_failures: u32, max_lock: Duration) -> LockoutPolicy {
        LockoutPolicy {
            max_failures,
            first_lock: Duration::from_secs(1).min(max_lock),
            max_lock,
            forget_after: Duration::from_secs(3600).max(max_lock),
            max_address_failures: max_failures.saturating_mul(8).max(8),
        }
    }

    /// The lock after the `n`-th failure in a row (`threshold`: the
    /// failures let through): none up to `threshold`, then `first_lock`,
    /// doubled for every failure after that, at most `max_lock`.
    fn lock_after(&self, n: u32, threshold: u32) -> Option<Duration> {
        let over = n.checked_sub(threshold)?.checked_sub(1)?;
        let factor = 1u32.checked_shl(over.min(31)).unwrap_or(u32::MAX);
        Some(
            self.first_lock
                .checked_mul(factor)
                .unwrap_or(self.max_lock)
                .min(self.max_lock),
        )
    }
}

/// One tracked key's failures.
#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    last: Instant,
    locked_until: Option<Instant>,
}

impl Failures {
    /// The lock still in force at `now`.
    fn locked_for(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .and_then(|t| t.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }
}

#[derive(Debug, Default)]
struct LockoutState {
    /// The policy in force (replaceable at run time, keeping the counts).
    policy: LockoutPolicy,
    /// Per (address, account hash).
    accounts: HashMap<(String, u64), Failures>,
    /// Per address, every account.
    addresses: HashMap<String, Failures>,
    /// (address, account hash) → its last successful authentication: an
    /// account known good at an address is not refused by that address's
    /// address-wide lock.
    known: HashMap<(String, u64), Instant>,
}

/// The default failed-login lockout (see the module docs): failures counted
/// per account and address with an exponential, expiring back-off, plus a
/// higher per-address bound across accounts. Thread-safe; memory only.
///
/// [`refusal`](Self::refusal) is asked **before** a password is hashed;
/// [`note_failure`](Self::note_failure) / [`note_success`](Self::note_success)
/// record the outcome after.
#[derive(Debug)]
pub struct LoginLockout {
    state: Mutex<LockoutState>,
    /// Account names are kept as keyed hashes (an attempt may carry any
    /// name, of any length).
    hasher: std::collections::hash_map::RandomState,
}

impl Default for LoginLockout {
    fn default() -> LoginLockout {
        LoginLockout::new(LockoutPolicy::default())
    }
}

impl LoginLockout {
    /// A lockout with `policy`.
    pub fn new(policy: LockoutPolicy) -> LoginLockout {
        LoginLockout {
            state: Mutex::new(LockoutState {
                policy,
                ..LockoutState::default()
            }),
            hasher: std::collections::hash_map::RandomState::new(),
        }
    }

    /// The policy in force.
    pub fn policy(&self) -> LockoutPolicy {
        self.state().policy
    }

    /// Replace the policy (`refreshconf`), keeping the counts and the locks
    /// already set: the next failure is judged by the new policy.
    pub fn set_policy(&self, policy: LockoutPolicy) {
        self.state().policy = policy;
    }

    fn state(&self) -> MutexGuard<'_, LockoutState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn key(&self, account: &str, ip: &str) -> (String, u64) {
        (lockout_address(ip), self.hasher.hash_one(account))
    }

    /// How much longer `account` is locked for address `ip` (its own lock
    /// or the address's), `None` when an attempt may be checked.
    pub fn refusal(&self, account: &str, ip: &str) -> Option<Duration> {
        self.refusal_at(account, ip, Instant::now())
    }

    /// [`refusal`](Self::refusal) with which lock refuses it.
    pub fn refusal_detail(&self, account: &str, ip: &str) -> Option<LockRefusal> {
        self.refusal_detail_at(account, ip, Instant::now())
    }

    fn refusal_at(&self, account: &str, ip: &str, now: Instant) -> Option<Duration> {
        self.refusal_detail_at(account, ip, now).map(|r| r.wait)
    }

    fn refusal_detail_at(&self, account: &str, ip: &str, now: Instant) -> Option<LockRefusal> {
        let key = self.key(account, ip);
        let st = self.state();
        let own = st.accounts.get(&key).and_then(|f| f.locked_for(now));
        // The address-wide lock spares an account known good there.
        let known = st
            .known
            .get(&key)
            .is_some_and(|t| now.duration_since(*t) < st.policy.forget_after);
        let addr = if known {
            None
        } else {
            st.addresses.get(&key.0).and_then(|f| f.locked_for(now))
        };
        match (own, addr) {
            (own, Some(a)) if own.is_none_or(|o| a > o) => Some(LockRefusal {
                wait: a,
                address_wide: true,
            }),
            (Some(o), _) => Some(LockRefusal {
                wait: o,
                address_wide: false,
            }),
            _ => None,
        }
    }

    /// The failures counted against `account` at `ip` (forgotten ones
    /// excluded).
    pub fn failures(&self, account: &str, ip: &str) -> u32 {
        let now = Instant::now();
        let key = self.key(account, ip);
        let st = self.state();
        let forget_after = st.policy.forget_after;
        st.accounts
            .get(&key)
            .filter(|f| now.duration_since(f.last) < forget_after)
            .map_or(0, |f| f.count)
    }

    /// Record a failed authentication of `account` from `ip`.
    pub fn note_failure(&self, account: &str, ip: &str) {
        self.note_failure_at(account, ip, Instant::now());
    }

    fn note_failure_at(&self, account: &str, ip: &str, now: Instant) {
        let key = self.key(account, ip);
        let mut st = self.state();
        let policy = st.policy;
        let bump = |f: &mut Failures, threshold: u32| {
            if now.duration_since(f.last) >= policy.forget_after {
                f.count = 0;
                f.locked_until = None;
            }
            f.count = f.count.saturating_add(1);
            f.last = now;
            if let Some(lock) = policy.lock_after(f.count, threshold) {
                f.locked_until = Some(now + lock);
            }
        };
        let fresh = Failures {
            count: 0,
            last: now,
            locked_until: None,
        };
        let ip_key = key.0.clone();
        if make_room(&mut st.accounts, &key, now, policy.forget_after) {
            bump(st.accounts.entry(key).or_insert(fresh), policy.max_failures);
        }
        if make_room(&mut st.addresses, &ip_key, now, policy.forget_after) {
            bump(
                st.addresses.entry(ip_key).or_insert(fresh),
                policy.max_address_failures,
            );
        }
    }

    /// Record a successful authentication of `account` from `ip`: that
    /// account's count at that address is cleared (no other account's, and
    /// not the address's count across accounts), and the account is known
    /// good there — the address-wide lock does not refuse it for the next
    /// `forget_after`.
    pub fn note_success(&self, account: &str, ip: &str) {
        self.note_success_at(account, ip, Instant::now());
    }

    fn note_success_at(&self, account: &str, ip: &str, now: Instant) {
        let key = self.key(account, ip);
        let mut st = self.state();
        st.accounts.remove(&key);
        let forget_after = st.policy.forget_after;
        if !st.known.contains_key(&key) && st.known.len() >= MAX_TRACKED {
            st.known
                .retain(|_, t| now.duration_since(*t) < forget_after);
        }
        if st.known.contains_key(&key) || st.known.len() < MAX_TRACKED {
            st.known.insert(key, now);
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> (usize, usize) {
        let st = self.state();
        (st.accounts.len(), st.addresses.len())
    }
}

/// Make room in `map` for `key` (bounded by [`MAX_TRACKED`]): forget the
/// expired entries, then one that is not locked. `false`: every tracked
/// entry is locked, so `key` is not tracked.
fn make_room<K: std::hash::Hash + Eq + Clone>(
    map: &mut HashMap<K, Failures>,
    key: &K,
    now: Instant,
    forget_after: Duration,
) -> bool {
    if map.contains_key(key) || map.len() < MAX_TRACKED {
        return true;
    }
    map.retain(|_, f| now.duration_since(f.last) < forget_after);
    if map.len() < MAX_TRACKED {
        return true;
    }
    let victim = map
        .iter()
        .find(|(_, f)| f.locked_for(now).is_none())
        .map(|(k, _)| k.clone());
    match victim {
        Some(k) => {
            map.remove(&k);
            true
        }
        None => false,
    }
}

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
    /// `#` are skipped, and so are lines shorter than 7 characters unless
    /// they are an entry — upstream skips every short line, which would drop
    /// the short IPv6 forms (`::1`, `::`, `1::`) that `ipmanage` accepts and
    /// writes, and could leave a saved allow list empty (review gapi#7).
    /// Entries that do not parse are listed but match nothing.
    fn parse(text: &str) -> IpList {
        let mut list = IpList::default();
        for line in text.lines() {
            let line = line.trim();
            if line.contains('#') {
                continue;
            }
            let rule = Rule::parse(line);
            if line.len() < 7 && rule.is_none() {
                continue;
            }
            if list.entries.insert(line.to_string()) {
                if let Some(rule) = rule {
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
    /// Failed authentications per address (`upstream_login_lockout`).
    failures: HashMap<String, u32>,
}

/// The server's IP access control and failed-login lockout (see the module
/// docs). Thread-safe.
#[derive(Debug, Default)]
pub struct IpFilter {
    state: Mutex<State>,
    /// The default lockout (per account and address).
    lockout: LoginLockout,
    /// `upstream_login_lockout`: upstream's per-address lockout instead.
    upstream_lockout: std::sync::atomic::AtomicBool,
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
    /// A filter with the given list files (loaded now) and the default
    /// lockout ([`auth_refusal`](Self::auth_refusal) /
    /// [`note_auth_failure`](Self::note_auth_failure)). Upstream's
    /// per-address lockout ([`note_failure`](Self::note_failure) /
    /// [`locked_out`](Self::locked_out)) counts but never locks out unless
    /// the filter is built [`with_upstream_lockout(true)`](Self::with_upstream_lockout)
    /// (round 2, review gapi#5: before, `locked_out` locked after more than
    /// [`MAX_AUTH_FAILURES`] `note_failure`s on every filter).
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
            lockout: LoginLockout::default(),
            upstream_lockout: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A filter from `conf.ini`'s `ip_allow_path` / `ip_deny_path`, with the
    /// lockout of `upstream_login_lockout`, `login_lockout_failures` and
    /// `login_lockout_max_seconds`.
    pub fn from_config(config: &GStoreConfig) -> IpFilter {
        let (policy, upstream) = IpFilter::lockout_of(config);
        IpFilter::new(config.ip_allow_path.clone(), config.ip_deny_path.clone())
            .with_lockout_policy(policy)
            .with_upstream_lockout(upstream)
    }

    /// Warnings about the list files `conf.ini` configures but that cannot be
    /// read (missing, a directory, unreadable). Such a list is empty, as
    /// upstream reads it — a deny list then keeps nobody out and an allow
    /// list lets nobody in — which a server must not decide silently (round-4
    /// review of bulkcli#9's fix: `ip_deny_path=./conf/ipDeny.config
    /// #blocked`, which earlier gstore-rust versions cut at the note, names
    /// no file now). `gserver` prints them at startup with the `conf.ini`
    /// warnings. A deny list missing at its built-in place
    /// (`./conf/ipDeny.config`, which the shipped example names too) is the
    /// out-of-the-box state — no address is denied — and not reported.
    pub fn list_file_warnings(config: &GStoreConfig) -> Vec<String> {
        let default_deny = GStoreConfig::default().ip_deny_path;
        let at_default = |p: &Path| {
            let p = p.strip_prefix(".").unwrap_or(p);
            default_deny.as_deref() == Some(p)
        };
        let lists = [
            (
                "ip_allow_path",
                &config.ip_allow_path,
                "the allow list is empty: no address is let in to /api",
            ),
            (
                "ip_deny_path",
                &config.ip_deny_path,
                "the deny list is empty: no address is kept out of /api",
            ),
        ];
        let mut out = Vec::new();
        for (key, path, effect) in lists {
            let Some(path) = path else { continue };
            let problem = match std::fs::metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if key == "ip_deny_path" && at_default(path) {
                        continue;
                    }
                    "does not exist".to_string()
                }
                Err(e) => format!("cannot be read ({e})"),
                Ok(m) if m.is_dir() => "is a directory".to_string(),
                Ok(_) => match std::fs::File::open(path) {
                    Ok(_) => continue,
                    Err(e) => format!("cannot be read ({e})"),
                },
            };
            out.push(format!("{key} '{}' {problem}; {effect}", path.display()));
        }
        out
    }

    /// Use upstream's per-address lockout (`on`; the
    /// `upstream_login_lockout` switch, see the module docs) instead of the
    /// default per-account one.
    pub fn with_upstream_lockout(self, on: bool) -> IpFilter {
        self.upstream_lockout
            .store(on, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Use `policy` for the default lockout (its counts start afresh).
    pub fn with_lockout_policy(mut self, policy: LockoutPolicy) -> IpFilter {
        self.lockout = LoginLockout::new(policy);
        self
    }

    /// Replace the lockout in force at run time (`refreshconf`): `policy`
    /// for the default lockout, `upstream` for `upstream_login_lockout`.
    /// The counts and locks already set are kept.
    pub fn set_lockout(&self, policy: LockoutPolicy, upstream: bool) {
        self.lockout.set_policy(policy);
        self.upstream_lockout
            .store(upstream, std::sync::atomic::Ordering::SeqCst);
    }

    /// The lockout `config` asks for: the default lockout's policy and
    /// whether `upstream_login_lockout` is on.
    pub fn lockout_of(config: &GStoreConfig) -> (LockoutPolicy, bool) {
        (
            LockoutPolicy::with_limits(
                config.login_lockout_failures,
                Duration::from_secs(config.login_lockout_max_seconds),
            ),
            config.upstream_login_lockout,
        )
    }

    /// Whether upstream's per-address lockout is in force
    /// (`upstream_login_lockout`).
    pub fn upstream_lockout(&self) -> bool {
        self.upstream_lockout
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The default lockout (consulted only when
    /// [`upstream_lockout`](Self::upstream_lockout) is off).
    pub fn lockout(&self) -> &LoginLockout {
        &self.lockout
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

    /// Whether `ip` is locked out of every operation after too many failed
    /// authentications (upstream's level-2 check) — only under
    /// `upstream_login_lockout`: the default lockout refuses password checks
    /// of a locked account ([`auth_refusal`](Self::auth_refusal)), not the
    /// address's other requests.
    pub fn locked_out(&self, ip: &str) -> bool {
        self.upstream_lockout() && self.failures(ip) > MAX_AUTH_FAILURES
    }

    /// The failed authentications counted against `ip` by upstream's
    /// per-address lockout ([`note_failure`](Self::note_failure)).
    pub fn failures(&self, ip: &str) -> u32 {
        self.state().failures.get(ip).copied().unwrap_or(0)
    }

    /// Whether a password check of `account` from `ip` must be refused
    /// without running it, and for how long (the default lockout; always
    /// `None` under `upstream_login_lockout`, whose locked-out address is
    /// refused by [`locked_out`](Self::locked_out) instead). Ask it before
    /// the password hash runs.
    pub fn auth_refusal(&self, account: &str, ip: &str) -> Option<Duration> {
        self.auth_refusal_detail(account, ip).map(|r| r.wait)
    }

    /// [`auth_refusal`](Self::auth_refusal) with which lock refuses it (for
    /// its message, [`LockRefusal::message`]).
    pub fn auth_refusal_detail(&self, account: &str, ip: &str) -> Option<LockRefusal> {
        if self.upstream_lockout() {
            None
        } else {
            self.lockout.refusal_detail(account, ip)
        }
    }

    /// Record a failed password check of `account` from `ip`: against the
    /// account at that address (default), or against the address
    /// (`upstream_login_lockout`, upstream `update_access_ip_error_num`).
    pub fn note_auth_failure(&self, account: &str, ip: &str) {
        if self.upstream_lockout() {
            self.note_failure(ip);
        } else {
            self.lockout.note_failure(account, ip);
        }
    }

    /// Record a successful password check of `account` from `ip`: clears
    /// that account's count at that address (default). Upstream clears an
    /// address only on a `login` ([`note_login_success`](Self::note_login_success)).
    pub fn note_auth_success(&self, account: &str, ip: &str) {
        if !self.upstream_lockout() {
            self.lockout.note_success(account, ip);
        }
    }

    /// Record a successful `login` of `account` from `ip`: clears that
    /// account's count at that address (default), or the address's count
    /// (`upstream_login_lockout`, upstream `reset_access_ip_error_num`).
    pub fn note_login_success(&self, account: &str, ip: &str) {
        if self.upstream_lockout() {
            self.reset(ip);
        } else {
            self.lockout.note_success(account, ip);
        }
    }

    /// Count a failed authentication from `ip` in upstream's per-address
    /// lockout (see [`note_auth_failure`](Self::note_auth_failure), which
    /// picks the lockout in force).
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

    /// Clear `ip`'s failed authentications in upstream's per-address lockout
    /// (a successful `login`).
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
    /// range or a pattern, and the file is written only if the loader reads
    /// back every entry (an allow list never silently loses one, review
    /// gapi#7).
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
        let saved = IpList::parse(&text);
        if let Some(lost) = ips
            .iter()
            .map(|ip| ip.trim())
            .find(|ip| !saved.entries.contains(*ip))
        {
            return Err(SaveError::Invalid(lost.to_string()));
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| SaveError::Io(e.to_string()))?;
        }
        // `Err`: the file holds its previous list, so the filter keeps it. A
        // file that was replaced but whose directory sync failed holds the new
        // list (a reload or a restart reads it): the filter takes it too, and
        // the failed sync is a durability warning.
        let unsynced = crate::db::fsutil::atomic_replace(path, text.as_bytes(), false)
            .map_err(|e| SaveError::Io(e.to_string()))?;
        if let Some(e) = unsynced {
            crate::gs_warn!(
                "{} was replaced, but syncing its directory failed: {e}; the new list is in \
                 effect but may not survive a crash",
                path.display()
            );
        }
        *list = saved;
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
        // review gapi#7: `::1` (shorter than 7 characters) used to be skipped
        // as upstream skips short lines, so a list ipmanage saved with it lost
        // the entry; a short line that is a valid entry now loads.
        assert_eq!(l.entries.len(), 5, "{:?}", l.entries);
        let m = |ip: &str| l.matches(ip.parse().ok().map(normalize));
        assert!(m("10.0.0.1"));
        assert!(!m("10.0.0.2"));
        assert!(m("192.168.1.120") && m("192.168.1.125") && m("192.168.1.129"));
        assert!(!m("192.168.1.130") && !m("192.168.1.119"));
        assert!(m("172.16.200.3"));
        assert!(!m("172.17.0.1"));
        assert!(m("fe80::1:2"));
        assert!(
            m("::1"),
            "review gapi#7: a short line that is an entry is loaded"
        );
        assert!(
            !l.entries.contains("short"),
            "a short line that is not an entry is skipped, as upstream"
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

    /// A save whose file was replaced but whose directory sync failed after
    /// the rename (also on the retry): the file holds the new list, so the
    /// filter takes it too — memory and file agree (round-1 review of
    /// gapi#7's rollback: the filter kept the old list while the file held
    /// the new one). A failure before the rename keeps the old list.
    #[test]
    fn a_save_whose_directory_sync_failed_after_the_rename_takes_the_new_list() {
        use crate::db::fsutil::FAIL_DIR_SYNC_AFTER_RENAME;
        /// Removes the directory, also when the test fails.
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = tmp("unsynced");
        let _cleanup = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let deny = dir.join("deny");
        std::fs::write(&deny, "10.1.1.1\n").unwrap();
        let f = IpFilter::new(None, Some(deny.clone()));
        FAIL_DIR_SYNC_AFTER_RENAME.with(|n| n.set(2));
        let saved = f.save(ListKind::Deny, &["10.2.2.2".into()]);
        FAIL_DIR_SYNC_AFTER_RENAME.with(|n| n.set(0));
        assert_eq!(saved, Ok(()));
        assert!(std::fs::read_to_string(&deny).unwrap().contains("10.2.2.2"));
        assert!(!f.permits("10.2.2.2"), "the new list is in effect");
        assert!(f.permits("10.1.1.1"), "the old list is not");
        f.reload();
        assert_eq!(f.entries(ListKind::Deny), Some(vec!["10.2.2.2".into()]));
        // Nothing replaced (the target is a directory): the old list stays.
        std::fs::remove_file(&deny).unwrap();
        std::fs::create_dir_all(deny.join("blocker")).unwrap();
        assert!(matches!(
            f.save(ListKind::Deny, &["10.3.3.3".into()]),
            Err(SaveError::Io(_))
        ));
        assert!(!f.permits("10.2.2.2") && f.permits("10.3.3.3"));
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
        // review gapi#5: upstream's per-address lockout is the
        // `upstream_login_lockout` switch now (the default counts per
        // account and address, see the tests below).
        let f = IpFilter::new(None, None).with_upstream_lockout(true);
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

    #[test]
    fn the_default_lockout_counts_per_account_and_address_with_backoff() {
        let l = LoginLockout::default();
        let t0 = Instant::now();
        for _ in 0..MAX_AUTH_FAILURES {
            l.note_failure_at("root", "10.0.0.1", t0);
        }
        assert_eq!(l.refusal_at("root", "10.0.0.1", t0), None, "4 failures");
        l.note_failure_at("root", "10.0.0.1", t0);
        assert_eq!(
            l.refusal_at("root", "10.0.0.1", t0),
            Some(Duration::from_secs(1)),
            "the 5th failure locks root at that address for 1 s"
        );
        assert_eq!(l.refusal_at("bob", "10.0.0.1", t0), None, "per account");
        assert_eq!(l.refusal_at("root", "10.0.0.2", t0), None, "per address");
        // The lock expires; each further failure doubles it.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(l.refusal_at("root", "10.0.0.1", t1), None, "expired");
        l.note_failure_at("root", "10.0.0.1", t1);
        assert_eq!(
            l.refusal_at("root", "10.0.0.1", t1),
            Some(Duration::from_secs(2))
        );
        let mut t = t1;
        for _ in 0..20 {
            t += Duration::from_secs(1000);
            l.note_failure_at("root", "10.0.0.1", t);
        }
        assert_eq!(
            l.refusal_at("root", "10.0.0.1", t),
            Some(Duration::from_secs(15 * 60)),
            "capped at max_lock"
        );
        // bob's success does not unlock root; root's own success does.
        l.note_success("bob", "10.0.0.1");
        assert!(l.refusal_at("root", "10.0.0.1", t).is_some());
        l.note_success("root", "10.0.0.1");
        assert_eq!(l.refusal_at("root", "10.0.0.1", t), None);
        assert_eq!(l.failures("root", "10.0.0.1"), 0);
    }

    #[test]
    fn the_default_lockout_forgets_after_a_quiet_hour() {
        let l = LoginLockout::default();
        let t0 = Instant::now();
        for _ in 0..MAX_AUTH_FAILURES {
            l.note_failure_at("alice", "10.0.0.9", t0);
        }
        // An hour without failures: the count starts afresh.
        let later = t0 + Duration::from_secs(3600);
        l.note_failure_at("alice", "10.0.0.9", later);
        assert_eq!(l.refusal_at("alice", "10.0.0.9", later), None);
    }

    #[test]
    fn guessing_across_accounts_locks_the_address() {
        let l = LoginLockout::default();
        let max = l.policy().max_address_failures;
        let t0 = Instant::now();
        // One failure per user name: no account reaches its own threshold.
        for i in 0..max {
            l.note_failure_at(&format!("user{i}"), "10.0.0.3", t0);
        }
        assert_eq!(l.refusal_at("someone", "10.0.0.3", t0), None);
        l.note_failure_at("one-more", "10.0.0.3", t0);
        assert!(
            l.refusal_at("someone", "10.0.0.3", t0).is_some(),
            "the address is locked for every account"
        );
        assert_eq!(l.refusal_at("someone", "10.0.0.4", t0), None);
        // review (round-2 land review, security-limits): a success does not
        // lift the address's lock for the other accounts — but the account
        // that authenticated is known good there now, and the address-wide
        // lock no longer refuses it (before, every account behind a shared
        // address stayed refused, this one included).
        l.note_success("someone", "10.0.0.3");
        assert!(l.refusal_at("someone-else", "10.0.0.3", t0).is_some());
        assert_eq!(l.refusal_at("someone", "10.0.0.3", t0), None);
    }

    /// Round-2 review (security-limits): the address-wide lock refuses only
    /// accounts not known good at the address, says so in its own message,
    /// and an account's own lock still applies to a known-good account.
    #[test]
    fn the_address_lock_spares_accounts_known_good_there() {
        let l = LoginLockout::default();
        let t0 = Instant::now();
        l.note_success_at("alice", "10.0.0.5", t0);
        for i in 0..=l.policy().max_address_failures {
            l.note_failure_at(&format!("nobody{i}"), "10.0.0.5", t0);
        }
        let refused = l.refusal_detail_at("carol", "10.0.0.5", t0).unwrap();
        assert!(refused.address_wide);
        assert!(
            refused
                .message()
                .starts_with("too many failed logins from this address"),
            "{}",
            refused.message()
        );
        assert_eq!(l.refusal_detail_at("alice", "10.0.0.5", t0), None);
        // Known good, and still bounded by its own count.
        for _ in 0..=MAX_AUTH_FAILURES {
            l.note_failure_at("alice", "10.0.0.5", t0);
        }
        let own = l.refusal_detail_at("alice", "10.0.0.5", t0).unwrap();
        assert!(!own.address_wide);
        assert!(
            own.message().contains("for this account"),
            "{}",
            own.message()
        );
        // Known good only for a while (`forget_after`).
        let later = t0 + l.policy().forget_after;
        l.note_failure_at("nobody", "10.0.0.5", later);
        for i in 0..=l.policy().max_address_failures {
            l.note_failure_at(&format!("again{i}"), "10.0.0.5", later);
        }
        assert!(l.refusal_at("alice", "10.0.0.5", later).is_some());
    }

    /// Round-2 review (security-limits): IPv6 clients count per /64 network
    /// (rotating addresses inside it gets no fresh counts), an IPv4-mapped
    /// address as its IPv4 address.
    #[test]
    fn ipv6_clients_are_counted_per_64_network() {
        assert_eq!(lockout_address("2001:db8:1:2::7"), "2001:db8:1:2::/64");
        assert_eq!(lockout_address("::ffff:10.1.2.3"), "10.1.2.3");
        assert_eq!(lockout_address("10.1.2.3"), "10.1.2.3");
        assert_eq!(lockout_address("unknown"), "unknown");
        let l = LoginLockout::default();
        let t0 = Instant::now();
        for i in 1..=MAX_AUTH_FAILURES + 1 {
            l.note_failure_at("root", &format!("2001:db8:1:2::{i:x}"), t0);
        }
        assert!(
            l.refusal_at("root", "2001:db8:1:2:ffff:ffff:ffff:ffff", t0)
                .is_some(),
            "the whole /64 is locked for root"
        );
        assert_eq!(l.refusal_at("root", "2001:db8:1:3::1", t0), None);
        for _ in 0..=MAX_AUTH_FAILURES {
            l.note_failure_at("bob", "::ffff:10.9.9.9", t0);
        }
        assert!(l.refusal_at("bob", "10.9.9.9", t0).is_some());
    }

    /// Round-2 review (security-limits): `refreshconf`'s new lockout policy
    /// applies to the next failure, and the counts so far are kept.
    #[test]
    fn a_new_policy_keeps_the_counts() {
        let l = LoginLockout::default();
        let t0 = Instant::now();
        for _ in 0..2 {
            l.note_failure_at("root", "10.0.0.6", t0);
        }
        assert_eq!(l.refusal_at("root", "10.0.0.6", t0), None);
        l.set_policy(LockoutPolicy::with_limits(2, Duration::from_secs(60)));
        assert_eq!(l.policy().max_failures, 2);
        l.note_failure_at("root", "10.0.0.6", t0);
        assert_eq!(
            l.refusal_at("root", "10.0.0.6", t0),
            Some(Duration::from_secs(1)),
            "the 3rd failure locks under the new policy"
        );
    }

    #[test]
    fn the_lockout_tracks_a_bounded_number_of_keys() {
        let l = LoginLockout::default();
        let t0 = Instant::now();
        for i in 0..(MAX_TRACKED + 10) {
            l.note_failure_at("x", &format!("ip{i}"), t0);
        }
        let (accounts, addresses) = l.tracked();
        assert!(accounts <= MAX_TRACKED && addresses <= MAX_TRACKED);
    }

    #[test]
    fn the_filter_picks_the_lockout_in_force() {
        let secure = IpFilter::new(None, None);
        for _ in 0..=MAX_AUTH_FAILURES {
            secure.note_auth_failure("root", "10.2.0.1");
        }
        assert!(secure.auth_refusal("root", "10.2.0.1").is_some());
        assert!(
            !secure.locked_out("10.2.0.1"),
            "the default lockout does not lock the address's other requests"
        );
        secure.note_auth_success("root", "10.2.0.1");
        assert!(secure.auth_refusal("root", "10.2.0.1").is_none());

        let upstream = IpFilter::new(None, None).with_upstream_lockout(true);
        for _ in 0..=MAX_AUTH_FAILURES {
            upstream.note_auth_failure("root", "10.2.0.1");
        }
        assert!(upstream.locked_out("10.2.0.1"));
        assert!(upstream.auth_refusal("root", "10.2.0.1").is_none());
        upstream.note_auth_success("root", "10.2.0.1");
        assert!(upstream.locked_out("10.2.0.1"), "only a login resets");
        upstream.note_login_success("bob", "10.2.0.1");
        assert!(
            !upstream.locked_out("10.2.0.1"),
            "any login resets (upstream)"
        );
    }

    #[test]
    fn lockout_messages_round_up_to_seconds() {
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
        assert_eq!(retry_after_secs(Duration::from_secs(3)), 3);
        assert!(lockout_msg(Duration::from_secs(3)).starts_with(LOCKOUT_MSG_PREFIX));
    }
}
