//! User store and role-based access control (RBAC) for the [`crate::http_api`]
//! HTTP endpoint — the analogue of gStore's `ApiUserOperation` /
//! `system.nt` user database.
//!
//! It is dependency-free (std only): passwords are never stored in plaintext,
//! sessions are opaque random tokens, and the whole store is persisted to a
//! small tab-separated file (`users.db`) so users survive a restart.
//!
//! ## Password hashing
//!
//! Passwords are stored as a **versioned PBKDF2-HMAC-SHA256** hash
//! (RFC 8018, built on the crate's own SHA-256 in [`crate::query::hash`]):
//!
//! ```text
//! pbkdf2-sha256$<iterations>$<salt_hex>$<hash_hex>
//! ```
//!
//! with [`PBKDF2_ITERATIONS`] iterations, a 16-byte salt from the OS random
//! source (`/dev/urandom`; see [`random_bytes`] for the fallback), and a 32-byte
//! derived key compared in constant time. Because the algorithm and its
//! parameters are recorded in the entry and fixed by their specifications, a
//! stored hash stays verifiable across Rust toolchain upgrades.
//!
//! **Legacy entries.** Earlier versions hashed with
//! `std::collections::hash_map::DefaultHasher`, whose algorithm the standard
//! library explicitly does not promise to keep stable across releases. Such
//! entries (`name<TAB>salt16hex<TAB>hash16hex<TAB>privileges`) are still read and
//! verified with the old function, and are transparently re-hashed into the
//! PBKDF2 format on the first successful [`UserStore::login`] /
//! [`UserStore::verify_and_upgrade`] (entries with fewer than the current
//! iteration count are upgraded the same way). A legacy entry may, however,
//! *fail* to verify if the binary was built with a toolchain whose
//! `DefaultHasher` differs from the one that wrote it. To recover, reset the
//! password offline with [`UserStore::reset_password_offline`] (for example
//! `root`'s), or stop the server, delete that user's line from `users.db` and
//! restart — a missing `root` is re-seeded with the configured initial root
//! password.
//!
//! ## Crash safety
//!
//! `users.db` is rewritten atomically (temp file, `fsync`, rename, directory
//! `fsync`), so a crash or a full disk mid-save leaves the previous file
//! intact. A file with a line that does not parse (e.g. torn by an older build
//! that rewrote it in place) is refused rather than loaded partially: skipping
//! the damaged lines would silently drop those users — and could re-seed `root`
//! with the command-line initial password. Fix or delete the reported line.
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
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::query::hash::{
    constant_time_eq, hex_decode, hex_encode, pbkdf2_hmac_sha256, HmacSha256,
};

/// The built-in super-user. Always present, always fully privileged.
pub const ROOT_USER: &str = "root";

/// Scheme tag of the current password-hash format (`pbkdf2-sha256$…`).
pub const PASSWORD_SCHEME: &str = "pbkdf2-sha256";

/// PBKDF2-HMAC-SHA256 iteration count for newly hashed passwords. Stored with
/// each hash, so raising it later keeps old entries verifiable (and they are
/// upgraded on their next successful login).
pub const PBKDF2_ITERATIONS: u32 = 100_000;

// The work factor must never drop below the 10,000-iteration floor.
const _: () = assert!(PBKDF2_ITERATIONS >= 10_000);

/// Salt length in bytes for new password hashes.
const SALT_LEN: usize = 16;

/// Derived-key (stored hash) length in bytes.
const HASH_LEN: usize = 32;

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

/// A stored password verifier (never the password itself).
#[derive(Clone, PartialEq, Eq)]
enum PasswordHash {
    /// The current format: `pbkdf2-sha256$<iterations>$<salt_hex>$<hash_hex>`.
    Pbkdf2Sha256 {
        iterations: u32,
        salt: Vec<u8>,
        hash: [u8; HASH_LEN],
    },
    /// The pre-versioning format: a 64-bit salt and a `DefaultHasher`-iterated
    /// 64-bit digest. Read and verified for migration only, never created.
    Legacy { salt: u64, hash: u64 },
}

