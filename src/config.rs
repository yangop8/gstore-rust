//! gStore's `conf.ini` configuration — the analogue of upstream
//! `src/Util/INIParser.cpp` + `Util::configure()` (`src/Util/Util.cpp`).
//!
//! Two layers:
//!
//! * [`Ini`] — a small INI reader with upstream's semantics: `[section]`
//!   headers, `key=value` lines split at the first `=`, keys and values
//!   whitespace-trimmed, `"` characters removed from values (upstream's
//!   `setGlobalConfig` does `replace_all(value, "\"", "")`), and the **first**
//!   occurrence of a key in a section wins (upstream inserts into a
//!   `std::map`). Lines starting with `#` or `;` are comments, as is the rest of
//!   an unquoted value after whitespace + `#`/`;`. Upstream has no comment
//!   syntax as such — a commented-out `# db_home=…` simply becomes a key named
//!   `#db_home` that nothing looks up — so treating those lines as comments is
//!   equivalent for every real file.
//! * [`GStoreConfig`] — every key of upstream's `conf/conf.ini` as a typed field,
//!   with upstream's built-in defaults (the `Util::configure()` defaults, i.e.
//!   what an empty `conf.ini` gives). An empty value means "unset" (upstream:
//!   `if (value.empty() == false)`), so the default applies. Unknown keys and
//!   unparsable values produce a [warning](GStoreConfig::warnings) and keep the
//!   default — loading a config never fails because of its content.
//!
//! **Precedence** (applied by `gserver`): command-line flag > `conf.ini` >
//! built-in default. Callers override a loaded config with
//! [`GStoreConfig::set`] (same parsing as the file) or by assigning the public
//! fields.
//!
//! Relative paths (`db_home`, `backup_path`, log paths, …) are relative to the
//! process's working directory, exactly as upstream (which is run from its
//! install directory). A shipped, commented example lives in
//! `conf/conf.ini.example`.
//!
//! A few **Rust-only extension keys** tune what upstream configures in code,
//! in a properties file, or not at all:
//!
//! * `[server]`: the HTTP layer — `listen_addr`, `keep_alive_timeout`,
//!   `keep_alive_max`, `queue_size` and `cors`;
//! * `[log]`: the diagnostic log that upstream configures in a log4cplus
//!   properties file — `log_level`, `log_file`, `log_days`, `log_stderr`
//!   (see [`crate::logging`]);
//! * `[upload]`: the limits a zip package must stay within —
//!   `unzip_max_entries`, `unzip_max_size` (see [`crate::zipio`]);
//! * `[query]`: how every served database evaluates queries
//!   ([`GStoreConfig::query_settings`], applied to each database the servers
//!   open, build, create or restore) — `integer_division`,
//!   `query_cache_size`, `early_termination`, `topk_bounded_heap`, `topk_dpb`
//!   and `gfun_max_output_bytes`. Upstream has no key for any of them (its
//!   query cache budget, 100 MB, is a constant in `QueryCache.h`);
//! * `[pfn]`: the sandbox limits of PFN user functions (applied the same
//!   way, `crate::pfn`) — `pfn_fuel_per_call`, `pfn_max_memory_pages`,
//!   `pfn_max_module_bytes` and `pfn_max_functions`. Upstream's only PFN key
//!   is `[system]`'s `pfn_base_path`, the directory the functions live in —
//!   read here too.
//!
//! They are documented on the fields below and in the example file.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::db::{QuerySettings, DEFAULT_QUERY_CACHE_BYTES};
use crate::query::{DpbMode, IntegerDivision, TopKOptions};
use crate::server::HttpOptions;

/// The servers' default for `gfun_max_output_bytes`: 64 MiB, the same bound
/// the crate puts on a response it downloads (a federated `SERVICE` result,
/// [`crate::http_client::Limits`]), and smaller than the 256 MiB an embedded
/// [`Database`](crate::Database) allows
/// ([`paths::MAX_OUTPUT_BYTES`](crate::analytics::gfun::paths::MAX_OUTPUT_BYTES)):
/// a server answers many clients at once and builds the response text from
/// the function's JSON, so one call should not hold hundreds of MiB.
pub const DEFAULT_GFUN_MAX_OUTPUT_BYTES: usize = 64 << 20;

// ---------------------------------------------------------------------------
// INI reader
// ---------------------------------------------------------------------------

/// One `key=value` entry of an [`Ini`] document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniEntry {
    /// The section the entry appeared in (`""` before the first `[section]`).
    pub section: String,
    pub key: String,
    /// The value with surrounding whitespace, `"` characters and any trailing
    /// comment removed. May be empty (upstream treats that as "unset").
    pub value: String,
    /// 1-based line number, for diagnostics.
    pub line: usize,
}

/// A parsed INI document (see the [module docs](self) for the syntax).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ini {
    entries: Vec<IniEntry>,
    /// Lines that were neither blank, a comment, a section header nor a
    /// `key=value` pair (1-based line number, text).
    malformed: Vec<(usize, String)>,
}

impl Ini {
    /// Parse INI text. Never fails: lines that make no sense are recorded in
    /// [`malformed`](Self::malformed) and otherwise ignored.
    pub fn parse(text: &str) -> Ini {
        let mut ini = Ini::default();
        let mut section = String::new();
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            // A UTF-8 BOM on the first line is not part of the content.
            let raw = if idx == 0 { raw.trim_start_matches('\u{feff}') } else { raw };
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                match rest.find(']') {
                    Some(end) => section = rest[..end].trim().to_string(),
                    None => ini.malformed.push((line_no, raw.to_string())),
                }
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                ini.malformed.push((line_no, raw.to_string()));
                continue;
            };
            let key = k.trim();
            if key.is_empty() {
                ini.malformed.push((line_no, raw.to_string()));
                continue;
            }
            ini.entries.push(IniEntry {
                section: section.clone(),
                key: key.to_string(),
                value: clean_value(v),
                line: line_no,
            });
        }
        ini
    }

    /// Every entry, in file order (duplicates included).
    pub fn entries(&self) -> &[IniEntry] {
        &self.entries
    }

    /// Lines that could not be understood, as `(line number, text)`.
    pub fn malformed(&self) -> &[(usize, String)] {
        &self.malformed
    }

    /// The value of `key` in `section`: the first occurrence wins, as in
    /// upstream's `INIParser::GetValue`. An empty value is returned as `Some("")`.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.section == section && e.key == key)
            .map(|e| e.value.as_str())
    }

    /// The distinct section names, in order of first appearance.
    pub fn sections(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for e in &self.entries {
            if !out.contains(&e.section.as_str()) {
                out.push(&e.section);
            }
        }
        out
    }
}

/// Normalize a raw value: drop a trailing comment (whitespace followed by `#`
/// or `;`, outside quotes), trim, and remove every `"` (upstream behaviour).
/// A value wrapped in single quotes loses that pair too.
fn clean_value(raw: &str) -> String {
    let mut in_quotes = false;
    let mut prev_ws = false;
    // A comment needs some value before it: `key = #x` keeps `#x` (upstream has
    // no inline comments, so a value may legitimately start with `#`).
    let mut seen_content = false;
    let mut cut = raw.len();
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' | ';' if !in_quotes && prev_ws && seen_content => {
                cut = i;
                break;
            }
            _ => {}
        }
        prev_ws = c.is_whitespace();
        seen_content |= !prev_ws;
    }
    let v = raw[..cut].trim().replace('"', "");
    let v = v.trim();
    match v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        Some(inner) if v.len() >= 2 => inner.to_string(),
        _ => v.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Typed configuration
// ---------------------------------------------------------------------------

