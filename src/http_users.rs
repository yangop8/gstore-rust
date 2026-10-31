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
//! (RFC 8018, built on the crate's own SHA-256 in [`crate::query::hash`]) of
//! the password's **lowercase hex MD5 digest**:
//!
//! ```text
//! pbkdf2-sha256-md5$<iterations>$<salt_hex>$<hash_hex>    key = PBKDF2(md5hex(password))
//! ```
//!
//! with [`PBKDF2_ITERATIONS`] iterations, a 16-byte salt from the OS random
//! source (`/dev/urandom`; see [`random_bytes`] for the fallback), and a 32-byte
//! derived key compared in constant time. Because the algorithm and its
//! parameters are recorded in the entry and fixed by their specifications, a
//! stored hash stays verifiable across Rust toolchain upgrades.
//!
//! Feeding `md5hex(password)` (rather than the password) to PBKDF2 is what lets
//! ONE stored slow hash serve both kinds of upstream gStore clients
//! ([`PasswordForm`]): a client sending the plaintext password (the server
//! computes its MD5, then PBKDF2), and an `encryption=1` client, which sends
//! the MD5 hex digest of the password instead of the password (PBKDF2 of the
//! received digest). MD5 is only a fixed, public pre-transform here; the slow,
//! salted PBKDF2 is what protects the stored entry.
//!
//! **Older entries.** Two earlier formats are still read and verified:
//!
//! * `pbkdf2-sha256$<iterations>$<salt_hex>$<hash_hex>`
//!   ([`PASSWORD_SCHEME_RAW`], the previous version): PBKDF2 of the raw
//!   password;
//! * the pre-versioning `name<TAB>salt16hex<TAB>hash16hex<TAB>privileges`
//!   entries, hashed with `std::collections::hash_map::DefaultHasher`, whose
//!   algorithm the standard library explicitly does not promise to keep stable
//!   across releases.
//!
//! Both are transparently re-hashed into the current format on the next
//! successful **plaintext** verification ([`UserStore::login`],
//! [`UserStore::verify_and_upgrade`], or [`UserStore::authenticate`] with
//! [`PasswordForm::Plain`]); entries with fewer than the current iteration
//! count are upgraded the same way. **Until that migration has happened, an
//! older entry cannot be verified for an `encryption=1` client** (it stores
//! nothing derivable from the MD5 digest): such a check yields
//! [`CredentialCheck::NeedsPlaintextLogin`], which the `/api` layer answers
//! with `1001` and a message asking the user to log in once without
//! encryption.
//!
//! A legacy entry may *fail* to verify if the binary was built with a toolchain
//! whose `DefaultHasher` differs from the one that wrote it. To recover, reset
//! the password offline with [`UserStore::reset_password_offline`] (for example
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
    constant_time_eq, hex_decode, hex_encode, md5_hex, pbkdf2_hmac_sha256, HmacSha256,
};

/// The built-in super-user. Always present, always fully privileged.
pub const ROOT_USER: &str = "root";

/// Scheme tag of the current password-hash format (`pbkdf2-sha256-md5$…`:
/// PBKDF2-HMAC-SHA256 of the password's lowercase MD5 hex digest; see the
/// module docs).
pub const PASSWORD_SCHEME: &str = "pbkdf2-sha256-md5";

/// Scheme tag of the previous format (`pbkdf2-sha256$…`: PBKDF2-HMAC-SHA256 of
/// the raw password). Still read and verified; re-hashed into
/// [`PASSWORD_SCHEME`] on the next plaintext login.
pub const PASSWORD_SCHEME_RAW: &str = "pbkdf2-sha256";

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

/// How a client presented its password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PasswordForm {
    /// The plaintext password.
    Plain,
    /// The lowercase hex MD5 digest of the password (upstream gStore's
    /// `encryption=1`). Upper-case hex digits are accepted too.
    Md5Hex,
}

impl PasswordForm {
    fn tag(self) -> u8 {
        match self {
            PasswordForm::Plain => 0,
            PasswordForm::Md5Hex => 1,
        }
    }
}

