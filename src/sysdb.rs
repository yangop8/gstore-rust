//! The system database: users, per-database privileges, and the registry of
//! databases — the analogue of upstream gStore's `system.db`
//! (`src/Api/APIUtil.cpp`: `init_system`, `user_add`, `add_privilege`,
//! `check_privilege`, `init_databaseinfo`, …).
//!
//! Upstream keeps all of this as triples in a reserved gStore database.
//! Here the system store is two small files under the server's root
//! directory, each rewritten atomically (temp file, `fsync`, rename, directory
//! `fsync`) on every change:
//!
//! * **`users.db`** — the user table, owned by [`UserStore`]: one line per
//!   user with the name, the password hash (the versioned PBKDF2 scheme of
//!   [`crate::http_users`], unchanged) and the user's grants — privileges
//!   held on one database (`query@lubm`, upstream's per-database privileges)
//!   or on every database (`query`, the earlier global model), and the legacy
//!   `admin` super-privilege. Grants live with their user, so deleting a user
//!   deletes its grants in the same atomic write.
//! * **`system.json`** — the database registry ([`SysDb`]): for each database
//!   its name, `creator` and `built_time`, what upstream's `show` reports
//!   (plus the run-time `status`, see [`DbStatus`]).
//!
//! [`open_system`] opens both. **First start** (no `system.json` yet — e.g. a
//! server root written by an earlier version, holding a database-agnostic
//! `users.db` and some `<name>.db` directories): the old `users.db` is copied
//! to `users.db.pre-sysdb` and rewritten in the current format (its global
//! privileges keep applying to every database), and the registry is seeded
//! with the databases found on disk (creator `root`, built time = the
//! directory's modification time). The registry is reconciled with the
//! database directories again at every start, and on demand
//! ([`SysDb::sync_with_disk`]), so a database built or removed outside the
//! server (or by a code path that does not register it) is picked up.
//!
//! # The privilege model
//!
//! [`requirement`] maps each operation to what a caller other than `root`
//! needs, exactly as upstream's `APIUtil::check_privilege`
//! (`src/Api/APIUtil.cpp` ~1197-1280); [`allowed`] applies it to a user and a
//! database. Both the upstream `/api` layer ([`crate::gapi::auth::check_privilege`])
//! and the Rust-only REST endpoints of [`crate::http_api`] decide through
//! [`allowed`]:
//!
//! * nobody may target the `system` database;
//! * `root` — and a user holding the legacy `admin` privilege — may do
//!   everything else;
//! * `login`, `testConnect`, `getCoreVersion`, `check`, `show`,
//!   `userpassword`, `upload`, `download`, `funquery`/`funcudb`/`funreview`
//!   are open to every user;
//! * `query`/`monitor` need the query privilege **on the target database**;
//!   `update`, `batchInsert`, `batchRemove`, `begin`, `tquery`, `commit`,
//!   `rollback` and `checkpoint` the update privilege; `load`, `unload`,
//!   `backup`, `restore` and `export` the privilege of the same name;
//! * every other operation (`build`, `drop`, `stat`, `usermanage`,
//!   `showuser`, `userprivilegemanage`, `ipmanage`, the log operations,
//!   `rename`, …) is reserved to `root`.
//!
//! (Upstream's API docs list `checkpoint` among the root-only operations; its
//! code — followed here, as it is what clients see — checks the update
//! privilege.)

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::http_users::{Privilege, User, UserStore, ROOT_USER};
use crate::json::{self, Json};

/// The user-table file under the server root.
pub const USERS_FILE: &str = "users.db";

/// The database-registry file under the server root.
pub const SYSTEM_FILE: &str = "system.json";

/// Where [`open_system`] keeps the pre-migration `users.db` on first start.
pub const USERS_BACKUP_FILE: &str = "users.db.pre-sysdb";

/// The current `system.json` format.
const FORMAT: i64 = 1;

/// The name of upstream's system database, which no operation may target.
pub const SYSTEM_DB: &str = "system";

// ---------------------------------------------------------------------------
// The privilege model
// ---------------------------------------------------------------------------

/// What an operation requires of a caller other than `root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Any authenticated user.
    Anyone,
    /// The given privilege on the target database.
    Privilege(Privilege),
    /// Only `root` (or an `admin`-privileged user): upstream's
    /// `check_privilege` grants every operation not listed to `root` alone.
    RootOnly,
}

