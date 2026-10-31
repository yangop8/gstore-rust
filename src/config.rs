//! `conf.ini` — the gStore server configuration file.
//!
//! Mirrors upstream gStore 1.4's `conf/conf.ini` (read by
//! `src/Util/INIParser.cpp` and applied in `Util::configure`,
//! `src/Util/Util.cpp`): an INI file of `[section]` headers and `key=value`
//! lines. [`Ini`] is the raw parser; [`GStoreConfig`] is the typed view with
//! upstream's built-in default for every key.
//!
//! **Parsing** ([`Ini::parse`]) follows upstream's semantics, minus its bugs:
//!
//! * `[section]` starts a section; `key=value` adds an entry to the current
//!   section (split at the first `=`). Keys and values are whitespace-trimmed.
//! * A value wrapped in a pair of double (or single) quotes loses them —
//!   upstream strips `"` from every value (`setGlobalConfig`), so
//!   `root_password="123456"` means `123456`.
//! * Lines whose first non-blank character is `#` or `;` are comments; blank
//!   lines are skipped. There are no inline comments (a `#` inside a value is
//!   part of the value, as upstream).
//! * An **empty value counts as unset**: upstream ignores `key=` lines, so the
//!   key keeps its built-in default (e.g. `ip_deny_path=` still means
//!   `conf/ipDeny.config`).
//! * When a key repeats within a section the **first** occurrence wins (as
//!   upstream's `std::map::insert`); later ones are reported as warnings.
//! * Anything else — a key outside any section, a line with no `=`, an unknown
//!   key, a value that does not parse — is a warning, never an error: the
//!   setting keeps its default and the server still starts.
//!
//! **Precedence** is command-line flag > `conf.ini` > built-in default:
//! [`GStoreConfig::load`] layers the file over [`GStoreConfig::default`], and
//! callers (e.g. `gserver`) assign their flags on top.
//!
//! **Paths** in the file are used as written; relative ones are relative to the
//! process's working directory, as upstream (which runs from its install root).
//!
//! Besides upstream's keys, a few Rust-only `[server]` keys tune the HTTP
//! layer (`bind_addr`, `keep_alive_timeout`, `keep_alive_max_requests`,
//! `max_body_size`, `cors_allow_origin`, `shutdown_grace`); see
//! `conf/conf.ini.example` for the annotated file.

use std::fmt;
use std::path::{Path, PathBuf};

/// The upstream default location of the configuration file (relative to the
/// working directory), used when no `-c/--config` is given.
pub const DEFAULT_CONFIG_PATH: &str = "conf/conf.ini";

/// The name of the pid file kept in the database home by `gserver`.
pub const PID_FILE_NAME: &str = "gstore.pid";

/// The name of the log file a background (`gserver -b`) server writes its
/// standard output and error to, in the database home.
pub const SERVER_LOG_NAME: &str = "gserver.log";

// ---------------------------------------------------------------------------
// Raw INI parsing
// ---------------------------------------------------------------------------

/// One `key=value` line of an INI file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniEntry {
    /// The section the line appeared in (without brackets).
    pub section: String,
    pub key: String,
    /// The value with surrounding whitespace and one pair of quotes removed.
    /// Never empty: empty values are dropped as "unset".
    pub value: String,
    /// 1-based line number, for messages.
    pub line: usize,
}

/// A parsed INI file: its entries in file order plus the non-fatal problems
/// met while parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ini {
    entries: Vec<IniEntry>,
    /// Section names in order of first appearance (including empty sections).
    sections: Vec<String>,
    warnings: Vec<String>,
}

impl Ini {
    /// Parse INI text (see the [module docs](self) for the rules). Never fails;
    /// problems are collected in [`warnings`](Self::warnings).
    pub fn parse(text: &str) -> Ini {
        let mut ini = Ini::default();
        let mut section: Option<String> = None;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                match rest.find(']') {
                    Some(end) => {
                        let name = rest[..end].trim().to_string();
                        if name.is_empty() {
                            ini.warn(line_no, "empty section name; following keys are ignored");
                            section = None;
                        } else {
                            if !ini.sections.contains(&name) {
                                ini.sections.push(name.clone());
                            }
                            section = Some(name);
                        }
                    }
                    None => ini.warn(line_no, &format!("malformed section header '{line}'")),
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                ini.warn(line_no, &format!("ignored line without '=': '{line}'"));
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                ini.warn(line_no, "ignored line with an empty key");
                continue;
            }
            let Some(sec) = section.clone() else {
                ini.warn(line_no, &format!("key '{key}' is outside any [section]; ignored"));
                continue;
            };
            let value = unquote(value.trim());
            if value.is_empty() {
                continue; // upstream: an empty value leaves the key unset
            }
            if ini.get(&sec, key).is_some() {
                ini.warn(
                    line_no,
                    &format!("duplicate key '{key}' in [{sec}]; the first value is kept"),
                );
                continue;
            }
            ini.entries.push(IniEntry {
                section: sec,
                key: key.to_string(),
                value: value.to_string(),
                line: line_no,
            });
        }
        ini
    }

    fn warn(&mut self, line: usize, msg: &str) {
        self.warnings.push(format!("line {line}: {msg}"));
    }

    /// The value of `key` in `section`, if set (and non-empty).
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.section == section && e.key == key)
            .map(|e| e.value.as_str())
    }

    /// Every entry, in file order.
    pub fn entries(&self) -> &[IniEntry] {
        &self.entries
    }

    /// Section names in order of first appearance.
    pub fn sections(&self) -> &[String] {
        &self.sections
    }

    /// Non-fatal problems met while parsing.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Strip one pair of matching surrounding quotes (`"…"` or `'…'`).
fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

// ---------------------------------------------------------------------------
// Typed configuration
// ---------------------------------------------------------------------------

/// The server configuration: every key of upstream gStore 1.4's
/// `conf/conf.ini` (and the defaults `Util::configure` applies when one is
/// absent), plus the Rust-only HTTP knobs. Build it with
/// [`GStoreConfig::default`], [`from_ini_str`](Self::from_ini_str) or
/// [`load`](Self::load), then override fields from the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct GStoreConfig {
    // ---- [server] -----------------------------------------------------------
    /// HTTP API port (`port`, default 9000).
    pub port: u16,
    /// Upstream's socket-API port (`tcp_port`, default 9100). Accepted for
    /// compatibility; the Rust server has no separate socket API.
    pub tcp_port: u16,
    /// Request worker threads (`thread_num`, default 30): the size of the
    /// bounded worker pool that serves HTTP connections.
    pub thread_num: usize,
    /// Most databases that may exist (`max_database_num`, default 100).
    pub max_database_num: usize,
    /// Most users that may exist (`max_user_num`, default 1000).
    pub max_user_num: usize,
    /// Run under a supervisor that restarts the server after an abnormal exit
    /// (`deamon` — upstream's spelling — `on`/`off`, default off).
    pub deamon: bool,
    /// Most result rows a query returns over the API (`max_output_size`,
    /// default 100000).
    pub max_output_size: usize,
    /// IP allow-list file (`ip_allow_path`, default unset). When set it takes
    /// precedence over the deny list.
    pub ip_allow_path: Option<PathBuf>,
    /// IP deny-list file (`ip_deny_path`, default `conf/ipDeny.config`).
    pub ip_deny_path: Option<PathBuf>,
    /// Rust-only: the address to bind (`bind_addr`, default `127.0.0.1`;
    /// upstream listens on every interface — use `0.0.0.0` for that).
    pub bind_addr: String,
    /// Rust-only: seconds an idle keep-alive connection is kept open
    /// (`keep_alive_timeout`, default 15; `0` disables keep-alive).
    pub keep_alive_timeout: u64,
    /// Rust-only: requests served on one connection before it is closed
    /// (`keep_alive_max_requests`, default 100).
    pub keep_alive_max_requests: usize,
    /// Rust-only: the largest accepted request body in bytes
    /// (`max_body_size`). Unset means `upload_max_body_size` plus 1 MiB of
    /// multipart overhead — see [`request_body_limit`](Self::request_body_limit).
    pub max_body_size: Option<u64>,
    /// Rust-only: the `Access-Control-Allow-Origin` sent on every response and
    /// preflight (`cors_allow_origin`, default unset = only upstream's
    /// `/file/*` preflights answer CORS).
    pub cors_allow_origin: Option<String>,
    /// Rust-only: seconds a graceful shutdown waits for in-flight requests
    /// (`shutdown_grace`, default 10).
    pub shutdown_grace: u64,

    // ---- [system] -----------------------------------------------------------
    /// Directory holding the databases, users and logs (`db_home`, default
    /// `./dbhome/`). `gserver` keeps its pid file here too.
    pub db_home: PathBuf,
    /// Database directory suffix (`db_suffix`, default `.db`; the Rust server
    /// only supports `.db`).
    pub db_suffix: String,
    /// The administrator account (`root_username`, default `root`).
    pub root_username: String,
    /// The administrator's initial password (`root_password`, default
    /// `123456`), used when the user store is first created.
    pub root_password: String,
    /// The account `gserver -t` / `/shutdown` use (`system_username`, default
    /// `system`); its password is generated at start and kept in the pid file.
    pub system_username: String,
    /// Personalized-function directory (`pfn_base_path`, default `./pfn/`).
    pub pfn_base_path: PathBuf,
    /// License type reported by `login` (`licensetype`, default `opensource`).
    pub licensetype: String,
    /// Resource-check thresholds (`min_memory` MB default 512,
    /// `min_million_disk` default 50, `min_million_memory` default 20) and
    /// switch (`resource_check`, default off). Accepted for compatibility.
    pub min_memory: u64,
    pub min_million_disk: u64,
    pub min_million_memory: u64,
    pub resource_check: bool,
    /// Save every write before acknowledging it (`check_point`, default on).
    pub check_point: bool,
    /// Multi-threaded build (`build_multi_thread`, default on).
    pub build_multi_thread: bool,

    // ---- [log] --------------------------------------------------------------
    /// Upstream's log4cplus properties file (`log_mode`); accepted, unused.
    pub log_mode: String,
    /// Keep the query log (`querylog_mode`, `1`/`0`, default 1).
    pub querylog_mode: bool,
    /// Query-log directory (`querylog_path`, default `logs/endpoint/`).
    pub querylog_path: PathBuf,
    /// Days of query log kept (`querylog_days`, default 30).
    pub querylog_days: u32,
    /// Keep the access log (`accesslog_mode`, default 1).
    pub accesslog_mode: bool,
    /// Access-log directory (`accesslog_path`, default `logs/ipaccess/`).
    pub accesslog_path: PathBuf,
    /// Days of access log kept (`accesslog_days`, default 30).
    pub accesslog_days: u32,

    // ---- [backup] -----------------------------------------------------------
    /// Default backup directory (`backup_path`, default `./backups/`).
    pub backup_path: PathBuf,
    /// Backups kept per database; older ones are removed (`max_backups`,
    /// default 3).
    pub max_backups: u32,

    // ---- [upload] -----------------------------------------------------------
    /// Upload staging directory (`upload_path`, default `./upload/`).
    pub upload_path: PathBuf,
    /// Largest uploaded file in bytes (`upload_max_body_size`, default 1 GiB).
    pub upload_max_body_size: u64,
    /// Allowed upload extensions (`upload_allow_extensions`, `|`-separated,
    /// default `nt|ttl|n3|rdf|txt`).
    pub upload_allow_extensions: Vec<String>,
    /// Allowed archive extensions (`upload_allow_compress_packages`, default
    /// `zip`).
    pub upload_allow_compress_packages: Vec<String>,

    // ---- provenance ----------------------------------------------------------
    /// The file this configuration was loaded from, if any.
    pub source: Option<PathBuf>,
    /// Non-fatal problems met while loading (unknown keys, bad values, …).
    pub warnings: Vec<String>,
}

