//! Runtime configuration, sourced from the environment so secrets are never
//! hard-coded. Read it once at startup with [`Config::from_env`].

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use crate::budget::CostLimits;
use crate::http_server::{HttpAuth, HttpLimits};
use crate::secret::Secret;

/// gNLQA runtime configuration. Secret fields are [`Secret`] so they never leak
/// through `Debug`.
#[derive(Debug, Clone)]
pub struct Config {
    /// Anthropic API key (`ANTHROPIC_API_KEY`). `None` ⇒ live LLM calls fail;
    /// the rest of the system (and all tests, via a mock client) still works.
    pub anthropic_api_key: Option<Secret>,
    /// Anthropic API base URL (`ANTHROPIC_BASE_URL`).
    pub anthropic_base_url: String,
    /// Which LLM backend to use (`GNLQA_LLM_PROVIDER`): `anthropic` (default) or
    /// `openai` (any OpenAI-compatible endpoint — OpenAI, DeepSeek, …).
    pub llm_provider: String,
    /// API key for the OpenAI-compatible backend (`OPENAI_API_KEY`).
    pub openai_api_key: Option<Secret>,
    /// Base URL for the OpenAI-compatible backend (`OPENAI_BASE_URL`), e.g.
    /// `https://api.openai.com/v1` or `https://api.deepseek.com`.
    pub openai_base_url: String,
    /// Default (most capable) model for understanding & generation.
    pub model: String,
    /// Cheaper/faster model for routing and simple questions.
    pub fast_model: String,
    /// gStore HTTP SPARQL endpoint (`GSTORE_ENDPOINT`), e.g. `http://127.0.0.1:9000/sparql`.
    pub gstore_endpoint: String,
    /// Optional gStore Basic-auth credentials.
    pub gstore_user: Option<String>,
    pub gstore_password: Option<Secret>,
    /// Minimum completion-token budget floored onto every LLM request
    /// (`GNLQA_MAX_TOKENS`). Raise it for reasoning models that spend tokens on
    /// hidden reasoning before the visible answer.
    pub max_tokens: u32,
    /// Per-request network timeout, seconds.
    pub timeout_secs: u64,
    /// Default answering mode (`GNLQA_MODE`): `auto`|`structured`|`graphrag`|`open`.
    pub mode: String,
    /// Answer-cache capacity in questions (`GNLQA_CACHE`; 0 disables the cache).
    pub cache_capacity: usize,
    /// Cache entry time-to-live in seconds (`GNLQA_CACHE_TTL_SECS`; 0 = no expiry).
    pub cache_ttl_secs: u64,
    /// Bearer token clients of `gnlqa serve` must present on `/ask` and
    /// `/gSolve` (`GNLQA_HTTP_TOKEN`). Without one (and without
    /// [`http_allow_unauthenticated`](Self::http_allow_unauthenticated)),
    /// `gnlqa serve` refuses to start.
    pub http_token: Option<Secret>,
    /// Explicit opt-out of HTTP client authentication
    /// (`GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on`, default off): the server then
    /// answers loopback clients only and refuses to bind a non-loopback address.
    pub http_allow_unauthenticated: bool,
    /// `/ask` + `/gSolve` requests per client IP per minute
    /// (`GNLQA_HTTP_RATE_PER_MIN`; 0 = unlimited).
    pub http_rate_per_min: u32,
    /// Maximum question length in bytes for `serve` (`GNLQA_MAX_QUESTION_BYTES`).
    pub max_question_bytes: usize,
    /// Maximum LLM calls per `serve`/`mcp` request (`GNLQA_MAX_LLM_CALLS`; 0 = unlimited).
    pub max_llm_calls: usize,
    /// Maximum KB rows fetched per `serve`/`mcp` request (`GNLQA_MAX_KB_ROWS`; 0 = unlimited).
    pub max_kb_rows: usize,
    /// Wall-clock budget per `serve`/`mcp` request in seconds
    /// (`GNLQA_REQUEST_TIMEOUT_SECS`; 0 = none).
    pub request_timeout_secs: u64,
}