/// The requirement of operation `op` (an upstream operation name, or a
/// privilege name such as `"update"`), after upstream's
/// `APIUtil::check_privilege`.
pub fn requirement(op: &str) -> Requirement {
    match op {
        "login" | "testConnect" | "getCoreVersion" | "funquery" | "funcudb" | "funreview"
        | "check" | "show" | "userpassword" | "upload" | "download" => Requirement::Anyone,
        "query" | "monitor" => Requirement::Privilege(Privilege::Query),
        "update" | "batchInsert" | "batchRemove" | "begin" | "tquery" | "commit" | "rollback"
        | "checkpoint" => Requirement::Privilege(Privilege::Update),
        "load" => Requirement::Privilege(Privilege::Load),
        "unload" => Requirement::Privilege(Privilege::Unload),
        "restore" => Requirement::Privilege(Privilege::Restore),
        "backup" => Requirement::Privilege(Privilege::Backup),
        "export" => Requirement::Privilege(Privilege::Export),
        _ => Requirement::RootOnly,
    }
}

/// Whether `user` may run operation `op` on database `db` (see the module
/// docs). An unknown user may do nothing; a privileged operation without a
/// database is refused to everyone but `root` (as upstream, whose grant sets
/// never contain the empty name).
pub fn allowed(users: &UserStore, user: &str, op: &str, db: Option<&str>) -> bool {
    if db == Some(SYSTEM_DB) {
        return false;
    }
    if user == ROOT_USER {
        return true;
    }
    let Some(u) = users.get(user) else {
        return false;
    };
    if u.is_superuser() {
        return true;
    }
    match requirement(op) {
        Requirement::Anyone => true,
        Requirement::Privilege(p) => db.is_some_and(|d| u.has_on(p, d)),
        Requirement::RootOnly => false,
    }
}

/// The per-privilege fields of upstream's `showuser` answer, in its order.
pub const PRIVILEGE_FIELDS: [(&str, Privilege); 7] = [
    ("query_privilege", Privilege::Query),
    ("update_privilege", Privilege::Update),
    ("load_privilege", Privilege::Load),
    ("unload_privilege", Privilege::Unload),
    ("backup_privilege", Privilege::Backup),
    ("restore_privilege", Privilege::Restore),
    ("export_privilege", Privilege::Export),
];

/// The databases `user` holds `p` on, as upstream's `showuser` lists them
/// (`DBUserInfo::getQuery` …): each name followed by a comma (`"lubm,movie,"`),
/// `"all"` for `root` (and an `admin` user). A privilege held on every
/// database (the Rust extension, see the module docs) is listed as `*`.
pub fn privilege_list(user: &User, p: Privilege) -> String {
    if user.is_superuser() {
        return "all".to_string();
    }
    let mut out = String::new();
    if user.privileges.contains(&p) {
        out.push_str("*,");
    }
    for db in user.databases_with(p) {
        out.push_str(db);
        out.push(',');
    }
    out
}

// ---------------------------------------------------------------------------
// The database registry
// ---------------------------------------------------------------------------

/// A database's run-time state, as upstream's `show` reports it
/// (`DatabseStatusMap` in `src/Api/APIDatabaseUtil.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbStatus {
    Restoring,
    Building,
    Loading,
    Unloading,
    Backuping,
    Inserting,
    /// Built and on disk, not loaded (upstream's `AREADY_BUILT`).
    AlreadyBuilt,
    Loaded,
}

impl DbStatus {
    /// Upstream's name (`"already_built"`, `"loaded"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            DbStatus::Restoring => "restoring",
            DbStatus::Building => "building",
            DbStatus::Loading => "loading",
            DbStatus::Unloading => "unloading",
            DbStatus::Backuping => "backuping",
            DbStatus::Inserting => "inserting",
            DbStatus::AlreadyBuilt => "already_built",
            DbStatus::Loaded => "loaded",
        }
    }
}

/// One registered database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbRecord {
    /// The database name (without the `.db` suffix).
    pub database: String,
    /// The user who built it.
    pub creator: String,
    /// When it was built, `YYYY-MM-DD HH:MM:SS` in local time.
    pub built_time: String,
}

impl DbRecord {
    /// The upstream `show` entry for this database in state `status`:
    /// `{"database", "creator", "built_time", "status"}`.
    pub fn to_json(&self, status: DbStatus) -> Json {
        Json::obj()
            .with("database", self.database.as_str())
            .with("creator", self.creator.as_str())
            .with("built_time", self.built_time.as_str())
            .with("status", status.as_str())
    }
}