impl Default for GStoreConfig {
    /// Upstream's built-in defaults (`Util::configure`), plus the Rust-only
    /// HTTP knobs' defaults.
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
            bind_addr: "127.0.0.1".to_string(),
            keep_alive_timeout: 15,
            keep_alive_max_requests: 100,
            max_body_size: None,
            cors_allow_origin: None,
            shutdown_grace: 10,
            db_home: PathBuf::from("./dbhome/"),
            db_suffix: ".db".to_string(),
            root_username: "root".to_string(),
            root_password: "123456".to_string(),
            system_username: "system".to_string(),
            pfn_base_path: PathBuf::from("./pfn/"),
            licensetype: "opensource".to_string(),
            min_memory: 512,
            min_million_disk: 50,
            min_million_memory: 20,
            resource_check: false,
            check_point: true,
            build_multi_thread: true,
            log_mode: "conf/slog.properties".to_string(),
            querylog_mode: true,
            querylog_path: PathBuf::from("logs/endpoint/"),
            querylog_days: 30,
            accesslog_mode: true,
            accesslog_path: PathBuf::from("logs/ipaccess/"),
            accesslog_days: 30,
            backup_path: PathBuf::from("./backups/"),
            max_backups: 3,
            upload_path: PathBuf::from("./upload/"),
            upload_max_body_size: 1_073_741_824,
            upload_allow_extensions: split_list("nt|ttl|n3|rdf|txt"),
            upload_allow_compress_packages: split_list("zip"),
            source: None,
            warnings: Vec::new(),
        }
    }
}

/// Every key [`GStoreConfig`] understands, by section (upstream's keys first,
/// then the Rust-only ones).
pub const KNOWN_KEYS: &[(&str, &[&str])] = &[
    (
        "server",
        &[
            "port",
            "tcp_port",
            "thread_num",
            "max_database_num",
            "max_user_num",
            "deamon",
            "max_output_size",
            "ip_allow_path",
            "ip_deny_path",
            "bind_addr",
            "keep_alive_timeout",
            "keep_alive_max_requests",
            "max_body_size",
            "cors_allow_origin",
            "shutdown_grace",
        ],
    ),
    (
        "system",
        &[
            "db_home",
            "db_suffix",
            "root_username",
            "root_password",
            "system_username",
            "pfn_base_path",
            "licensetype",
            "min_memory",
            "min_million_disk",
            "min_million_memory",
            "resource_check",
            "check_point",
            "build_multi_thread",
            // upstream keeps these commented out in [system]
            "version",
            "product_name",
            "website",
        ],
    ),
    (
        "log",
        &[
            "log_mode",
            "querylog_mode",
            "querylog_path",
            "querylog_days",
            "accesslog_mode",
            "accesslog_path",
            "accesslog_days",
        ],
    ),
    ("backup", &["backup_path", "max_backups"]),
    (
        "upload",
        &[
            "upload_path",
            "upload_max_body_size",
            "upload_allow_extensions",
            "upload_allow_compress_packages",
        ],
    ),
];