impl std::fmt::Debug for PasswordHash {
    // Redacted: salts and digests stay out of logs and `{:?}` dumps.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasswordHash::Pbkdf2Sha256 { iterations, .. } => {
                write!(f, "{PASSWORD_SCHEME}(iterations={iterations}, ..)")
            }
            PasswordHash::Legacy { .. } => write!(f, "legacy(..)"),
        }
    }
}

impl PasswordHash {
    /// Hash `password` under a fresh random salt with the current parameters.
    fn new(password: &str) -> PasswordHash {
        let mut salt = vec![0u8; SALT_LEN];
        random_bytes(&mut salt);
        PasswordHash::pbkdf2(password, salt, PBKDF2_ITERATIONS)
    }

    fn pbkdf2(password: &str, salt: Vec<u8>, iterations: u32) -> PasswordHash {
        let mut hash = [0u8; HASH_LEN];
        pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, &mut hash);
        PasswordHash::Pbkdf2Sha256 {
            iterations,
            salt,
            hash,
        }
    }

    /// Whether `password` matches, comparing digests in constant time.
    fn verify(&self, password: &str) -> bool {
        match self {
            PasswordHash::Pbkdf2Sha256 {
                iterations,
                salt,
                hash,
            } => {
                let mut got = [0u8; HASH_LEN];
                pbkdf2_hmac_sha256(password.as_bytes(), salt, *iterations, &mut got);
                constant_time_eq(&got, hash)
            }
            PasswordHash::Legacy { salt, hash } => {
                let got = legacy_hash_password(password, *salt);
                std::hint::black_box(got ^ *hash) == 0
            }
        }
    }

    /// Whether a verified entry should be re-hashed with current parameters.
    fn needs_upgrade(&self) -> bool {
        match self {
            PasswordHash::Pbkdf2Sha256 {
                iterations, salt, ..
            } => *iterations < PBKDF2_ITERATIONS || salt.len() < SALT_LEN,
            PasswordHash::Legacy { .. } => true,
        }
    }

    /// The persisted credential column(s): `pbkdf2-sha256$<iter>$<salt>$<hash>`
    /// for a current entry; for a not-yet-migrated legacy entry, its original
    /// two tab-separated hex columns so the file stays readable as before.
    fn encode(&self) -> String {
        match self {
            PasswordHash::Pbkdf2Sha256 {
                iterations,
                salt,
                hash,
            } => format!(
                "{PASSWORD_SCHEME}${iterations}${}${}",
                hex_encode(salt),
                hex_encode(hash)
            ),
            PasswordHash::Legacy { salt, hash } => format!("{salt:016x}\t{hash:016x}"),
        }
    }

    /// Parse a `pbkdf2-sha256$<iter>$<salt_hex>$<hash_hex>` string.
    fn parse_pbkdf2(s: &str) -> Option<PasswordHash> {
        let mut parts = s.split('$');
        if parts.next()? != PASSWORD_SCHEME {
            return None;
        }
        let iterations: u32 = parts.next()?.parse().ok().filter(|&n| n > 0)?;
        let salt = hex_decode(parts.next()?)?;
        let hash: [u8; HASH_LEN] = hex_decode(parts.next()?)?.try_into().ok()?;
        if parts.next().is_some() || salt.is_empty() {
            return None;
        }
        Some(PasswordHash::Pbkdf2Sha256 {
            iterations,
            salt,
            hash,
        })
    }
}

/// Hash `password` in the current versioned format
/// (`pbkdf2-sha256$<iterations>$<salt_hex>$<hash_hex>`, random salt).
pub fn hash_password(password: &str) -> String {
    PasswordHash::new(password).encode()
}

/// Verify `password` against a hash produced by [`hash_password`]. Malformed
/// or unknown-scheme input never verifies.
pub fn verify_password(stored: &str, password: &str) -> bool {
    PasswordHash::parse_pbkdf2(stored).is_some_and(|h| h.verify(password))
}