/// The outcome of checking a client's credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialCheck {
    /// The credentials are correct.
    Valid,
    /// Unknown user or wrong password.
    Invalid,
    /// The user's stored entry predates the MD5-compatible format, so a
    /// [`PasswordForm::Md5Hex`] client cannot be verified against it yet: the
    /// user must log in once with the plaintext password, which migrates it.
    NeedsPlaintextLogin,
}

/// A credential check set up by [`UserStore::begin_check`], to be [`run`]
/// without the store borrowed (it does the slow PBKDF2 work) and then handed
/// to [`UserStore::finish_check`].
///
/// [`run`]: PendingCheck::run
pub struct PendingCheck {
    name: String,
    form: PasswordForm,
    /// The secret (an MD5 digest normalized to lowercase).
    secret: String,
    /// The verified-credentials cache tag of `(name, form, secret)`.
    tag: [u8; 32],
    /// A copy of the user's stored verifier; `None` for an unknown user.
    stored: Option<PasswordHash>,
}

impl std::fmt::Debug for PendingCheck {
    // Redacted: the secret stays out of logs and `{:?}` dumps.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingCheck")
            .field("name", &self.name)
            .field("form", &self.form)
            .finish_non_exhaustive()
    }
}

/// A [`PendingCheck`] that has run: its outcome, and for a valid secret whose
/// stored entry needs migrating, the re-hashed entry. Hand it to
/// [`UserStore::finish_check`].
pub struct CheckedCredentials {
    pending: PendingCheck,
    outcome: CredentialCheck,
    rehashed: Option<PasswordHash>,
}

impl CheckedCredentials {
    /// The outcome of the verification itself (before
    /// [`UserStore::finish_check`] re-checks it against the current store).
    pub fn outcome(&self) -> CredentialCheck {
        self.outcome
    }
}

impl PendingCheck {
    /// Verify the secret (slow: PBKDF2) and, when it is valid and the stored
    /// entry is not in the current format, compute the re-hashed entry.
    pub fn run(self) -> CheckedCredentials {
        let mut checked = self.run_verify_only();
        let p = &checked.pending;
        if checked.outcome == CredentialCheck::Valid
            && p.stored.as_ref().is_some_and(PasswordHash::needs_upgrade)
        {
            checked.rehashed = match p.form {
                PasswordForm::Plain => Some(PasswordHash::new(&p.secret)),
                // Valid ⇒ the entry is MD5-based and the secret a normalized
                // digest (an older entry cannot verify a digest at all).
                PasswordForm::Md5Hex => Some(PasswordHash::from_md5_hex(&p.secret)),
            };
        }
        checked
    }

    /// Verify only (no re-hash). An unknown user costs the same PBKDF2 work
    /// as a wrong password (no user-enumeration timing signal).
    fn run_verify_only(self) -> CheckedCredentials {
        let outcome = match &self.stored {
            None => {
                let dummy = PasswordHash::Pbkdf2Sha256 {
                    input: Prehash::Md5Hex,
                    iterations: PBKDF2_ITERATIONS,
                    salt: vec![0u8; SALT_LEN],
                    hash: [0u8; HASH_LEN],
                };
                let _ = std::hint::black_box(dummy.verify_as(&self.secret, self.form));
                CredentialCheck::Invalid
            }
            Some(stored) => match stored.verify_as(&self.secret, self.form) {
                Some(true) => CredentialCheck::Valid,
                Some(false) => CredentialCheck::Invalid,
                None => CredentialCheck::NeedsPlaintextLogin,
            },
        };
        CheckedCredentials {
            pending: self,
            outcome,
            rehashed: None,
        }
    }
}

/// What a stored PBKDF2 entry was computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prehash {
    /// `pbkdf2-sha256`: the raw password (previous format).
    Raw,
    /// `pbkdf2-sha256-md5`: the password's lowercase MD5 hex digest (current).
    Md5Hex,
}

