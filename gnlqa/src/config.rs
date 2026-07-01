//! Runtime configuration, sourced from the environment so secrets are never
//! hard-coded. Read it once at startup with [`Config::from_env`].

use std::collections::HashMap;
use std::env;

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
    /// Max tokens for an LLM completion.
    pub max_tokens: u32,
    /// Sampling temperature (low — we want deterministic SPARQL).
    pub temperature: f32,
    /// Per-request network timeout, seconds.
    pub timeout_secs: u64,
}

impl Config {
    /// Build a config from a config file plus environment variables (env wins),
    /// falling back to sensible defaults. The file (default `gnlqa.conf` in the
    /// working dir, or the path in `GNLQA_CONFIG`) is a simple `KEY=VALUE` list
    /// using the same names as the env vars — so you configure once instead of
    /// exporting every session. See `gnlqa.conf.example`.
    pub fn from_env() -> Config {
        let file = load_config_file();
        // env takes precedence over the file, so a one-off `KEY=… gnlqa …` still works.
        let get = |k: &str| env::var(k).ok().or_else(|| file.get(k).cloned());
        Config {
            anthropic_api_key: non_empty(get("ANTHROPIC_API_KEY")).map(Secret::new),
            anthropic_base_url: get("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            llm_provider: get("GNLQA_LLM_PROVIDER").unwrap_or_else(|| "anthropic".to_string()),
            openai_api_key: non_empty(get("OPENAI_API_KEY")).map(Secret::new),
            openai_base_url: get("OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            model: get("GNLQA_MODEL").unwrap_or_else(|| "claude-opus-4-8".to_string()),
            fast_model: get("GNLQA_FAST_MODEL").unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
            gstore_endpoint: get("GSTORE_ENDPOINT")
                .unwrap_or_else(|| "http://127.0.0.1:9000/sparql".to_string()),
            gstore_user: non_empty(get("GSTORE_USER")),
            gstore_password: non_empty(get("GSTORE_PASSWORD")).map(Secret::new),
            max_tokens: parse_or(get("GNLQA_MAX_TOKENS"), 1024),
            temperature: parse_or(get("GNLQA_TEMPERATURE"), 0.0),
            timeout_secs: parse_or(get("GNLQA_TIMEOUT_SECS"), 60),
        }
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
            temperature: 0.0,
            timeout_secs: 60,
        }
    }
}

/// Treat empty strings as absent.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
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

/// Load a `KEY=VALUE` config file (`GNLQA_CONFIG` path, else `gnlqa.conf` in the
/// working dir). Blank lines and `#` comments are ignored; surrounding quotes are
/// stripped. Missing file → empty map (env-only, as before).
fn load_config_file() -> HashMap<String, String> {
    let path = env::var("GNLQA_CONFIG").unwrap_or_else(|_| "gnlqa.conf".to_string());
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_config(&content),
        Err(_) => HashMap::new(), // missing file → env-only, as before
    }
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
        assert_eq!(m.get("OPENAI_BASE_URL").unwrap(), "https://api.deepseek.com"); // trimmed
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
}