/// One stored user: name, password verifier, and granted privileges.
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    password: PasswordHash,
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
    /// Fast path for repeated verification of the same credentials (HTTP Basic
    /// auth re-sends them on every request): user → HMAC-SHA256 of the last
    /// password that verified, under the per-process random `verified_key`. A
    /// hit skips the deliberately slow PBKDF2; a miss falls back to it.
    /// Invalidated whenever a user's password changes or the user is dropped.
    /// Memory only — never persisted.
    verified: Mutex<HashMap<String, [u8; 32]>>,
    verified_key: HmacSha256,
}

impl UserStore {
    /// Load the user store from `path`, creating an empty one if the file is
    /// absent, then ensure the built-in `root` user exists (seeded with the
    /// given password the first time). The file is (re)written so a freshly
    /// seeded `root` is persisted.
    pub fn open(path: impl AsRef<Path>, root_password: &str) -> io::Result<UserStore> {
        let mut store = UserStore::load(path)?;
        if store.seed_root(root_password) {
            store.save()?;
        }
        Ok(store)
    }

    /// Read the user file at `path` (empty store if absent) without seeding.
    fn load(path: impl AsRef<Path>) -> io::Result<UserStore> {
        let mut key = [0u8; 32];
        random_bytes(&mut key);
        let mut store = UserStore {
            path: path.as_ref().to_path_buf(),
            users: HashMap::new(),
            sessions: HashMap::new(),
            ttl: Duration::from_secs(24 * 3600),
            verified: Mutex::new(HashMap::new()),
            verified_key: HmacSha256::new(&key),
        };
        if store.path.is_file() {
            store.read_file()?;
        }
        Ok(store)
    }

    /// Create the built-in `root` with `password` if it is missing; returns
    /// whether it was created.
    fn seed_root(&mut self, password: &str) -> bool {
        if self.users.contains_key(ROOT_USER) {
            return false;
        }
        self.users.insert(
            ROOT_USER.to_string(),
            User {
                name: ROOT_USER.to_string(),
                password: PasswordHash::new(password),
                privileges: BTreeSet::from([Privilege::Admin]),
            },
        );
        true
    }