impl Prehash {
    fn scheme(self) -> &'static str {
        match self {
            Prehash::Raw => PASSWORD_SCHEME_RAW,
            Prehash::Md5Hex => PASSWORD_SCHEME,
        }
    }
}

/// A stored password verifier (never the password itself).
#[derive(Clone, PartialEq, Eq)]
enum PasswordHash {
    /// `<scheme>$<iterations>$<salt_hex>$<hash_hex>`; the scheme records what
    /// was fed to PBKDF2 (see [`Prehash`]).
    Pbkdf2Sha256 {
        input: Prehash,
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
            PasswordHash::Pbkdf2Sha256 {
                input, iterations, ..
            } => write!(f, "{}(iterations={iterations}, ..)", input.scheme()),
            PasswordHash::Legacy { .. } => write!(f, "legacy(..)"),
        }
    }
}

/// Normalize a client-supplied MD5 hex digest: exactly 32 hex digits, which
/// are lowercased. `None` for anything else.
fn normalize_md5_hex(digest: &str) -> Option<String> {
    let d = digest.trim();
    (d.len() == 32 && d.bytes().all(|b| b.is_ascii_hexdigit())).then(|| d.to_ascii_lowercase())
}

impl PasswordHash {
    /// Hash the plaintext `password` in the current format under a fresh salt.
    fn new(password: &str) -> PasswordHash {
        PasswordHash::from_md5_hex(&md5_hex(password))
    }

    /// Hash a normalized (lowercase) MD5 hex digest in the current format
    /// under a fresh salt.
    fn from_md5_hex(digest: &str) -> PasswordHash {
        let mut salt = vec![0u8; SALT_LEN];
        random_bytes(&mut salt);
        PasswordHash::pbkdf2(Prehash::Md5Hex, digest, salt, PBKDF2_ITERATIONS)
    }

    fn pbkdf2(input: Prehash, secret: &str, salt: Vec<u8>, iterations: u32) -> PasswordHash {
        let mut hash = [0u8; HASH_LEN];
        pbkdf2_hmac_sha256(secret.as_bytes(), &salt, iterations, &mut hash);
        PasswordHash::Pbkdf2Sha256 {
            input,
            iterations,
            salt,
            hash,
        }
    }

    /// Whether `secret`, presented in `form`, matches (digests compared in
    /// constant time). `None` when this entry cannot be checked in that form:
    /// an older entry against an MD5 digest.
    fn verify_as(&self, secret: &str, form: PasswordForm) -> Option<bool> {
        match self {
            PasswordHash::Pbkdf2Sha256 {
                input,
                iterations,
                salt,
                hash,
            } => {
                let fed = match (input, form) {
                    (Prehash::Raw, PasswordForm::Plain) => secret.to_string(),
                    (Prehash::Raw, PasswordForm::Md5Hex) => return None,
                    (Prehash::Md5Hex, PasswordForm::Plain) => md5_hex(secret),
                    (Prehash::Md5Hex, PasswordForm::Md5Hex) => match normalize_md5_hex(secret) {
                        Some(d) => d,
                        None => return Some(false),
                    },
                };
                let mut got = [0u8; HASH_LEN];
                pbkdf2_hmac_sha256(fed.as_bytes(), salt, *iterations, &mut got);
                Some(constant_time_eq(&got, hash))
            }
            PasswordHash::Legacy { salt, hash } => match form {
                PasswordForm::Plain => {
                    let got = legacy_hash_password(secret, *salt);
                    Some(std::hint::black_box(got ^ *hash) == 0)
                }
                PasswordForm::Md5Hex => None,
            },
        }
    }

    /// Whether the plaintext `password` matches.
    fn verify(&self, password: &str) -> bool {
        self.verify_as(password, PasswordForm::Plain) == Some(true)
    }

    /// Whether a verified entry should be re-hashed into the current format
    /// with current parameters.
    fn needs_upgrade(&self) -> bool {
        match self {
            PasswordHash::Pbkdf2Sha256 {
                input,
                iterations,
                salt,
                ..
            } => {
                *input != Prehash::Md5Hex
                    || *iterations < PBKDF2_ITERATIONS
                    || salt.len() < SALT_LEN
            }
            PasswordHash::Legacy { .. } => true,
        }
    }

