//! User store and role-based access control (RBAC) for the [`crate::http_api`]
//! HTTP endpoint — the analogue of gStore's `ApiUserOperation` /
//! `system.nt` user database.
//!
//! It is dependency-free (std only): passwords are stored as a **salted,
//! iterated hash** (never plaintext), sessions are opaque random tokens, and the
//! whole store is persisted to a small tab-separated file so users survive a
//! restart.
//!
//! Privilege model (matching gStore's numeric privileges 1..7 plus an `admin`
//! super-privilege):
//!
//! | # | name      | gates                                  |
//! |---|-----------|----------------------------------------|
//! | 1 | `query`   | `/sparql`, read `/tquery`, `/export`   |
//! | 2 | `load`    | `/build`, `/load`                      |
//! | 3 | `unload`  | `/unload`, `/drop`                     |
//! | 4 | `update`  | `/update`, `/batchInsert`, txns        |
//! | 5 | `backup`  | `/backup`                              |
//! | 6 | `restore` | `/restore`                             |
//! | 7 | `export`  | `/export`                              |
//! | - | `admin`   | user management + logs (implies all)   |
//!
//! The built-in `root` user always holds every privilege and cannot be dropped
//! or have its privileges changed (gStore semantics).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The built-in super-user. Always present, always fully privileged.
pub const ROOT_USER: &str = "root";

/// A single access privilege. `Admin` implies every other privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Privilege {
    Query,
    Load,
    Unload,
    Update,
    Backup,
    Restore,
    Export,
    Admin,
}

impl Privilege {
    /// The canonical lowercase token used in the persisted file and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Privilege::Query => "query",
            Privilege::Load => "load",
            Privilege::Unload => "unload",
            Privilege::Update => "update",
            Privilege::Backup => "backup",
            Privilege::Restore => "restore",
            Privilege::Export => "export",
            Privilege::Admin => "admin",
        }
    }

    /// Parse a privilege from either its name (`"query"`) or gStore's numeric
    /// code (`"1"` … `"7"`, plus `"8"`/`"admin"`).
    pub fn parse(s: &str) -> Option<Privilege> {
        Some(match s.trim() {
            "query" | "1" => Privilege::Query,
            "load" | "2" => Privilege::Load,
            "unload" | "3" => Privilege::Unload,
            "update" | "4" => Privilege::Update,
            "backup" | "5" => Privilege::Backup,
            "restore" | "6" => Privilege::Restore,
            "export" | "7" => Privilege::Export,
            "admin" | "8" => Privilege::Admin,
            _ => return None,
        })
    }

    /// Parse a comma-separated privilege list (names and/or numeric codes),
    /// silently skipping blanks. Unknown tokens make the whole parse fail.
    pub fn parse_list(s: &str) -> Option<BTreeSet<Privilege>> {
        let mut out = BTreeSet::new();
        for tok in s.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            out.insert(Privilege::parse(tok)?);
        }
        Some(out)
    }
}

/// One stored user: name, salted password hash, and granted privileges.
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    salt: u64,
    hash: u64,
    pub privileges: BTreeSet<Privilege>,
}

impl User {
    /// Whether this user holds `p` (a `root`/`Admin` user holds everything).
    pub fn has(&self, p: Privilege) -> bool {
        self.name == ROOT_USER
            || self.privileges.contains(&Privilege::Admin)
            || self.privileges.contains(&p)
    }