/// The database registry (`<root>/system.json`). See the module docs.
#[derive(Debug)]
pub struct SysDb {
    root: PathBuf,
    path: PathBuf,
    dbs: BTreeMap<String, DbRecord>,
    /// Whether this open created the registry (the first start).
    first_start: bool,
}

impl SysDb {
    /// Open the registry of the server rooted at `root`: read
    /// `<root>/system.json` (a file that does not parse is refused rather
    /// than silently replaced), then reconcile it with the database
    /// directories on disk and persist the result if anything changed (or the
    /// file did not exist yet).
    pub fn open(root: impl AsRef<Path>) -> io::Result<SysDb> {
        let root = root.as_ref().to_path_buf();
        let path = root.join(SYSTEM_FILE);
        let mut sys = SysDb {
            root,
            path,
            dbs: BTreeMap::new(),
            first_start: false,
        };
        match fs::read_to_string(&sys.path) {
            Ok(text) => sys.dbs = parse_registry(&text, &sys.path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => sys.first_start = true,
            Err(e) => return Err(e),
        }
        let changed = sys.reconcile(&[]);
        if changed || sys.first_start {
            sys.save()?;
        }
        Ok(sys)
    }

    /// Whether this registry was created by [`open`](Self::open) (the server
    /// root had no `system.json` before).
    pub fn first_start(&self) -> bool {
        self.first_start
    }

    /// The registry file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persist the registry atomically.
    pub fn save(&self) -> io::Result<()> {
        let mut list = Json::arr();
        for r in self.dbs.values() {
            list.push(
                Json::obj()
                    .with("database", r.database.as_str())
                    .with("creator", r.creator.as_str())
                    .with("built_time", r.built_time.as_str()),
            );
        }
        let doc = Json::obj().with("format", FORMAT).with("databases", list);
        let mut text = doc.to_string();
        text.push('\n');
        fs::create_dir_all(&self.root)?;
        crate::db::fsutil::atomic_write(&self.path, text.as_bytes()).map_err(to_io)
    }

    /// The record of database `name`.
    pub fn get(&self, name: &str) -> Option<&DbRecord> {
        self.dbs.get(name)
    }

    /// Whether database `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.dbs.contains_key(name)
    }

    /// Every registered database, sorted by name.
    pub fn records(&self) -> impl Iterator<Item = &DbRecord> {
        self.dbs.values()
    }

