//! A blocking OpenAI-compatible Chat-Completions client over HTTPS (`ureq` +
//! rustls). Works with OpenAI and any compatible endpoint — DeepSeek, Together,
//! Groq, a local vLLM, etc. — so gNLQA isn't tied to a single provider.
//!
//! Wire shape differs from Anthropic's Messages API: `POST {base}/chat/completions`,
//! `Authorization: Bearer …`, the system prompt is a leading `{role:"system"}`
//! message, and the reply is `choices[0].message.content`.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::secret::Secret;

use super::{LlmClient, LlmRequest};

/// A minimum token budget. Reasoning models (e.g. deepseek-v4-pro) spend
/// completion tokens on hidden reasoning *before* the visible answer, so a small
/// `max_tokens` can yield an empty `content`. Floor the budget so short answers
/// still come back.
const MIN_MAX_TOKENS: u32 = 1024;

/// Client for an OpenAI-compatible Chat Completions API.
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    api_key: Secret,
    base_url: String,
    default_model: String,
    timeout: Duration,
    /// Minimum `max_tokens` floored onto every request (never below
    /// [`MIN_MAX_TOKENS`]). Raised via `GNLQA_MAX_TOKENS` for reasoning models.
    max_tokens_floor: u32,
}

impl OpenAiClient {
    /// Construct directly (default 60s timeout).
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        default_model: impl Into<String>,
    ) -> OpenAiClient {
        OpenAiClient {
            api_key: Secret::new(api_key),
            base_url: base_url.into(),
            default_model: default_model.into(),
            timeout: Duration::from_secs(60),
            max_tokens_floor: MIN_MAX_TOKENS,
        }
    }

    /// Override the per-request network timeout (builder-style).
    pub fn with_timeout(mut self, timeout: Duration) -> OpenAiClient {
        self.timeout = timeout;
        self
    }

    /// Set the minimum completion-token budget floored onto every request
    /// (clamped to at least [`MIN_MAX_TOKENS`], so reasoning models still return
    /// content). Wired from `GNLQA_MAX_TOKENS`.
    pub fn with_max_tokens(mut self, floor: u32) -> OpenAiClient {
        self.max_tokens_floor = floor.max(MIN_MAX_TOKENS);
        self
    }

    /// Build from [`Config`] (`openai_*` fields); errors if no key is set.
    pub fn from_config(cfg: &Config) -> Result<OpenAiClient> {
        let key = cfg
            .openai_api_key
            .clone()
            .ok_or_else(|| Error::Config("OPENAI_API_KEY is not set".into()))?;
        Ok(OpenAiClient {
            api_key: key,
            base_url: cfg.openai_base_url.clone(),
            default_model: cfg.model.clone(),
            timeout: Duration::from_secs(cfg.timeout_secs),
            max_tokens_floor: cfg.max_tokens.max(MIN_MAX_TOKENS),
        })
    }

    /// Serialize a request into the Chat-Completions JSON body (also used by tests).
    pub fn request_body(&self, req: &LlmRequest) -> Value {
        let model = req.model.clone().unwrap_or_else(|| self.default_model.clone());
        // The system prompt becomes a leading system message.
        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = &req.system {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        for m in &req.messages {
            messages.push(json!({ "role": m.role.as_str(), "content": m.content }));
        }
        json!({
            "model": model,
            "max_tokens": req.max_tokens.max(self.max_tokens_floor),
            "temperature": req.temperature,
            "messages": messages,
        })
    }

    /// Extract the assistant text from a Chat-Completions response body.
    pub fn extract_text(resp: &Value) -> Result<String> {
        let content = resp["choices"][0]["message"]["content"].as_str().ok_or_else(|| {
            Error::Llm(format!("response has no choices[0].message.content: {resp}"))
        })?;
        if content.trim().is_empty() {
            // A reasoning model that exhausted its budget on reasoning returns an
            // empty content — treat as a failed completion so callers can degrade.
            return Err(Error::Llm(format!(
                "empty completion (a reasoning model may have exhausted max_tokens): {resp}"
            )));
        }
        Ok(content.to_string())
    }
}

impl LlmClient for OpenAiClient {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = self.request_body(req);
        let resp = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.api_key.expose()))
            .set("content-type", "application/json")
            .timeout(self.timeout)
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let body = resp.into_string().unwrap_or_default();
                    Error::Http(format!("HTTP {code}: {body}"))
                }
                other => Error::Http(other.to_string()),
            })?;
        let v: Value = resp.into_json().map_err(|e| Error::Json(e.to_string()))?;
        OpenAiClient::extract_text(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_maps_system_to_leading_message() {
        let c = OpenAiClient::new("k", "https://api.deepseek.com", "deepseek-v4-pro");
        let req = LlmRequest::prompt("hi").system("be terse").max_tokens(64);
        let b = c.request_body(&req);
        assert_eq!(b["model"], "deepseek-v4-pro");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][0]["content"], "be terse");
        assert_eq!(b["messages"][1]["role"], "user");
        assert_eq!(b["messages"][1]["content"], "hi");
        // small budgets are floored so a reasoning model still returns content
        assert_eq!(b["max_tokens"], MIN_MAX_TOKENS);
    }

    #[test]
    fn model_override_and_no_system() {
        let c = OpenAiClient::new("k", "u", "default");
        let req = LlmRequest::prompt("x").model("deepseek-v4-flash").max_tokens(4096);
        let b = c.request_body(&req);
        assert_eq!(b["model"], "deepseek-v4-flash");
        assert_eq!(b["messages"][0]["role"], "user"); // no system message prepended
        assert_eq!(b["max_tokens"], 4096); // larger budget preserved
    }

    #[test]
    fn with_max_tokens_raises_the_floor() {
        // GNLQA_MAX_TOKENS wiring: a higher floor lifts small requests to it.
        let c = OpenAiClient::new("k", "u", "m").with_max_tokens(8192);
        assert_eq!(c.request_body(&LlmRequest::prompt("x").max_tokens(64))["max_tokens"], 8192);
        // a request already above the floor is preserved
        assert_eq!(c.request_body(&LlmRequest::prompt("x").max_tokens(9000))["max_tokens"], 9000);
        // the floor can never drop below the reasoning-model minimum
        let low = OpenAiClient::new("k", "u", "m").with_max_tokens(10);
        assert_eq!(low.request_body(&LlmRequest::prompt("x").max_tokens(1))["max_tokens"], MIN_MAX_TOKENS);
    }

    #[test]
    fn extract_text_reads_choice_content() {
        let v = json!({"choices":[{"message":{"role":"assistant","content":"Ridley Scott"}}]});
        assert_eq!(OpenAiClient::extract_text(&v).unwrap(), "Ridley Scott");
    }

    #[test]
    fn extract_text_errors_on_empty_or_missing() {
        let empty = json!({"choices":[{"message":{"content":""}}]});
        assert!(OpenAiClient::extract_text(&empty).is_err());
        let missing = json!({"choices":[]});
        assert!(OpenAiClient::extract_text(&missing).is_err());
    }
}