impl Config {
    /// Build a config from a config file plus environment variables (env wins),
    /// falling back to sensible defaults. The file is a simple `KEY=VALUE` list
    /// using the same names as the env vars — so you configure once instead of
    /// exporting every session. See `gnlqa.conf.example` and
    /// [`config_file_path`] for where it is read from (never the current
    /// directory: review cluster-nlqa#15).
    pub fn from_env() -> Config {
        let cwd = env::current_dir().ok();
        Config::from_lookup(|k| env::var(k).ok(), cwd.as_deref())
    }

    /// [`from_env`](Self::from_env) with the environment replaced by `var`
    /// (a variable lookup) and the working directory by `cwd` (used only to
    /// point out a `gnlqa.conf` there that is *not* read). Lets tests and
    /// embedders build a config without touching the process environment.
    pub fn from_lookup(var: impl Fn(&str) -> Option<String>, cwd: Option<&Path>) -> Config {
        let file = load_config_file(&var, cwd);
        // env takes precedence over the file, so a one-off `KEY=… gnlqa …` still
        // works. A *blank* env var (`KEY=`, common in shell profiles / compose
        // files) is treated as absent so it can't shadow a real value in the file
        // and silently drop us back to the built-in default.
        let get = |k: &str| non_empty(var(k)).or_else(|| file.get(k).cloned());
        // A blank/whitespace value (e.g. `GNLQA_MODEL=` in the file) is treated as
        // absent, falling back to the default rather than an empty string that
        // would make the LLM/model call fail.
        let or = |v: Option<String>, d: &str| non_empty(v).unwrap_or_else(|| d.to_string());
        Config {
            anthropic_api_key: non_empty(get("ANTHROPIC_API_KEY")).map(Secret::new),
            anthropic_base_url: or(get("ANTHROPIC_BASE_URL"), "https://api.anthropic.com"),
            llm_provider: normalize_provider(get("GNLQA_LLM_PROVIDER")),
            openai_api_key: non_empty(get("OPENAI_API_KEY")).map(Secret::new),
            openai_base_url: or(get("OPENAI_BASE_URL"), "https://api.openai.com/v1"),
            model: or(get("GNLQA_MODEL"), "claude-opus-4-8"),
            fast_model: or(get("GNLQA_FAST_MODEL"), "claude-sonnet-4-6"),
            gstore_endpoint: or(get("GSTORE_ENDPOINT"), "http://127.0.0.1:9000/sparql"),
            gstore_user: non_empty(get("GSTORE_USER")),
            gstore_password: non_empty(get("GSTORE_PASSWORD")).map(Secret::new),
            max_tokens: parse_or(get("GNLQA_MAX_TOKENS"), 1024),
            timeout_secs: parse_or(get("GNLQA_TIMEOUT_SECS"), 60),
            mode: or(get("GNLQA_MODE"), "auto"),
            cache_capacity: parse_or(get("GNLQA_CACHE"), 256),
            cache_ttl_secs: parse_or(get("GNLQA_CACHE_TTL_SECS"), 0),
            http_token: non_empty(get("GNLQA_HTTP_TOKEN")).map(Secret::new),
            http_allow_unauthenticated: parse_switch(get("GNLQA_HTTP_ALLOW_UNAUTHENTICATED")),
            http_rate_per_min: parse_or(
                get("GNLQA_HTTP_RATE_PER_MIN"),
                crate::http_server::DEFAULT_REQUESTS_PER_MINUTE,
            ),
            max_question_bytes: parse_or(
                get("GNLQA_MAX_QUESTION_BYTES"),
                crate::http_server::DEFAULT_MAX_QUESTION_BYTES,
            ),
            max_llm_calls: parse_or(
                get("GNLQA_MAX_LLM_CALLS"),
                crate::http_server::DEFAULT_MAX_LLM_CALLS,
            ),
            max_kb_rows: parse_or(
                get("GNLQA_MAX_KB_ROWS"),
                crate::http_server::DEFAULT_MAX_KB_ROWS,
            ),
            request_timeout_secs: parse_or(
                get("GNLQA_REQUEST_TIMEOUT_SECS"),
                crate::http_server::DEFAULT_REQUEST_TIMEOUT_SECS,
            ),
        }
    }