/// Every recognised key as `(section, key)`, in upstream `conf.ini` order
/// followed by the Rust-only extensions.
pub const KNOWN_KEYS: &[(&str, &str)] = &[
    ("server", "port"),
    ("server", "tcp_port"),
    ("server", "thread_num"),
    ("server", "max_database_num"),
    ("server", "max_user_num"),
    ("server", "deamon"),
    ("server", "max_output_size"),
    ("server", "ip_allow_path"),
    ("server", "ip_deny_path"),
    ("system", "min_memory"),
    ("system", "min_million_disk"),
    ("system", "min_million_memory"),
    ("system", "resource_check"),
    ("system", "db_home"),
    ("system", "db_suffix"),
    ("system", "root_username"),
    ("system", "root_password"),
    ("system", "system_username"),
    ("system", "pfn_base_path"),
    ("system", "licensetype"),
    ("system", "check_point"),
    ("system", "build_multi_thread"),
    ("log", "log_mode"),
    ("log", "querylog_mode"),
    ("log", "querylog_path"),
    ("log", "querylog_days"),
    ("log", "accesslog_mode"),
    ("log", "accesslog_path"),
    ("log", "accesslog_days"),
    ("backup", "backup_path"),
    ("backup", "max_backups"),
    ("upload", "upload_path"),
    ("upload", "upload_max_body_size"),
    ("upload", "upload_allow_extensions"),
    ("upload", "upload_allow_compress_packages"),
    // Rust-only extensions.
    ("upload", "unzip_max_entries"),
    ("upload", "unzip_max_size"),
    ("server", "listen_addr"),
    ("server", "keep_alive_timeout"),
    ("server", "keep_alive_max"),
    ("server", "queue_size"),
    ("server", "cors"),
    ("log", "log_level"),
    ("log", "log_file"),
    ("log", "log_days"),
    ("log", "log_stderr"),
    ("query", "integer_division"),
    ("query", "query_cache_size"),
    ("query", "early_termination"),
    ("query", "topk_bounded_heap"),
    ("query", "topk_dpb"),
    ("query", "gfun_max_output_bytes"),
    ("pfn", "pfn_fuel_per_call"),
    ("pfn", "pfn_max_memory_pages"),
    ("pfn", "pfn_max_module_bytes"),
    ("pfn", "pfn_max_functions"),
];

/// Accepted spellings that map onto a [`KNOWN_KEYS`] entry.
const KEY_ALIASES: &[(&str, &str)] = &[("daemon", "deamon")];

/// The section a known key belongs in.
fn section_of(key: &str) -> Option<&'static str> {
    KNOWN_KEYS.iter().find(|(_, k)| *k == key).map(|(s, _)| *s)
}

/// Upstream's default `root_password` (`conf.ini`), which every gStore
/// install shares: `gserver` refuses to expose a root that still uses it.
pub const DEFAULT_ROOT_PASSWORD: &str = "123456";

/// gStore's server configuration: every key of upstream's `conf/conf.ini`
/// (plus a few Rust-only HTTP knobs), typed, with upstream's defaults.
///
/// `GStoreConfig::default()` is the configuration an empty `conf.ini` yields
/// upstream (`Util::configure()` defaults). Load a file with
/// [`load`](Self::load) or [`from_ini_str`](Self::from_ini_str); override single
/// keys with [`set`](Self::set) (e.g. from command-line flags).
///
/// Which of these settings this crate *acts on* today: `port`, `thread_num`,
/// `deamon`, `db_home`, `root_password`, `system_username`, `check_point`,
/// `upload_max_body_size` (request body cap) and the extension keys — in
/// `gserver` and [`ApiServer::with_config`](crate::http_api::ApiServer::with_config).
/// The rest are parsed and exposed so the upstream-compatible API layer can
/// read them (`max_output_size`, IP lists, logs, backups, uploads, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GStoreConfig {
    // ---- [server] -------------------------------------------------------
    /// HTTP API port (default `9000`).
    pub port: u16,
    /// Upstream's socket-API port (default `9100`). The socket API is not
    /// built in gStore 1.4 and not served here; parsed for compatibility.
    pub tcp_port: u16,
    /// Worker threads serving HTTP requests (default `30`, at least 1).
    pub thread_num: usize,
    /// Maximum number of databases (default `100`).
    pub max_database_num: usize,
    /// Maximum number of users (default `1000`).
    pub max_user_num: usize,
    /// `deamon=on` (sic, upstream's spelling; `daemon` is accepted too): run the
    /// server under a supervisor process that restarts it after an abnormal
    /// exit (default off).
    pub deamon: bool,
    /// Maximum number of result rows returned by a query (default `100000`).
    pub max_output_size: usize,
    /// IP allow-list file; when set it takes precedence over the deny list
    /// (default unset).
    pub ip_allow_path: Option<PathBuf>,
    /// IP deny-list file (default `conf/ipDeny.config`).
    pub ip_deny_path: Option<PathBuf>,
    /// *(Rust extension)* Address the HTTP listener binds (default
    /// `127.0.0.1`; upstream binds every interface — set `0.0.0.0` for that).
    pub listen_addr: String,
    /// *(Rust extension)* Keep-alive idle timeout in seconds; `0` turns
    /// keep-alive off (default `60`, upstream workflow's default).
    pub keep_alive_timeout: u64,
    /// *(Rust extension)* Maximum requests served on one connection (default `100`).
    pub keep_alive_max: usize,
    /// *(Rust extension)* Accepted connections that may wait for a free worker
    /// before the server stops accepting (default `128`).
    pub queue_size: usize,
    /// *(Rust extension)* Send CORS headers and answer `OPTIONS` preflights
    /// (default on).
    pub cors: bool,

    // ---- [system] -------------------------------------------------------
    /// Minimum free memory in MB for the resource check (default `512`).
    pub min_memory: u64,
    /// Disk needed per million triples, MB (default `50`).
    pub min_million_disk: u64,
    /// Memory needed per million triples, MB (default `20`).
    pub min_million_memory: u64,
    /// Check resources before building/loading (default off).
    pub resource_check: bool,
    /// Directory holding the databases (default `./dbhome/`).
    pub db_home: PathBuf,
    /// Database directory suffix (default `.db`).
    pub db_suffix: String,
    /// The root user's name (default `root`).
    pub root_username: String,
    /// The root user's initial password (default `123456`).
    pub root_password: String,
    /// The `system` user allowed to call `/shutdown` with the per-run password
    /// from the pid file (default `system`).
    pub system_username: String,
    /// Personalized-function (PFN) base directory (default `./pfn/`,
    /// upstream's). The servers read it only when it is set
    /// ([`is_set`](Self::is_set)); without it they keep the functions under
    /// `<db_home>/pfn`. Read at startup: `refreshconf` does not move them.
    pub pfn_base_path: PathBuf,
    /// License type reported by `login` (default `opensource`).
    pub licensetype: String,
    /// Save every write before it is acknowledged (default on).
    pub check_point: bool,
    /// Build with multiple threads (default on).
    pub build_multi_thread: bool,

    // ---- [log] ----------------------------------------------------------
    /// Upstream's log4cplus properties file (default `conf/slog.properties`).
    pub log_mode: String,
    /// Record the query log (default on — upstream `querylog_mode=1`).
    pub querylog_mode: bool,
    /// Query-log directory (default `logs/endpoint/`).
    pub querylog_path: PathBuf,
    /// Days of query logs to keep (default `30`).
    pub querylog_days: u32,
    /// Record the access log (default on — upstream `accesslog_mode=1`).
    pub accesslog_mode: bool,
    /// Access-log directory (default `logs/ipaccess/`).
    pub accesslog_path: PathBuf,
    /// Days of access logs to keep (default `30`).
    pub accesslog_days: u32,
    /// *(Rust extension)* The diagnostic log's level — `trace`, `debug`,
    /// `info`, `warn`, `error` or `off` — overriding the properties file
    /// `log_mode` names (default unset: the file's root level, else `info`).
    pub log_level: String,
    /// *(Rust extension)* The diagnostic log file (rolled daily), or `off`
    /// for none — overriding the properties file (default unset: the file's
    /// `DailyRollingFileAppender`, else no file).
    pub log_file: String,
    /// *(Rust extension)* Days of rolled diagnostic log files kept, `0` for
    /// all (applies when set; otherwise a properties file's log keeps them
    /// all, as upstream's log4cplus, and one `log_file` sets keeps `30`).
    pub log_days: u32,
    /// *(Rust extension)* Write the diagnostic log to standard error too
    /// (default on; applies when set, overriding the properties file).
    pub log_stderr: bool,

    // ---- [backup] -------------------------------------------------------
    /// Backup directory (default `./backups/`).
    pub backup_path: PathBuf,
    /// Backups kept per database before the oldest is removed (default `3`).
    pub max_backups: usize,

    // ---- [upload] -------------------------------------------------------
    /// Upload staging directory (default `./upload/`).
    pub upload_path: PathBuf,
    /// Maximum upload / request body size in bytes (default 1 GiB).
    pub upload_max_body_size: u64,
    /// Allowed upload extensions, and the extensions `build` / `batchInsert`
    /// / `batchRemove` sources may carry (default `nt|ttl|n3|rdf|txt|nq|trig`:
    /// upstream's, plus the quad formats — so a database's own export, N-Quads
    /// once it has named graphs, can be built again).
    pub upload_allow_extensions: Vec<String>,
    /// Allowed compressed upload package types (default `zip`).
    pub upload_allow_compress_packages: Vec<String>,
    /// *(Rust extension)* Entries a zip package may hold to be unpacked
    /// (`build` / `batchInsert` / `restore` of a `.zip`; default `10000`).
    pub unzip_max_entries: usize,
    /// *(Rust extension)* Bytes the files of a zip package may unpack to, in
    /// all (default 10 GiB) — the zip-bomb guard of `crate::zipio`.
    pub unzip_max_size: u64,

    // ---- [query] (Rust extension) ---------------------------------------
    /// *(Rust extension)* How `/` divides two integers: `decimal` (SPARQL
    /// 1.1, `7/2` = `3.5`, the default) or `truncate` (upstream gStore,
    /// `7/2` = `3`, `AVG` over integers truncates too); `w3c` / `upstream`
    /// are accepted as aliases.
    pub integer_division: IntegerDivision,
    /// *(Rust extension)* Each database's read-query cache budget in bytes;
    /// `0` turns the cache off (default 64 MiB,
    /// [`DEFAULT_QUERY_CACHE_BYTES`]).
    pub query_cache_size: usize,
    /// *(Rust extension)* Stream `LIMIT` (no `ORDER BY`), `ASK` and `EXISTS`
    /// evaluations and stop once enough solutions are found (default on).
    pub early_termination: bool,
    /// *(Rust extension)* Answer `ORDER BY … LIMIT` with a bounded heap
    /// instead of a full sort (default on).
    pub topk_bounded_heap: bool,
    /// *(Rust extension)* When DP-B Top-K answers `ORDER BY <linear
    /// expression> LIMIT` over a tree-shaped BGP: `auto` (when the
    /// statistics predict it pays off, the default), `always` or `off`.
    pub topk_dpb: DpbMode,
    /// *(Rust extension)* The most bytes of JSON one graph-function call
    /// (`shortestPaths(…)`, `kHopEnumerate(…)`, …) may produce before the
    /// query fails (default [`DEFAULT_GFUN_MAX_OUTPUT_BYTES`], 64 MiB).
    /// Also bounds the text one PFN function call may emit.
    pub gfun_max_output_bytes: usize,

    // ---- [pfn] (Rust extension) -----------------------------------------
    /// *(Rust extension)* The most WebAssembly instructions one PFN function
    /// call may run; `0` = unlimited (it still stops when the query is
    /// cancelled). Default [`crate::pfn::DEFAULT_FUEL_PER_CALL`] (10^9).
    pub pfn_fuel_per_call: u64,
    /// *(Rust extension)* The most 64 KiB pages a PFN function's memory may
    /// grow to (1..=65536; default 1024 = 64 MiB).
    pub pfn_max_memory_pages: u32,
    /// *(Rust extension)* The largest PFN module, source and compiled, in
    /// bytes (default 1 MiB).
    pub pfn_max_module_bytes: usize,
    /// *(Rust extension)* The most PFN functions one user may own (default
    /// 100).
    pub pfn_max_functions: usize,

    // ---- provenance -----------------------------------------------------
    /// The file this configuration was loaded from, if any.
    source: Option<PathBuf>,
    /// Keys given a non-empty value by the file or by [`set`](Self::set).
    explicit: BTreeSet<String>,
    /// Diagnostics collected while loading (unknown keys, bad values, …).
    warnings: Vec<String>,
}