    /// Render the granted privileges as a sorted comma-separated string.
    pub fn privilege_str(&self) -> String {
        self.privileges
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// An issued login session.
struct Session {
    user: String,
    created: Instant,
}

/// The persistent user database plus in-memory sessions.
pub struct UserStore {
    path: PathBuf,
    users: HashMap<String, User>,
    sessions: HashMap<String, Session>,
    ttl: Duration,
}

impl UserStore {
    /// Load the user store from `path`, creating an empty one if the file is
    /// absent, then ensure the built-in `root` user exists (seeded with the
    /// given password the first time). The file is (re)written so a freshly
    /// seeded `root` is persisted.
    pub fn open(path: impl AsRef<Path>, root_password: &str) -> io::Result<UserStore> {
        let path = path.as_ref().to_path_buf();
        let mut store = UserStore {
            path,
            users: HashMap::new(),
            sessions: HashMap::new(),
            ttl: Duration::from_secs(24 * 3600),
        };
        if store.path.is_file() {
            store.read_file()?;
        }
        if !store.users.contains_key(ROOT_USER) {
            let (salt, hash) = make_hash(root_password);
            store.users.insert(
                ROOT_USER.to_string(),
                User {
                    name: ROOT_USER.to_string(),
                    salt,
                    hash,
                    privileges: BTreeSet::from([Privilege::Admin]),
                },
            );
            store.save()?;
        }
        Ok(store)
    }

    fn read_file(&mut self) -> io::Result<()> {
        let text = fs::read_to_string(&self.path)?;
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let mut cols = line.split('\t');
            let (Some(name), Some(salt), Some(hash)) =
                (cols.next(), cols.next(), cols.next())
            else {
                continue;
            };
            let privileges = cols
                .next()
                .and_then(Privilege::parse_list)
                .unwrap_or_default();
            let (Ok(salt), Ok(hash)) = (
                u64::from_str_radix(salt, 16),
                u64::from_str_radix(hash, 16),
            ) else {
                continue;
            };
            self.users.insert(
                name.to_string(),
                User {
                    name: name.to_string(),
                    salt,
                    hash,
                    privileges,
                },
            );
        }
        Ok(())
    }

    /// Persist the user table to disk (sorted by name for stable output).
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut names: Vec<&String> = self.users.keys().collect();
        names.sort();
        let mut out = String::new();
        for name in names {
            let u = &self.users[name];
            out.push_str(&format!(
                "{}\t{:016x}\t{:016x}\t{}\n",
                u.name,
                u.salt,
                u.hash,
                u.privilege_str()
            ));
        }
        fs::write(&self.path, out)
    }

    // ---- user management --------------------------------------------------

    pub fn exists(&self, name: &str) -> bool {
        self.users.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&User> {
        self.users.get(name)
    }

    pub fn count(&self) -> usize {
        self.users.len()
    }

    /// All users sorted by name (for `/user/show`).
    pub fn list(&self) -> Vec<&User> {
        let mut v: Vec<&User> = self.users.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Add a new user with the given privileges. Errors if the name already
    /// exists or is invalid.
    pub fn add(
        &mut self,
        name: &str,
        password: &str,
        privileges: BTreeSet<Privilege>,
    ) -> Result<(), String> {
        validate_name(name)?;
        if password.is_empty() {
            return Err("password must not be empty".into());
        }
        if self.users.contains_key(name) {
            return Err(format!("user '{name}' already exists"));
        }
        let (salt, hash) = make_hash(password);
        self.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                salt,
                hash,
                privileges,
            },
        );
        self.save().map_err(|e| e.to_string())
    }

    /// Drop a user. `root` cannot be dropped.
    pub fn drop_user(&mut self, name: &str) -> Result<(), String> {
        if name == ROOT_USER {
            return Err("cannot drop the root user".into());
        }
        if self.users.remove(name).is_none() {
            return Err(format!("user '{name}' does not exist"));
        }
        // Invalidate any live sessions for the dropped user.
        self.sessions.retain(|_, s| s.user != name);
        self.save().map_err(|e| e.to_string())
    }

    /// Change a user's password.
    pub fn set_password(&mut self, name: &str, password: &str) -> Result<(), String> {
        if password.is_empty() {
            return Err("password must not be empty".into());
        }
        let user = self
            .users
            .get_mut(name)
            .ok_or_else(|| format!("user '{name}' does not exist"))?;
        let (salt, hash) = make_hash(password);
        user.salt = salt;
        user.hash = hash;
        self.save().map_err(|e| e.to_string())
    }