    /// The per-request cost budget for the server surfaces (`serve`, `mcp`).
    pub fn cost_limits(&self) -> CostLimits {
        let opt = |n: usize| (n > 0).then_some(n);
        CostLimits {
            max_llm_calls: opt(self.max_llm_calls),
            max_kb_rows: opt(self.max_kb_rows),
            timeout: (self.request_timeout_secs > 0)
                .then(|| std::time::Duration::from_secs(self.request_timeout_secs)),
        }
    }

    /// The HTTP server limits (rate, question size, cost budget).
    pub fn http_limits(&self) -> HttpLimits {
        HttpLimits {
            requests_per_minute: self.http_rate_per_min,
            max_question_bytes: self.max_question_bytes.max(1),
            cost: self.cost_limits(),
        }
    }

    /// The HTTP client-authentication mode: a token if one is configured,
    /// else the loopback-only opt-out if it was switched on, else
    /// [`HttpAuth::Unconfigured`] (`gnlqa serve` refuses to start).
    pub fn http_auth(&self) -> HttpAuth {
        match (&self.http_token, self.http_allow_unauthenticated) {
            (Some(t), _) => HttpAuth::Token(t.clone()),
            (None, true) => HttpAuth::LoopbackOnly,
            (None, false) => HttpAuth::Unconfigured,
        }
    }

    /// Configuration that works but puts credentials at risk, one line each
    /// (printed by the CLI at startup): a credential sent in clear text over
    /// `http://` to a non-loopback host.
    pub fn security_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut check = |key: &str, url: &str, credential: Option<&str>| {
            let Some(cred) = credential else { return };
            if let Some(host) = plain_http_remote_host(url) {
                out.push(format!(
                    "{key} sends {cred} in clear text to {host} over http:// — use https:// \
                     (or a loopback endpoint)"
                ));
            }
        };
        let gstore_cred = self
            .gstore_password
            .as_ref()
            .map(|_| "the gStore password (GSTORE_PASSWORD)");
        check("GSTORE_ENDPOINT", &self.gstore_endpoint, gstore_cred);
        if self.llm_provider == "openai" {
            let key = self.openai_api_key.as_ref().map(|_| "OPENAI_API_KEY");
            check("OPENAI_BASE_URL", &self.openai_base_url, key);
        } else {
            let key = self.anthropic_api_key.as_ref().map(|_| "ANTHROPIC_API_KEY");
            check("ANTHROPIC_BASE_URL", &self.anthropic_base_url, key);
        }
        out
    }

    /// Whether live LLM calls are possible (a key for the active provider is set).
    pub fn has_api_key(&self) -> bool {
        match self.llm_provider.as_str() {
            "openai" => self.openai_api_key.is_some(),
            _ => self.anthropic_api_key.is_some(),
        }
    }
}