/// Why a single setting was rejected (the setting keeps its previous value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl GStoreConfig {
    /// Layer the INI `text` over the defaults. Never fails: unknown keys and
    /// unparsable values are recorded in [`warnings`](Self::warnings).
    pub fn from_ini_str(text: &str) -> GStoreConfig {
        let mut cfg = GStoreConfig::default();
        cfg.merge_ini(&Ini::parse(text));
        cfg
    }

    /// Load `path` over the defaults. Only an unreadable file is an error.
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<GStoreConfig> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        let mut cfg = GStoreConfig::from_ini_str(&text);
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Load `path` if given (an unreadable explicit file is an error), else
    /// [`DEFAULT_CONFIG_PATH`] if it exists, else the built-in defaults.
    pub fn load_or_default(path: Option<&Path>) -> std::io::Result<GStoreConfig> {
        match path {
            Some(p) => GStoreConfig::load(p),
            None if Path::new(DEFAULT_CONFIG_PATH).is_file() => {
                GStoreConfig::load(DEFAULT_CONFIG_PATH)
            }
            None => Ok(GStoreConfig::default()),
        }
    }

    /// Apply every entry of `ini` (later sources override earlier ones, so
    /// callers can merge several files), collecting warnings.
    pub fn merge_ini(&mut self, ini: &Ini) {
        self.warnings.extend(ini.warnings().iter().cloned());
        for e in ini.entries() {
            if let Err(err) = self.set(&e.section, &e.key, &e.value) {
                self.warnings.push(format!("line {}: {err}", e.line));
            }
        }
    }

    /// Set one key, as if it had been read from `[section] key=value`.
    /// An unknown key or an invalid value is an error and changes nothing.
    pub fn set(&mut self, section: &str, key: &str, value: &str) -> Result<(), ConfigError> {
        let known = KNOWN_KEYS
            .iter()
            .find(|(s, _)| *s == section)
            .is_some_and(|(_, keys)| keys.contains(&key));
        if !known {
            let home = KNOWN_KEYS
                .iter()
                .find(|(_, keys)| keys.contains(&key))
                .map(|(s, _)| *s);
            return Err(ConfigError(match home {
                Some(s) => format!("key '{key}' belongs in [{s}], not [{section}]; ignored"),
                None => format!("unknown key '{key}' in [{section}]; ignored"),
            }));
        }
        let v = value.trim();
        match key {
            "port" => self.port = parse_num(key, v)?,
            "tcp_port" => self.tcp_port = parse_num(key, v)?,
            "thread_num" => {
                let n: usize = parse_num(key, v)?;
                if n == 0 {
                    return Err(ConfigError("thread_num must be at least 1; ignored".into()));
                }
                self.thread_num = n;
            }
            "max_database_num" => self.max_database_num = parse_num(key, v)?,
            "max_user_num" => self.max_user_num = parse_num(key, v)?,
            "deamon" => self.deamon = parse_bool(key, v)?,
            "max_output_size" => self.max_output_size = parse_num(key, v)?,
            "ip_allow_path" => self.ip_allow_path = opt_path(v),
            "ip_deny_path" => self.ip_deny_path = opt_path(v),
            "bind_addr" => self.bind_addr = v.to_string(),
            "keep_alive_timeout" => self.keep_alive_timeout = parse_num(key, v)?,
            "keep_alive_max_requests" => self.keep_alive_max_requests = parse_num(key, v)?,
            "max_body_size" => self.max_body_size = Some(parse_num(key, v)?),
            "cors_allow_origin" => {
                self.cors_allow_origin = Some(v.to_string()).filter(|s| !s.is_empty())
            }
            "shutdown_grace" => self.shutdown_grace = parse_num(key, v)?,
            "db_home" => self.db_home = PathBuf::from(v),
            "db_suffix" => {
                if v != crate::DB_SUFFIX {
                    return Err(ConfigError(format!(
                        "db_suffix '{v}' is not supported (databases always use '{}'); ignored",
                        crate::DB_SUFFIX
                    )));
                }
                self.db_suffix = v.to_string();
            }
            "root_username" => self.root_username = v.to_string(),
            "root_password" => self.root_password = v.to_string(),
            "system_username" => self.system_username = v.to_string(),
            "pfn_base_path" => self.pfn_base_path = PathBuf::from(v),
            "licensetype" => self.licensetype = v.to_string(),
            "min_memory" => self.min_memory = parse_num(key, v)?,
            "min_million_disk" => self.min_million_disk = parse_num(key, v)?,
            "min_million_memory" => self.min_million_memory = parse_num(key, v)?,
            "resource_check" => self.resource_check = parse_bool(key, v)?,
            "check_point" => self.check_point = parse_bool(key, v)?,
            "build_multi_thread" => self.build_multi_thread = parse_bool(key, v)?,
            // Informational keys upstream keeps (commented out) in [system].
            "version" | "product_name" | "website" => {}
            "log_mode" => self.log_mode = v.to_string(),
            "querylog_mode" => self.querylog_mode = parse_bool(key, v)?,
            "querylog_path" => self.querylog_path = PathBuf::from(v),
            "querylog_days" => self.querylog_days = parse_num(key, v)?,
            "accesslog_mode" => self.accesslog_mode = parse_bool(key, v)?,
            "accesslog_path" => self.accesslog_path = PathBuf::from(v),
            "accesslog_days" => self.accesslog_days = parse_num(key, v)?,
            "backup_path" => self.backup_path = PathBuf::from(v),
            "max_backups" => self.max_backups = parse_num(key, v)?,
            "upload_path" => self.upload_path = PathBuf::from(v),
            "upload_max_body_size" => self.upload_max_body_size = parse_num(key, v)?,
            "upload_allow_extensions" => self.upload_allow_extensions = split_list(v),
            "upload_allow_compress_packages" => {
                self.upload_allow_compress_packages = split_list(v)
            }
            _ => unreachable!("every KNOWN_KEYS entry is handled"),
        }
        Ok(())
    }

    /// The largest request body the HTTP layer accepts: `max_body_size` when
    /// set, else `upload_max_body_size` plus 1 MiB (so a maximal upload still
    /// fits with its multipart framing).
    pub fn request_body_limit(&self) -> usize {
        let limit = self
            .max_body_size
            .unwrap_or_else(|| self.upload_max_body_size.saturating_add(1 << 20));
        usize::try_from(limit).unwrap_or(usize::MAX)
    }

    /// The keep-alive idle timeout (`None` when keep-alive is disabled).
    pub fn keep_alive(&self) -> Option<std::time::Duration> {
        (self.keep_alive_timeout > 0 && self.keep_alive_max_requests > 1)
            .then(|| std::time::Duration::from_secs(self.keep_alive_timeout))
    }

    /// The pid file `gserver` maintains for this database home.
    pub fn pid_file(&self) -> PathBuf {
        self.db_home.join(PID_FILE_NAME)
    }

    /// The log file a background `gserver` writes to.
    pub fn server_log_file(&self) -> PathBuf {
        self.db_home.join(SERVER_LOG_NAME)
    }

    /// Whether `ext` (without the dot, case-insensitive) may be uploaded:
    /// an allowed extension or an allowed archive type.
    pub fn upload_extension_allowed(&self, ext: &str) -> bool {
        let ext = ext.trim_start_matches('.');
        self.upload_allow_extensions
            .iter()
            .chain(&self.upload_allow_compress_packages)
            .any(|e| e.eq_ignore_ascii_case(ext))
    }

    /// The effective settings as `(section, key, value)` rows, in
    /// [`KNOWN_KEYS`] order (secrets masked) — for logging what a server runs with.
    pub fn summary(&self) -> Vec<(&'static str, &'static str, String)> {
        let p = |p: &Path| p.display().to_string();
        let op = |p: &Option<PathBuf>| p.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
        let onoff = |b: bool| if b { "on" } else { "off" }.to_string();
        let mut rows = Vec::new();
        for (section, keys) in KNOWN_KEYS {
            for key in keys.iter() {
                let v = match *key {
                    "port" => self.port.to_string(),
                    "tcp_port" => self.tcp_port.to_string(),
                    "thread_num" => self.thread_num.to_string(),
                    "max_database_num" => self.max_database_num.to_string(),
                    "max_user_num" => self.max_user_num.to_string(),
                    "deamon" => onoff(self.deamon),
                    "max_output_size" => self.max_output_size.to_string(),
                    "ip_allow_path" => op(&self.ip_allow_path),
                    "ip_deny_path" => op(&self.ip_deny_path),
                    "bind_addr" => self.bind_addr.clone(),
                    "keep_alive_timeout" => self.keep_alive_timeout.to_string(),
                    "keep_alive_max_requests" => self.keep_alive_max_requests.to_string(),
                    "max_body_size" => self.request_body_limit().to_string(),
                    "cors_allow_origin" => self.cors_allow_origin.clone().unwrap_or_default(),
                    "shutdown_grace" => self.shutdown_grace.to_string(),
                    "db_home" => p(&self.db_home),
                    "db_suffix" => self.db_suffix.clone(),
                    "root_username" => self.root_username.clone(),
                    "root_password" => "******".to_string(),
                    "system_username" => self.system_username.clone(),
                    "pfn_base_path" => p(&self.pfn_base_path),
                    "licensetype" => self.licensetype.clone(),
                    "min_memory" => self.min_memory.to_string(),
                    "min_million_disk" => self.min_million_disk.to_string(),
                    "min_million_memory" => self.min_million_memory.to_string(),
                    "resource_check" => onoff(self.resource_check),
                    "check_point" => onoff(self.check_point),
                    "build_multi_thread" => onoff(self.build_multi_thread),
                    "log_mode" => self.log_mode.clone(),
                    "querylog_mode" => (self.querylog_mode as u8).to_string(),
                    "querylog_path" => p(&self.querylog_path),
                    "querylog_days" => self.querylog_days.to_string(),
                    "accesslog_mode" => (self.accesslog_mode as u8).to_string(),
                    "accesslog_path" => p(&self.accesslog_path),
                    "accesslog_days" => self.accesslog_days.to_string(),
                    "backup_path" => p(&self.backup_path),
                    "max_backups" => self.max_backups.to_string(),
                    "upload_path" => p(&self.upload_path),
                    "upload_max_body_size" => self.upload_max_body_size.to_string(),
                    "upload_allow_extensions" => self.upload_allow_extensions.join("|"),
                    "upload_allow_compress_packages" => {
                        self.upload_allow_compress_packages.join("|")
                    }
                    _ => continue, // informational keys
                };
                rows.push((*section, *key, v));
            }
        }
        rows
    }
}