    /// Grant privileges to a user (`root`'s privileges are immutable).
    pub fn grant(&mut self, name: &str, privs: &BTreeSet<Privilege>) -> Result<(), String> {
        self.modify_privs(name, |p| p.extend(privs.iter().copied()))
    }

    /// Revoke privileges from a user (`root`'s privileges are immutable).
    pub fn revoke(&mut self, name: &str, privs: &BTreeSet<Privilege>) -> Result<(), String> {
        self.modify_privs(name, |p| p.retain(|x| !privs.contains(x)))
    }

    /// Clear all privileges from a user (gStore privilege type "3").
    pub fn clear_privileges(&mut self, name: &str) -> Result<(), String> {
        self.modify_privs(name, |p| p.clear())
    }

    fn modify_privs(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut BTreeSet<Privilege>),
    ) -> Result<(), String> {
        if name == ROOT_USER {
            return Err("cannot change privileges for the root user".into());
        }
        let user = self
            .users
            .get_mut(name)
            .ok_or_else(|| format!("user '{name}' does not exist"))?;
        f(&mut user.privileges);
        self.save().map_err(|e| e.to_string())
    }

    // ---- authentication ---------------------------------------------------

    /// Verify a username/password pair (constant-ish time on the hash compare).
    pub fn verify(&self, name: &str, password: &str) -> bool {
        match self.users.get(name) {
            Some(u) => verify_hash(password, u.salt, u.hash),
            None => false,
        }
    }

    /// Verify credentials and, on success, issue an opaque session token.
    pub fn login(&mut self, name: &str, password: &str) -> Option<String> {
        if !self.verify(name, password) {
            return None;
        }
        let token = new_token();
        self.sessions.insert(
            token.clone(),
            Session {
                user: name.to_string(),
                created: Instant::now(),
            },
        );
        Some(token)
    }

    /// Resolve a session token to its username, pruning it if expired.
    pub fn session_user(&mut self, token: &str) -> Option<String> {
        let expired = match self.sessions.get(token) {
            Some(s) => s.created.elapsed() > self.ttl,
            None => return None,
        };
        if expired {
            self.sessions.remove(token);
            return None;
        }
        // The session must still point at an existing user.
        let name = self.sessions.get(token)?.user.clone();
        if self.users.contains_key(&name) {
            Some(name)
        } else {
            self.sessions.remove(token);
            None
        }
    }

    /// Invalidate a session token (logout). Returns whether it existed.
    pub fn logout(&mut self, token: &str) -> bool {
        self.sessions.remove(token).is_some()
    }

    /// Whether `name` holds privilege `p` (unknown user ⇒ false).
    pub fn has_priv(&self, name: &str, p: Privilege) -> bool {
        self.users.get(name).is_some_and(|u| u.has(p))
    }
}

// --- password hashing & token generation -------------------------------------

/// Derive `(salt, hash)` for a fresh password using a random salt.
fn make_hash(password: &str) -> (u64, u64) {
    let salt = random_u64();
    (salt, hash_password(password, salt))
}

/// Recompute the salted hash and compare without short-circuiting.
fn verify_hash(password: &str, salt: u64, expected: u64) -> bool {
    let got = hash_password(password, salt);
    // Constant-time-ish compare of two u64s.
    got ^ expected == 0
}

/// A salted, iterated hash. Not bcrypt-grade, but std-only and never stores the
/// plaintext; the iteration count slows brute-force guessing.
fn hash_password(password: &str, salt: u64) -> u64 {
    let mut acc = salt ^ 0x9E37_79B9_7F4A_7C15;
    for i in 0..8192u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        acc.hash(&mut h);
        salt.hash(&mut h);
        password.hash(&mut h);
        i.hash(&mut h);
        acc = h.finish();
    }
    acc
}