impl Default for Config {
    /// Pure, side-effect-free defaults (no env reads) so `..Default::default()`
    /// and tests behave predictably. Use [`Config::from_env`] for real startup.
    fn default() -> Self {
        Config {
            anthropic_api_key: None,
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            llm_provider: "anthropic".to_string(),
            openai_api_key: None,
            openai_base_url: "https://api.openai.com/v1".to_string(),
            model: "claude-opus-4-8".to_string(),
            fast_model: "claude-sonnet-4-6".to_string(),
            gstore_endpoint: "http://127.0.0.1:9000/sparql".to_string(),
            gstore_user: None,
            gstore_password: None,
            max_tokens: 1024,
            timeout_secs: 60,
            mode: "auto".to_string(),
            cache_capacity: 256,
            cache_ttl_secs: 0,
            http_token: None,
            http_allow_unauthenticated: false,
            http_rate_per_min: crate::http_server::DEFAULT_REQUESTS_PER_MINUTE,
            max_question_bytes: crate::http_server::DEFAULT_MAX_QUESTION_BYTES,
            max_llm_calls: crate::http_server::DEFAULT_MAX_LLM_CALLS,
            max_kb_rows: crate::http_server::DEFAULT_MAX_KB_ROWS,
            request_timeout_secs: crate::http_server::DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

/// The host of an `http://` URL whose host is not a loopback address (`None`
/// for https, loopback hosts and unparsable URLs).
fn plain_http_remote_host(url: &str) -> Option<String> {
    let rest = url.trim();
    let (scheme, rest) = rest.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    let loopback = host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    (!host.is_empty() && !loopback).then_some(host)
}

/// Treat empty strings as absent.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Canonicalize the LLM provider to `"anthropic"` or `"openai"` (the only two
/// [`from_env`](Config::from_env)/build_engine understand). Absent → `anthropic`;
/// case/whitespace-insensitive; common aliases accepted; an unrecognized value
/// warns and falls back to `anthropic` rather than silently misrouting.
fn normalize_provider(v: Option<String>) -> String {
    match non_empty(v) {
        None => "anthropic".to_string(),
        Some(s) => match s.trim().to_lowercase().as_str() {
            "anthropic" | "claude" => "anthropic".to_string(),
            "openai" | "oai" | "deepseek" | "compatible" => "openai".to_string(),
            other => {
                eprintln!(
                    "gnlqa: warning: unknown GNLQA_LLM_PROVIDER {other:?}; \
                     using 'anthropic' (valid: anthropic, openai)"
                );
                "anthropic".to_string()
            }
        },
    }
}

/// Parse an on/off switch (`on|off|true|false|yes|no|1|0`, case-insensitive);
/// absent → off. An unrecognized value warns and stays off (the safe side).
fn parse_switch(v: Option<String>) -> bool {
    let Some(s) = non_empty(v) else { return false };
    match s.trim().to_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => true,
        "off" | "false" | "no" | "0" => false,
        other => {
            eprintln!("gnlqa: warning: switch value {other:?} is not on/off; using off");
            false
        }
    }
}

/// Parse an optional value, falling back to `default`. A *present but
/// unparseable* value is a likely typo, so warn rather than silently ignore it.
fn parse_or<T: std::str::FromStr>(v: Option<String>, default: T) -> T {
    let Some(s) = v else { return default };
    match s.parse() {
        Ok(x) => x,
        Err(_) => {
            eprintln!("gnlqa: warning: value {s:?} is not parseable; using default");
            default
        }
    }
}

/// The file name looked up in the per-user config directory.
pub const CONFIG_FILE_NAME: &str = "gnlqa.conf";

/// Where the config file is read from, given an environment lookup:
///
/// 1. `GNLQA_CONFIG`, if set and non-empty — an explicit path (relative paths
///    are relative to the working directory, so `GNLQA_CONFIG=./gnlqa.conf`
///    opts back into a file in the current directory);
/// 2. else `$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`;
/// 3. else `$HOME/.config/gnlqa/gnlqa.conf`.
///
/// A `gnlqa.conf` in the current directory is **not** read by default
/// (review cluster-nlqa#15): an untrusted checkout could ship one that
/// points `OPENAI_BASE_URL` / `GSTORE_ENDPOINT` at a collector while the API
/// key and the gStore password come from the environment.
pub fn config_file_path(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(p) = non_empty(var("GNLQA_CONFIG")) {
        return Some(PathBuf::from(p));
    }
    let base = non_empty(var("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| non_empty(var("HOME")).map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("gnlqa").join(CONFIG_FILE_NAME))
}

/// Load the `KEY=VALUE` config file found by [`config_file_path`]. Blank lines
/// and `#` comments are ignored; surrounding quotes are stripped. A missing
/// file → empty map (env-only). The loaded path is printed to stderr; a
/// `gnlqa.conf` in `cwd` that is not the configured file is pointed out
/// (and ignored); a world-writable file is refused.
fn load_config_file(
    var: &impl Fn(&str) -> Option<String>,
    cwd: Option<&Path>,
) -> HashMap<String, String> {
    let explicit = non_empty(var("GNLQA_CONFIG")).is_some();
    let path = config_file_path(var);
    if let Some(dir) = cwd {
        let local = dir.join(CONFIG_FILE_NAME);
        let same = path.as_ref().is_some_and(|p| {
            let abs = if p.is_absolute() {
                p.clone()
            } else {
                dir.join(p)
            };
            match (abs.canonicalize(), local.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        });
        if !same && local.is_file() {
            eprintln!(
                "gnlqa: note: ignoring ./{CONFIG_FILE_NAME} in the current directory — config \
                 is read only from $GNLQA_CONFIG or ~/.config/gnlqa/{CONFIG_FILE_NAME}; set \
                 GNLQA_CONFIG=./{CONFIG_FILE_NAME} if you trust it"
            );
        }
    }
    let Some(path) = path else {
        return HashMap::new();
    };
    let shown = path.display();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            if world_writable(&path) {
                eprintln!(
                    "gnlqa: warning: refusing to read {shown}: it is world-writable \
                     (anyone could redirect your credentials); chmod o-w it"
                );
                return HashMap::new();
            }
            let map = parse_config(&content);
            if !map.is_empty() {
                let how = if explicit { " (GNLQA_CONFIG)" } else { "" };
                eprintln!("gnlqa: loaded {} setting(s) from {shown}{how}", map.len());
            }
            map
        }
        Err(e) => {
            // An explicitly named file that can't be read is a mistake worth
            // saying out loud; the default location is simply optional.
            if explicit {
                eprintln!("gnlqa: warning: cannot read GNLQA_CONFIG={shown}: {e}");
            }
            HashMap::new()
        }
    }
}

/// Whether `path` is writable by every user (Unix permission bits).
#[cfg(unix)]
fn world_writable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o002 != 0)
}

#[cfg(not(unix))]
fn world_writable(_path: &Path) -> bool {
    false
}

/// Parse a `KEY=VALUE` config body. Blank lines and `#` comments are ignored;
/// surrounding quotes are stripped.
fn parse_config(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            if !k.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_reads_pairs_comments_and_quotes() {
        let body = "\
            # a comment\n\
            GNLQA_LLM_PROVIDER=openai\n\
            OPENAI_BASE_URL = https://api.deepseek.com \n\
            OPENAI_API_KEY=\"sk-abc\"\n\
            \n\
            GNLQA_MODEL='deepseek-v4-pro'\n\
            no_equals_line\n";
        let m = parse_config(body);
        assert_eq!(m.get("GNLQA_LLM_PROVIDER").unwrap(), "openai");
        assert_eq!(
            m.get("OPENAI_BASE_URL").unwrap(),
            "https://api.deepseek.com"
        ); // trimmed
        assert_eq!(m.get("OPENAI_API_KEY").unwrap(), "sk-abc"); // quotes stripped
        assert_eq!(m.get("GNLQA_MODEL").unwrap(), "deepseek-v4-pro");
        assert!(!m.contains_key("no_equals_line"));
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn parse_or_warns_and_defaults_on_garbage() {
        assert_eq!(parse_or(Some("42".to_string()), 0u32), 42);
        assert_eq!(parse_or(None, 7u32), 7);
        assert_eq!(parse_or(Some("nope".to_string()), 9u32), 9); // unparseable → default
    }

    #[test]
    fn default_provider_is_anthropic_and_openai_fields_present() {
        let c = Config::default();
        assert_eq!(c.llm_provider, "anthropic");
        assert!(c.openai_api_key.is_none());
        assert_eq!(c.openai_base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn normalize_provider_canonicalizes_and_defaults() {
        assert_eq!(normalize_provider(None), "anthropic");
        assert_eq!(normalize_provider(Some("   ".into())), "anthropic"); // blank → default
        assert_eq!(normalize_provider(Some("OpenAI".into())), "openai"); // case-insensitive
        assert_eq!(normalize_provider(Some(" deepseek ".into())), "openai"); // alias + trim
        assert_eq!(normalize_provider(Some("claude".into())), "anthropic"); // alias
        assert_eq!(normalize_provider(Some("gpt5".into())), "anthropic"); // unknown → safe default
    }

    #[test]
    fn non_empty_treats_blank_as_absent() {
        assert!(non_empty(Some("".into())).is_none());
        assert!(non_empty(Some("   ".into())).is_none());
        assert_eq!(non_empty(Some("x".into())).as_deref(), Some("x"));
    }

    #[test]
    fn default_has_cache_and_token_floor() {
        let c = Config::default();
        assert_eq!(c.cache_capacity, 256);
        assert_eq!(c.cache_ttl_secs, 0);
        assert_eq!(c.max_tokens, 1024);
    }

    #[test]
    fn server_limits_default_to_the_http_server_defaults() {
        // review cluster-nlqa#13: the config defaults and HttpServer::bind's
        // defaults must agree, and the server is never anonymous by default.
        let c = Config::default();
        assert_eq!(c.http_limits(), HttpLimits::default());
        assert!(matches!(c.http_auth(), HttpAuth::Unconfigured));
        let t = Config {
            http_token: Some(Secret::new("t")),
            ..Config::default()
        };
        assert!(matches!(t.http_auth(), HttpAuth::Token(_)));
        let open = Config {
            http_allow_unauthenticated: true,
            ..Config::default()
        };
        assert!(matches!(open.http_auth(), HttpAuth::LoopbackOnly));
        let unlimited = Config {
            max_llm_calls: 0,
            max_kb_rows: 0,
            request_timeout_secs: 0,
            ..Config::default()
        };
        assert_eq!(unlimited.cost_limits(), CostLimits::unlimited());
    }

    #[test]
    fn switches_parse_on_off_and_default_off() {
        assert!(parse_switch(Some("on".into())));
        assert!(parse_switch(Some(" TRUE ".into())));
        assert!(!parse_switch(Some("off".into())));
        assert!(!parse_switch(None));
        assert!(!parse_switch(Some("maybe".into())));
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn config_path_prefers_explicit_then_xdg_then_home_never_cwd() {
        // review cluster-nlqa#15
        assert_eq!(
            config_file_path(lookup(&[("GNLQA_CONFIG", "/etc/x.conf"), ("HOME", "/h")])),
            Some(PathBuf::from("/etc/x.conf"))
        );
        assert_eq!(
            config_file_path(lookup(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")])),
            Some(PathBuf::from("/xdg/gnlqa/gnlqa.conf"))
        );
        // a relative XDG_CONFIG_HOME is invalid per the spec and ignored
        assert_eq!(
            config_file_path(lookup(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/gnlqa/gnlqa.conf"))
        );
        assert_eq!(
            config_file_path(lookup(&[("GNLQA_CONFIG", "  "), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/gnlqa/gnlqa.conf"))
        );
        assert_eq!(config_file_path(lookup(&[])), None);
    }

    #[test]
    fn plain_http_credentials_to_remote_hosts_are_flagged() {
        assert_eq!(
            plain_http_remote_host("http://kb.internal:9000/sparql").as_deref(),
            Some("kb.internal")
        );
        assert_eq!(
            plain_http_remote_host("http://u:p@10.0.0.5/x").as_deref(),
            Some("10.0.0.5")
        );
        assert!(plain_http_remote_host("https://api.deepseek.com").is_none());
        assert!(plain_http_remote_host("http://127.0.0.1:9000/sparql").is_none());
        assert!(plain_http_remote_host("http://localhost:9000").is_none());
        assert!(plain_http_remote_host("http://[::1]:9000/sparql").is_none());
        let c = Config {
            gstore_endpoint: "http://kb.internal:9000/sparql".into(),
            gstore_password: Some(Secret::new("pw")),
            ..Config::default()
        };
        let w = c.security_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("GSTORE_ENDPOINT") && !w[0].contains("pw"));
        // no credential, nothing to leak
        let anon = Config {
            gstore_endpoint: "http://kb.internal:9000/sparql".into(),
            ..Config::default()
        };
        assert!(anon.security_warnings().is_empty());
    }
}