impl Default for GStoreConfig {
    fn default() -> Self {
        GStoreConfig {
            port: 9000,
            tcp_port: 9100,
            thread_num: 30,
            max_database_num: 100,
            max_user_num: 1000,
            deamon: false,
            max_output_size: 100_000,
            ip_allow_path: None,
            ip_deny_path: Some(PathBuf::from("conf/ipDeny.config")),
            listen_addr: "127.0.0.1".to_string(),
            keep_alive_timeout: 60,
            keep_alive_max: 100,
            queue_size: 128,
            cors: true,
            min_memory: 512,
            min_million_disk: 50,
            min_million_memory: 20,
            resource_check: false,
            db_home: PathBuf::from("./dbhome/"),
            db_suffix: ".db".to_string(),
            root_username: "root".to_string(),
            root_password: DEFAULT_ROOT_PASSWORD.to_string(),
            system_username: "system".to_string(),
            pfn_base_path: PathBuf::from("./pfn/"),
            licensetype: "opensource".to_string(),
            check_point: true,
            build_multi_thread: true,
            log_mode: "conf/slog.properties".to_string(),
            querylog_mode: true,
            querylog_path: PathBuf::from("logs/endpoint/"),
            querylog_days: 30,
            accesslog_mode: true,
            accesslog_path: PathBuf::from("logs/ipaccess/"),
            accesslog_days: 30,
            log_level: String::new(),
            log_file: String::new(),
            log_days: crate::logging::DEFAULT_LOG_DAYS,
            log_stderr: true,
            backup_path: PathBuf::from("./backups/"),
            max_backups: 3,
            upload_path: PathBuf::from("./upload/"),
            upload_max_body_size: 1_073_741_824,
            upload_allow_extensions: split_list("nt|ttl|n3|rdf|txt|nq|trig"),
            upload_allow_compress_packages: split_list("zip"),
            unzip_max_entries: crate::zipio::DEFAULT_MAX_ENTRIES,
            unzip_max_size: crate::zipio::DEFAULT_MAX_SIZE,
            integer_division: IntegerDivision::default(),
            query_cache_size: DEFAULT_QUERY_CACHE_BYTES,
            early_termination: true,
            topk_bounded_heap: true,
            topk_dpb: DpbMode::Auto,
            gfun_max_output_bytes: DEFAULT_GFUN_MAX_OUTPUT_BYTES,
            pfn_fuel_per_call: crate::pfn::DEFAULT_FUEL_PER_CALL,
            pfn_max_memory_pages: crate::pfn::DEFAULT_MAX_MEMORY_PAGES,
            pfn_max_module_bytes: crate::pfn::DEFAULT_MAX_MODULE_BYTES,
            pfn_max_functions: crate::pfn::DEFAULT_MAX_FUNCTIONS,
            source: None,
            explicit: BTreeSet::new(),
            warnings: Vec::new(),
        }
    }
}