/// A 128-bit opaque session token rendered as hex.
fn new_token() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

/// A best-effort random `u64` from OS-seeded `RandomState` mixed with the
/// current time and a process-wide counter (std-only; not a CSPRNG, sufficient
/// for session tokens and salts in this scope).
fn random_u64() -> u64 {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos.hash(&mut h);
    CTR.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    h.finish()
}

/// Validate a username: non-empty, ≤ 64 chars, ASCII alphanumeric plus `_`/`-`,
/// no tab/newline (which would corrupt the persisted file).
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("username must be 1..64 characters".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("username may only contain [A-Za-z0-9_-]".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("gstore_users_{}_{}.db", name, random_u64()));
        p
    }

    #[test]
    fn hash_is_salted_and_verifies() {
        let (s1, h1) = make_hash("secret");
        let (s2, h2) = make_hash("secret");
        assert_ne!(s1, s2, "salts differ");
        assert_ne!(h1, h2, "same password hashes to different digests");
        assert!(verify_hash("secret", s1, h1));
        assert!(!verify_hash("wrong", s1, h1));
    }

    #[test]
    fn privileges_parse_names_and_numbers() {
        assert_eq!(Privilege::parse("query"), Some(Privilege::Query));
        assert_eq!(Privilege::parse("4"), Some(Privilege::Update));
        assert_eq!(Privilege::parse("nope"), None);
        let set = Privilege::parse_list("1,update, 7 ,").unwrap();
        assert!(set.contains(&Privilege::Query));
        assert!(set.contains(&Privilege::Update));
        assert!(set.contains(&Privilege::Export));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn root_is_seeded_and_omnipotent() {
        let path = tmp("root");
        let store = UserStore::open(&path, "rootpw").unwrap();
        assert!(store.verify(ROOT_USER, "rootpw"));
        assert!(store.has_priv(ROOT_USER, Privilege::Update));
        assert!(store.has_priv(ROOT_USER, Privilege::Admin));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn add_drop_grant_revoke_and_persist() {
        let path = tmp("crud");
        {
            let mut store = UserStore::open(&path, "rootpw").unwrap();
            store
                .add("alice", "pw", BTreeSet::from([Privilege::Query]))
                .unwrap();
            assert!(store.add("alice", "x", BTreeSet::new()).is_err());
            assert!(store.has_priv("alice", Privilege::Query));
            assert!(!store.has_priv("alice", Privilege::Update));
            store
                .grant("alice", &BTreeSet::from([Privilege::Update]))
                .unwrap();
            assert!(store.has_priv("alice", Privilege::Update));
            store
                .revoke("alice", &BTreeSet::from([Privilege::Query]))
                .unwrap();
            assert!(!store.has_priv("alice", Privilege::Query));
        }
        // Reload from disk: alice survives with her updated privileges.
        {
            let store = UserStore::open(&path, "rootpw").unwrap();
            assert!(store.exists("alice"));
            assert!(store.has_priv("alice", Privilege::Update));
            assert!(!store.has_priv("alice", Privilege::Query));
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn root_cannot_be_dropped_or_edited() {
        let path = tmp("rootguard");
        let mut store = UserStore::open(&path, "rootpw").unwrap();
        assert!(store.drop_user(ROOT_USER).is_err());
        assert!(store
            .grant(ROOT_USER, &BTreeSet::from([Privilege::Query]))
            .is_err());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn login_issues_and_resolves_session() {
        let path = tmp("session");
        let mut store = UserStore::open(&path, "rootpw").unwrap();
        store
            .add("bob", "pw", BTreeSet::from([Privilege::Query]))
            .unwrap();
        assert!(store.login("bob", "bad").is_none());
        let token = store.login("bob", "pw").unwrap();
        assert_eq!(store.session_user(&token).as_deref(), Some("bob"));
        assert!(store.logout(&token));
        assert!(store.session_user(&token).is_none());
        let _ = fs::remove_file(&path);
    }
}
