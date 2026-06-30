//! Runtime configuration, sourced from the environment so secrets are never
//! hard-coded. Read it once at startup with [`Config::from_env`].

use std::env;

/// gNLQA runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Anthropic API key (`ANTHROPIC_API_KEY`). `None` ⇒ live LLM calls fail;
    /// the rest of the system (and all tests, via a mock client) still works.
    pub anthropic_api_key: Option<String>,
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
    pub gstore_password: Option<String>,
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
            anthropic_api_key: non_empty(env::var("ANTHROPIC_API_KEY").ok()),
            anthropic_base_url: env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string()),
            model: env::var("GNLQA_MODEL").unwrap_or_else(|_| "claude-opus-4-8".to_string()),
            fast_model: env::var("GNLQA_FAST_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-6".to_string()),
            gstore_endpoint: env::var("GSTORE_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:9000/sparql".to_string()),
            gstore_user: non_empty(env::var("GSTORE_USER").ok()),
            gstore_password: non_empty(env::var("GSTORE_PASSWORD").ok()),
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
    fn default() -> Self {
        Config::from_env()
    }
}

/// Treat empty strings as absent.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Parse an env var, falling back to `default` on absence/parse error.
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}