    /// The registered database names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.dbs.keys().cloned().collect()
    }

    /// Register database `name`, built by `creator` now (upstream
    /// `init_databaseinfo`). A database that is already registered is
    /// replaced (it was rebuilt).
    pub fn register(&mut self, name: &str, creator: &str) -> io::Result<()> {
        self.register_at(name, creator, &clock::now().datetime())
    }

    /// [`register`](Self::register) with an explicit `built_time` (e.g. kept
    /// from a backup being restored).
    pub fn register_at(&mut self, name: &str, creator: &str, built_time: &str) -> io::Result<()> {
        if !registrable(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid database name '{name}'"),
            ));
        }
        let rec = DbRecord {
            database: name.to_string(),
            creator: creator.to_string(),
            built_time: built_time.to_string(),
        };
        if self.dbs.get(name) == Some(&rec) {
            return Ok(());
        }
        let before = self.dbs.insert(name.to_string(), rec);
        self.save_or_undo(name, before)
    }

    /// Unregister database `name` (it was dropped). Returns whether it was
    /// registered. Its grants are kept by the user table — drop them with
    /// [`UserStore::forget_database`].
    pub fn unregister(&mut self, name: &str) -> io::Result<bool> {
        let Some(before) = self.dbs.remove(name) else {
            return Ok(false);
        };
        self.save_or_undo(name, Some(before))?;
        Ok(true)
    }

    /// Rename database `from` to `to`, keeping its creator and built time
    /// (upstream `rename_databaseinfo`). Returns whether `from` was
    /// registered.
    pub fn rename(&mut self, from: &str, to: &str) -> io::Result<bool> {
        if !registrable(to) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid database name '{to}'"),
            ));
        }
        let Some(mut rec) = self.dbs.remove(from) else {
            return Ok(false);
        };
        let old = rec.clone();
        rec.database = to.to_string();
        let replaced = self.dbs.insert(to.to_string(), rec);
        if let Err(e) = self.save() {
            self.dbs.remove(to);
            if let Some(r) = replaced {
                self.dbs.insert(to.to_string(), r);
            }
            self.dbs.insert(from.to_string(), old);
            return Err(e);
        }
        Ok(true)
    }

    /// Reconcile the registry with the database directories under the root
    /// (`<name>.db`): register a directory that is not registered (creator
    /// `root`, built time = its modification time) and unregister a database
    /// whose directory is gone — unless it is one of `loaded` (a database
    /// that exists only in memory so far). Persists the registry when it
    /// changed; returns whether it did.
    pub fn sync_with_disk(&mut self, loaded: &[String]) -> io::Result<bool> {
        let changed = self.reconcile(loaded);
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    fn reconcile(&mut self, loaded: &[String]) -> bool {
        let on_disk = database_dirs(&self.root);
        let mut changed = false;
        for (name, dir) in &on_disk {
            if !self.dbs.contains_key(name) {
                let built_time = fs::metadata(dir)
                    .and_then(|m| m.modified())
                    .map(|t| clock::at(t).datetime())
                    .unwrap_or_else(|_| clock::now().datetime());
                self.dbs.insert(
                    name.clone(),
                    DbRecord {
                        database: name.clone(),
                        creator: ROOT_USER.to_string(),
                        built_time,
                    },
                );
                changed = true;
            }
        }
        let before = self.dbs.len();
        self.dbs
            .retain(|name, _| on_disk.contains_key(name) || loaded.iter().any(|l| l == name));
        changed | (self.dbs.len() != before)
    }

    fn save_or_undo(&mut self, name: &str, before: Option<DbRecord>) -> io::Result<()> {
        if let Err(e) = self.save() {
            match before {
                Some(r) => self.dbs.insert(name.to_string(), r),
                None => self.dbs.remove(name),
            };
            return Err(e);
        }
        Ok(())
    }
}

/// Whether `name` can be a registered database: a valid database name that
/// is not the system database.
fn registrable(name: &str) -> bool {
    name != SYSTEM_DB && crate::http_api::valid_db_name(name)
}

/// The database directories under `root`: `<name>.db` directories whose
/// `name` is a valid, non-system database name.
fn database_dirs(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = fname.strip_suffix(crate::DB_SUFFIX) else {
            continue;
        };
        if registrable(name) && entry.path().is_dir() {
            out.insert(name.to_string(), entry.path());
        }
    }
    out
}

fn parse_registry(text: &str, path: &Path) -> io::Result<BTreeMap<String, DbRecord>> {
    let bad = |msg: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "system registry '{}' is malformed ({msg}); refusing to load it — fix or \
                 delete the file (it is rebuilt from the database directories)",
                path.display()
            ),
        )
    };
    let doc = json::parse(text).map_err(|e| bad(e.to_string()))?;
    let format = doc.get("format").and_then(Json::as_i64).unwrap_or(FORMAT);
    if format > FORMAT {
        return Err(bad(format!(
            "format {format} is newer than this server's ({FORMAT})"
        )));
    }
    let mut out = BTreeMap::new();
    let list = match doc.get("databases") {
        None => return Ok(out),
        Some(l) => l
            .as_array()
            .ok_or_else(|| bad("databases is not an array".into()))?,
    };
    for item in list {
        let field = |k: &str| item.get(k).and_then(Json::as_str).map(str::to_string);
        let Some(database) = field("database").filter(|n| registrable(n)) else {
            return Err(bad(format!("invalid entry {item}")));
        };
        out.insert(
            database.clone(),
            DbRecord {
                database,
                creator: field("creator").unwrap_or_else(|| ROOT_USER.to_string()),
                built_time: field("built_time").unwrap_or_default(),
            },
        );
    }
    Ok(out)
}

