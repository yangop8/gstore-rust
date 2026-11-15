//! `ipmanage` writes only list entries its own loader reads back (review
//! gapi#7).
//!
//! Before the fix `IpFilter::save` accepted `::1` (and the other IPv6 forms
//! shorter than 7 characters), wrote it, and then reloaded the file with the
//! upstream rule that skips every line shorter than 7 characters: the saved
//! allow list was empty, `::1` was refused, and a restart did not help (the
//! file still held `::1`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::gapi::ipfilter::{IpFilter, ListKind};

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("gstore_ipmanage_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn short_ipv6_entries_round_trip_through_save_and_restart() {
    let tmp = TmpDir::new("short");
    let allow = tmp.0.join("ipAllow.config");
    let filter = IpFilter::new(Some(allow.clone()), None);
    let ips: Vec<String> = ["::1", "::", "1::", "fe80::1", "127.0.0.1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    filter.save(ListKind::Allow, &ips).unwrap();
    let mut expect = ips.clone();
    expect.sort();
    assert_eq!(filter.entries(ListKind::Allow), Some(expect.clone()));
    for ip in ["::1", "::", "1::", "fe80::1", "127.0.0.1"] {
        assert!(filter.permits(ip), "{ip} is on the saved allow list");
    }
    assert!(!filter.permits("10.0.0.1"));
    // A restart (a fresh filter on the same file) reads the same list.
    let restarted = IpFilter::new(Some(allow.clone()), None);
    assert_eq!(restarted.entries(ListKind::Allow), Some(expect));
    assert!(restarted.permits("::1"));
    // Re-reading the file in place, too.
    filter.reload();
    assert!(filter.permits("::1"));
}

#[test]
fn an_allow_list_of_only_the_ipv6_loopback_is_not_saved_empty() {
    let tmp = TmpDir::new("loopback");
    let allow = tmp.0.join("ipAllow.config");
    let filter = IpFilter::new(Some(allow.clone()), None);
    filter.save(ListKind::Allow, &["::1".to_string()]).unwrap();
    assert_eq!(
        filter.entries(ListKind::Allow),
        Some(vec!["::1".to_string()]),
        "the allow list silently emptied"
    );
    assert!(filter.permits("::1"), "the IPv6 loopback is locked out");
    let restarted = IpFilter::new(Some(allow), None);
    assert!(restarted.permits("::1"));
}

#[test]
fn hand_written_short_lines_that_are_not_entries_stay_comments() {
    let tmp = TmpDir::new("comments");
    let allow = tmp.0.join("ipAllow.config");
    std::fs::write(&allow, "short\n#::2\n::1\n\nabc\n10.1.2.3\n").unwrap();
    let filter = IpFilter::new(Some(allow), None);
    assert_eq!(
        filter.entries(ListKind::Allow),
        Some(vec!["10.1.2.3".to_string(), "::1".to_string()])
    );
    assert!(filter.permits("::1") && filter.permits("10.1.2.3"));
    assert!(!filter.permits("::2"), "a line with # is a comment");
}