fn parse_num<T: std::str::FromStr>(key: &str, v: &str) -> Result<T, ConfigError> {
    v.parse()
        .map_err(|_| ConfigError(format!("invalid value '{v}' for '{key}'; the default is kept")))
}

/// Upstream switches are `on`/`off` (and `1`/`0` for the log modes); the
/// usual boolean spellings are accepted too.
fn parse_bool(key: &str, v: &str) -> Result<bool, ConfigError> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        _ => Err(ConfigError(format!(
            "invalid switch '{v}' for '{key}' (use on/off); the default is kept"
        ))),
    }
}

fn opt_path(v: &str) -> Option<PathBuf> {
    (!v.is_empty()).then(|| PathBuf::from(v))
}

/// Split upstream's `a|b|c` lists, dropping blanks and leading dots.
fn split_list(v: &str) -> Vec<String> {
    v.split('|')
        .map(|s| s.trim().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream's `conf/conf.ini`, verbatim.
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
    fn ini_sections_comments_quotes_and_trimming() {
        let ini = Ini::parse(
            "\u{feff}; leading comment\n\
             [ a ]\n\
             k1 = v1 \n\
             \t# indented comment\n\
             k2=\"quoted value\"\n\
             k3='single'\n\
             k4 = a=b=c\n\
             k5 = # not a comment\n\
             empty =\n\
             [b]\n\
             k1=other\n",
        );
        assert_eq!(ini.sections(), ["a", "b"]);
        assert_eq!(ini.get("a", "k1"), Some("v1"));
        assert_eq!(ini.get("a", "k2"), Some("quoted value"));
        assert_eq!(ini.get("a", "k3"), Some("single"));
        assert_eq!(ini.get("a", "k4"), Some("a=b=c"), "split at the first '='");
        assert_eq!(ini.get("a", "k5"), Some("# not a comment"));
        assert_eq!(ini.get("a", "empty"), None, "an empty value is unset");
        assert_eq!(ini.get("b", "k1"), Some("other"), "sections are separate");
        assert_eq!(ini.get("b", "k2"), None);
        assert!(ini.warnings().is_empty(), "{:?}", ini.warnings());
    }

    #[test]
    fn ini_problems_are_warnings() {
        let ini = Ini::parse("orphan=1\n[s]\nno equals sign\nk=1\nk=2\n=v\n[broken\n");
        assert_eq!(ini.get("s", "k"), Some("1"), "the first duplicate wins");
        let w = ini.warnings().join("\n");
        assert!(w.contains("line 1") && w.contains("outside any [section]"), "{w}");
        assert!(w.contains("line 3") && w.contains("without '='"), "{w}");
        assert!(w.contains("line 5") && w.contains("duplicate key 'k'"), "{w}");
        assert!(w.contains("line 6") && w.contains("empty key"), "{w}");
        assert!(w.contains("line 7") && w.contains("malformed section"), "{w}");
    }

    #[test]
    fn upstream_conf_ini_parses_without_warnings() {
        let cfg = GStoreConfig::from_ini_str(UPSTREAM_CONF);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.tcp_port, 9100);
        assert_eq!(cfg.thread_num, 30);
        assert_eq!(cfg.max_database_num, 100);
        assert_eq!(cfg.max_user_num, 1000);
        assert!(!cfg.deamon);
        assert_eq!(cfg.max_output_size, 100_000, "commented out: default");
        assert_eq!(cfg.ip_allow_path, None, "empty: unset");
        assert_eq!(cfg.ip_deny_path, Some(PathBuf::from("./conf/ipDeny.config")));
        // The file's values override the code defaults…
        assert_eq!(cfg.min_memory, 1024);
        assert_eq!(cfg.min_million_disk, 200);
        assert_eq!(cfg.min_million_memory, 200);
        assert!(!cfg.build_multi_thread);
        // …and commented-out keys keep them.
        assert_eq!(cfg.db_home, PathBuf::from("./dbhome/"));
        assert_eq!(cfg.db_suffix, ".db");
        assert!(!cfg.resource_check);
        assert_eq!(cfg.root_username, "root");
        assert_eq!(cfg.root_password, "123456");
        assert_eq!(cfg.system_username, "system");
        assert!(cfg.check_point);
        assert_eq!(cfg.log_mode, "./conf/slog.properties");
        assert!(cfg.querylog_mode && cfg.accesslog_mode);
        assert_eq!(cfg.querylog_path, PathBuf::from("logs/endpoint/"));
        assert_eq!(cfg.accesslog_path, PathBuf::from("logs/ipaccess/"));
        assert_eq!((cfg.querylog_days, cfg.accesslog_days), (30, 30));
        assert_eq!(cfg.backup_path, PathBuf::from("./backups/"));
        assert_eq!(cfg.max_backups, 3);
        assert_eq!(cfg.upload_path, PathBuf::from("./upload/"));
        assert_eq!(cfg.upload_max_body_size, 1_073_741_824);
        assert_eq!(cfg.upload_allow_extensions, ["nt", "ttl", "n3", "rdf", "txt"]);
        assert_eq!(cfg.upload_allow_compress_packages, ["zip"]);
        assert!(cfg.upload_extension_allowed("TTL") && cfg.upload_extension_allowed(".zip"));
        assert!(!cfg.upload_extension_allowed("exe"));
    }

    #[test]
    fn shipped_example_parses_without_warnings() {
        let example = include_str!("../conf/conf.ini.example");
        let c = GStoreConfig::from_ini_str(example);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        // Every Rust-only key the example documents (commented out) is known.
        for line in example.lines() {
            let l = line.trim_start_matches('#').trim();
            if let Some((k, _)) = l.split_once('=') {
                let k = k.trim();
                if !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                    assert!(
                        KNOWN_KEYS.iter().any(|(_, keys)| keys.contains(&k)),
                        "example documents unknown key '{k}'"
                    );
                }
            }
        }
        assert_eq!(c.port, 9000);
        assert_eq!(c.ip_deny_path, Some(PathBuf::from("./conf/ipDeny.config")));
    }

    #[test]
    fn every_key_is_settable() {
        let text = "[server]\nport=9123\ntcp_port=9124\nthread_num=4\nmax_database_num=7\n\
            max_user_num=8\ndeamon=on\nmax_output_size=50\nip_allow_path=allow.cfg\n\
            ip_deny_path=deny.cfg\nbind_addr=0.0.0.0\nkeep_alive_timeout=3\n\
            keep_alive_max_requests=9\nmax_body_size=4096\ncors_allow_origin=*\nshutdown_grace=2\n\
            [system]\ndb_home=/tmp/h\ndb_suffix=.db\nroot_username=admin\nroot_password=pw\n\
            system_username=sys\npfn_base_path=p/\nlicensetype=x\nmin_memory=1\n\
            min_million_disk=2\nmin_million_memory=3\nresource_check=on\ncheck_point=off\n\
            build_multi_thread=on\nversion=1.4\n\
            [log]\nlog_mode=l\nquerylog_mode=0\nquerylog_path=q/\nquerylog_days=5\n\
            accesslog_mode=0\naccesslog_path=a/\naccesslog_days=6\n\
            [backup]\nbackup_path=b/\nmax_backups=9\n\
            [upload]\nupload_path=u/\nupload_max_body_size=10\n\
            upload_allow_extensions=nt|.ttl\nupload_allow_compress_packages=zip|tar\n";
        let c = GStoreConfig::from_ini_str(text);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!((c.port, c.tcp_port, c.thread_num), (9123, 9124, 4));
        assert_eq!((c.max_database_num, c.max_user_num, c.max_output_size), (7, 8, 50));
        assert!(c.deamon);
        assert_eq!(c.ip_allow_path, Some(PathBuf::from("allow.cfg")));
        assert_eq!(c.ip_deny_path, Some(PathBuf::from("deny.cfg")));
        assert_eq!(c.bind_addr, "0.0.0.0");
        assert_eq!((c.keep_alive_timeout, c.keep_alive_max_requests), (3, 9));
        assert_eq!(c.max_body_size, Some(4096));
        assert_eq!(c.request_body_limit(), 4096);
        assert_eq!(c.cors_allow_origin.as_deref(), Some("*"));
        assert_eq!(c.shutdown_grace, 2);
        assert_eq!(c.db_home, PathBuf::from("/tmp/h"));
        assert_eq!(c.pid_file(), PathBuf::from("/tmp/h").join(PID_FILE_NAME));
        assert_eq!((c.root_username.as_str(), c.root_password.as_str()), ("admin", "pw"));
        assert_eq!(c.system_username, "sys");
        assert_eq!(c.pfn_base_path, PathBuf::from("p/"));
        assert_eq!(c.licensetype, "x");
        assert_eq!((c.min_memory, c.min_million_disk, c.min_million_memory), (1, 2, 3));
        assert!(c.resource_check && !c.check_point && c.build_multi_thread);
        assert_eq!(c.log_mode, "l");
        assert!(!c.querylog_mode && !c.accesslog_mode);
        assert_eq!((c.querylog_days, c.accesslog_days), (5, 6));
        assert_eq!(c.querylog_path, PathBuf::from("q/"));
        assert_eq!(c.accesslog_path, PathBuf::from("a/"));
        assert_eq!((c.backup_path.clone(), c.max_backups), (PathBuf::from("b/"), 9));
        assert_eq!(c.upload_path, PathBuf::from("u/"));
        assert_eq!(c.upload_max_body_size, 10);
        assert_eq!(c.upload_allow_extensions, ["nt", "ttl"]);
        assert_eq!(c.upload_allow_compress_packages, ["zip", "tar"]);
        // Every known key appears in the summary, with the password masked.
        let rows = c.summary();
        assert!(rows.iter().any(|(s, k, v)| *s == "server" && *k == "port" && v == "9123"));
        assert!(rows.iter().any(|(_, k, v)| *k == "root_password" && v == "******"));
        assert!(!rows.iter().any(|(_, _, v)| v == "pw"));
    }

    #[test]
    fn unknown_keys_and_bad_values_warn_and_keep_defaults() {
        let c = GStoreConfig::from_ini_str(
            "[server]\nport=notanumber\nthread_num=0\nbogus=1\ndeamon=maybe\n\
             [system]\nport=1234\ndb_suffix=.gdb\n[nosuch]\nkey=v\n",
        );
        let d = GStoreConfig::default();
        assert_eq!(c.port, d.port);
        assert_eq!(c.thread_num, d.thread_num);
        assert_eq!(c.deamon, d.deamon);
        assert_eq!(c.db_suffix, ".db");
        let w = c.warnings.join("\n");
        assert!(w.contains("invalid value 'notanumber' for 'port'"), "{w}");
        assert!(w.contains("thread_num must be at least 1"), "{w}");
        assert!(w.contains("unknown key 'bogus' in [server]"), "{w}");
        assert!(w.contains("invalid switch 'maybe'"), "{w}");
        assert!(w.contains("'port' belongs in [server], not [system]"), "{w}");
        assert!(w.contains("db_suffix '.gdb' is not supported"), "{w}");
        assert!(w.contains("unknown key 'key' in [nosuch]"), "{w}");
    }

    #[test]
    fn defaults_match_upstream_util_configure() {
        let d = GStoreConfig::default();
        assert_eq!(d.port, 9000);
        assert_eq!(d.thread_num, 30);
        assert_eq!(d.max_output_size, 100_000);
        assert_eq!(d.ip_deny_path, Some(PathBuf::from("conf/ipDeny.config")));
        assert_eq!(d.min_memory, 512);
        assert!(d.build_multi_thread);
        assert_eq!(d.db_home, PathBuf::from("./dbhome/"));
        assert_eq!(d.pfn_base_path, PathBuf::from("./pfn/"));
        assert_eq!(d.licensetype, "opensource");
        assert_eq!(d.request_body_limit(), 1_073_741_824 + (1 << 20));
        assert_eq!(d.keep_alive(), Some(std::time::Duration::from_secs(15)));
        let off = GStoreConfig::from_ini_str("[server]\nkeep_alive_timeout=0\n");
        assert_eq!(off.keep_alive(), None);
    }

    #[test]
    fn precedence_file_over_default_then_caller_over_file() {
        let dir = std::env::temp_dir().join(format!("gstore_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conf.ini");
        std::fs::write(&path, "[server]\nport=9555\nthread_num=3\n").unwrap();
        let mut c = GStoreConfig::load(&path).unwrap();
        assert_eq!(c.source.as_deref(), Some(path.as_path()));
        assert_eq!((c.port, c.thread_num), (9555, 3), "file over default");
        assert_eq!(c.max_output_size, 100_000, "default where the file is silent");
        c.port = 9600; // a command-line flag
        assert_eq!(c.port, 9600);
        // A later file layered on top overrides the earlier one.
        c.merge_ini(&Ini::parse("[server]\nthread_num=5\n"));
        assert_eq!(c.thread_num, 5);
        assert!(GStoreConfig::load(dir.join("missing.ini")).is_err());
        assert!(GStoreConfig::load_or_default(Some(&dir.join("missing.ini"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