    /// Offline password reset: set `name`'s password in the user file at `path`
    /// without knowing the old one (the recovery path for a forgotten password
    /// or a legacy entry that no longer verifies, see the module docs). A
    /// missing `root` is created with `new_password`. Run it while no server is
    /// using `path` (the server would overwrite the file from its own copy).
    pub fn reset_password_offline(
        path: impl AsRef<Path>,
        name: &str,
        new_password: &str,
    ) -> io::Result<()> {
        let mut store = UserStore::load(path)?;
        if name == ROOT_USER && store.seed_root(new_password) {
            return store.save();
        }
        store
            .set_password(name, new_password)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    fn read_file(&mut self) -> io::Result<()> {
        let text = fs::read_to_string(&self.path)?;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let malformed = || {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "user store '{}': line {} is malformed (damaged file?); refusing to \
                         load it, since skipping it would silently drop that user — fix or \
                         delete the line",
                        self.path.display(),
                        n + 1
                    ),
                )
            };
            let mut cols = line.split('\t');
            let (Some(name), Some(cred)) = (cols.next(), cols.next()) else {
                return Err(malformed());
            };
            // Current format: `name<TAB>pbkdf2-sha256$…<TAB>privileges`.
            // Legacy format:  `name<TAB>salt16hex<TAB>hash16hex<TAB>privileges`.
            let password = if cred.starts_with(PASSWORD_SCHEME) {
                PasswordHash::parse_pbkdf2(cred).ok_or_else(malformed)?
            } else {
                let Some(hash) = cols.next() else {
                    return Err(malformed());
                };
                let (Ok(salt), Ok(hash)) =
                    (u64::from_str_radix(cred, 16), u64::from_str_radix(hash, 16))
                else {
                    return Err(malformed());
                };
                PasswordHash::Legacy { salt, hash }
            };
            let privileges = cols
                .next()
                .and_then(Privilege::parse_list)
                .unwrap_or_default();
            self.users.insert(
                name.to_string(),
                User {
                    name: name.to_string(),
                    password,
                    privileges,
                },
            );
        }
        Ok(())
    }

    /// Persist the user table to disk (sorted by name for stable output),
    /// atomically: a crash or I/O error mid-save leaves the previous file.
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut names: Vec<&String> = self.users.keys().collect();
        names.sort();
        let mut out = String::new();
        for name in names {
            let u = &self.users[name];
            // A legacy entry not yet migrated (its user has not logged in since
            // the upgrade) keeps its two-column form, so it stays readable.
            let cred = u.password.encode();
            out.push_str(&format!("{}\t{cred}\t{}\n", u.name, u.privilege_str()));
        }
        crate::db::fsutil::atomic_write(&self.path, out.as_bytes()).map_err(|e| match e {
            crate::error::GStoreError::Io(io) => io,
            other => io::Error::other(other.to_string()),
        })
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
        self.forget_verified(name);
        self.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                password: PasswordHash::new(password),
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
        self.forget_verified(name);
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
        user.password = PasswordHash::new(password);
        self.forget_verified(name);
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

    /// Verify a username/password pair. Digests are compared in constant time,
    /// and an unknown user costs the same PBKDF2 work as a wrong password (no
    /// user-enumeration timing signal). Does not modify the store — use
    /// [`verify_and_upgrade`](Self::verify_and_upgrade) to also migrate a
    /// legacy entry.
    pub fn verify(&self, name: &str, password: &str) -> bool {
        let Some(u) = self.users.get(name) else {
            // Burn the same work as a real check against a fixed dummy entry.
            let dummy = PasswordHash::Pbkdf2Sha256 {
                iterations: PBKDF2_ITERATIONS,
                salt: vec![0u8; SALT_LEN],
                hash: [0u8; HASH_LEN],
            };
            let _ = std::hint::black_box(dummy.verify(password));
            return false;
        };
        let tag = self.verified_key.mac(&[name.as_bytes(), &[0], password.as_bytes()]);
        let cache = self.verified.lock().unwrap_or_else(|e| e.into_inner());
        if cache.get(name).is_some_and(|t| constant_time_eq(t, &tag)) {
            return true;
        }
        drop(cache);
        let ok = u.password.verify(password);
        if ok {
            self.verified
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_string(), tag);
        }
        ok
    }

    /// [`verify`](Self::verify), and on success transparently re-hash an entry
    /// stored in the legacy format (or with fewer than [`PBKDF2_ITERATIONS`]
    /// iterations) into the current format, persisting the store. A failure to
    /// persist the upgrade does not fail the verification.
    pub fn verify_and_upgrade(&mut self, name: &str, password: &str) -> bool {
        if !self.verify(name, password) {
            return false;
        }
        if let Some(u) = self.users.get_mut(name) {
            if u.password.needs_upgrade() {
                u.password = PasswordHash::new(password);
                let _ = self.save();
            }
        }
        true
    }

    /// Drop `name`'s cached verified-credential tag.
    fn forget_verified(&self, name: &str) {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// Verify credentials and, on success, issue an opaque session token. A
    /// legacy-format password entry is upgraded to the current format here.
    pub fn login(&mut self, name: &str, password: &str) -> Option<String> {
        if !self.verify_and_upgrade(name, password) {
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
        let expired = self.sessions.get(token)?.created.elapsed() > self.ttl;
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

/// The **legacy** (pre-versioning) password hash, kept only to verify and
/// migrate old `users.db` entries. Its output depends on `DefaultHasher`, whose
/// algorithm `std` does not guarantee across Rust releases, so an entry written
/// by a differently-built binary may not verify (see the module docs for the
/// reset path). Never used for new hashes.
fn legacy_hash_password(password: &str, salt: u64) -> u64 {
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

/// A 128-bit opaque session token rendered as hex (32 characters).
fn new_token() -> String {
    let mut t = [0u8; 16];
    random_bytes(&mut t);
    hex_encode(&t)
}

/// Fill `buf` from the operating system's CSPRNG (`/dev/urandom`). Returns
/// `false` when it is unavailable (non-Unix target, restricted sandbox).
fn os_random_bytes(buf: &mut [u8]) -> bool {
    use std::io::Read;
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok()
}

/// Fill `buf` with random bytes for salts, session tokens and keys.
///
/// Reads `/dev/urandom`. **Fallback** (only when that fails): SHA-256 over
/// `std`'s per-process `RandomState` SipHash keys — which `std` seeds from the
/// OS random source on every supported platform — mixed with the wall clock,
/// the process id and a process-wide counter. That is unpredictable in
/// practice but is not a vetted CSPRNG; deployments should make sure
/// `/dev/urandom` is readable.
pub fn random_bytes(buf: &mut [u8]) {
    if os_random_bytes(buf) {
        return;
    }
    static CTR: AtomicU64 = AtomicU64::new(0);
    for (i, chunk) in buf.chunks_mut(32).enumerate() {
        let mut ctx = crate::query::hash::Sha256::new();
        for _ in 0..4 {
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let h = std::collections::hash_map::RandomState::new().hash_one(n);
            ctx.update(&h.to_le_bytes());
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        ctx.update(&nanos.to_le_bytes());
        ctx.update(&std::process::id().to_le_bytes());
        ctx.update(&(i as u64).to_le_bytes());
        let block = ctx.finalize();
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
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
        p.push(format!("gstore_users_{}_{}.db", name, new_token()));
        p
    }

    #[test]
    fn hash_is_salted_and_verifies() {
        let h1 = hash_password("secret");
        let h2 = hash_password("secret");
        assert_ne!(h1, h2, "same password hashes to different strings (salted)");
        assert!(verify_password(&h1, "secret"));
        assert!(verify_password(&h2, "secret"));
        assert!(!verify_password(&h1, "wrong"));
        assert!(!verify_password(&h1, ""));
    }

    #[test]
    fn hash_format_is_versioned_pbkdf2_sha256() {
        let h = hash_password("pw");
        let parts: Vec<&str> = h.split('$').collect();
        assert_eq!(parts.len(), 4, "{h}");
        assert_eq!(parts[0], "pbkdf2-sha256");
        assert_eq!(parts[1], PBKDF2_ITERATIONS.to_string());
        assert_eq!(parts[2].len(), SALT_LEN * 2, "16-byte salt, hex");
        assert_eq!(parts[3].len(), HASH_LEN * 2, "32-byte key, hex");
        assert!(parts[2..].iter().all(|p| p.bytes().all(|b| b.is_ascii_hexdigit())));
    }

    /// The stored form is plain PBKDF2-HMAC-SHA256, so it is pinned by the
    /// published test vector — independent of the Rust toolchain.
    #[test]
    fn stored_hash_is_stable_against_a_published_vector() {
        let stored = format!(
            "pbkdf2-sha256$4096${}${}",
            hex_encode(b"salt"),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
        assert!(verify_password(&stored, "password"));
        assert!(!verify_password(&stored, "Password"));
    }

    #[test]
    fn malformed_hashes_never_verify() {
        for bad in [
            "",
            "pbkdf2-sha256",
            "pbkdf2-sha256$0$73616c74$00",
            "pbkdf2-sha256$x$73616c74$c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
            "pbkdf2-sha256$4096$$c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
            "pbkdf2-sha256$4096$73616c74$c5e4",
            "pbkdf2-sha256$4096$zz$c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
            "md5$4096$73616c74$c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
            "pbkdf2-sha256$4096$73616c74$c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a$x",
        ] {
            assert!(!verify_password(bad, "password"), "{bad:?}");
        }
    }

    #[test]
    fn salts_and_tokens_come_from_the_os_rng() {
        let mut a = [0u8; 16];
        #[cfg(target_os = "linux")]
        assert!(os_random_bytes(&mut a), "/dev/urandom must be readable on Linux");
        random_bytes(&mut a);
        let mut b = [0u8; 16];
        random_bytes(&mut b);
        assert_ne!(a, b);
        let t = new_token();
        assert_eq!(t.len(), 32);
        assert_ne!(t, new_token());
    }

    #[test]
    fn debug_output_redacts_hash_material() {
        let path = tmp("debug");
        let store = UserStore::open(&path, "rootpw").unwrap();
        let dbg = format!("{:?}", store.get(ROOT_USER).unwrap());
        let file = fs::read_to_string(&path).unwrap();
        let stored_hash = file.split('\t').nth(1).unwrap().rsplit('$').next().unwrap();
        assert!(!dbg.contains(stored_hash), "{dbg}");
        assert!(dbg.contains("pbkdf2-sha256(iterations="), "{dbg}");
        let _ = fs::remove_file(&path);
    }

    /// A pre-versioning `users.db` (DefaultHasher digests) still loads; the
    /// entry verifies with the legacy function, is re-hashed into the PBKDF2
    /// format on login, and survives a reload.
    #[test]
    fn legacy_entries_verify_and_migrate_on_login() {
        let path = tmp("legacy");
        let salt = 0x0123_4567_89ab_cdefu64;
        let root_salt = 0x1111_2222_3333_4444u64;
        let legacy = format!(
            "alice\t{salt:016x}\t{:016x}\tquery,update\nroot\t{root_salt:016x}\t{:016x}\tadmin\n",
            legacy_hash_password("alicepw", salt),
            legacy_hash_password("rootpw", root_salt),
        );
        fs::write(&path, &legacy).unwrap();

        let mut store = UserStore::open(&path, "ignored-root-seed").unwrap();
        assert!(store.has_priv("alice", Privilege::Update));
        assert!(store.verify("alice", "alicepw"), "legacy hash verifies");
        assert!(!store.verify("alice", "wrong"));
        // `verify` alone does not rewrite the file: legacy lines are preserved.
        store.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("alice\t{salt:016x}\t")), "{text}");

        // Login upgrades alice (only alice) to the versioned format.
        assert!(store.login("alice", "alicepw").is_some());
        let text = fs::read_to_string(&path).unwrap();
        let alice = text.lines().find(|l| l.starts_with("alice\t")).unwrap();
        assert!(alice.starts_with("alice\tpbkdf2-sha256$"), "{alice}");
        assert!(alice.ends_with("\tquery,update"), "{alice}");
        let root = text.lines().find(|l| l.starts_with("root\t")).unwrap();
        assert!(root.starts_with(&format!("root\t{root_salt:016x}\t")), "{root}");

        // After a reload the migrated entry still verifies; root still legacy.
        let mut store = UserStore::open(&path, "ignored").unwrap();
        assert!(store.verify("alice", "alicepw"));
        assert!(!store.verify("alice", "wrong"));
        assert!(store.verify_and_upgrade(ROOT_USER, "rootpw"));
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.lines().any(|l| !l.contains("pbkdf2-sha256$")), "{text}");
        let _ = fs::remove_file(&path);
    }

    /// Recovery for an entry that no longer verifies (e.g. a legacy hash after a
    /// toolchain change): reset it offline, without the old password.
    #[test]
    fn root_password_can_be_reset_offline() {
        let path = tmp("reset");
        // A legacy root line whose digest cannot verify any password we try.
        fs::write(&path, "root\t00000000000000ff\tdeadbeefdeadbeef\tadmin\n").unwrap();
        let store = UserStore::open(&path, "seed").unwrap();
        assert!(!store.verify(ROOT_USER, "seed"));
        drop(store);

        UserStore::reset_password_offline(&path, ROOT_USER, "new-root-pw").unwrap();
        let store = UserStore::open(&path, "seed").unwrap();
        assert!(store.verify(ROOT_USER, "new-root-pw"));
        assert!(store.has_priv(ROOT_USER, Privilege::Admin));
        // Missing file: root is created with the new password.
        let fresh = tmp("reset_fresh");
        UserStore::reset_password_offline(&fresh, ROOT_USER, "pw2").unwrap();
        assert!(UserStore::open(&fresh, "other").unwrap().verify(ROOT_USER, "pw2"));
        // Unknown users are an error, not silently created.
        assert!(UserStore::reset_password_offline(&fresh, "nobody", "x").is_err());
        // Resetting a non-root user never seeds a root with that password.
        let no_root = tmp("reset_no_root");
        fs::write(&no_root, format!("erin\t{}\tquery\n", hash_password("old"))).unwrap();
        UserStore::reset_password_offline(&no_root, "erin", "new").unwrap();
        let text = fs::read_to_string(&no_root).unwrap();
        assert!(!text.contains("root\t"), "{text}");
        let store = UserStore::open(&no_root, "seeded-root").unwrap();
        assert!(store.verify("erin", "new") && !store.verify("erin", "old"));
        let _ = fs::remove_file(&no_root);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&fresh);
    }

    #[test]
    fn verified_credentials_cache_is_invalidated_on_password_change() {
        let path = tmp("cache");
        let mut store = UserStore::open(&path, "rootpw").unwrap();
        store.add("carol", "old", BTreeSet::new()).unwrap();
        assert!(store.verify("carol", "old"));
        assert!(store.verify("carol", "old"), "cached fast path");
        store.set_password("carol", "new").unwrap();
        assert!(!store.verify("carol", "old"), "stale cache entry must not verify");
        assert!(store.verify("carol", "new"));
        store.drop_user("carol").unwrap();
        assert!(!store.verify("carol", "new"));
        // Re-creating the user with a different password does not resurrect it.
        store.add("carol", "third", BTreeSet::new()).unwrap();
        assert!(!store.verify("carol", "new"));
        assert!(store.verify("carol", "third"));
        assert!(!store.verify("nobody", "x"));
        let _ = fs::remove_file(&path);
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

    /// A save replaces `users.db` (temp file + rename) instead of truncating and
    /// rewriting it in place, so a crash or a full disk mid-save can never
    /// leave a torn file. Observable through a hard link to the old file: an
    /// in-place rewrite would change what the link sees too.
    #[cfg(unix)]
    #[test]
    fn save_replaces_the_file_instead_of_rewriting_it_in_place() {
        let path = tmp("atomic");
        let mut store = UserStore::open(&path, "rootpw").unwrap();
        let before = fs::read(&path).unwrap();
        let link = PathBuf::from(format!("{}.link", path.display()));
        let _ = fs::remove_file(&link);
        fs::hard_link(&path, &link).unwrap();
        store
            .add("carol", "pw", BTreeSet::from([Privilege::Query]))
            .unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("carol"));
        assert_eq!(fs::read(&link).unwrap(), before, "the old inode is untouched");
        assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&link);
    }

    /// A torn `users.db` (cut mid-line, as an interrupted in-place rewrite
    /// leaves it) is refused instead of loaded partially — loading it would
    /// silently drop the users after the cut and, with `root`'s line gone,
    /// re-seed `root` with the command-line password.
    #[test]
    fn a_torn_user_file_is_refused_rather_than_partially_loaded() {
        let path = tmp("torn");
        let mut store = UserStore::open(&path, "operator-root-pw").unwrap();
        for u in ["alice", "bob"] {
            store.add(u, "pw", BTreeSet::from([Privilege::Query])).unwrap();
        }
        let full = fs::read_to_string(&path).unwrap();
        // Cut in the middle of the last line (`root`, which sorts last).
        let cut = full.trim_end().len() - 10;
        fs::write(&path, &full[..cut]).unwrap();
        let err = UserStore::open(&path, "123456").err().expect("must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("line 3"), "{err}");
        // Nothing was rewritten: the damaged file is left for the operator.
        assert_eq!(fs::read_to_string(&path).unwrap(), &full[..cut]);
        let _ = fs::remove_file(&path);
    }
}