fn to_io(e: crate::error::GStoreError) -> io::Error {
    match e {
        crate::error::GStoreError::Io(io) => io,
        other => io::Error::other(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Opening the system store
// ---------------------------------------------------------------------------

/// Open the system store of the server rooted at `root`: the user table
/// (`<root>/users.db`, with `root` seeded at `root_password` the first time)
/// and the database registry (`<root>/system.json`). On the first start
/// (no registry yet) an existing `users.db` is first copied to
/// `users.db.pre-sysdb` and then rewritten in the current format — see the
/// module docs.
pub fn open_system(root: impl AsRef<Path>, root_password: &str) -> io::Result<(UserStore, SysDb)> {
    let root = root.as_ref();
    fs::create_dir_all(root)?;
    let users_path = root.join(USERS_FILE);
    let first_start = !root.join(SYSTEM_FILE).exists();
    let migrate = first_start && users_path.is_file();
    if migrate {
        let backup = root.join(USERS_BACKUP_FILE);
        if !backup.exists() {
            // `fs::copy` keeps the (owner-only) permissions of `users.db`.
            fs::copy(&users_path, &backup)?;
        }
    }
    let users = UserStore::open(&users_path, root_password)?;
    if migrate {
        users.save()?;
    }
    let sysdb = SysDb::open(root)?;
    Ok((users, sysdb))
}

// ---------------------------------------------------------------------------
// Local time
// ---------------------------------------------------------------------------

/// Wall-clock time in the server's local time zone, formatted as upstream's
/// `TimeUtil` does (`localtime` + `strftime`). The zone offset comes from the
/// C library's `localtime_r` on 64-bit Linux and macOS; elsewhere UTC is used.
pub mod clock {
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A broken-down local time.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LocalTime {
        pub year: i64,
        pub month: u32,
        pub day: u32,
        pub hour: u32,
        pub minute: u32,
        pub second: u32,
        pub millis: u32,
    }

    impl LocalTime {
        /// `YYYYMMDD` (upstream `TimeUtil::today()`, the daily log file name).
        pub fn date_compact(&self) -> String {
            format!("{:04}{:02}{:02}", self.year, self.month, self.day)
        }

        /// `YYYY-MM-DD HH:MM:SS` (upstream `NORM_DATETIME_PATTERN`).
        pub fn datetime(&self) -> String {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                self.year, self.month, self.day, self.hour, self.minute, self.second
            )
        }

        /// `YYYY-MM-DD HH:MM:SS.mmm` (upstream `NORM_DATETIME_MS_PATTERN`).
        pub fn datetime_ms(&self) -> String {
            format!("{}.{:03}", self.datetime(), self.millis)
        }
    }

    /// The current local time.
    pub fn now() -> LocalTime {
        at(SystemTime::now())
    }

    /// `t` in local time.
    pub fn at(t: SystemTime) -> LocalTime {
        let (secs, millis) = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX / 2),
                d.subsec_millis(),
            ),
            Err(_) => (0, 0),
        };
        from_unix(secs.saturating_add(utc_offset_secs(secs)), millis)
    }

    /// Milliseconds since the Unix epoch.
    pub fn unix_millis() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Break down `secs` seconds since the epoch (already shifted into the
    /// wanted zone) into a civil date and time.
    pub fn from_unix(secs: i64, millis: u32) -> LocalTime {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        LocalTime {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: (rem % 3600 / 60) as u32,
            second: (rem % 60) as u32,
            millis,
        }
    }

    /// The proleptic Gregorian date of day `z` since 1970-01-01 (Howard
    /// Hinnant's `civil_from_days`).
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = yoe + era * 400 + i64::from(month <= 2);
        (year, month, day)
    }

    /// The local zone's offset from UTC at `secs`, in seconds.
    #[cfg(all(
        any(target_os = "linux", target_os = "macos"),
        target_pointer_width = "64"
    ))]
    fn utc_offset_secs(secs: i64) -> i64 {
        use std::os::raw::{c_char, c_int, c_long};
        // `struct tm` on 64-bit Linux (glibc, musl) and macOS: nine ints, then
        // `long tm_gmtoff` and `const char *tm_zone`. Spare room at the end
        // guards against a libc with extra private fields.
        #[repr(C)]
        struct Tm {
            tm_sec: c_int,
            tm_min: c_int,
            tm_hour: c_int,
            tm_mday: c_int,
            tm_mon: c_int,
            tm_year: c_int,
            tm_wday: c_int,
            tm_yday: c_int,
            tm_isdst: c_int,
            tm_gmtoff: c_long,
            tm_zone: *const c_char,
            _spare: [u64; 8],
        }
        extern "C" {
            fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
        }
        let mut tm = Tm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
            _spare: [0; 8],
        };
        // SAFETY: `secs` is a valid `time_t` (64-bit on these targets) and
        // `tm` is a writable `struct tm` (with spare room); `localtime_r` is
        // the re-entrant variant and writes only into `tm`.
        let r = unsafe { localtime_r(&secs, &mut tm) };
        if r.is_null() {
            return 0;
        }
        let _ = (tm.tm_sec, tm.tm_isdst, tm.tm_zone);
        // Offsets are within ±26 hours; anything else is garbage.
        // `long` is 64-bit on every target this is compiled for.
        let off: i64 = tm.tm_gmtoff;
        if off.abs() <= 26 * 3600 {
            off
        } else {
            0
        }
    }

    #[cfg(not(all(
        any(target_os = "linux", target_os = "macos"),
        target_pointer_width = "64"
    )))]
    fn utc_offset_secs(_secs: i64) -> i64 {
        0
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn civil_dates_and_formats() {
            let t = from_unix(0, 0);
            assert_eq!(t.datetime(), "1970-01-01 00:00:00");
            // 2021-11-16 14:55:52 UTC (an upstream doc example) + 90 ms.
            let t = from_unix(1_637_074_552, 90);
            assert_eq!(t.datetime_ms(), "2021-11-16 14:55:52.090");
            assert_eq!(t.date_compact(), "20211116");
            // Leap day, end of a century, before the epoch.
            assert_eq!(from_unix(951_782_400, 0).datetime(), "2000-02-29 00:00:00");
            assert_eq!(from_unix(-1, 0).datetime(), "1969-12-31 23:59:59");
            assert_eq!(
                from_unix(4_102_444_799, 0).datetime(),
                "2099-12-31 23:59:59"
            );
        }

        #[test]
        fn local_time_is_within_a_day_of_utc() {
            let now = std::time::SystemTime::now();
            let secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
            let local = at(now);
            let utc = from_unix(secs, 0);
            let as_secs = |t: &LocalTime| {
                (t.year * 372 + i64::from(t.month) * 31 + i64::from(t.day)) * 86_400
                    + i64::from(t.hour) * 3600
                    + i64::from(t.minute) * 60
                    + i64::from(t.second)
            };
            assert!((as_secs(&local) - as_secs(&utc)).abs() <= 3 * 86_400);
        }
    }
}

