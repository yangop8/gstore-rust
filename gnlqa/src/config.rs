//! Runtime configuration, sourced from the environment so secrets are never
//! hard-coded. Read it once at startup with [`Config::from_env`].

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
    /// Build a config from environment variables, falling back to sensible
    /// defaults. Model IDs default to the latest Claude family.
    pub fn from_env() -> Config {
        Config {
            anthropic_api_key: non_empty(env::var("ANTHROPIC_API_KEY").ok()).map(Secret::new),
            anthropic_base_url: env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string()),
            model: env::var("GNLQA_MODEL").unwrap_or_else(|_| "claude-opus-4-8".to_string()),
            fast_model: env::var("GNLQA_FAST_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-6".to_string()),
            gstore_endpoint: env::var("GSTORE_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:9000/sparql".to_string()),
            gstore_user: non_empty(env::var("GSTORE_USER").ok()),
            gstore_password: non_empty(env::var("GSTORE_PASSWORD").ok()).map(Secret::new),
            max_tokens: env_parse("GNLQA_MAX_TOKENS", 1024),
            temperature: env_parse("GNLQA_TEMPERATURE", 0.0),
            timeout_secs: env_parse("GNLQA_TIMEOUT_SECS", 60),
        }
    }

    /// Whether live LLM calls are possible (an API key is present).
    pub fn has_api_key(&self) -> bool {
        self.anthropic_api_key.is_some()
    }
}

impl Default for Config {
    /// Pure, side-effect-free defaults (no env reads) so `..Default::default()`
    /// and tests behave predictably. Use [`Config::from_env`] for real startup.
    fn default() -> Self {
        Config {
            anthropic_api_key: None,
            anthropic_base_url: "https://api.anthropic.com".to_string(),
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

/// Parse an env var, falling back to `default`. A *present but unparseable*
/// value is a likely typo, so warn rather than silently ignore it.
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    match env::var(key) {
        Ok(s) => match s.parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("gnlqa: warning: env {key}={s:?} is not parseable; using default");
                default
            }
        },
        Err(_) => default,
    }
}