    /// The persisted credential column(s): `<scheme>$<iter>$<salt>$<hash>` for
    /// a PBKDF2 entry; for a not-yet-migrated legacy entry, its original two
    /// tab-separated hex columns so the file stays readable as before.
    fn encode(&self) -> String {
        match self {
            PasswordHash::Pbkdf2Sha256 {
                input,
                iterations,
                salt,
                hash,
            } => format!(
                "{}${iterations}${}${}",
                input.scheme(),
                hex_encode(salt),
                hex_encode(hash)
            ),
            PasswordHash::Legacy { salt, hash } => format!("{salt:016x}\t{hash:016x}"),
        }
    }

    /// Parse a `<scheme>$<iter>$<salt_hex>$<hash_hex>` string (either PBKDF2
    /// scheme).
    fn parse_pbkdf2(s: &str) -> Option<PasswordHash> {
        let mut parts = s.split('$');
        let input = match parts.next()? {
            PASSWORD_SCHEME => Prehash::Md5Hex,
            PASSWORD_SCHEME_RAW => Prehash::Raw,
            _ => return None,
        };
        let iterations: u32 = parts.next()?.parse().ok().filter(|&n| n > 0)?;
        let salt = hex_decode(parts.next()?)?;
        let hash: [u8; HASH_LEN] = hex_decode(parts.next()?)?.try_into().ok()?;
        if parts.next().is_some() || salt.is_empty() {
            return None;
        }
        Some(PasswordHash::Pbkdf2Sha256 {
            input,
            iterations,
            salt,
            hash,
        })
    }
}

/// Hash `password` in the current versioned format
/// (`pbkdf2-sha256-md5$<iterations>$<salt_hex>$<hash_hex>`, random salt).
pub fn hash_password(password: &str) -> String {
    PasswordHash::new(password).encode()
}

/// Verify the plaintext `password` against a hash produced by
/// [`hash_password`] (or in the previous `pbkdf2-sha256$…` format). Malformed
/// or unknown-scheme input never verifies.
pub fn verify_password(stored: &str, password: &str) -> bool {
    PasswordHash::parse_pbkdf2(stored).is_some_and(|h| h.verify(password))
}

