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
//!   `std::map`). Lines starting with `#` or `;` are comments. Upstream has
//!   no comment syntax as such — a commented-out `# db_home=…` simply becomes
//!   a key named `#db_home` that nothing looks up — so treating those lines
//!   as comments is equivalent for every real file.
//!
//!   **Inline comments** (a gstore-rust extension; review bulkcli#9): the
//!   rest of an unquoted value is a comment from a `#` or `;` that follows
//!   whitespace, comes after some of the value, and is followed by
//!   whitespace or the end of the line — `port = 9000  # the HTTP port`
//!   reads `9000`. A marker that is not a word of its own stays part of the
//!   value, as upstream reads it: `Tr0ub4dor #3`, `/srv/a ;b`, `#x`. Inside
//!   double quotes nothing is a comment (`"a # b"` reads `a # b`). The value
//!   of `root_password` is never cut: a password may contain ` # `. Every
//!   inline comment that cuts a value is reported as a
//!   [warning](GStoreConfig::warnings) (upstream gStore reads it as part of
//!   the value), so a value copied from an upstream file is never shortened
//!   silently.
//!
//!   Earlier gstore-rust versions cut an unquoted value at *any* whitespace
//!   followed by `#` or `;` (after some of the value), glued marker or not:
//!   `db_home=/srv/gstore #primary` read `/srv/gstore`. Wherever that rule
//!   would cut and this one keeps more of the value, a warning says so
//!   (review of bulkcli#9's fix: `db_home`, `backup_path` or `ip_deny_path`
//!   changed on upgrade without a word) — for `root_password` too, without
//!   quoting it. Quote the whole value to keep the text in it without the
//!   warning, or put whitespace after the marker (or the comment on a line
//!   of its own) to keep it a comment.
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
//! **The environment overrides `conf.ini`** for the storage defaults a
//! process installs ([`GStoreConfig::install_storage_defaults`],
//! [`set_default_open_options`](crate::db::set_default_open_options)):
//! `GSTORE_DISK_QUERY_MODE` and `GSTORE_DISK_CACHE_MB` over `[storage]
//! disk_query_mode` / `disk_cache_mb`, `GSTORE_VSTREE_ON_OPEN` over
//! `vstree_on_open`, `GSTORE_ID_RECLAIM` over `[system] id_reclaim`,
//! `GSTORE_ROCKS_BLOCK_CACHE_MB` over `rocksdb_block_cache_mb` — applied
//! whenever a database is opened, so a variable left in a service's
//! environment wins over the file (review design#11: documented, not
//! changed; the tools without a `conf.ini` — `gadd`, `gsub`, `gquery`, … —
//! follow the variables only). `gserver` logs the effective values at
//! startup. `GSTORE_VS_POLICY` has no `conf.ini` key.
//!
//! **Accepted but not acted on**: `tcp_port` (upstream's socket API, not
//! built in gStore 1.4), `min_memory`, `min_million_disk`,
//! `min_million_memory` and `resource_check` (gstore-rust performs no
//! resource check: `resource_check=on` is a [warning](GStoreConfig::warnings)),
//! `db_suffix` (databases always end with `.db`; `gserver` warns about
//! another value) and `root_username` (the root user is `root`; `gserver`
//! warns).
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
//!   `keep_alive_max`, `queue_size`, `cors`, the slow-client limits
//!   `min_body_rate` and `max_conns_per_ip` — and the browser-facing secure
//!   defaults with their upstream-compat switches (`cors_allow_origin`,
//!   `allowed_hosts`, `allow_get_update`, `trust_proxy_headers`,
//!   `allow_default_root_password`; see
//!   [`GStoreConfig::upstream_compat_warnings`]);
//! * `[log]`: the diagnostic log that upstream configures in a log4cplus
//!   properties file — `log_level`, `log_file`, `log_days`, `log_stderr`
//!   (see [`crate::logging`]);
//! * `[upload]`: the request body caps before authentication and in all
//!   (`preauth_max_body_size`, `request_body_budget`; see
//!   [`crate::preauth`]), the limits a zip package must stay within —
//!   `unzip_max_entries`, `unzip_max_size` (see [`crate::zipio`]) — the
//!   parser limits `rdf_max_token_size` (one token of the streaming RDF
//!   readers, [`crate::parser::stream::default_max_token_bytes`]) and
//!   `sparql_max_size` (one SPARQL request,
//!   [`crate::parser::sparql::lexer::max_request_bytes`]), and the
//!   download and file-retention policy of the `/api` layer
//!   (`anonymous_download`, an upstream-compat switch;
//!   `download_max_concurrent`, `file_retention_hours`,
//!   `upload_user_quota`);
//! * `[query]`: how every served database evaluates queries
//!   ([`GStoreConfig::query_settings`], applied to each database the servers
//!   open, build, create or restore) — `integer_division`,
//!   `query_cache_size`, `early_termination`, `topk_bounded_heap`, `topk_dpb`
//!   and `gfun_max_output_bytes`. Upstream has no key for any of them (its
//!   query cache budget, 100 MB, is a constant in `QueryCache.h`);
//! * `[pfn]`: the sandbox limits of PFN user functions (applied the same
//!   way, `crate::pfn`) — `pfn_fuel_per_call`, `pfn_max_memory_pages`,
//!   `pfn_max_module_bytes`, `pfn_max_functions` and `pfn_fuel_per_query`
//!   (the fuel budget of all PFN calls of one query, review ext#1).
//!   Upstream's only PFN key is `[system]`'s `pfn_base_path`, the directory
//!   the functions live in — read here too.
//!
//! They are documented on the fields below and in the example file.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::db::{
    DiskQueryMode, IdReclaim, OpenOptions, QuerySettings, StorageKind, DEFAULT_QUERY_CACHE_BYTES,
};
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
    /// Inline comments cut from values: (1-based line number, key, the
    /// comment as written).
    inline_comments: Vec<(usize, String, String)>,
    /// Values of [`VERBATIM_KEYS`] kept whole although they hold what
    /// earlier gstore-rust versions read as the start of an inline comment
    /// ([`legacy_comment_start`]): (1-based line number, key).
    verbatim_markers: Vec<(usize, String)>,
    /// Text earlier gstore-rust versions cut from a value as a comment
    /// ([`legacy_comment_start`]) that is part of the value now — a marker
    /// glued to the next word: (1-based line number, key, the text as
    /// written). [`VERBATIM_KEYS`] are in `verbatim_markers` instead.
    legacy_cuts: Vec<(usize, String, String)>,
}

/// Keys whose value is taken whole, an inline-comment marker included (see
/// the module docs): a password may contain ` # `.
const VERBATIM_KEYS: &[&str] = &["root_password"];