/// Error from [`GStoreConfig::set`]: the key or its value was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Not one of [`KNOWN_KEYS`] (or an alias).
    UnknownKey(String),
    /// The value does not parse for this key.
    BadValue { key: String, value: String, expected: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnknownKey(k) => write!(f, "unknown configuration key '{k}'"),
            ConfigError::BadValue { key, value, expected } => {
                write!(f, "invalid value '{value}' for '{key}' (expected {expected})")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl GStoreConfig {
    /// Read and apply `path`. Fails only when the file cannot be read; problems
    /// with its content become [warnings](Self::warnings).
    pub fn load(path: impl AsRef<Path>) -> io::Result<GStoreConfig> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        let mut cfg = GStoreConfig::from_ini_str(&text);
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Apply INI text on top of the defaults.
    pub fn from_ini_str(text: &str) -> GStoreConfig {
        let mut cfg = GStoreConfig::default();
        cfg.apply_ini(&Ini::parse(text));
        cfg
    }

    /// Apply a parsed INI document on top of this configuration. The first
    /// occurrence of a key wins (upstream); empty values leave the current
    /// value; unknown keys, misplaced keys and bad values are warnings.
    pub fn apply_ini(&mut self, ini: &Ini) {
        for (line, text) in ini.malformed() {
            self.warnings
                .push(format!("line {line}: ignoring unrecognised line '{}'", text.trim()));
        }
        // Keys in their own section first, so a correctly placed key always
        // beats a misplaced copy; within a section the first occurrence wins.
        let in_home = |e: &&IniEntry| section_of(canonical_key(&e.key)) == Some(e.section.as_str());
        let (home_entries, other_entries): (Vec<&IniEntry>, Vec<&IniEntry>) =
            ini.entries().iter().partition(in_home);
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for e in home_entries.into_iter().chain(other_entries) {
            let key = canonical_key(&e.key);
            let Some(home) = section_of(key) else {
                self.warnings.push(format!(
                    "line {}: unknown key '{}' in [{}] ignored",
                    e.line, e.key, e.section
                ));
                continue;
            };
            if e.section.is_empty() {
                // Upstream ignores keys before the first section header.
                self.warnings.push(format!(
                    "line {}: key '{}' appears before any [section] and is ignored \
                     (it belongs in [{home}])",
                    e.line, e.key
                ));
                continue;
            }
            if !seen.insert(key) {
                self.warnings.push(format!(
                    "line {}: duplicate key '{}' in [{}] ignored (the first one in [{home}] wins)",
                    e.line, e.key, e.section
                ));
                continue;
            }
            if e.section != home {
                self.warnings.push(format!(
                    "line {}: key '{}' belongs in [{home}], not [{}] (applied anyway)",
                    e.line, e.key, e.section
                ));
            }
            if e.value.is_empty() {
                // Upstream: an empty value means "use the default". For a path
                // whose default is a file (ip_deny_path) that still yields the
                // default — set the key to `off`/`none` to disable it.
                continue;
            }
            if let Err(err) = self.set(key, &e.value) {
                self.warnings
                    .push(format!("line {}: {err}; keeping the default", e.line));
            }
        }
    }

    /// Set one key from its textual form, exactly as if it appeared in
    /// `conf.ini` (used for command-line overrides). On error the old value is
    /// kept.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let key = canonical_key(key);
        let v = value.trim();
        let bad = |expected: &'static str| ConfigError::BadValue {
            key: key.to_string(),
            value: v.to_string(),
            expected,
        };
        match key {
            "port" => self.port = parse_port(v).ok_or_else(|| bad("a port number 0-65535"))?,
            "tcp_port" => {
                self.tcp_port = parse_port(v).ok_or_else(|| bad("a port number 0-65535"))?
            }
            "thread_num" => {
                self.thread_num = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| bad("a positive integer"))?
            }
            "max_database_num" => self.max_database_num = parse_num(v).ok_or_else(|| bad("an integer"))?,
            "max_user_num" => self.max_user_num = parse_num(v).ok_or_else(|| bad("an integer"))?,
            "deamon" => self.deamon = parse_switch(v).ok_or_else(|| bad("on or off"))?,
            "max_output_size" => self.max_output_size = parse_num(v).ok_or_else(|| bad("an integer"))?,
            "ip_allow_path" => self.ip_allow_path = parse_opt_path(v),
            "ip_deny_path" => self.ip_deny_path = parse_opt_path(v),
            "listen_addr" => {
                if v.is_empty() || v.contains(char::is_whitespace) {
                    return Err(bad("a host name or IP address"));
                }
                self.listen_addr = v.to_string()
            }
            "keep_alive_timeout" => {
                self.keep_alive_timeout = parse_num(v).ok_or_else(|| bad("seconds (0 = off)"))?
            }
            "keep_alive_max" => {
                self.keep_alive_max = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| bad("a positive integer"))?
            }
            "queue_size" => self.queue_size = parse_num(v).ok_or_else(|| bad("an integer"))?,
            "cors" => self.cors = parse_switch(v).ok_or_else(|| bad("on or off"))?,
            "min_memory" => self.min_memory = parse_num(v).ok_or_else(|| bad("an integer (MB)"))?,
            "min_million_disk" => {
                self.min_million_disk = parse_num(v).ok_or_else(|| bad("an integer (MB)"))?
            }
            "min_million_memory" => {
                self.min_million_memory = parse_num(v).ok_or_else(|| bad("an integer (MB)"))?
            }
            "resource_check" => self.resource_check = parse_switch(v).ok_or_else(|| bad("on or off"))?,
            "db_home" => self.db_home = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?),
            "db_suffix" => self.db_suffix = nonempty(v).ok_or_else(|| bad("a suffix such as .db"))?.to_string(),
            "root_username" => {
                self.root_username = nonempty(v).ok_or_else(|| bad("a user name"))?.to_string()
            }
            "root_password" => {
                self.root_password = nonempty(v).ok_or_else(|| bad("a password"))?.to_string()
            }
            "system_username" => {
                self.system_username = nonempty(v).ok_or_else(|| bad("a user name"))?.to_string()
            }
            "pfn_base_path" => {
                self.pfn_base_path = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "licensetype" => self.licensetype = v.to_string(),
            "check_point" => self.check_point = parse_switch(v).ok_or_else(|| bad("on or off"))?,
            "build_multi_thread" => {
                self.build_multi_thread = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "log_mode" => self.log_mode = v.to_string(),
            "querylog_mode" => self.querylog_mode = parse_switch(v).ok_or_else(|| bad("0 or 1"))?,
            "querylog_path" => {
                self.querylog_path = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "querylog_days" => self.querylog_days = parse_num(v).ok_or_else(|| bad("a number of days"))?,
            "accesslog_mode" => self.accesslog_mode = parse_switch(v).ok_or_else(|| bad("0 or 1"))?,
            "accesslog_path" => {
                self.accesslog_path = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "accesslog_days" => {
                self.accesslog_days = parse_num(v).ok_or_else(|| bad("a number of days"))?
            }
            "log_level" => {
                let level = crate::logging::Level::parse_filter(v)
                    .map_err(|()| bad("trace, debug, info, warn, error or off"))?;
                self.log_level = crate::logging::filter_name(level).to_string()
            }
            "log_file" => self.log_file = nonempty(v).ok_or_else(|| bad("a file path or off"))?.to_string(),
            "log_days" => self.log_days = parse_num(v).ok_or_else(|| bad("a number of days"))?,
            "log_stderr" => self.log_stderr = parse_switch(v).ok_or_else(|| bad("on or off"))?,
            "backup_path" => {
                self.backup_path = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "max_backups" => self.max_backups = parse_num(v).ok_or_else(|| bad("an integer"))?,
            "upload_path" => {
                self.upload_path = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "upload_max_body_size" => {
                self.upload_max_body_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "upload_allow_extensions" => self.upload_allow_extensions = split_list(v),
            "upload_allow_compress_packages" => self.upload_allow_compress_packages = split_list(v),
            "unzip_max_entries" => {
                self.unzip_max_entries = parse_num(v).ok_or_else(|| bad("a number of entries"))?
            }
            "unzip_max_size" => {
                self.unzip_max_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "integer_division" => {
                self.integer_division =
                    IntegerDivision::parse(v).ok_or_else(|| bad("decimal or truncate"))?
            }
            "query_cache_size" => {
                self.query_cache_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "early_termination" => {
                self.early_termination = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "topk_bounded_heap" => {
                self.topk_bounded_heap = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "topk_dpb" => self.topk_dpb = parse_dpb(v).ok_or_else(|| bad("auto, always or off"))?,
            "gfun_max_output_bytes" => {
                self.gfun_max_output_bytes = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "pfn_fuel_per_call" => {
                self.pfn_fuel_per_call =
                    parse_num(v).ok_or_else(|| bad("a number of instructions (0 = unlimited)"))?
            }
            "pfn_max_memory_pages" => {
                self.pfn_max_memory_pages = parse_num(v)
                    .filter(|n| (1..=65_536).contains(n))
                    .ok_or_else(|| bad("a number of 64 KiB pages, 1 to 65536"))?
            }
            "pfn_max_module_bytes" => {
                self.pfn_max_module_bytes = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "pfn_max_functions" => {
                self.pfn_max_functions = parse_num(v).ok_or_else(|| bad("an integer"))?
            }
            _ => return Err(ConfigError::UnknownKey(key.to_string())),
        }
        self.explicit.insert(key.to_string());
        Ok(())
    }

    /// The effective value of `key` in upstream's textual form (what
    /// `Util::getConfigureValue` returns): switches as `on`/`off` (`1`/`0` for
    /// the two log modes), lists `|`-joined, unset paths as `""`. `None` for an
    /// unknown key.
    pub fn value(&self, key: &str) -> Option<String> {
        let key = canonical_key(key);
        let switch = |b: bool| if b { "on" } else { "off" }.to_string();
        let digit = |b: bool| if b { "1" } else { "0" }.to_string();
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let opt_path = |p: &Option<PathBuf>| p.as_deref().map(path).unwrap_or_default();
        Some(match key {
            "port" => self.port.to_string(),
            "tcp_port" => self.tcp_port.to_string(),
            "thread_num" => self.thread_num.to_string(),
            "max_database_num" => self.max_database_num.to_string(),
            "max_user_num" => self.max_user_num.to_string(),
            "deamon" => switch(self.deamon),
            "max_output_size" => self.max_output_size.to_string(),
            "ip_allow_path" => opt_path(&self.ip_allow_path),
            "ip_deny_path" => opt_path(&self.ip_deny_path),
            "listen_addr" => self.listen_addr.clone(),
            "keep_alive_timeout" => self.keep_alive_timeout.to_string(),
            "keep_alive_max" => self.keep_alive_max.to_string(),
            "queue_size" => self.queue_size.to_string(),
            "cors" => switch(self.cors),
            "min_memory" => self.min_memory.to_string(),
            "min_million_disk" => self.min_million_disk.to_string(),
            "min_million_memory" => self.min_million_memory.to_string(),
            "resource_check" => switch(self.resource_check),
            "db_home" => path(&self.db_home),
            "db_suffix" => self.db_suffix.clone(),
            "root_username" => self.root_username.clone(),
            "root_password" => self.root_password.clone(),
            "system_username" => self.system_username.clone(),
            "pfn_base_path" => path(&self.pfn_base_path),
            "licensetype" => self.licensetype.clone(),
            "check_point" => switch(self.check_point),
            "build_multi_thread" => switch(self.build_multi_thread),
            "log_mode" => self.log_mode.clone(),
            "querylog_mode" => digit(self.querylog_mode),
            "querylog_path" => path(&self.querylog_path),
            "querylog_days" => self.querylog_days.to_string(),
            "accesslog_mode" => digit(self.accesslog_mode),
            "accesslog_path" => path(&self.accesslog_path),
            "accesslog_days" => self.accesslog_days.to_string(),
            "log_level" => self.log_level.clone(),
            "log_file" => self.log_file.clone(),
            "log_days" => self.log_days.to_string(),
            "log_stderr" => switch(self.log_stderr),
            "backup_path" => path(&self.backup_path),
            "max_backups" => self.max_backups.to_string(),
            "upload_path" => path(&self.upload_path),
            "upload_max_body_size" => self.upload_max_body_size.to_string(),
            "upload_allow_extensions" => self.upload_allow_extensions.join("|"),
            "upload_allow_compress_packages" => self.upload_allow_compress_packages.join("|"),
            "unzip_max_entries" => self.unzip_max_entries.to_string(),
            "unzip_max_size" => self.unzip_max_size.to_string(),
            "integer_division" => match self.integer_division {
                IntegerDivision::Decimal => "decimal",
                IntegerDivision::Truncate => "truncate",
            }
            .to_string(),
            "query_cache_size" => self.query_cache_size.to_string(),
            "early_termination" => switch(self.early_termination),
            "topk_bounded_heap" => switch(self.topk_bounded_heap),
            "topk_dpb" => match self.topk_dpb {
                DpbMode::Auto => "auto",
                DpbMode::Always => "always",
                DpbMode::Off => "off",
            }
            .to_string(),
            "gfun_max_output_bytes" => self.gfun_max_output_bytes.to_string(),
            "pfn_fuel_per_call" => self.pfn_fuel_per_call.to_string(),
            "pfn_max_memory_pages" => self.pfn_max_memory_pages.to_string(),
            "pfn_max_module_bytes" => self.pfn_max_module_bytes.to_string(),
            "pfn_max_functions" => self.pfn_max_functions.to_string(),
            _ => return None,
        })
    }

    /// Whether `key` was given a value by the loaded file or by [`set`](Self::set)
    /// (as opposed to holding its built-in default).
    pub fn is_set(&self, key: &str) -> bool {
        self.explicit.contains(canonical_key(key))
    }

    /// The file this configuration was loaded from, if any.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Diagnostics from loading: unknown or misplaced keys, duplicates,
    /// unparsable values, unrecognised lines. Never fatal.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Record an extra diagnostic (e.g. a setting this build cannot honour).
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    /// The directory of database `name`: `db_home/<name><db_suffix>`
    /// (upstream `GlobalTypedef::db_path`).
    pub fn db_path(&self, name: &str) -> PathBuf {
        self.db_home.join(format!("{name}{}", self.db_suffix))
    }

    /// The HTTP connection settings implied by this configuration: the worker
    /// pool (`thread_num`, `queue_size`), keep-alive, CORS, and the request
    /// body cap (`upload_max_body_size`).
    pub fn http_options(&self) -> HttpOptions {
        HttpOptions {
            threads: self.thread_num.max(1),
            queue_len: self.queue_size,
            keep_alive_timeout: Duration::from_secs(self.keep_alive_timeout),
            keep_alive_max: self.keep_alive_max.max(1),
            max_body: usize::try_from(self.upload_max_body_size).unwrap_or(usize::MAX),
            cors: self.cors,
            ..HttpOptions::default()
        }
    }

    /// The query-evaluation settings of the `[query]` section, which the
    /// servers apply to every database they open, build, create or restore
    /// ([`Database::apply_query_settings`](crate::Database::apply_query_settings)).
    pub fn query_settings(&self) -> QuerySettings {
        QuerySettings {
            integer_division: self.integer_division,
            query_cache_bytes: self.query_cache_size,
            topk: TopKOptions {
                early_termination: self.early_termination,
                bounded_heap: self.topk_bounded_heap,
                dpb: self.topk_dpb,
            },
            gfun_max_output_bytes: self.gfun_max_output_bytes,
            pfn: crate::pfn::PfnLimits {
                fuel_per_call: self.pfn_fuel_per_call,
                max_memory_pages: self.pfn_max_memory_pages,
                max_module_bytes: self.pfn_max_module_bytes,
                max_functions: self.pfn_max_functions,
            },
        }
    }

    /// Whether an upload named `file_name` has an allowed extension
    /// (`upload_allow_extensions` or `upload_allow_compress_packages`,
    /// compared case-insensitively).
    pub fn upload_allowed(&self, file_name: &str) -> bool {
        let Some((_, ext)) = file_name.rsplit_once('.') else {
            return false;
        };
        self.upload_allow_extensions
            .iter()
            .chain(&self.upload_allow_compress_packages)
            .any(|e| e.eq_ignore_ascii_case(ext))
    }
}

/// Map an accepted alias onto its canonical key.
fn canonical_key(key: &str) -> &str {
    KEY_ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map(|(_, k)| *k)
        .unwrap_or(key)
}

/// `topk_dpb`: `auto`, `always`, or `off` (`never` / `0` / `false` too).
fn parse_dpb(v: &str) -> Option<DpbMode> {
    match v.to_ascii_lowercase().as_str() {
        "auto" => Some(DpbMode::Auto),
        "always" | "on" => Some(DpbMode::Always),
        "off" | "never" | "0" | "false" | "no" => Some(DpbMode::Off),
        _ => None,
    }
}

fn parse_port(v: &str) -> Option<u16> {
    v.parse::<u16>().ok()
}

fn parse_num<T: std::str::FromStr>(v: &str) -> Option<T> {
    v.parse::<T>().ok()
}

/// `on`/`off` switches (upstream compares with `"on"`; the log modes with
/// `"1"`). Both spellings, plus `true`/`false`/`yes`/`no`, are accepted for
/// every switch.
fn parse_switch(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Some(true),
        "off" | "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// An optional path: `off`/`none` disable it (an empty value keeps the
/// default, see [`GStoreConfig::apply_ini`]).
fn parse_opt_path(v: &str) -> Option<PathBuf> {
    match v.to_ascii_lowercase().as_str() {
        "" | "off" | "none" => None,
        _ => Some(PathBuf::from(v)),
    }
}

fn nonempty(v: &str) -> Option<&str> {
    (!v.is_empty()).then_some(v)
}

/// Split a `|`-separated list (upstream `getline(ss, item, '|')`), dropping
/// empty items and surrounding whitespace.
fn split_list(v: &str) -> Vec<String> {
    v.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream's shipped `conf/conf.ini`, verbatim.
    const UPSTREAM_CONF: &str = r#"[server]
# the http api port
port=9000
# the socket api port
tcp_port=9100
thread_num=30
max_database_num=100
max_user_num=1000
# use deamon thread to run the server, values: on or off
deamon=off
# the maximum output result, default 100000
# max_output_size=100000
# ip_allow_path first if both path are configured
ip_allow_path=
ip_deny_path="./conf/ipDeny.config"

[system]
# unit MB
min_memory=1024
min_million_disk=200
min_million_memory=200
resource_check=off
# the directory do you want to place your database in
# NOTICE:the position is ideally in the root of system directory
# db_home="./dbhome/"

# which suffix do you want to add to your database name? please set it here
# NOTICE:all database name ends with ".db" by default, but you shouldn't add ".db" to the name you pass to system
# db_suffix=".db"

# root username
root_username="root"
# root default password
root_password="123456"
# system username for shutdown
system_username="system"
# auto check point, default on
check_point=on
# build use multi thread increase efficiency
build_multi_thread=off
[log]
# choose the system core log output model
# config/slog.stdout.properties for console
# config/slog.properties for file
log_mode="./conf/slog.properties"
# save the query log
# 0: not save
# 1: save the log
querylog_mode=1
querylog_path="logs/endpoint/"
querylog_days=30
# you can choose whether to record the operation log, by default it is opened.
# If it is closed(that is, the option is uncommented and set to 0), then system will run fatser but maybe not safe and recoverable
accesslog_mode=1
accesslog_path="logs/ipaccess/"
accesslog_days=30

[backup]
# backup database path
backup_path="./backups/"
# max backups file, if the number of backup files more than the value, system will remove the old backup files.
max_backups=3

[upload]
upload_path = "./upload/"
# max body size, 1 GB = 1024 MB = 1048576 KB(1M=1024KB) = 1073741824 B(1KB=1024B)
upload_max_body_size = 1073741824
# allow extensions
upload_allow_extensions = "nt|ttl|n3|rdf|txt"
# allow compress package
upload_allow_compress_packages = "zip"
"#;

    #[test]
    fn ini_sections_comments_trimming_and_quotes() {
        let ini = Ini::parse(
            "\u{feff}; leading comment\n\
             top = ignored\n\
             [ a ]\n\
             k1 = v1  \n\
             k2=\"quoted value\"\n\
             # k3 = commented out\n\
             k4 = 'single'\n\
             k5 = 7 # trailing comment\n\
             k6 = \"a # not a comment\"\n\
             k1 = second\n\
             garbage line\n\
             [b]\n\
             k1=\n\
             k2 = #not-a-comment\n",
        );
        assert_eq!(ini.get("b", "k2"), Some("#not-a-comment"), "a value may start with #");
        assert_eq!(ini.get("", "top"), Some("ignored"));
        assert_eq!(ini.get("a", "k1"), Some("v1"), "first occurrence wins");
        assert_eq!(ini.get("a", "k2"), Some("quoted value"));
        assert_eq!(ini.get("a", "k3"), None);
        assert_eq!(ini.get("a", "#k3"), None, "comment lines are not keys");
        assert_eq!(ini.get("a", "k4"), Some("single"));
        assert_eq!(ini.get("a", "k5"), Some("7"));
        assert_eq!(ini.get("a", "k6"), Some("a # not a comment"));
        assert_eq!(ini.get("b", "k1"), Some(""));
        assert_eq!(ini.get("c", "k1"), None);
        assert_eq!(ini.sections(), vec!["", "a", "b"]);
        assert_eq!(ini.malformed(), &[(11, "garbage line".to_string())]);
    }

    #[test]
    fn defaults_mirror_upstream_util_configure() {
        let c = GStoreConfig::default();
        assert_eq!(c.port, 9000);
        assert_eq!(c.tcp_port, 9100);
        assert_eq!(c.thread_num, 30);
        assert_eq!(c.max_database_num, 100);
        assert_eq!(c.max_user_num, 1000);
        assert!(!c.deamon);
        assert_eq!(c.max_output_size, 100_000);
        assert_eq!(c.ip_allow_path, None);
        assert_eq!(c.ip_deny_path, Some(PathBuf::from("conf/ipDeny.config")));
        assert_eq!(c.db_home, PathBuf::from("./dbhome/"));
        assert_eq!(c.db_suffix, ".db");
        assert_eq!(c.root_username, "root");
        assert_eq!(c.root_password, "123456");
        assert_eq!(c.system_username, "system");
        assert_eq!(c.pfn_base_path, PathBuf::from("./pfn/"));
        assert_eq!(c.licensetype, "opensource");
        assert_eq!((c.min_memory, c.min_million_disk, c.min_million_memory), (512, 50, 20));
        assert!(!c.resource_check && c.check_point && c.build_multi_thread);
        assert_eq!(c.log_mode, "conf/slog.properties");
        assert!(c.querylog_mode && c.accesslog_mode);
        assert_eq!(c.querylog_path, PathBuf::from("logs/endpoint/"));
        assert_eq!(c.accesslog_path, PathBuf::from("logs/ipaccess/"));
        assert_eq!((c.querylog_days, c.accesslog_days), (30, 30));
        assert_eq!((c.log_level.as_str(), c.log_file.as_str()), ("", ""));
        assert_eq!(c.log_days, 30);
        assert!(c.log_stderr);
        assert_eq!(c.backup_path, PathBuf::from("./backups/"));
        assert_eq!(c.max_backups, 3);
        assert_eq!(c.upload_path, PathBuf::from("./upload/"));
        assert_eq!(c.upload_max_body_size, 1_073_741_824);
        assert_eq!(
            c.upload_allow_extensions,
            ["nt", "ttl", "n3", "rdf", "txt", "nq", "trig"]
        );
        assert_eq!(c.upload_allow_compress_packages, ["zip"]);
        // [query]: an embedded database's defaults, except the servers'
        // smaller graph-function output bound.
        assert_eq!(
            c.query_settings(),
            QuerySettings {
                gfun_max_output_bytes: DEFAULT_GFUN_MAX_OUTPUT_BYTES,
                ..QuerySettings::default()
            }
        );
        assert!(c.warnings().is_empty());
        assert!(KNOWN_KEYS.iter().all(|(_, k)| !c.is_set(k)));
        // Every known key has a textual value and round-trips through `set`.
        for (_, key) in KNOWN_KEYS {
            let v = c.value(key).unwrap_or_else(|| panic!("value({key})"));
            let mut d = GStoreConfig::default();
            if !v.is_empty() {
                d.set(key, &v).unwrap_or_else(|e| panic!("set({key}, {v}): {e}"));
                assert_eq!(d.value(key).as_deref(), Some(v.as_str()), "{key}");
            }
        }
    }

    #[test]
    fn upstream_conf_ini_parses_without_warnings() {
        let c = GStoreConfig::from_ini_str(UPSTREAM_CONF);
        assert!(c.warnings().is_empty(), "{:?}", c.warnings());
        assert_eq!(c.port, 9000);
        assert_eq!(c.thread_num, 30);
        assert!(!c.deamon);
        // Commented out upstream ⇒ the default.
        assert_eq!(c.max_output_size, 100_000);
        assert!(!c.is_set("max_output_size"));
        assert_eq!(c.db_home, PathBuf::from("./dbhome/"));
        assert!(!c.is_set("db_home"));
        // `ip_allow_path=` is empty ⇒ unset; quotes are stripped.
        assert_eq!(c.ip_allow_path, None);
        assert_eq!(c.ip_deny_path, Some(PathBuf::from("./conf/ipDeny.config")));
        assert_eq!((c.min_memory, c.min_million_disk, c.min_million_memory), (1024, 200, 200));
        assert!(!c.build_multi_thread);
        assert_eq!(c.root_password, "123456");
        assert_eq!(c.log_mode, "./conf/slog.properties");
        assert_eq!(c.upload_path, PathBuf::from("./upload/"));
        assert_eq!(c.upload_allow_extensions, ["nt", "ttl", "n3", "rdf", "txt"]);
        assert!(c.is_set("upload_max_body_size"));
    }

    #[test]
    fn the_shipped_example_matches_upstream_and_parses_cleanly() {
        let text = include_str!("../conf/conf.ini.example");
        let c = GStoreConfig::from_ini_str(text);
        assert!(c.warnings().is_empty(), "{:?}", c.warnings());
        let upstream = GStoreConfig::from_ini_str(UPSTREAM_CONF);
        for (_, key) in KNOWN_KEYS {
            if *key == "upload_allow_extensions" {
                continue;
            }
            assert_eq!(c.value(key), upstream.value(key), "{key}");
        }
        // The one deliberate difference: the quad formats are allowed too,
        // so an export of a database with named graphs (N-Quads) builds
        // again.
        assert_eq!(
            c.upload_allow_extensions,
            [upstream.upload_allow_extensions.clone(), vec!["nq".into(), "trig".into()]].concat()
        );
    }

    /// The shipped example documents every key the code reads, in the
    /// section the code reads it from — set, or commented out with an
    /// example value — and every commented-out example parses; in the
    /// `[query]` and `[pfn]` sections (whose comments say "the values shown
    /// are the defaults") the example value is the default.
    #[test]
    fn the_shipped_example_documents_every_key() {
        let text = include_str!("../conf/conf.ini.example");
        let mut section = String::new();
        let mut documented: Vec<(String, String, String)> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if let Some(rest) = line.strip_prefix('[') {
                section = rest.trim_end_matches(']').trim().to_string();
                continue;
            }
            let line = line.trim_start_matches(['#', ';']).trim();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim();
            if KNOWN_KEYS.iter().any(|(_, known)| *known == key) {
                documented.push((section.clone(), key.to_string(), v.trim().replace('"', "")));
            }
        }
        for (sec, key) in KNOWN_KEYS {
            let mine: Vec<_> = documented.iter().filter(|(_, k, _)| k == key).collect();
            assert!(!mine.is_empty(), "[{sec}] {key} is not in conf.ini.example");
            assert!(
                mine.iter().all(|(s, _, _)| s == sec),
                "{key} documented outside [{sec}]: {mine:?}"
            );
        }
        let defaults = GStoreConfig::default();
        for (sec, key, value) in &documented {
            let mut c = GStoreConfig::default();
            c.set(key, value)
                .unwrap_or_else(|e| panic!("[{sec}] {key}={value}: {e}"));
            if sec == "query" || sec == "pfn" {
                assert_eq!(c.value(key), defaults.value(key), "[{sec}] {key}={value}");
            }
        }
    }

    #[test]
    fn every_key_is_read_from_its_section() {
        let text = "[server]\nport=9100\ntcp_port=9200\nthread_num=4\nmax_database_num=5\n\
                    max_user_num=6\ndeamon=on\nmax_output_size=7\nip_allow_path=allow.txt\n\
                    ip_deny_path=off\nlisten_addr=0.0.0.0\nkeep_alive_timeout=0\nkeep_alive_max=3\n\
                    queue_size=0\ncors=off\n\
                    [system]\nmin_memory=1\nmin_million_disk=2\nmin_million_memory=3\n\
                    resource_check=on\ndb_home=/data/gs\ndb_suffix=.gdb\nroot_username=admin\n\
                    root_password=s3cret\nsystem_username=sys\npfn_base_path=/p\nlicensetype=x\n\
                    check_point=off\nbuild_multi_thread=off\n\
                    [log]\nlog_mode=l.properties\nquerylog_mode=0\nquerylog_path=q/\n\
                    querylog_days=9\naccesslog_mode=0\naccesslog_path=a/\naccesslog_days=8\n\
                    log_level=DEBUG\nlog_file=diag/gs.log\nlog_days=7\nlog_stderr=off\n\
                    [backup]\nbackup_path=/b\nmax_backups=11\n\
                    [upload]\nupload_path=/u\nupload_max_body_size=1024\n\
                    upload_allow_extensions=nt | ttl\nupload_allow_compress_packages=zip|tar\n\
                    unzip_max_entries=12\nunzip_max_size=4096\n\
                    [query]\ninteger_division=truncate\nquery_cache_size=0\n\
                    early_termination=off\ntopk_bounded_heap=off\ntopk_dpb=always\n\
                    gfun_max_output_bytes=4096\n\
                    [pfn]\npfn_fuel_per_call=0\npfn_max_memory_pages=2\n\
                    pfn_max_module_bytes=5000\npfn_max_functions=3\n";
        let c = GStoreConfig::from_ini_str(text);
        assert!(c.warnings().is_empty(), "{:?}", c.warnings());
        assert!(KNOWN_KEYS.iter().all(|(_, k)| c.is_set(k)), "every key counted as set");
        assert_eq!((c.port, c.tcp_port, c.thread_num), (9100, 9200, 4));
        assert_eq!((c.max_database_num, c.max_user_num, c.max_output_size), (5, 6, 7));
        assert!(c.deamon);
        assert_eq!(c.ip_allow_path, Some(PathBuf::from("allow.txt")));
        assert_eq!(c.ip_deny_path, None, "`off` disables the deny list");
        assert_eq!(c.listen_addr, "0.0.0.0");
        assert_eq!((c.keep_alive_timeout, c.keep_alive_max, c.queue_size), (0, 3, 0));
        assert!(!c.cors);
        assert_eq!((c.min_memory, c.min_million_disk, c.min_million_memory), (1, 2, 3));
        assert!(c.resource_check && !c.check_point && !c.build_multi_thread);
        assert_eq!(c.db_home, PathBuf::from("/data/gs"));
        assert_eq!(c.db_suffix, ".gdb");
        assert_eq!(c.db_path("lubm"), PathBuf::from("/data/gs/lubm.gdb"));
        assert_eq!(c.root_username, "admin");
        assert_eq!(c.root_password, "s3cret");
        assert_eq!(c.system_username, "sys");
        assert_eq!(c.pfn_base_path, PathBuf::from("/p"));
        assert_eq!(c.licensetype, "x");
        assert_eq!(c.log_mode, "l.properties");
        assert!(!c.querylog_mode && !c.accesslog_mode);
        assert_eq!((c.querylog_days, c.accesslog_days), (9, 8));
        assert_eq!(c.querylog_path, PathBuf::from("q/"));
        assert_eq!(c.accesslog_path, PathBuf::from("a/"));
        assert_eq!(c.log_level, "debug", "stored lower-case");
        assert_eq!(c.log_file, "diag/gs.log");
        assert_eq!(c.log_days, 7);
        assert!(!c.log_stderr);
        assert_eq!(c.backup_path, PathBuf::from("/b"));
        assert_eq!(c.max_backups, 11);
        assert_eq!(c.upload_path, PathBuf::from("/u"));
        assert_eq!(c.upload_max_body_size, 1024);
        assert_eq!(c.upload_allow_extensions, ["nt", "ttl"]);
        assert_eq!(c.upload_allow_compress_packages, ["zip", "tar"]);
        assert_eq!(c.value("upload_allow_compress_packages").unwrap(), "zip|tar");
        assert_eq!((c.unzip_max_entries, c.unzip_max_size), (12, 4096));
        assert_eq!(c.value("querylog_mode").unwrap(), "0");
        assert_eq!(c.value("deamon").unwrap(), "on");
        assert!(c.upload_allowed("data.TTL") && c.upload_allowed("x.tar"));
        assert!(!c.upload_allowed("x.exe") && !c.upload_allowed("noext"));

        assert_eq!(
            c.query_settings(),
            QuerySettings {
                integer_division: IntegerDivision::Truncate,
                query_cache_bytes: 0,
                topk: TopKOptions {
                    early_termination: false,
                    bounded_heap: false,
                    dpb: DpbMode::Always,
                },
                gfun_max_output_bytes: 4096,
                pfn: crate::pfn::PfnLimits {
                    fuel_per_call: 0,
                    max_memory_pages: 2,
                    max_module_bytes: 5000,
                    max_functions: 3,
                },
            }
        );
        assert_eq!(c.value("integer_division").unwrap(), "truncate");
        assert_eq!(c.value("topk_dpb").unwrap(), "always");

        let h = c.http_options();
        assert_eq!(h.threads, 4);
        assert_eq!(h.queue_len, 0);
        assert_eq!(h.keep_alive_timeout, Duration::ZERO);
        assert_eq!(h.keep_alive_max, 3);
        assert_eq!(h.max_body, 1024);
        assert!(!h.cors);
    }

    #[test]
    fn unknown_keys_bad_values_and_misplacement_warn_but_never_fail() {
        let c = GStoreConfig::from_ini_str(
            "port=1\n[server]\nport=abc\nthread_num=0\nbogus=1\nport=9001\ndaemon=yes\n\
             [system]\nmax_backups=7\ncheck_point=maybe\n[log]\n=novalue\n",
        );
        let w = c.warnings().join("\n");
        assert!(w.contains("before any [section]"), "{w}");
        assert!(w.contains("invalid value 'abc' for 'port'"), "{w}");
        assert!(w.contains("'thread_num'"), "{w}");
        assert!(w.contains("unknown key 'bogus'"), "{w}");
        assert!(w.contains("duplicate key 'port'"), "{w}");
        assert!(w.contains("belongs in [backup]"), "{w}");
        assert!(w.contains("'check_point'"), "{w}");
        assert!(w.contains("unrecognised line '=novalue'"), "{w}");
        // A bad first value keeps the default; the later duplicate is ignored.
        assert_eq!(c.port, 9000);
        assert_eq!(c.thread_num, 30);
        assert!(c.check_point);
        // The alias and the misplaced key are applied.
        assert!(c.deamon);
        assert_eq!(c.max_backups, 7);
    }

    #[test]
    fn log_extension_keys_validate_their_values() {
        let c = GStoreConfig::from_ini_str(
            "[log]\nlog_level=chatty\nlog_days=many\nlog_stderr=maybe\nlog_file=\n",
        );
        let w = c.warnings().join("\n");
        assert!(w.contains("invalid value 'chatty' for 'log_level'"), "{w}");
        assert!(w.contains("'log_days'") && w.contains("'log_stderr'"), "{w}");
        assert!(!c.is_set("log_level") && !c.is_set("log_file"));
        let mut c = GStoreConfig::default();
        c.set("log_level", "OFF").unwrap();
        assert_eq!(c.value("log_level").unwrap(), "off");
        c.set("log_level", "warning").unwrap();
        assert_eq!(c.log_level, "warn");
        c.set("log_file", "off").unwrap();
        assert_eq!(c.value("log_file").unwrap(), "off");
    }

    #[test]
    fn command_line_overrides_take_precedence() {
        let mut c = GStoreConfig::from_ini_str("[server]\nport=9100\nthread_num=8\n");
        assert!(c.is_set("port") && !c.is_set("max_output_size"));
        c.set("port", "9200").unwrap();
        assert_eq!(c.port, 9200);
        c.check_point = false;
        assert_eq!(c.value("check_point").unwrap(), "off");
        assert_eq!(c.thread_num, 8, "untouched keys keep the file's value");
        assert_eq!(
            c.set("port", "70000"),
            Err(ConfigError::BadValue {
                key: "port".into(),
                value: "70000".into(),
                expected: "a port number 0-65535"
            })
        );
        assert_eq!(c.port, 9200, "a rejected override keeps the old value");
        assert_eq!(c.set("nope", "1"), Err(ConfigError::UnknownKey("nope".into())));
    }

    #[test]
    fn load_reads_a_file_and_reports_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("gstore_config_unit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conf.ini");
        std::fs::write(&path, "[server]\nport=9555\n").unwrap();
        let c = GStoreConfig::load(&path).unwrap();
        assert_eq!(c.port, 9555);
        assert_eq!(c.source(), Some(path.as_path()));
        assert!(GStoreConfig::load(dir.join("missing.ini")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[pfn]`: the PFN sandbox limits reach the query settings; bad values
    /// warn and keep the default.
    #[test]
    fn pfn_limits_are_read_and_validated() {
        let c = GStoreConfig::from_ini_str(
            "[pfn]\npfn_max_memory_pages=0\npfn_fuel_per_call=-1\npfn_max_module_bytes=12\n\
             pfn_max_functions=2\n",
        );
        let w = c.warnings().join("\n");
        assert!(w.contains("'pfn_max_memory_pages'"), "{w}");
        assert!(w.contains("'pfn_fuel_per_call'"), "{w}");
        let pfn = c.query_settings().pfn;
        assert_eq!(pfn.max_memory_pages, crate::pfn::DEFAULT_MAX_MEMORY_PAGES);
        assert_eq!(pfn.fuel_per_call, crate::pfn::DEFAULT_FUEL_PER_CALL);
        assert_eq!((pfn.max_module_bytes, pfn.max_functions), (12, 2));
        assert_eq!(c.value("pfn_max_module_bytes").unwrap(), "12");
        let mut c = GStoreConfig::default();
        assert_eq!(c.query_settings().pfn, crate::pfn::PfnLimits::default());
        c.set("pfn_max_memory_pages", "65536").unwrap();
        assert!(c.set("pfn_max_memory_pages", "65537").is_err());
        c.set("pfn_fuel_per_call", "0").unwrap();
        assert_eq!(c.query_settings().pfn.fuel_per_call, 0, "0 = unlimited");
    }
}