/// The modification time of `path` in local time (`YYYY-MM-DD HH:MM:SS`).
pub fn modified_datetime(path: &Path) -> Option<String> {
    let t: SystemTime = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(clock::at(t).datetime())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gstore_sysdb_{tag}_{}_{}",
            std::process::id(),
            clock::unix_millis()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn requirements_follow_upstream_check_privilege() {
        assert_eq!(requirement("show"), Requirement::Anyone);
        assert_eq!(requirement("userpassword"), Requirement::Anyone);
        assert_eq!(
            requirement("monitor"),
            Requirement::Privilege(Privilege::Query)
        );
        assert_eq!(
            requirement("checkpoint"),
            Requirement::Privilege(Privilege::Update)
        );
        assert_eq!(
            requirement("restore"),
            Requirement::Privilege(Privilege::Restore)
        );
        for op in [
            "build", "drop", "showuser", "ipmanage", "querylog", "rename",
        ] {
            assert_eq!(requirement(op), Requirement::RootOnly, "{op}");
        }
    }

    #[test]
    fn allowed_checks_grants_per_database() {
        let root = tmp_root("allowed");
        let (mut users, _sys) = open_system(&root, "pw").unwrap();
        users.add("alice", "pw", BTreeSet::new()).unwrap();
        users
            .grant_on("alice", "lubm", &BTreeSet::from([Privilege::Query]))
            .unwrap();
        users
            .add("bob", "pw", BTreeSet::from([Privilege::Update]))
            .unwrap();
        users
            .add("ada", "pw", BTreeSet::from([Privilege::Admin]))
            .unwrap();
        assert!(allowed(&users, "alice", "query", Some("lubm")));
        assert!(!allowed(&users, "alice", "query", Some("movie")));
        assert!(!allowed(&users, "alice", "update", Some("lubm")));
        assert!(!allowed(&users, "alice", "query", None));
        assert!(allowed(&users, "alice", "show", None));
        assert!(!allowed(&users, "alice", "drop", Some("lubm")));
        // A privilege held on every database.
        assert!(allowed(&users, "bob", "commit", Some("anything")));
        assert!(!allowed(&users, "bob", "query", Some("anything")));
        // root and admin: everything but the system database.
        for su in [ROOT_USER, "ada"] {
            assert!(allowed(&users, su, "drop", Some("lubm")), "{su}");
            assert!(allowed(&users, su, "showuser", None), "{su}");
            assert!(!allowed(&users, su, "query", Some(SYSTEM_DB)), "{su}");
        }
        assert!(!allowed(&users, "ghost", "show", None));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn privilege_lists_render_like_upstream() {
        let root = tmp_root("lists");
        let (mut users, _sys) = open_system(&root, "pw").unwrap();
        users
            .add("alice", "pw", BTreeSet::from([Privilege::Export]))
            .unwrap();
        for db in ["movie", "lubm"] {
            users
                .grant_on("alice", db, &BTreeSet::from([Privilege::Query]))
                .unwrap();
        }
        let alice = users.get("alice").unwrap();
        assert_eq!(privilege_list(alice, Privilege::Query), "lubm,movie,");
        assert_eq!(privilege_list(alice, Privilege::Update), "");
        assert_eq!(privilege_list(alice, Privilege::Export), "*,");
        let root_user = users.get(ROOT_USER).unwrap();
        assert_eq!(privilege_list(root_user, Privilege::Load), "all");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_registry_persists_and_follows_the_disk() {
        let root = tmp_root("registry");
        fs::create_dir_all(root.join("old.db")).unwrap();
        fs::create_dir_all(root.join("system.db")).unwrap();
        fs::create_dir_all(root.join("x.db.bak")).unwrap();
        let mut sys = SysDb::open(&root).unwrap();
        assert!(sys.first_start());
        assert_eq!(sys.names(), vec!["old".to_string()], "found on disk");
        assert_eq!(sys.get("old").unwrap().creator, ROOT_USER);
        assert_eq!(sys.get("old").unwrap().built_time.len(), 19);
        fs::create_dir_all(root.join("lubm.db")).unwrap();
        sys.register("lubm", "alice").unwrap();
        assert!(sys.register("system", "root").is_err());
        assert!(sys.register("../x", "root").is_err());
        let rec = sys.get("lubm").unwrap().clone();
        assert_eq!(rec.creator, "alice");
        let j = rec.to_json(DbStatus::Loaded).to_string();
        assert!(j.contains(r#""database":"lubm","creator":"alice""#), "{j}");
        assert!(j.contains(r#""status":"loaded""#), "{j}");
        // Reopened: kept, not first start; a vanished directory is dropped.
        fs::remove_dir_all(root.join("old.db")).unwrap();
        let mut sys = SysDb::open(&root).unwrap();
        assert!(!sys.first_start());
        assert_eq!(sys.names(), vec!["lubm".to_string()]);
        assert_eq!(sys.get("lubm"), Some(&rec));
        // In memory only (loaded, not yet saved): kept by a sync.
        sys.register("fresh", "root").unwrap();
        assert!(!sys.sync_with_disk(&["fresh".to_string()]).unwrap());
        assert!(sys.sync_with_disk(&[]).unwrap());
        assert!(!sys.contains("fresh"));
        // Rename and unregister.
        assert!(sys.rename("lubm", "lubm2").unwrap());
        assert_eq!(sys.get("lubm2").unwrap().creator, "alice");
        assert!(sys.unregister("lubm2").unwrap());
        assert!(!sys.unregister("lubm2").unwrap());
        // A damaged file is refused.
        fs::write(root.join(SYSTEM_FILE), "{not json").unwrap();
        let err = SysDb::open(&root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn first_start_keeps_a_copy_of_the_old_user_table() {
        let root = tmp_root("migrate");
        let (mut users, _) = open_system(&root, "pw").unwrap();
        users
            .add("alice", "pw", BTreeSet::from([Privilege::Query]))
            .unwrap();
        drop(users);
        // Pretend an older server wrote it: no registry yet.
        fs::remove_file(root.join(SYSTEM_FILE)).unwrap();
        let before = fs::read_to_string(root.join(USERS_FILE)).unwrap();
        let (users, sys) = open_system(&root, "other").unwrap();
        assert!(sys.first_start());
        assert_eq!(
            fs::read_to_string(root.join(USERS_BACKUP_FILE)).unwrap(),
            before
        );
        assert!(users.has_priv_on("alice", Privilege::Query, "any"));
        assert!(users.verify(ROOT_USER, "pw"), "root keeps its password");
        let _ = fs::remove_dir_all(&root);
    }
}