/// Verify a client-supplied MD5 hex digest of the password (upstream gStore's
/// `encryption=1`) against a stored hash: `Some(matches)` for an entry in the
/// current format, `None` for one in the previous `pbkdf2-sha256$…` format
/// (which cannot be checked from a digest). Malformed input is `Some(false)`.
pub fn verify_password_md5(stored: &str, md5_hex_digest: &str) -> Option<bool> {
    match PasswordHash::parse_pbkdf2(stored) {
        Some(h) => h.verify_as(md5_hex_digest, PasswordForm::Md5Hex),
        None => Some(false),
    }
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
    /// auth and the `/api` protocol re-send them on every request): (user,
    /// [`PasswordForm`] tag) → HMAC-SHA256 of the last secret that verified in
    /// that form, under the per-process random `verified_key`. A hit skips the
    /// deliberately slow PBKDF2; a miss falls back to it. Invalidated whenever
    /// a user's password changes or the user is dropped. Memory only — never
    /// persisted.
    verified: Mutex<HashMap<(String, u8), [u8; 32]>>,
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
            // PBKDF2 formats: `name<TAB>pbkdf2-sha256[-md5]$…<TAB>privileges`.
            // Legacy format:  `name<TAB>salt16hex<TAB>hash16hex<TAB>privileges`.
            let password = if cred.starts_with(PASSWORD_SCHEME_RAW) {
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
    /// [`verify_and_upgrade`](Self::verify_and_upgrade) to also migrate an
    /// older entry.
    pub fn verify(&self, name: &str, password: &str) -> bool {
        self.check_credentials(name, password, PasswordForm::Plain) == CredentialCheck::Valid
    }

    /// Check credentials presented in `form` (the plaintext password, or its
    /// MD5 hex digest for upstream's `encryption=1`) without modifying the
    /// store. [`CredentialCheck::NeedsPlaintextLogin`] means `form` is
    /// [`PasswordForm::Md5Hex`] but the user's entry is in an older format
    /// that only a plaintext login can verify (and migrate).
    pub fn check_credentials(&self, name: &str, secret: &str, form: PasswordForm) -> CredentialCheck {
        let pending = match self.prepare(name, secret, form, false) {
            Ok(outcome) => return outcome,
            Err(pending) => pending,
        };
        let checked = pending.run_verify_only();
        if checked.outcome == CredentialCheck::Valid {
            self.remember_verified(&checked.pending);
        }
        checked.outcome
    }

    /// [`check_credentials`](Self::check_credentials), and on success
    /// transparently re-hash an entry that is not in the current format (or
    /// has fewer than [`PBKDF2_ITERATIONS`] iterations), persisting the store.
    /// A plaintext login can migrate any entry; an MD5-digest login can only
    /// refresh the parameters of an entry already in the current format. A
    /// failure to persist the upgrade does not fail the verification.
    ///
    /// This runs the (deliberately slow) PBKDF2 work with `self` borrowed; a
    /// server that keeps the store behind a lock shared by every request
    /// should split the check with [`begin_check`](Self::begin_check) instead.
    pub fn authenticate(&mut self, name: &str, secret: &str, form: PasswordForm) -> CredentialCheck {
        match self.begin_check(name, secret, form) {
            Ok(outcome) => outcome,
            Err(pending) => {
                let checked = pending.run();
                self.finish_check(checked)
            }
        }
    }

    /// The first, cheap step of [`authenticate`](Self::authenticate) split
    /// in three so the slow PBKDF2 work can run **without** the store borrowed
    /// (e.g. with the server's state lock released):
    ///
    /// 1. `begin_check` (store borrowed, fast): `Ok(outcome)` when the answer
    ///    is known at once — the same credentials verified recently (the
    ///    verified-credentials cache) and the entry needs no re-hash; else a
    ///    [`PendingCheck`] holding a copy of the stored verifier;
    /// 2. [`PendingCheck::run`] (no borrow, slow): verify, and compute the
    ///    re-hashed entry if the user's entry needs migrating;
    /// 3. [`finish_check`](Self::finish_check) (store borrowed mutably, fast):
    ///    remember the verified credentials and install the re-hashed entry.
    pub fn begin_check(&self, name: &str, secret: &str, form: PasswordForm) -> Result<CredentialCheck, PendingCheck> {
        self.prepare(name, secret, form, true)
    }

    /// The last step of a split [`authenticate`](Self::authenticate) (see
    /// [`begin_check`](Self::begin_check)): returns the final outcome.
    ///
    /// The store may have changed since `begin_check` (the step in between
    /// runs without it). A success is kept only if the user still exists and
    /// either still has the entry that was verified (then the credentials are
    /// remembered and a computed re-hash is installed and saved) or has an
    /// entry the very same credentials were verified against meanwhile (a
    /// concurrent request migrated it); after a password change or a drop it
    /// is [`CredentialCheck::Invalid`].
    pub fn finish_check(&mut self, checked: CheckedCredentials) -> CredentialCheck {
        let CheckedCredentials {
            pending,
            outcome,
            rehashed,
        } = checked;
        if outcome != CredentialCheck::Valid {
            return outcome;
        }
        let Some(u) = self.users.get(&pending.name) else {
            return CredentialCheck::Invalid;
        };
        let unchanged = pending.stored.as_ref() == Some(&u.password);
        if !unchanged {
            return if self.cached(&pending) {
                CredentialCheck::Valid
            } else {
                CredentialCheck::Invalid
            };
        }
        self.remember_verified(&pending);
        if let Some(h) = rehashed {
            if let Some(u) = self.users.get_mut(&pending.name) {
                u.password = h;
            }
            let _ = self.save();
        }
        CredentialCheck::Valid
    }

    /// Set up a credential check (see [`begin_check`](Self::begin_check));
    /// `upgrading`: a cache hit on an entry that needs re-hashing still goes
    /// through [`PendingCheck::run`], which computes the new entry.
    fn prepare(
        &self,
        name: &str,
        secret: &str,
        form: PasswordForm,
        upgrading: bool,
    ) -> Result<CredentialCheck, PendingCheck> {
        // The cache is keyed by the normalized secret, so `ABC…` and `abc…`
        // digests share an entry.
        let secret = match form {
            PasswordForm::Plain => secret.to_string(),
            PasswordForm::Md5Hex => normalize_md5_hex(secret).unwrap_or_else(|| secret.to_string()),
        };
        let tag = self
            .verified_key
            .mac(&[name.as_bytes(), &[0, form.tag()], secret.as_bytes()]);
        let stored = self.users.get(name).map(|u| u.password.clone());
        let pending = PendingCheck {
            name: name.to_string(),
            form,
            secret,
            tag,
            stored,
        };
        if let Some(stored) = &pending.stored {
            if self.cached(&pending) && !(upgrading && stored.needs_upgrade()) {
                return Ok(CredentialCheck::Valid);
            }
        }
        Err(pending)
    }

    /// Whether `pending`'s credentials are in the verified-credentials cache.
    fn cached(&self, pending: &PendingCheck) -> bool {
        let cache = self.verified.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(&(pending.name.clone(), pending.form.tag()))
            .is_some_and(|t| constant_time_eq(t, &pending.tag))
    }

    /// Remember `pending`'s credentials as verified.
    fn remember_verified(&self, pending: &PendingCheck) {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((pending.name.clone(), pending.form.tag()), pending.tag);
    }

    /// [`verify`](Self::verify), and on success transparently re-hash an entry
    /// stored in an older format (or with fewer than [`PBKDF2_ITERATIONS`]
    /// iterations) into the current format, persisting the store. A failure to
    /// persist the upgrade does not fail the verification.
    pub fn verify_and_upgrade(&mut self, name: &str, password: &str) -> bool {
        self.authenticate(name, password, PasswordForm::Plain) == CredentialCheck::Valid
    }

    /// Drop `name`'s cached verified-credential tags (every form).
    fn forget_verified(&self, name: &str) {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(n, _), _| n != name);
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
        assert_eq!(parts[0], "pbkdf2-sha256-md5");
        assert_eq!(parts[0], PASSWORD_SCHEME);
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
        assert!(dbg.contains("pbkdf2-sha256-md5(iterations="), "{dbg}");
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
        assert!(alice.starts_with("alice\tpbkdf2-sha256-md5$"), "{alice}");
        assert!(alice.ends_with("\tquery,update"), "{alice}");
        let root = text.lines().find(|l| l.starts_with("root\t")).unwrap();
        assert!(root.starts_with(&format!("root\t{root_salt:016x}\t")), "{root}");

        // After a reload the migrated entry still verifies; root still legacy.
        let mut store = UserStore::open(&path, "ignored").unwrap();
        assert!(store.verify("alice", "alicepw"));
        assert!(!store.verify("alice", "wrong"));
        assert!(store.verify_and_upgrade(ROOT_USER, "rootpw"));
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.lines().any(|l| !l.contains("pbkdf2-sha256-md5$")), "{text}");
        let _ = fs::remove_file(&path);
    }

    /// The current entry is PBKDF2 over the lowercase MD5 hex of the password,
    /// pinned by a vector computed independently (Python `hashlib`:
    /// `pbkdf2_hmac("sha256", md5(b"password").hexdigest().encode(), b"salt",
    /// 4096, 32)`), and verifies both from the plaintext and from the digest.
    #[test]
    fn md5_scheme_verifies_plaintext_and_digest_clients() {
        let stored = format!(
            "pbkdf2-sha256-md5$4096${}${}",
            hex_encode(b"salt"),
            "09f557c40ab742331833f9598ca7f7926223f8367dbc77a5866ec166283d19b7"
        );
        assert!(verify_password(&stored, "password"));
        assert!(!verify_password(&stored, "Password"));
        let digest = "5f4dcc3b5aa765d61d8327deb882cf99";
        assert_eq!(md5_hex("password"), digest);
        assert_eq!(verify_password_md5(&stored, digest), Some(true));
        assert_eq!(verify_password_md5(&stored, &digest.to_ascii_uppercase()), Some(true));
        assert_eq!(verify_password_md5(&stored, &md5_hex("nope")), Some(false));
        // The plaintext itself (or a malformed digest) is not a valid digest.
        assert_eq!(verify_password_md5(&stored, "password"), Some(false));
        assert_eq!(verify_password_md5(&stored, &digest[..31]), Some(false));
        // The previous (raw) format cannot be verified from a digest.
        let raw = format!(
            "pbkdf2-sha256$4096${}${}",
            hex_encode(b"salt"),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
        assert_eq!(verify_password_md5(&raw, digest), None);
        assert!(verify_password(&raw, "password"));
        assert_eq!(verify_password_md5("garbage", digest), Some(false));
    }

    /// An entry in the previous `pbkdf2-sha256$` (raw) format and a legacy
    /// entry both refuse MD5-digest clients until a plaintext login migrated
    /// them; afterwards one stored hash serves both kinds of clients.
    #[test]
    fn older_entries_need_one_plaintext_login_before_md5_clients_work() {
        let path = tmp("md5_migrate");
        let mut raw_hash = [0u8; HASH_LEN];
        pbkdf2_hmac_sha256(b"bobpw", b"0123456789abcdef", PBKDF2_ITERATIONS, &mut raw_hash);
        let raw = format!(
            "pbkdf2-sha256${PBKDF2_ITERATIONS}${}${}",
            hex_encode(b"0123456789abcdef"),
            hex_encode(&raw_hash)
        );
        let salt = 0x0123_4567_89ab_cdefu64;
        fs::write(
            &path,
            format!(
                "bob\t{raw}\tquery\ncarl\t{salt:016x}\t{:016x}\tquery\nroot\t{}\tadmin\n",
                legacy_hash_password("carlpw", salt),
                hash_password("rootpw"),
            ),
        )
        .unwrap();
        let mut store = UserStore::open(&path, "ignored").unwrap();
        let md5 = PasswordForm::Md5Hex;
        let plain = PasswordForm::Plain;
        for (user, pw) in [("bob", "bobpw"), ("carl", "carlpw")] {
            assert_eq!(
                store.authenticate(user, &md5_hex(pw), md5),
                CredentialCheck::NeedsPlaintextLogin,
                "{user}"
            );
            assert_eq!(store.authenticate(user, "wrong", plain), CredentialCheck::Invalid);
            assert_eq!(store.authenticate(user, pw, plain), CredentialCheck::Valid);
            assert_eq!(store.authenticate(user, &md5_hex(pw), md5), CredentialCheck::Valid, "{user}");
            assert_eq!(
                store.authenticate(user, &md5_hex("wrong"), md5),
                CredentialCheck::Invalid
            );
        }
        // root was created in the current format: digest clients work at once.
        assert_eq!(store.check_credentials(ROOT_USER, &md5_hex("rootpw"), md5), CredentialCheck::Valid);
        assert_eq!(store.check_credentials("nobody", &md5_hex("x"), md5), CredentialCheck::Invalid);
        // Migrated entries survive a reload, in the current format only.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.lines().all(|l| l.split('\t').nth(1).unwrap().starts_with("pbkdf2-sha256-md5$")), "{text}");
        let store = UserStore::open(&path, "ignored").unwrap();
        assert!(store.verify("bob", "bobpw"));
        assert_eq!(store.check_credentials("carl", &md5_hex("carlpw"), md5), CredentialCheck::Valid);
        let _ = fs::remove_file(&path);
    }

    /// The verified-credential cache keeps the two forms apart and is dropped
    /// for both on a password change.
    #[test]
    fn verified_cache_is_per_form_and_invalidated_for_both() {
        let path = tmp("cache_forms");
        let mut store = UserStore::open(&path, "rootpw").unwrap();
        store.add("dana", "old", BTreeSet::new()).unwrap();
        let md5 = PasswordForm::Md5Hex;
        assert_eq!(store.check_credentials("dana", &md5_hex("old"), md5), CredentialCheck::Valid);
        assert!(store.verify("dana", "old"));
        // A plaintext password that happens to be the digest is not the digest.
        assert!(!store.verify("dana", &md5_hex("old")));
        store.set_password("dana", "new").unwrap();
        assert_eq!(store.check_credentials("dana", &md5_hex("old"), md5), CredentialCheck::Invalid);
        assert!(!store.verify("dana", "old"));
        assert_eq!(store.check_credentials("dana", &md5_hex("new"), md5), CredentialCheck::Valid);
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

    /// The split check (`begin_check` → `run` without the store → `finish_check`)
    /// gives the same answers as `authenticate`, and a store change while the
    /// slow step runs is honoured: a password change or a drop invalidates the
    /// result, a concurrent migration with the same password does not.
    #[test]
    fn split_credential_checks_tolerate_concurrent_changes() {
        let path = tmp("split");
        let mut raw_hash = [0u8; HASH_LEN];
        pbkdf2_hmac_sha256(b"danpw", b"0123456789abcdef", PBKDF2_ITERATIONS, &mut raw_hash);
        fs::write(
            &path,
            format!(
                "dan\tpbkdf2-sha256${PBKDF2_ITERATIONS}${}${}\tquery\n",
                hex_encode(b"0123456789abcdef"),
                hex_encode(&raw_hash)
            ),
        )
        .unwrap();
        let mut store = UserStore::open(&path, "rootpw").unwrap();
        let plain = PasswordForm::Plain;
        let split = |store: &UserStore, name: &str, pw: &str| -> CheckedCredentials {
            store
                .begin_check(name, pw, plain)
                .expect_err("not cached yet")
                .run()
        };

        // Fresh credentials need the slow step; afterwards they are cached.
        let checked = split(&store, ROOT_USER, "rootpw");
        assert_eq!(checked.outcome(), CredentialCheck::Valid);
        assert_eq!(store.finish_check(checked), CredentialCheck::Valid);
        assert_eq!(store.begin_check(ROOT_USER, "rootpw", plain).unwrap(), CredentialCheck::Valid);
        // Wrong passwords and unknown users are never cached.
        for (u, pw) in [(ROOT_USER, "nope"), ("nobody", "x")] {
            let checked = split(&store, u, pw);
            assert_eq!(store.finish_check(checked), CredentialCheck::Invalid);
            assert!(store.begin_check(u, pw, plain).is_err());
        }

        // Two concurrent first logins of an entry in the older format: both
        // compute a migrated entry; the first installs it, the second still
        // succeeds (same password, verified against the new entry meanwhile).
        let a = split(&store, "dan", "danpw");
        let b = split(&store, "dan", "danpw");
        assert_eq!(store.finish_check(a), CredentialCheck::Valid);
        assert_eq!(store.finish_check(b), CredentialCheck::Valid);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("dan\tpbkdf2-sha256-md5$"), "{text}");
        assert_eq!(
            store.check_credentials("dan", &md5_hex("danpw"), PasswordForm::Md5Hex),
            CredentialCheck::Valid
        );

        // A password change while the slow step runs invalidates its result.
        store.add("eve", "old", BTreeSet::new()).unwrap();
        let pending = split(&store, "eve", "old");
        store.set_password("eve", "new").unwrap();
        assert_eq!(store.finish_check(pending), CredentialCheck::Invalid);
        assert!(store.begin_check("eve", "old", plain).is_err(), "never cached");
        // So does dropping the user.
        let pending = split(&store, "eve", "new");
        store.drop_user("eve").unwrap();
        assert_eq!(store.finish_check(pending), CredentialCheck::Invalid);
        let _ = fs::remove_file(&path);
    }
}