impl Ini {
    /// Parse INI text. Never fails: lines that make no sense are recorded in
    /// [`malformed`](Self::malformed) and otherwise ignored.
    pub fn parse(text: &str) -> Ini {
        let mut ini = Ini::default();
        let mut section = String::new();
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            // A UTF-8 BOM on the first line is not part of the content.
            let raw = if idx == 0 {
                raw.trim_start_matches('\u{feff}')
            } else {
                raw
            };
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
            let (value, comment) = if VERBATIM_KEYS.contains(&key) {
                if legacy_comment_start(v).is_some() {
                    ini.verbatim_markers.push((line_no, key.to_string()));
                }
                (clean_value(v), None)
            } else {
                // Where the old rule cut, the new rule cuts there too or
                // later (a word-like marker is also a marker after
                // whitespace); text between the two cuts is now kept.
                let cut = comment_start(v);
                if let Some(old) = legacy_comment_start(v).filter(|&old| cut != Some(old)) {
                    let kept = v[old..cut.unwrap_or(v.len())].trim_end();
                    ini.legacy_cuts
                        .push((line_no, key.to_string(), kept.to_string()));
                }
                split_comment(v)
            };
            if let Some(c) = comment {
                ini.inline_comments
                    .push((line_no, key.to_string(), c.to_string()));
            }
            ini.entries.push(IniEntry {
                section: section.clone(),
                key: key.to_string(),
                value,
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

    /// The inline comments cut from values (see the module docs), as `(line
    /// number, key, comment)`.
    pub fn inline_comments(&self) -> &[(usize, String, String)] {
        &self.inline_comments
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

/// Split a raw value into the value and its inline comment, if any (see the
/// module docs for the rule), and normalize the value ([`clean_value`]).
fn split_comment(raw: &str) -> (String, Option<&str>) {
    match comment_start(raw) {
        Some(at) => (clean_value(&raw[..at]), Some(raw[at..].trim_end())),
        None => (clean_value(raw), None),
    }
}

/// Where the inline comment of `raw` starts: a `#` or `;` outside double
/// quotes that follows whitespace, comes after some of the value, and is
/// followed by whitespace or the end — a word of its own. Upstream has no
/// inline comments, so a marker glued to more text (`Tr0ub4dor #3`) stays
/// part of the value, as does one at the start (`key = #x`).
fn comment_start(raw: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut prev_ws = false;
    let mut seen_content = false;
    let mut chars = raw.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' | ';'
                if !in_quotes
                    && prev_ws
                    && seen_content
                    && chars.peek().is_none_or(|(_, n)| n.is_whitespace()) =>
            {
                return Some(i);
            }
            _ => {}
        }
        prev_ws = c.is_whitespace();
        seen_content |= !prev_ws;
    }
    None
}

/// Where earlier gstore-rust versions cut `raw` as an inline comment: at the
/// first `#` or `;` outside double quotes that follows whitespace and comes
/// after some of the value, whatever follows it. Only used to warn about
/// values that rule cut and [`comment_start`] keeps (see the module docs).
fn legacy_comment_start(raw: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut prev_ws = false;
    let mut seen_content = false;
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' | ';' if !in_quotes && prev_ws && seen_content => return Some(i),
            _ => {}
        }
        prev_ws = c.is_whitespace();
        seen_content |= !prev_ws;
    }
    None
}

/// Normalize a value (its inline comment already removed): trim, and remove
/// every `"` (upstream behaviour). A value wrapped in single quotes loses
/// that pair too.
fn clean_value(raw: &str) -> String {
    let v = raw.trim().replace('"', "");
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
    ("server", "query_timeout"),
    ("server", "max_result_bytes"),
    ("server", "ip_allow_path"),
    ("server", "ip_deny_path"),
    ("server", "upstream_login_lockout"),
    ("server", "login_lockout_failures"),
    ("server", "login_lockout_max_seconds"),
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
    ("system", "txn_idle_timeout"),
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
    ("upload", "preauth_max_body_size"),
    ("upload", "request_body_budget"),
    ("upload", "upload_allow_extensions"),
    ("upload", "upload_allow_compress_packages"),
    // Rust-only extensions.
    ("upload", "unzip_max_entries"),
    ("upload", "unzip_max_size"),
    ("upload", "rdf_max_token_size"),
    ("upload", "sparql_max_size"),
    ("upload", "anonymous_download"),
    ("upload", "download_max_concurrent"),
    ("upload", "file_retention_hours"),
    ("upload", "upload_user_quota"),
    ("system", "build_memory_mb"),
    ("system", "build_threads"),
    ("system", "build_tmp_dir"),
    ("system", "build_fill_factor"),
    ("server", "listen_addr"),
    ("server", "keep_alive_timeout"),
    ("server", "min_body_rate"),
    ("server", "max_conns_per_ip"),
    ("server", "keep_alive_max"),
    ("server", "queue_size"),
    ("server", "cors"),
    ("server", "cors_allow_origin"),
    ("server", "allowed_hosts"),
    ("server", "allow_get_update"),
    ("server", "trust_proxy_headers"),
    ("server", "allow_default_root_password"),
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
    ("pfn", "pfn_fuel_per_query"),
    ("storage", "disk_query_mode"),
    ("storage", "disk_cache_mb"),
    ("storage", "dict_cache_mb"),
    ("storage", "vstree_on_open"),
    ("storage", "default_layout"),
    ("storage", "rocksdb_block_cache_mb"),
    ("storage", "rocksdb_bloom"),
    ("storage", "rocksdb_max_open_files"),
    ("system", "id_reclaim"),
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

/// The default [`GStoreConfig::query_timeout`]: one hour.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(3600);

/// The default [`GStoreConfig::max_result_bytes`]: 1 GiB.
pub const DEFAULT_MAX_RESULT_BYTES: u64 = 1 << 30;

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
    /// *(Rust extension)* How long one SPARQL statement may run (default
    /// 1 h; `conf.ini`: seconds, fractions allowed, `0` = no limit). A
    /// statement still running then is stopped at its next cancellation
    /// check — releasing the database it holds, so a writer queued behind a
    /// long read is not blocked for good — and answered as timed out (REST
    /// `503`, `/api` `1005`).
    pub query_timeout: Duration,
    /// *(Rust extension)* The most bytes one serialized query answer may
    /// take (default 1 GiB; `0` = no limit). Serialization stops as soon as
    /// it passes the budget, and the request is answered "result too large"
    /// (REST `503`; a streamed `?stream=true` answer is cut off) — ask for
    /// less, or page with LIMIT / OFFSET.
    pub max_result_bytes: u64,
    /// IP allow-list file; when set it takes precedence over the deny list
    /// (default unset).
    pub ip_allow_path: Option<PathBuf>,
    /// IP deny-list file (default `conf/ipDeny.config`).
    pub ip_deny_path: Option<PathBuf>,
    /// *(Rust extension, upstream-compat switch)* Upstream's failed-login
    /// lockout: counted per address, every operation but `login` /
    /// `testConnect` (and REST `/login`) refused from an address with more
    /// than four failures until a successful login or a restart (default off:
    /// counted per account and address, every password check refused while
    /// locked, with an expiring back-off; see `crate::gapi::ipfilter`).
    pub upstream_login_lockout: bool,
    /// *(Rust extension)* Failed logins of one account from one address that
    /// are let through before the account is locked for that address
    /// (default `4`, upstream's count).
    pub login_lockout_failures: u32,
    /// *(Rust extension)* The longest lock of the default lockout, in seconds
    /// (default `900`): the first lock lasts one second, each further failure
    /// doubles it up to this.
    pub login_lockout_max_seconds: u64,
    /// *(Rust extension)* Address the HTTP listener binds (default
    /// `127.0.0.1`; upstream binds every interface — set `0.0.0.0` for that).
    pub listen_addr: String,
    /// *(Rust extension)* Keep-alive idle timeout in seconds; `0` turns
    /// keep-alive off (default `60`, upstream workflow's default).
    pub keep_alive_timeout: u64,
    /// *(Rust extension)* Minimum average rate, in bytes per second, at which
    /// a request body must arrive after a 30 s grace (the allowance grows
    /// with the bytes received, not with the declared `Content-Length`), so
    /// a client trickling it in cannot hold a worker thread; `0` turns this
    /// off (default `65536`, 64 KiB/s). See
    /// [`HttpOptions::min_body_rate`].
    pub min_body_rate: u64,
    /// *(Rust extension)* Most connections one client IP address may have
    /// open at once (waiting for a worker included); further ones are closed
    /// at once. Clients behind one NAT or proxy share an address. `0`: no
    /// limit (default `64`). See [`HttpOptions::max_conns_per_ip`].
    pub max_conns_per_ip: usize,
    /// *(Rust extension)* Maximum requests served on one connection (default `100`).
    pub keep_alive_max: usize,
    /// *(Rust extension)* Accepted connections that may wait for a free worker
    /// before the server stops accepting (default `128`).
    pub queue_size: usize,
    /// *(Rust extension)* CORS processing: answer `OPTIONS` preflights and
    /// send `Access-Control-Allow-Origin` — to the origins of
    /// [`cors_allow_origin`](Self::cors_allow_origin) only (default on).
    pub cors: bool,
    /// *(Rust extension)* The origins (`scheme://host[:port]`, separated by
    /// `|`, `,` or spaces) whose web pages may call the server cross-origin
    /// ([`HttpOptions::cors_allow_origin`]). Empty by default; `*` is
    /// upstream's `Access-Control-Allow-Origin: *` (an upstream-compat
    /// switch: gserver warns at startup).
    pub cors_allow_origin: Vec<String>,
    /// *(Rust extension)* Host names a loopback request may carry besides IP
    /// literals, `localhost` and names without a dot (the DNS-rebinding
    /// guard, [`HttpOptions::allowed_hosts`]); `*` turns the check off.
    /// Empty by default (the listen address, when it is a name, is added).
    pub allowed_hosts: Vec<String>,
    /// *(Rust extension, upstream-compat switch)* Let `GET /sparql` run an
    /// UPDATE and `/update` take a body of any type, as upstream does
    /// (default off, [`HttpOptions::allow_get_update`]).
    pub allow_get_update: bool,
    /// *(Rust extension, upstream-compat switch)* Treat a loopback peer as
    /// local even when it forwards `Forwarded` / `X-Forwarded-For` /
    /// `X-Real-IP` (default off, [`HttpOptions::trust_proxy_headers`]).
    pub trust_proxy_headers: bool,
    /// *(Rust extension, upstream-compat switch)* Let `gserver` start the
    /// API server although root still has the default password
    /// ([`DEFAULT_ROOT_PASSWORD`]) — on a loopback address only, and the
    /// password is then accepted from local clients only (default off:
    /// refused on every address).
    pub allow_default_root_password: bool,

    // ---- [system] -------------------------------------------------------
    /// Minimum free memory in MB for upstream's resource check (default
    /// `512`). Parsed for compatibility; no check uses it (see
    /// [`resource_check`](Self::resource_check)).
    pub min_memory: u64,
    /// Disk needed per million triples, MB (default `50`). Parsed only.
    pub min_million_disk: u64,
    /// Memory needed per million triples, MB (default `20`). Parsed only.
    pub min_million_memory: u64,
    /// Upstream: check resources before building/loading (default off).
    /// gstore-rust performs no such check: `on` is accepted with a
    /// [warning](Self::warnings) (review design#11).
    pub resource_check: bool,
    /// Directory holding the databases (default `./dbhome/`).
    pub db_home: PathBuf,
    /// Database directory suffix (default `.db`). Accepted for upstream
    /// compatibility; the databases always use `.db` (`gserver` warns about
    /// another value, and [`db_path`](Self::db_path) follows the servers).
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
    /// *(Rust extension)* Seconds after which an open HTTP transaction
    /// (`/begin`, `/api` `begin`) with no statement running is rolled back
    /// and logged as aborted — by the next `begin` on its database, the
    /// next request, or a background pass every `min(timeout / 4, 30 s)` —
    /// so a vanished client cannot block its database's writes and
    /// maintenance indefinitely (default `600`; `0` = never, upstream's
    /// behaviour). A transaction whose owner was deleted is rolled back
    /// too.
    pub txn_idle_timeout: u64,
    /// Build with multiple threads (default on). `off` makes the bulk
    /// builds use one thread, whatever `build_threads` says (a conflicting
    /// `build_threads` is reported as a [warning](Self::warnings); upstream's
    /// shipped `conf.ini` has `off`, the example shipped here leaves it at
    /// the default, review bulkcli#8).
    pub build_multi_thread: bool,
    /// *(Rust extension)* Memory budget of `gbuild --disk`'s bulk build in
    /// MiB: its sort buffers stay within it (spilling sorted runs to disk);
    /// the dictionary under construction is extra (default `1024`).
    pub build_memory_mb: u64,
    /// *(Rust extension)* Threads of the bulk build; `0` = the available
    /// parallelism (default `0`).
    pub build_threads: usize,
    /// *(Rust extension)* Directory for the bulk build's spilled runs
    /// (default unset: inside the database directory's staging area).
    pub build_tmp_dir: Option<PathBuf>,
    /// *(Rust extension)* B+ tree node fill of the bulk build in percent,
    /// 50–100 (default `90`; the slack takes later inserts without splits).
    pub build_fill_factor: u8,

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
    /// Maximum upload / request body size in bytes (default 256 MiB;
    /// upstream ships 1 GiB). The HTTP layer lets a request carry this much
    /// only when it is authenticated from its head (credentials in the URL
    /// query string or an `Authorization` header) on a route that carries
    /// payloads; every other request is held to
    /// [`preauth_max_body_size`](Self::preauth_max_body_size) (see
    /// [`crate::preauth`]).
    pub upload_max_body_size: u64,
    /// *(Rust extension)* The largest body a request may have before it is
    /// authenticated (default 64 MiB; at most
    /// [`upload_max_body_size`](Self::upload_max_body_size)) — every route.
    /// Upstream clients send their credentials inside the body (`/api` JSON,
    /// the `/file/upload` form), so this is also the most they can upload;
    /// set it to `upload_max_body_size` for upstream's limit.
    pub preauth_max_body_size: u64,
    /// *(Rust extension)* The request-body bytes all connections may hold in
    /// memory at once (default 512 MiB; at least
    /// [`upload_max_body_size`](Self::upload_max_body_size)): a request that
    /// would exceed it is answered `503`.
    pub request_body_budget: u64,
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
    /// *(Rust extension)* Bytes one token (a literal, IRI, name, number or
    /// language tag) of the streaming RDF readers — N-Triples, N-Quads,
    /// Turtle, TriG — and one N-Triples / N-Quads line may take: a longer
    /// one fails its statement, after the reader held only this much of it
    /// (default 64 MiB; `0` = no limit; review parser#5). Installed as the
    /// process default by [`GStoreConfig::install_storage_defaults`].
    pub rdf_max_token_size: u64,
    /// *(Rust extension)* Bytes a SPARQL query or update may have: a longer
    /// one is refused before it is tokenized (default 16 MiB; `0` = no
    /// limit; review parser#8). Installed as the process limit by
    /// [`GStoreConfig::install_storage_defaults`] (`gserver`, `gbuild`); a
    /// process that installs none has no limit.
    pub sparql_max_size: u64,
    /// *(Rust extension; upstream-compat switch)* Serve `/file/download`
    /// without credentials, as upstream does: the export directory (exports
    /// and `format=file` query results) to anyone. Default **off**: a
    /// download needs a valid user, who may read a file only when it holds
    /// the matching privilege on the database the file came from (review
    /// sec#4, gapi#2). `gserver` warns at startup while it is on.
    pub anonymous_download: bool,
    /// *(Rust extension)* `/file/download` responses streamed at once
    /// (each holds a worker while it lasts); more are answered `503`
    /// with `Retry-After`. `0` = no limit (default `8`).
    pub download_max_concurrent: usize,
    /// *(Rust extension)* Hours after which the files the server wrote for
    /// clients — uploads, exports, `format=file` query results — are
    /// removed (checked lazily on uploads, exports, downloads and
    /// asynchronous submissions, at most once a minute). Files an operator
    /// put there are never removed. `0` keeps them forever (default `168`,
    /// one week).
    pub file_retention_hours: u64,
    /// *(Rust extension)* Bytes of uploads each user may keep in the
    /// upload directory; an upload beyond it is refused (`1005`). `0` = no
    /// quota (the default).
    pub upload_user_quota: u64,

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
    /// *(Rust extension)* The most bytes of JSON the graph-function calls
    /// (`shortestPaths(…)`, `kHopEnumerate(…)`, …) of one query may produce
    /// together — every call, `GROUP BY` group and `GRAPH` block of the
    /// query charged against one budget — before the query fails (default
    /// [`DEFAULT_GFUN_MAX_OUTPUT_BYTES`], 64 MiB; review gap-gfun-fuzz#5).
    /// The text the PFN function calls of the query emit counts against it
    /// too.
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
    /// *(Rust extension)* The most fuel (instructions, host work and
    /// instantiation) the PFN calls of one query may use together; `0` =
    /// unlimited. Default [`crate::pfn::DEFAULT_FUEL_PER_QUERY`] (10^10;
    /// review ext#1).
    pub pfn_fuel_per_query: u64,

    // ---- [storage] (Rust extension) -------------------------------------
    /// *(Rust extension)* How an on-disk (`kvstore.kv`) database answers
    /// queries: `stream` (straight off the B+ trees, out-of-core dictionary)
    /// or `memory` (load the dictionary and triples at open). Default
    /// [`DEFAULT_DISK_QUERY_MODE`](crate::db::DEFAULT_DISK_QUERY_MODE); the
    /// `GSTORE_DISK_QUERY_MODE` environment variable overrides it.
    pub disk_query_mode: DiskQueryMode,
    /// *(Rust extension)* MiB of an on-disk store's page cache (default 16;
    /// `GSTORE_DISK_CACHE_MB` overrides it).
    pub disk_cache_mb: usize,
    /// *(Rust extension)* MiB a Stream-mode dictionary may keep in its
    /// id → string and string → id caches (default 80).
    pub dict_cache_mb: usize,
    /// *(Rust extension)* Build the VS-tree at open when none is persisted
    /// fresh (RocksDB always; a Stream-mode disk database whose `vstree.kv`
    /// is stale) instead of evaluating without it (default off).
    pub vstree_on_open: bool,
    /// *(Rust extension)* The layout a build that names none creates:
    /// `snapshot`, `disk` or `rocksdb` (default unset = `snapshot`). Used by
    /// REST `/build`, `/api build` (a `layout` parameter overrides it) and
    /// `gbuild` (`--layout` / `--disk` / `--rocksdb` override it).
    pub default_layout: Option<StorageKind>,
    /// *(Rust extension)* MiB of the LRU block cache a RocksDB database
    /// shares across its column families (default 256; `0` = RocksDB's own
    /// per-column-family default). The `GSTORE_ROCKS_BLOCK_CACHE_MB`
    /// environment variable overrides it.
    pub rocksdb_block_cache_mb: usize,
    /// *(Rust extension)* Bits per key of the bloom filters RocksDB writes for
    /// its point-lookup column families (default 10; `0` / `off` = none,
    /// `on` = 10). Options only: files open with or without them.
    pub rocksdb_bloom: u32,
    /// *(Rust extension)* The most files a RocksDB database keeps open
    /// (default 4096; `0` = unlimited, RocksDB's own default, which holds a
    /// descriptor per table file for as long as the database is open).
    /// Options only: files open with any value.
    pub rocksdb_max_open_files: u32,
    /// *(Rust extension, `[system]`)* Free the ids of terms a delete leaves
    /// unreferenced, for reuse (`on`, the default), or keep them until an
    /// explicit `gcompact --reclaim` (`off`). The `GSTORE_ID_RECLAIM`
    /// environment variable overrides it. A `kvstore.kv` written before
    /// Phase 6 keeps reclamation off until `gcompact --enable-reclaim`.
    pub id_reclaim: IdReclaim,

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
            query_timeout: DEFAULT_QUERY_TIMEOUT,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            ip_allow_path: None,
            ip_deny_path: Some(PathBuf::from("conf/ipDeny.config")),
            upstream_login_lockout: false,
            login_lockout_failures: crate::gapi::ipfilter::MAX_AUTH_FAILURES,
            login_lockout_max_seconds: 900,
            listen_addr: "127.0.0.1".to_string(),
            keep_alive_timeout: 60,
            min_body_rate: crate::server::DEFAULT_MIN_BODY_RATE,
            max_conns_per_ip: crate::server::DEFAULT_MAX_CONNS_PER_IP,
            keep_alive_max: 100,
            queue_size: 128,
            cors: true,
            cors_allow_origin: Vec::new(),
            allowed_hosts: Vec::new(),
            allow_get_update: false,
            trust_proxy_headers: false,
            allow_default_root_password: false,
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
            txn_idle_timeout: 600,
            build_multi_thread: true,
            build_memory_mb: crate::bulk::DEFAULT_MEMORY_BUDGET as u64 >> 20,
            build_threads: 0,
            build_tmp_dir: None,
            build_fill_factor: crate::bulk::DEFAULT_FILL_PERCENT,
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
            // 256 MiB (upstream: 1 GiB; review infra#1).
            upload_max_body_size: 268_435_456,
            preauth_max_body_size: crate::preauth::PREAUTH_MAX_BODY as u64,
            request_body_budget: crate::preauth::DEFAULT_BODY_BUDGET as u64,
            upload_allow_extensions: split_list("nt|ttl|n3|rdf|txt|nq|trig"),
            upload_allow_compress_packages: split_list("zip"),
            unzip_max_entries: crate::zipio::DEFAULT_MAX_ENTRIES,
            unzip_max_size: crate::zipio::DEFAULT_MAX_SIZE,
            rdf_max_token_size: crate::parser::stream::DEFAULT_MAX_TOKEN_BYTES as u64,
            sparql_max_size: crate::parser::sparql::lexer::DEFAULT_MAX_REQUEST_BYTES as u64,
            anonymous_download: false,
            download_max_concurrent: 8,
            file_retention_hours: 168,
            upload_user_quota: 0,
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
            pfn_fuel_per_query: crate::pfn::DEFAULT_FUEL_PER_QUERY,
            disk_query_mode: crate::db::DEFAULT_DISK_QUERY_MODE,
            disk_cache_mb: (crate::db::DEFAULT_DISK_CACHE_PAGES * crate::kvstore::PAGE_SIZE) >> 20,
            dict_cache_mb: crate::db::DEFAULT_DICT_CACHE_BYTES >> 20,
            vstree_on_open: false,
            default_layout: None,
            rocksdb_block_cache_mb: 256,
            rocksdb_bloom: 10,
            rocksdb_max_open_files: 4096,
            id_reclaim: IdReclaim::Immediate,
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
    BadValue {
        key: String,
        value: String,
        expected: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnknownKey(k) => write!(f, "unknown configuration key '{k}'"),
            ConfigError::BadValue {
                key,
                value,
                expected,
            } => {
                write!(
                    f,
                    "invalid value '{value}' for '{key}' (expected {expected})"
                )
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
            self.warnings.push(format!(
                "line {line}: ignoring unrecognised line '{}'",
                text.trim()
            ));
        }
        // Review bulkcli#9: a value cut at an inline comment is never
        // shortened silently — upstream reads the comment as part of it.
        for (line, key, comment) in ini.inline_comments() {
            self.warnings.push(format!(
                "line {line}: '{comment}' after the value of '{key}' is read as a comment \
                 (upstream gStore reads it as part of the value; quote the value to keep it)"
            ));
        }
        // Text earlier gstore-rust versions cut as a comment (any whitespace
        // + `#`/`;`) that is part of the value now (a marker glued to the
        // next word): `db_home=/srv/gstore #primary` read `/srv/gstore` and
        // now reads all of it, as upstream does (round-4 review of
        // bulkcli#9's fix: such a path changed on upgrade without a word).
        for (line, key, kept) in &ini.legacy_cuts {
            self.warnings.push(format!(
                "line {line}: '{kept}' after the value of '{key}' is part of the value, as \
                 upstream gStore reads it (earlier gstore-rust versions read it as a comment; \
                 put whitespace after the '#' or ';', or the comment on a line of its own, to \
                 keep it a comment, or quote the whole value to keep it in the value)"
            ));
        }
        // A password kept whole where earlier gstore-rust versions cut it at
        // the marker (a quoted `root_password="pw"  # note` read `pw`, and
        // `root_password=pw #x` read `pw`; both now read all of it, as
        // upstream does): said, without quoting it.
        for (line, key) in &ini.verbatim_markers {
            self.warnings.push(format!(
                "line {line}: the value of '{key}' holds a '#' or ';' after whitespace; all of \
                 it is the value ({key} is never cut at an inline comment, as upstream gStore \
                 reads it; earlier gstore-rust versions read the rest as a comment)"
            ));
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
        self.warn_inert_settings();
    }

    /// Warn about settings this configuration accepts but cannot honour as
    /// written (review design#11, bulkcli#8): a key whose upstream meaning
    /// is not implemented, or one another key overrides.
    fn warn_inert_settings(&mut self) {
        if self.resource_check {
            self.warnings.push(
                "resource_check=on has no effect: gstore-rust does not check free memory or disk \
                 space before a build or load (min_memory, min_million_disk and \
                 min_million_memory are not used either)"
                    .to_string(),
            );
        }
        if !self.build_multi_thread && self.is_set("build_threads") && self.build_threads != 1 {
            self.warnings.push(format!(
                "build_threads={} has no effect: build_multi_thread=off makes bulk builds use one \
                 thread (set build_multi_thread=on, or remove build_threads)",
                self.build_threads
            ));
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
            "max_database_num" => {
                self.max_database_num = parse_num(v).ok_or_else(|| bad("an integer"))?
            }
            "max_user_num" => self.max_user_num = parse_num(v).ok_or_else(|| bad("an integer"))?,
            "deamon" => self.deamon = parse_switch(v).ok_or_else(|| bad("on or off"))?,
            "max_output_size" => {
                self.max_output_size = parse_num(v).ok_or_else(|| bad("an integer"))?
            }
            "query_timeout" => {
                self.query_timeout =
                    parse_seconds(v).ok_or_else(|| bad("seconds (fractions allowed; 0 = off)"))?
            }
            "max_result_bytes" => {
                self.max_result_bytes =
                    parse_num(v).ok_or_else(|| bad("a size in bytes (0 = no limit)"))?
            }
            "ip_allow_path" => self.ip_allow_path = parse_opt_path(v),
            "ip_deny_path" => self.ip_deny_path = parse_opt_path(v),
            "upstream_login_lockout" => {
                self.upstream_login_lockout = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "login_lockout_failures" => {
                self.login_lockout_failures = parse_num(v).ok_or_else(|| bad("an integer"))?
            }
            "login_lockout_max_seconds" => {
                self.login_lockout_max_seconds = parse_num(v)
                    .filter(|n: &u64| *n >= 1)
                    .ok_or_else(|| bad("a positive number of seconds"))?
            }
            "listen_addr" => {
                if v.is_empty() || v.contains(char::is_whitespace) {
                    return Err(bad("a host name or IP address"));
                }
                self.listen_addr = v.to_string()
            }
            "keep_alive_timeout" => {
                self.keep_alive_timeout = parse_num(v).ok_or_else(|| bad("seconds (0 = off)"))?
            }
            "min_body_rate" => {
                self.min_body_rate =
                    parse_num(v).ok_or_else(|| bad("bytes per second (0 = off)"))?
            }
            "max_conns_per_ip" => {
                self.max_conns_per_ip =
                    parse_num(v).ok_or_else(|| bad("an integer (0 = no limit)"))?
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
            "cors_allow_origin" => self.cors_allow_origin = split_words(v),
            "allowed_hosts" => self.allowed_hosts = split_words(v),
            "allow_get_update" => {
                self.allow_get_update = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "trust_proxy_headers" => {
                self.trust_proxy_headers = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "allow_default_root_password" => {
                self.allow_default_root_password =
                    parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "min_memory" => self.min_memory = parse_num(v).ok_or_else(|| bad("an integer (MB)"))?,
            "min_million_disk" => {
                self.min_million_disk = parse_num(v).ok_or_else(|| bad("an integer (MB)"))?
            }
            "min_million_memory" => {
                self.min_million_memory = parse_num(v).ok_or_else(|| bad("an integer (MB)"))?
            }
            "resource_check" => {
                self.resource_check = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "db_home" => {
                self.db_home = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "db_suffix" => {
                self.db_suffix = nonempty(v)
                    .ok_or_else(|| bad("a suffix such as .db"))?
                    .to_string()
            }
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
            "txn_idle_timeout" => {
                self.txn_idle_timeout = v
                    .parse()
                    .map_err(|_| bad("a number of seconds (0 = never)"))?
            }
            "build_multi_thread" => {
                self.build_multi_thread = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "build_memory_mb" => {
                self.build_memory_mb = v
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (1..=(1 << 30)).contains(n))
                    .ok_or_else(|| bad("a positive number of MiB"))?
            }
            "build_threads" => {
                self.build_threads = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n <= 1024)
                    .ok_or_else(|| bad("a number of threads (0 = all cores)"))?
            }
            "build_tmp_dir" => self.build_tmp_dir = parse_opt_path(v),
            "build_fill_factor" => {
                self.build_fill_factor = v
                    .trim_end_matches('%')
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (50..=100).contains(n))
                    .ok_or_else(|| bad("a percentage from 50 to 100"))?
            }
            "log_mode" => self.log_mode = v.to_string(),
            "querylog_mode" => self.querylog_mode = parse_switch(v).ok_or_else(|| bad("0 or 1"))?,
            "querylog_path" => {
                self.querylog_path = PathBuf::from(nonempty(v).ok_or_else(|| bad("a directory"))?)
            }
            "querylog_days" => {
                self.querylog_days = parse_num(v).ok_or_else(|| bad("a number of days"))?
            }
            "accesslog_mode" => {
                self.accesslog_mode = parse_switch(v).ok_or_else(|| bad("0 or 1"))?
            }
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
            "log_file" => {
                self.log_file = nonempty(v)
                    .ok_or_else(|| bad("a file path or off"))?
                    .to_string()
            }
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
            "preauth_max_body_size" => {
                self.preauth_max_body_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "request_body_budget" => {
                self.request_body_budget = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "upload_allow_extensions" => self.upload_allow_extensions = split_list(v),
            "upload_allow_compress_packages" => self.upload_allow_compress_packages = split_list(v),
            "unzip_max_entries" => {
                self.unzip_max_entries = parse_num(v).ok_or_else(|| bad("a number of entries"))?
            }
            "unzip_max_size" => {
                self.unzip_max_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "rdf_max_token_size" => {
                self.rdf_max_token_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "sparql_max_size" => {
                self.sparql_max_size = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
            }
            "anonymous_download" => {
                self.anonymous_download = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "download_max_concurrent" => {
                self.download_max_concurrent =
                    parse_num(v).ok_or_else(|| bad("a number of downloads"))?
            }
            "file_retention_hours" => {
                self.file_retention_hours = parse_num(v).ok_or_else(|| bad("a number of hours"))?
            }
            "upload_user_quota" => {
                self.upload_user_quota = parse_num(v).ok_or_else(|| bad("a size in bytes"))?
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
            "pfn_fuel_per_query" => {
                self.pfn_fuel_per_query =
                    parse_num(v).ok_or_else(|| bad("a number of instructions (0 = unlimited)"))?
            }
            "disk_query_mode" => {
                self.disk_query_mode =
                    DiskQueryMode::parse(v).ok_or_else(|| bad("stream or memory"))?
            }
            "disk_cache_mb" => {
                self.disk_cache_mb = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=(1 << 20)).contains(n))
                    .ok_or_else(|| bad("a positive number of MiB"))?
            }
            "dict_cache_mb" => {
                self.dict_cache_mb = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n <= 1 << 20)
                    .ok_or_else(|| bad("a number of MiB"))?
            }
            "vstree_on_open" => {
                self.vstree_on_open = parse_switch(v).ok_or_else(|| bad("on or off"))?
            }
            "default_layout" => {
                self.default_layout =
                    parse_layout(v).ok_or_else(|| bad("snapshot, disk or rocksdb"))?
            }
            "rocksdb_block_cache_mb" => {
                self.rocksdb_block_cache_mb = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n <= 1 << 20)
                    .ok_or_else(|| bad("a number of MiB (0 = RocksDB's default)"))?
            }
            "rocksdb_bloom" => {
                self.rocksdb_bloom = match parse_switch(v) {
                    Some(true) => 10,
                    Some(false) => 0,
                    None => v
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n <= 64)
                        .ok_or_else(|| bad("on, off or bits per key (0..=64)"))?,
                }
            }
            "rocksdb_max_open_files" => {
                self.rocksdb_max_open_files = v
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n == 0 || (20..=1 << 20).contains(n))
                    .ok_or_else(|| bad("a number of files (20..=1048576; 0 = unlimited)"))?
            }
            "id_reclaim" => {
                self.id_reclaim = v.parse::<IdReclaim>().map_err(|_| bad("on or off"))?
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
            "query_timeout" => seconds(self.query_timeout),
            "max_result_bytes" => self.max_result_bytes.to_string(),
            "ip_allow_path" => opt_path(&self.ip_allow_path),
            "ip_deny_path" => opt_path(&self.ip_deny_path),
            "upstream_login_lockout" => switch(self.upstream_login_lockout),
            "login_lockout_failures" => self.login_lockout_failures.to_string(),
            "login_lockout_max_seconds" => self.login_lockout_max_seconds.to_string(),
            "listen_addr" => self.listen_addr.clone(),
            "keep_alive_timeout" => self.keep_alive_timeout.to_string(),
            "min_body_rate" => self.min_body_rate.to_string(),
            "max_conns_per_ip" => self.max_conns_per_ip.to_string(),
            "keep_alive_max" => self.keep_alive_max.to_string(),
            "queue_size" => self.queue_size.to_string(),
            "cors" => switch(self.cors),
            "cors_allow_origin" => self.cors_allow_origin.join("|"),
            "allowed_hosts" => self.allowed_hosts.join("|"),
            "allow_get_update" => switch(self.allow_get_update),
            "trust_proxy_headers" => switch(self.trust_proxy_headers),
            "allow_default_root_password" => switch(self.allow_default_root_password),
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
            "txn_idle_timeout" => self.txn_idle_timeout.to_string(),
            "build_multi_thread" => switch(self.build_multi_thread),
            "build_memory_mb" => self.build_memory_mb.to_string(),
            "build_threads" => self.build_threads.to_string(),
            "build_tmp_dir" => opt_path(&self.build_tmp_dir),
            "build_fill_factor" => self.build_fill_factor.to_string(),
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
            "preauth_max_body_size" => self.preauth_max_body_size.to_string(),
            "request_body_budget" => self.request_body_budget.to_string(),
            "upload_allow_extensions" => self.upload_allow_extensions.join("|"),
            "upload_allow_compress_packages" => self.upload_allow_compress_packages.join("|"),
            "unzip_max_entries" => self.unzip_max_entries.to_string(),
            "unzip_max_size" => self.unzip_max_size.to_string(),
            "rdf_max_token_size" => self.rdf_max_token_size.to_string(),
            "sparql_max_size" => self.sparql_max_size.to_string(),
            "anonymous_download" => switch(self.anonymous_download),
            "download_max_concurrent" => self.download_max_concurrent.to_string(),
            "file_retention_hours" => self.file_retention_hours.to_string(),
            "upload_user_quota" => self.upload_user_quota.to_string(),
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
            "pfn_fuel_per_query" => self.pfn_fuel_per_query.to_string(),
            "disk_query_mode" => self.disk_query_mode.name().to_string(),
            "disk_cache_mb" => self.disk_cache_mb.to_string(),
            "dict_cache_mb" => self.dict_cache_mb.to_string(),
            "vstree_on_open" => switch(self.vstree_on_open),
            "default_layout" => self
                .default_layout
                .map(layout_name)
                .unwrap_or_default()
                .to_string(),
            "rocksdb_block_cache_mb" => self.rocksdb_block_cache_mb.to_string(),
            "rocksdb_bloom" => self.rocksdb_bloom.to_string(),
            "rocksdb_max_open_files" => self.rocksdb_max_open_files.to_string(),
            "id_reclaim" => self.id_reclaim.to_string(),
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

    /// The directory the databases live in: `db_home` — what `gserver`
    /// serves (its API root when no directory is named) and what
    /// [`db_path`](Self::db_path) puts a database's directory in.
    pub fn db_root(&self) -> &Path {
        &self.db_home
    }

    /// The directory of database `name` (upstream `GlobalTypedef::db_path`):
    /// `db_home/<name>.db`, where the servers look for it (review design#11:
    /// this used `db_suffix`, which the servers do not honour — they always
    /// use [`DB_SUFFIX`](crate::DB_SUFFIX) and `gserver` warns about another
    /// suffix — so it named a directory no server would open). A name that
    /// already ends with `.db` is taken as is, as the servers take it.
    pub fn db_path(&self, name: &str) -> PathBuf {
        self.db_root().join(crate::db_dir_for(name))
    }

    /// The HTTP connection settings implied by this configuration: the worker
    /// pool (`thread_num`, `queue_size`), keep-alive, CORS, and the request
    /// body caps (`upload_max_body_size`, `preauth_max_body_size`) and budget
    /// (`request_body_budget`).
    pub fn http_options(&self) -> HttpOptions {
        HttpOptions {
            threads: self.thread_num.max(1),
            queue_len: self.queue_size,
            keep_alive_timeout: Duration::from_secs(self.keep_alive_timeout),
            keep_alive_max: self.keep_alive_max.max(1),
            min_body_rate: self.min_body_rate,
            max_conns_per_ip: self.max_conns_per_ip,
            max_body: usize::try_from(self.upload_max_body_size).unwrap_or(usize::MAX),
            preauth_max_body: usize::try_from(self.preauth_max_body_size).unwrap_or(usize::MAX),
            body_budget: usize::try_from(self.request_body_budget).unwrap_or(usize::MAX),
            cors: self.cors,
            cors_allow_origin: self.cors_allow_origin.clone(),
            allowed_hosts: self.allowed_hosts_with_listen_addr(),
            allow_get_update: self.allow_get_update,
            trust_proxy_headers: self.trust_proxy_headers,
            ..HttpOptions::default()
        }
    }

    /// One warning line per upstream-compat switch that is on (the D1 secure
    /// defaults of the review fixes, each restoring an upstream behaviour):
    /// `gserver` logs them at startup, each containing
    /// `upstream-compat: <key>=<value>`.
    pub fn upstream_compat_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.cors_allow_origin.iter().any(|o| o.trim() == "*") {
            out.push(
                "upstream-compat: cors_allow_origin=* (every web page may read this server's \
                 answers and send it writes through a visitor's browser)"
                    .to_string(),
            );
        } else if !self.cors_allow_origin.is_empty() {
            out.push(format!(
                "upstream-compat: cors_allow_origin={} (these origins may call this server \
                 from a browser)",
                self.cors_allow_origin.join("|")
            ));
        }
        if !self.allowed_hosts.is_empty() {
            out.push(format!(
                "upstream-compat: allowed_hosts={} (accepted in the Host header of loopback \
                 requests)",
                self.allowed_hosts.join("|")
            ));
        }
        if self.allow_get_update {
            out.push(
                "upstream-compat: allow_get_update=on (GET /sparql runs SPARQL UPDATE and \
                 /update takes any body type: a web page can change data through a visitor's \
                 browser)"
                    .to_string(),
            );
        }
        if self.trust_proxy_headers {
            out.push(
                "upstream-compat: trust_proxy_headers=on (a loopback peer forwarding \
                 X-Forwarded-For / Forwarded / X-Real-IP counts as a local client)"
                    .to_string(),
            );
        }
        if self.allow_default_root_password {
            out.push(format!(
                "upstream-compat: allow_default_root_password=on (root may keep the default \
                 password '{DEFAULT_ROOT_PASSWORD}' on a loopback server: change it and remove \
                 the switch)"
            ));
        }
        if self.txn_idle_timeout == 0 {
            out.push(
                "upstream-compat: txn_idle_timeout=0 (an open transaction never expires: a \
                 client that vanishes after begin blocks its database's writes until root \
                 rolls it back)"
                    .to_string(),
            );
        }
        if self.anonymous_download {
            out.push(
                "upstream-compat: anonymous_download=on (anyone may download the files of \
                 the export directory through /file/download without credentials)"
                    .to_string(),
            );
        }
        if self.upstream_login_lockout {
            out.push(
                "upstream-compat: upstream_login_lockout=on (failed logins lock out an address \
                 until a successful login or a restart, but login and testConnect keep \
                 checking passwords from it: unlimited guessing, and behind a proxy anyone \
                 locks out every client)"
                    .to_string(),
            );
        }
        out
    }

    /// [`allowed_hosts`](Self::allowed_hosts) plus `listen_addr` when that is
    /// a host name (an address is accepted anyway).
    fn allowed_hosts_with_listen_addr(&self) -> Vec<String> {
        let mut hosts = self.allowed_hosts.clone();
        let listen = self.listen_addr.trim();
        if !listen.is_empty() && listen.parse::<std::net::IpAddr>().is_err() {
            hosts.push(listen.to_string());
        }
        hosts
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
                fuel_per_query: self.pfn_fuel_per_query,
            },
        }
    }

    /// The bulk-build options of the `[system]` `build_*` keys (`gbuild
    /// --disk`): `build_multi_thread=off` means one thread.
    pub fn bulk_options(&self) -> crate::bulk::BulkOptions {
        crate::bulk::BulkOptions {
            memory_budget: usize::try_from(self.build_memory_mb.saturating_mul(1 << 20))
                .unwrap_or(usize::MAX),
            threads: if self.build_multi_thread {
                self.build_threads
            } else {
                1
            },
            temp_dir: self.build_tmp_dir.clone(),
            fill_percent: self.build_fill_factor,
            ..crate::bulk::BulkOptions::default()
        }
    }

    /// The open options of the `[storage]` section (contract C7), which
    /// `gserver` installs as the process defaults
    /// ([`set_default_open_options`](crate::db::set_default_open_options));
    /// the environment still overrides them when a database is opened.
    pub fn open_options(&self) -> OpenOptions {
        OpenOptions {
            disk_query_mode: self.disk_query_mode,
            disk_cache_pages: self
                .disk_cache_mb
                .saturating_mul((1 << 20) / crate::kvstore::PAGE_SIZE)
                .max(16),
            dict_cache_bytes: self.dict_cache_mb.saturating_mul(1 << 20),
            vstree_on_open: self.vstree_on_open,
            default_layout: self.default_layout,
            ..OpenOptions::default()
        }
    }

    /// Install this configuration's process-wide storage defaults that are
    /// not [`OpenOptions`]: the id-reclamation policy (`[system]
    /// id_reclaim`, [`crate::db::set_default_id_reclaim`]), the parser
    /// limits (`[upload] rdf_max_token_size`,
    /// [`crate::parser::stream::set_default_max_token_bytes`];
    /// `sparql_max_size`,
    /// [`crate::parser::sparql::lexer::set_max_request_bytes`]) and, with the
    /// `rocksdb` feature, the RocksDB tuning (`[storage]
    /// rocksdb_block_cache_mb` / `rocksdb_bloom` / `rocksdb_max_open_files`).
    /// The environment
    /// (`GSTORE_ID_RECLAIM`, `GSTORE_ROCKS_BLOCK_CACHE_MB`) still overrides
    /// them. A server calls it where it installs
    /// [`open_options`](Self::open_options); databases opened afterwards use
    /// them.
    pub fn install_storage_defaults(&self) {
        crate::db::set_default_id_reclaim(self.id_reclaim);
        crate::parser::stream::set_default_max_token_bytes(
            usize::try_from(self.rdf_max_token_size).unwrap_or(usize::MAX),
        );
        crate::parser::sparql::lexer::set_max_request_bytes(
            usize::try_from(self.sparql_max_size).unwrap_or(usize::MAX),
        );
        #[cfg(feature = "rocksdb")]
        crate::backend::rocks::set_default_rocks_tuning(self.rocks_tuning());
    }

    /// The RocksDB tuning of the `[storage]` keys (without the environment).
    #[cfg(feature = "rocksdb")]
    pub fn rocks_tuning(&self) -> crate::backend::rocks::RocksTuning {
        crate::backend::rocks::RocksTuning {
            block_cache_bytes: self.rocksdb_block_cache_mb.saturating_mul(1 << 20),
            bloom_bits_per_key: f64::from(self.rocksdb_bloom),
            max_open_files: match self.rocksdb_max_open_files {
                0 => -1,
                n => i32::try_from(n).unwrap_or(i32::MAX),
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

/// `default_layout`: `snapshot` (also `memory`), `disk` or `rocksdb`; an
/// empty value or `auto` / `none` means unset (`Some(None)`).
fn parse_layout(v: &str) -> Option<Option<StorageKind>> {
    match v.to_ascii_lowercase().as_str() {
        "" | "auto" | "none" => Some(None),
        "snapshot" | "memory" => Some(Some(StorageKind::Snapshot)),
        "disk" | "kvstore" => Some(Some(StorageKind::Disk)),
        "rocksdb" | "rocks" => Some(Some(StorageKind::RocksDb)),
        _ => None,
    }
}

/// The `default_layout` spelling of a layout.
fn layout_name(kind: StorageKind) -> &'static str {
    match kind {
        StorageKind::Snapshot => "snapshot",
        StorageKind::Disk => "disk",
        StorageKind::RocksDb => "rocksdb",
    }
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

/// A duration in seconds, fractions allowed (`30`, `0.5`); `0` is zero.
fn parse_seconds(v: &str) -> Option<Duration> {
    let secs = v.parse::<f64>().ok()?;
    (secs.is_finite() && secs >= 0.0)
        .then(|| Duration::try_from_secs_f64(secs).ok())
        .flatten()
}

/// A duration as [`parse_seconds`] reads it: whole seconds as an integer,
/// otherwise with the fraction (to the nanosecond, trailing zeros dropped —
/// a sub-millisecond timeout must not read back as `0`, which is "off").
fn seconds(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        d.as_secs().to_string()
    } else {
        let s = format!("{}.{:09}", d.as_secs(), d.subsec_nanos());
        s.trim_end_matches('0').to_string()
    }
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

/// A list separated by `|`, `,` or whitespace, dropping empty items.
fn split_words(v: &str) -> Vec<String> {
    v.split(|c: char| c == '|' || c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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

    /// `query_timeout` reads back as it was set — also below a millisecond,
    /// which used to print as `0.` and so read back as `0`, "no limit".
    #[test]
    fn query_timeout_values_round_trip() {
        for (set, shown) in [
            ("2.5", "2.5"),
            ("0.0005", "0.0005"),
            ("1.125", "1.125"),
            ("60", "60"),
            ("0", "0"),
        ] {
            let mut c = GStoreConfig::default();
            c.set("query_timeout", set).unwrap();
            assert_eq!(c.value("query_timeout").as_deref(), Some(shown), "{set}");
            let mut again = GStoreConfig::default();
            again.set("query_timeout", shown).unwrap();
            assert_eq!(again.query_timeout, c.query_timeout, "{set}");
        }
    }

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
        assert_eq!(
            ini.get("b", "k2"),
            Some("#not-a-comment"),
            "a value may start with #"
        );
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

    /// Round-4 review of bulkcli#9's fix: earlier gstore-rust versions cut an
    /// unquoted value at any whitespace + `#`/`;`; the narrower rule keeps a
    /// glued marker in the value, as upstream does. A path that changes
    /// meaning on upgrade (`db_home`, `backup_path`, `ip_deny_path` — a
    /// deny list that silently names no file lets every address in) must
    /// not change silently.
    #[test]
    fn a_value_earlier_versions_cut_at_a_glued_marker_is_kept_and_reported() {
        let c = GStoreConfig::from_ini_str(
            "[system]\ndb_home=/x #y\n\
             [backup]\nbackup_path=./backups ;nightly\n\
             [server]\nip_deny_path=\"./conf/ipDeny.config\" #blocked hosts\n",
        );
        assert_eq!(c.db_home, PathBuf::from("/x #y"));
        assert_eq!(c.backup_path, PathBuf::from("./backups ;nightly"));
        assert_eq!(
            c.ip_deny_path,
            Some(PathBuf::from("./conf/ipDeny.config #blocked hosts"))
        );
        let w = c.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(
            w[0].starts_with("line 2: '#y' after the value of 'db_home' is part of the value"),
            "{w:?}"
        );
        assert!(w[1].starts_with("line 4: ';nightly' after the value of 'backup_path'"));
        assert!(w[2].starts_with("line 6: '#blocked hosts' after the value of 'ip_deny_path'"));
        assert!(w.iter().all(|w| w.contains("earlier gstore-rust versions")));

        // Text between the old cut and a later word-like comment is kept and
        // reported once; the comment itself is reported as cut.
        let c = GStoreConfig::from_ini_str("[system]\ndb_home=/x #y # note\n");
        assert_eq!(c.db_home, PathBuf::from("/x #y"));
        assert_eq!(c.warnings().len(), 2, "{:?}", c.warnings());
        assert!(
            c.warnings()[0].contains("'# note' after the value of 'db_home' is read as a comment")
        );
        assert!(c.warnings()[1].contains("'#y' after the value of 'db_home' is part of the value"));

        // A glued marker in root_password: reported without quoting it.
        let c = GStoreConfig::from_ini_str("[system]\nroot_password=pw #x\n");
        assert_eq!(c.root_password, "pw #x");
        assert_eq!(c.warnings().len(), 1, "{:?}", c.warnings());
        assert!(c.warnings()[0].starts_with("line 2: the value of 'root_password' holds"));
        assert!(!c.warnings()[0].contains("pw") && !c.warnings()[0].contains("#x"));

        // Where both rules agree, nothing is said: a quoted value, a marker
        // at the start, a marker without whitespace before it.
        let c = GStoreConfig::from_ini_str(
            "[system]\ndb_home=\"/x #y\"\nlicensetype=#x\n[backup]\nbackup_path=/a#b\n",
        );
        assert_eq!(c.db_home, PathBuf::from("/x #y"));
        assert!(c.warnings().is_empty(), "{:?}", c.warnings());
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
        assert_eq!(
            (c.min_memory, c.min_million_disk, c.min_million_memory),
            (512, 50, 20)
        );
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
        // review infra#1: the upload cap is lowered from upstream's 1 GiB, and
        // what a request may carry before it is authenticated, and all
        // bodies in memory at once, are bounded on their own.
        assert_eq!(c.upload_max_body_size, 256 << 20);
        assert_eq!(c.preauth_max_body_size, 64 << 20);
        assert_eq!(c.request_body_budget, 512 << 20);
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
                d.set(key, &v)
                    .unwrap_or_else(|e| panic!("set({key}, {v}): {e}"));
                assert_eq!(d.value(key).as_deref(), Some(v.as_str()), "{key}");
            }
        }
    }

    #[test]
    fn bulk_build_defaults_and_bad_values() {
        let c = GStoreConfig::default();
        let bulk = c.bulk_options();
        assert_eq!(bulk, crate::bulk::BulkOptions::default());
        let mut d = GStoreConfig::default();
        assert!(d.set("build_fill_factor", "40").is_err());
        assert!(d.set("build_fill_factor", "101").is_err());
        assert!(d.set("build_memory_mb", "0").is_err());
        d.set("build_fill_factor", "100%").unwrap();
        d.set("build_tmp_dir", "off").unwrap();
        assert_eq!(d.build_fill_factor, 100);
        assert_eq!(d.build_tmp_dir, None);
    }

    #[test]
    fn storage_defaults_and_bad_values() {
        let c = GStoreConfig::default();
        assert_eq!(c.open_options(), OpenOptions::default());
        assert_eq!(c.value("default_layout").unwrap(), "");
        let mut d = GStoreConfig::default();
        assert!(d.set("disk_query_mode", "fast").is_err());
        assert!(d.set("disk_cache_mb", "0").is_err());
        assert!(d.set("default_layout", "tape").is_err());
        assert!(d.set("vstree_on_open", "maybe").is_err());
        d.set("disk_query_mode", "Materialize").unwrap();
        d.set("default_layout", "rocksdb").unwrap();
        assert_eq!(d.disk_query_mode, DiskQueryMode::Materialize);
        assert_eq!(d.value("disk_query_mode").unwrap(), "memory");
        assert_eq!(d.default_layout, Some(StorageKind::RocksDb));
        // The build paths read it through the installed open options.
        assert_eq!(d.open_options().default_layout, Some(StorageKind::RocksDb));
        d.set("default_layout", "none").unwrap();
        assert_eq!(d.default_layout, None);
        // A tiny cache still gets a usable number of pages.
        d.set("disk_cache_mb", "1").unwrap();
        assert_eq!(d.open_options().disk_cache_pages, 256);
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
        assert_eq!(
            (c.min_memory, c.min_million_disk, c.min_million_memory),
            (1024, 200, 200)
        );
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
            // review infra#1: the shipped upload cap is lowered (see below).
            // review bulkcli#8: the shipped example no longer forces
            // single-threaded bulk builds (see below).
            if *key == "upload_allow_extensions"
                || *key == "upload_max_body_size"
                || *key == "build_multi_thread"
            {
                continue;
            }
            assert_eq!(c.value(key), upstream.value(key), "{key}");
        }
        // review bulkcli#8: upstream ships build_multi_thread=off, which made
        // every bulk build of an installed example single-threaded; the
        // example leaves it at the default, on.
        assert!(!upstream.build_multi_thread);
        assert!(c.build_multi_thread && !c.is_set("build_multi_thread"));
        assert_eq!(c.bulk_options().threads, 0, "every core");
        // review infra#1: the shipped example lowers upstream's 1 GiB upload
        // cap to the built-in default, 256 MiB.
        assert_eq!(upstream.upload_max_body_size, 1 << 30);
        assert_eq!(
            c.upload_max_body_size,
            GStoreConfig::default().upload_max_body_size
        );
        // The one deliberate difference: the quad formats are allowed too,
        // so an export of a database with named graphs (N-Quads) builds
        // again.
        assert_eq!(
            c.upload_allow_extensions,
            [
                upstream.upload_allow_extensions.clone(),
                vec!["nq".into(), "trig".into()]
            ]
            .concat()
        );
    }

    /// Upstream's test scripts (`tests/upstream_scripts/*.sh`, run by
    /// `scripts/conformance.sh`) take the server's port from the first line
    /// of `conf.ini` that contains `port=` (`grep -m 1 'port='`), and the
    /// harness sets the port by replacing the line `port=9000`: in the
    /// shipped example that line must come first, so no comment above it
    /// may contain `port=` (review bulkcli#9: the inline-comment example
    /// added to the header once did, and every upstream suite then talked
    /// to port "9000  # the HTTP port …").
    #[test]
    fn the_shipped_example_names_the_http_port_first() {
        let text = include_str!("../conf/conf.ini.example");
        let first = text.lines().find(|l| l.contains("port="));
        assert_eq!(first, Some("port=9000"));
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
                    min_body_rate=0\nmax_conns_per_ip=5\n\
                    max_user_num=6\ndeamon=on\nmax_output_size=7\nip_allow_path=allow.txt\n\
                    query_timeout=2.5\nmax_result_bytes=4096\n\
                    upstream_login_lockout=on\nlogin_lockout_failures=3\n\
                    login_lockout_max_seconds=60\n\
                    ip_deny_path=off\nlisten_addr=0.0.0.0\nkeep_alive_timeout=0\nkeep_alive_max=3\n\
                    queue_size=0\ncors=off\ncors_allow_origin=https://a.example|http://b.example:81\n\
                    allowed_hosts=gs.example, gs2.example\nallow_get_update=on\n\
                    trust_proxy_headers=on\nallow_default_root_password=on\n\
                    [system]\nmin_memory=1\nmin_million_disk=2\nmin_million_memory=3\n\
                    resource_check=on\ndb_home=/data/gs\ndb_suffix=.gdb\nroot_username=admin\n\
                    root_password=s3cret\nsystem_username=sys\npfn_base_path=/p\nlicensetype=x\n\
                    check_point=off\ntxn_idle_timeout=30\nbuild_multi_thread=off\n\
                    build_memory_mb=128\nbuild_threads=3\nbuild_tmp_dir=/scratch\n\
                    build_fill_factor=75\nid_reclaim=off\n\
                    [log]\nlog_mode=l.properties\nquerylog_mode=0\nquerylog_path=q/\n\
                    querylog_days=9\naccesslog_mode=0\naccesslog_path=a/\naccesslog_days=8\n\
                    log_level=DEBUG\nlog_file=diag/gs.log\nlog_days=7\nlog_stderr=off\n\
                    [backup]\nbackup_path=/b\nmax_backups=11\n\
                    [upload]\nupload_path=/u\nupload_max_body_size=1024\n\
                    preauth_max_body_size=512\nrequest_body_budget=4096\n\
                    upload_allow_extensions=nt | ttl\nupload_allow_compress_packages=zip|tar\n\
                    unzip_max_entries=12\nunzip_max_size=4096\n\
                    rdf_max_token_size=2048\nsparql_max_size=0\n\
                    anonymous_download=on\ndownload_max_concurrent=2\nfile_retention_hours=5\n\
                    upload_user_quota=77\n\
                    [query]\ninteger_division=truncate\nquery_cache_size=0\n\
                    early_termination=off\ntopk_bounded_heap=off\ntopk_dpb=always\n\
                    gfun_max_output_bytes=4096\n\
                    [pfn]\npfn_fuel_per_call=0\npfn_max_memory_pages=2\n\
                    pfn_max_module_bytes=5000\npfn_max_functions=3\npfn_fuel_per_query=9\n\
                    [storage]\ndisk_query_mode=stream\ndisk_cache_mb=64\ndict_cache_mb=32\n\
                    vstree_on_open=on\ndefault_layout=disk\nrocksdb_block_cache_mb=64\n\
                    rocksdb_bloom=off\nrocksdb_max_open_files=512\n";
        let c = GStoreConfig::from_ini_str(text);
        // review design#11, bulkcli#8: every key is read without a parse
        // warning; two of these values are accepted but cannot be honoured
        // (resource_check=on, build_threads=3 under build_multi_thread=off),
        // which is now said instead of ignored silently.
        assert_eq!(c.warnings().len(), 2, "{:?}", c.warnings());
        assert!(c.warnings()[0].starts_with("resource_check=on has no effect"));
        assert!(c.warnings()[1].starts_with("build_threads=3 has no effect"));
        assert!(
            KNOWN_KEYS.iter().all(|(_, k)| c.is_set(k)),
            "every key counted as set"
        );
        assert_eq!((c.port, c.tcp_port, c.thread_num), (9100, 9200, 4));
        assert_eq!(
            (c.max_database_num, c.max_user_num, c.max_output_size),
            (5, 6, 7)
        );
        assert!(c.deamon);
        assert_eq!(
            (c.query_timeout, c.max_result_bytes),
            (Duration::from_millis(2500), 4096)
        );
        assert_eq!(c.value("query_timeout").as_deref(), Some("2.5"));
        assert!(c.upstream_login_lockout);
        assert_eq!(
            (c.login_lockout_failures, c.login_lockout_max_seconds),
            (3, 60)
        );
        assert_eq!(c.ip_allow_path, Some(PathBuf::from("allow.txt")));
        assert_eq!(c.ip_deny_path, None, "`off` disables the deny list");
        assert_eq!(c.listen_addr, "0.0.0.0");
        assert_eq!((c.min_body_rate, c.max_conns_per_ip), (0, 5));
        assert_eq!(
            (c.keep_alive_timeout, c.keep_alive_max, c.queue_size),
            (0, 3, 0)
        );
        assert!(!c.cors);
        assert_eq!(
            c.cors_allow_origin,
            ["https://a.example", "http://b.example:81"]
        );
        assert_eq!(c.allowed_hosts, ["gs.example", "gs2.example"]);
        assert!(c.allow_get_update && c.trust_proxy_headers && c.allow_default_root_password);
        assert_eq!(
            (c.min_memory, c.min_million_disk, c.min_million_memory),
            (1, 2, 3)
        );
        assert!(c.resource_check && !c.check_point && !c.build_multi_thread);
        assert_eq!(c.txn_idle_timeout, 30);
        assert_eq!(
            (c.build_memory_mb, c.build_threads, c.build_fill_factor),
            (128, 3, 75)
        );
        assert_eq!(c.build_tmp_dir, Some(PathBuf::from("/scratch")));
        let bulk = c.bulk_options();
        assert_eq!(bulk.memory_budget, 128 << 20);
        assert_eq!(bulk.threads, 1, "build_multi_thread=off means one thread");
        assert_eq!(bulk.fill_percent, 75);
        assert_eq!(bulk.temp_dir, Some(PathBuf::from("/scratch")));
        assert_eq!(c.db_home, PathBuf::from("/data/gs"));
        assert_eq!(c.db_suffix, ".gdb");
        // review design#11: db_path names the directory the servers use; a
        // db_suffix other than ".db" is not honoured (gserver warns), and
        // this assertion used to pin the suffix the servers ignore.
        assert_eq!(c.db_path("lubm"), PathBuf::from("/data/gs/lubm.db"));
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
        assert_eq!(
            (c.preauth_max_body_size, c.request_body_budget),
            (512, 4096)
        );
        assert_eq!(c.upload_allow_extensions, ["nt", "ttl"]);
        assert_eq!(c.upload_allow_compress_packages, ["zip", "tar"]);
        assert_eq!(
            c.value("upload_allow_compress_packages").unwrap(),
            "zip|tar"
        );
        assert_eq!((c.unzip_max_entries, c.unzip_max_size), (12, 4096));
        assert_eq!((c.rdf_max_token_size, c.sparql_max_size), (2048, 0));
        assert!(c.anonymous_download);
        assert_eq!(c.value("anonymous_download").unwrap(), "on");
        assert_eq!(
            (
                c.download_max_concurrent,
                c.file_retention_hours,
                c.upload_user_quota
            ),
            (2, 5, 77)
        );
        assert_eq!(c.value("querylog_mode").unwrap(), "0");
        assert_eq!(c.value("deamon").unwrap(), "on");
        assert!(c.upload_allowed("data.TTL") && c.upload_allowed("x.tar"));
        assert!(!c.upload_allowed("x.exe") && !c.upload_allowed("noext"));
        assert_eq!(
            c.open_options(),
            OpenOptions {
                disk_query_mode: DiskQueryMode::Stream,
                disk_cache_pages: 64 * 256,
                dict_cache_bytes: 32 << 20,
                vstree_on_open: true,
                default_layout: Some(StorageKind::Disk),
                ..OpenOptions::default()
            }
        );
        assert_eq!(c.value("disk_query_mode").unwrap(), "stream");
        assert_eq!(c.id_reclaim, IdReclaim::Off);
        assert_eq!((c.rocksdb_block_cache_mb, c.rocksdb_bloom), (64, 0));
        // review gap-rocksdb-exec#2: the new `[storage] rocksdb_max_open_files`
        // key (in the text above too, so that every key is counted as set).
        assert_eq!(c.rocksdb_max_open_files, 512);
        assert_eq!(c.value("id_reclaim").unwrap(), "off");
        assert_eq!(c.value("default_layout").unwrap(), "disk");

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
                    fuel_per_query: 9,
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
        assert_eq!((h.min_body_rate, h.max_conns_per_ip), (0, 5));
        assert_eq!(h.max_body, 1024);
        assert_eq!((h.preauth_max_body, h.body_budget), (512, 4096));
        assert!(!h.cors);
        assert_eq!(h.cors_allow_origin, c.cors_allow_origin);
        // listen_addr (0.0.0.0) is an address: nothing to add.
        assert_eq!(h.allowed_hosts, c.allowed_hosts);
        assert!(h.allow_get_update && h.trust_proxy_headers);
        let compat = c.upstream_compat_warnings();
        for key in [
            "cors_allow_origin",
            "allowed_hosts",
            "allow_get_update",
            "trust_proxy_headers",
            "allow_default_root_password",
        ] {
            assert!(
                compat
                    .iter()
                    .any(|w| w.starts_with(&format!("upstream-compat: {key}="))),
                "{key}: {compat:?}"
            );
        }
        assert!(GStoreConfig::default()
            .upstream_compat_warnings()
            .is_empty());
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
        assert!(
            w.contains("'log_days'") && w.contains("'log_stderr'"),
            "{w}"
        );
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
        assert_eq!(
            c.set("nope", "1"),
            Err(ConfigError::UnknownKey("nope".into()))
        );
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

    /// Convention 3/13c of the review-fix campaign: every D1 upstream-compat
    /// switch, when on, yields exactly one startup warning line
    /// `upstream-compat: <key>=<value>` from
    /// [`GStoreConfig::upstream_compat_warnings`] (gserver prints them and
    /// `scripts/conformance.sh` checks them), and none is reported while
    /// the secure defaults are in place. A package adding a switch adds it
    /// to `SWITCHES`.
    #[test]
    fn every_upstream_compat_switch_reports_one_startup_warning() {
        const SWITCHES: &[(&str, &str)] = &[
            ("cors_allow_origin", "*"),
            ("allowed_hosts", "gstore.example"),
            ("allow_get_update", "on"),
            ("trust_proxy_headers", "on"),
            ("allow_default_root_password", "on"),
            ("anonymous_download", "on"),
            // Round 2: r2-limits-auth's lockout switch and r2-txn's
            // upstream value of the transaction idle timeout.
            ("upstream_login_lockout", "on"),
            ("txn_idle_timeout", "0"),
        ];
        let defaults = GStoreConfig::default();
        assert_eq!(
            defaults.upstream_compat_warnings(),
            Vec::<String>::new(),
            "the secure defaults report no compat switch"
        );
        for (key, value) in SWITCHES {
            let mut c = GStoreConfig::default();
            c.set(key, value).unwrap();
            let lines = c.upstream_compat_warnings();
            let prefix = format!("upstream-compat: {key}={value}");
            assert_eq!(
                lines.iter().filter(|l| l.starts_with(&prefix)).count(),
                1,
                "{key}: {lines:?}"
            );
            assert_eq!(lines.len(), 1, "{key} alone: {lines:?}");
        }
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
