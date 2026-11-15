//! The user store keeps memory and `users.db` in agreement when a save fails
//! (review sec#15), and its login-session table stays bounded (review
//! sec#12 / http#12).
//!
//! A failed save is injected by replacing `users.db` with a non-empty
//! directory: the atomic replace of the file then fails whoever runs the test
//! (root included).

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gstore::http_users::{Privilege, UserStore};

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_user_store_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn query_only() -> BTreeSet<Privilege> {
    BTreeSet::from([Privilege::Query])
}

/// Make every save of the store at `path` fail; returns the file's bytes.
fn break_saves(path: &PathBuf) -> Vec<u8> {
    let saved = fs::read(path).unwrap();
    fs::remove_file(path).unwrap();
    fs::create_dir_all(path.join("blocker")).unwrap();
    saved
}

/// Undo [`break_saves`], putting `saved` back.
fn restore_saves(path: &PathBuf, saved: &[u8]) {
    fs::remove_dir_all(path).unwrap();
    fs::write(path, saved).unwrap();
}

#[test]
fn a_failed_save_leaves_users_passwords_and_sessions_unchanged() {
    let tmp = TmpDir::new("rollback");
    let path = tmp.0.join("users.db");
    let mut store = UserStore::open(&path, "rootpw").unwrap();
    store.add("alice", "oldpw", query_only()).unwrap();
    store.add("bob", "bobpw", query_only()).unwrap();
    let alice_token = store.login("alice", "oldpw").expect("alice logs in");
    let bob_token = store.login("bob", "bobpw").expect("bob logs in");

    let saved = break_saves(&path);

    // set_password: the old password stays in force, the new one does not.
    assert!(store.set_password("alice", "newpw").is_err());
    assert!(
        store.verify("alice", "oldpw"),
        "old password after a failed change"
    );
    assert!(
        !store.verify("alice", "newpw"),
        "new password after a failed change"
    );
    assert_eq!(
        store.session_user(&alice_token).as_deref(),
        Some("alice"),
        "a failed password change revokes nothing"
    );

    // drop_user: the user stays, with their session.
    assert!(store.drop_user("bob").is_err());
    assert!(store.exists("bob"), "a failed drop removed the user");
    assert_eq!(store.session_user(&bob_token).as_deref(), Some("bob"));

    // add: the user does not exist.
    assert!(store.add("carol", "carolpw", query_only()).is_err());
    assert!(!store.exists("carol"), "a failed add created the user");

    // forget_database / copy_database_privileges: grants unchanged.
    restore_saves(&path, &saved);
    store
        .grant_on("alice", "lubm", &BTreeSet::from([Privilege::Update]))
        .unwrap();
    let saved = break_saves(&path);
    assert!(store.forget_database("lubm").is_err());
    assert!(store.has_priv_on("alice", Privilege::Update, "lubm"));
    assert!(store.copy_database_privileges("lubm", "lubm2").is_err());
    assert!(!store.has_priv_on("alice", Privilege::Update, "lubm2"));

    // What a restart reads is what memory holds.
    restore_saves(&path, &saved);
    drop(store);
    let reopened = UserStore::open(&path, "rootpw").unwrap();
    assert!(reopened.exists("alice") && reopened.exists("bob"));
    assert!(!reopened.exists("carol"));
    assert!(reopened.verify("alice", "oldpw"));
    assert!(reopened.has_priv_on("alice", Privilege::Update, "lubm"));
    assert!(!reopened.has_priv_on("alice", Privilege::Update, "lubm2"));
}

#[test]
fn successful_changes_still_apply_and_revoke_sessions() {
    let tmp = TmpDir::new("apply");
    let path = tmp.0.join("users.db");
    let mut store = UserStore::open(&path, "rootpw").unwrap();
    store.add("alice", "oldpw", query_only()).unwrap();
    store.add("bob", "bobpw", query_only()).unwrap();
    let alice_token = store.login("alice", "oldpw").unwrap();
    let bob_token = store.login("bob", "bobpw").unwrap();
    store.set_password("alice", "newpw").unwrap();
    assert!(store.session_user(&alice_token).is_none());
    assert!(store.verify("alice", "newpw"));
    store.drop_user("bob").unwrap();
    assert!(store.session_user(&bob_token).is_none());
    assert!(!store.exists("bob"));
    let reopened = UserStore::open(&path, "rootpw").unwrap();
    assert!(reopened.verify("alice", "newpw"));
    assert!(!reopened.exists("bob"));
}

/// A user who logs in again and again (and never logs out) only ends their
/// own oldest sessions; the table does not grow without bound.
#[test]
fn a_user_flooding_logins_ends_only_their_own_oldest_sessions() {
    let tmp = TmpDir::new("flood");
    let path = tmp.0.join("users.db");
    let mut store = UserStore::open(&path, "rootpw").unwrap();
    store.add("alice", "alicepw", query_only()).unwrap();
    store.add("mallory", "malpw", query_only()).unwrap();
    let alice = store.start_session("alice");
    let first = store.start_session("mallory");
    // Well above the per-user cap.
    let mut last = String::new();
    for _ in 0..5000 {
        last = store.start_session("mallory");
    }
    assert!(
        store.session_user(&first).is_none(),
        "the flooding user's oldest session is still live"
    );
    assert_eq!(store.session_user(&last).as_deref(), Some("mallory"));
    assert_eq!(
        store.session_user(&alice).as_deref(),
        Some("alice"),
        "another user's session was ended"
    );
}

#[test]
fn expired_sessions_are_swept_on_login() {
    let tmp = TmpDir::new("sweep");
    let path = tmp.0.join("users.db");
    let mut store = UserStore::open(&path, "rootpw").unwrap();
    // Live sessions are kept, whoever holds them.
    for i in 0..3000 {
        store.start_session(&format!("user{i}"));
    }
    assert_eq!(store.session_count(), 3000);
    // With a zero TTL every session is expired as soon as it exists: the
    // logins sweep them out instead of accumulating 10 000 entries.
    store.set_session_ttl(Duration::ZERO);
    for i in 0..10_000 {
        store.start_session(&format!("other{i}"));
    }
    assert!(
        store.session_count() <= 2500,
        "{} sessions held after 10 000 logins whose sessions all expired",
        store.session_count()
    );
    let t = store.start_session("root");
    assert!(
        store.session_user(&t).is_none(),
        "an expired session resolves"
    );
}
