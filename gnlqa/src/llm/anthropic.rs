//! A blocking Anthropic (Claude) Messages-API client over HTTPS (`ureq` + rustls).

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{redact_userinfo, Error, Result};
use crate::secret::Secret;

use super::{LlmClient, LlmRequest};
use crate::net::RetryPolicy;

/// Client for the Anthropic Messages API (`POST {base}/v1/messages`).
#[derive(Debug, Clone)]
pub struct AnthropicClient {
    api_key: Secret,
    base_url: String,
    default_model: String,
    timeout: Duration,
    /// Minimum `max_tokens` floored onto every request (1 = effectively none).
    /// Wired from `GNLQA_MAX_TOKENS`.
    max_tokens_floor: u32,
    /// Retries of transient provider failures (429/5xx/529; review
    /// cluster-nlqa#16).
    retry: RetryPolicy,
}

impl AnthropicClient {
    /// Construct directly (default 60s timeout; see [`with_timeout`](Self::with_timeout)).
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        default_model: impl Into<String>,
    ) -> AnthropicClient {
        AnthropicClient {
            api_key: Secret::new(api_key),
            base_url: base_url.into(),
            default_model: default_model.into(),
            timeout: Duration::from_secs(60),
            max_tokens_floor: 1,
            retry: RetryPolicy::default(),
        }
    }

    /// How transient provider failures (429, 5xx, 529 "overloaded") are
    /// retried (default: 3 retries with exponential back-off honouring
    /// `Retry-After`).
    pub fn with_retry(mut self, retry: RetryPolicy) -> AnthropicClient {
        self.retry = retry;
        self
    }

    /// Override the per-request network timeout (builder-style).
    pub fn with_timeout(mut self, timeout: Duration) -> AnthropicClient {
        self.timeout = timeout;
        self
    }

    /// Set the minimum completion-token budget floored onto every request
    /// (clamped to ≥1). Wired from `GNLQA_MAX_TOKENS`.
    pub fn with_max_tokens(mut self, floor: u32) -> AnthropicClient {
        self.max_tokens_floor = floor.max(1);
        self
    }

    /// Build from [`Config`]; errors if no API key is set.
    pub fn from_config(cfg: &Config) -> Result<AnthropicClient> {
        let key = cfg
            .anthropic_api_key
            .clone()
            .ok_or_else(|| Error::Config("ANTHROPIC_API_KEY is not set".into()))?;
        Ok(AnthropicClient {
            api_key: key,
            base_url: cfg.anthropic_base_url.clone(),
            default_model: cfg.model.clone(),
            timeout: Duration::from_secs(cfg.timeout_secs),
            max_tokens_floor: cfg.max_tokens.max(1),
            retry: cfg.retry_policy(),
        })
    }

    /// Serialize a request into the Messages-API JSON body (also used by tests).
    pub fn request_body(&self, req: &LlmRequest) -> Value {
        let model = req
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| json!({ "role": m.role.as_str(), "content": m.content }))
            .collect();
        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens.max(self.max_tokens_floor),
            "temperature": req.temperature,
            "messages": messages,
        });
        if let Some(sys) = &req.system {
            body["system"] = json!(sys);
        }
        body
    }

    /// Extract the concatenated text from a Messages-API response body.
    pub fn extract_text(resp: &Value) -> Result<String> {
        let blocks = resp["content"]
            .as_array()
            .ok_or_else(|| Error::Llm(format!("response has no content array: {resp}")))?;
        let mut out = String::new();
        for b in blocks {
            if let Some(t) = b["text"].as_str() {
                out.push_str(t);
            }
        }
        if out.is_empty() {
            return Err(Error::Llm(format!("response had no text blocks: {resp}")));
        }
        Ok(out)
    }
}

impl LlmClient for AnthropicClient {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = self.request_body(req);
        // No redirects: `x-api-key` is not an Authorization header, so ureq
        // would carry it to whatever host a redirect names (review
        // cluster-nlqa#15).
        let agent = crate::net::agent();
        let resp = crate::net::send_with_retry(
            &self.retry,
            "the Anthropic API",
            crate::net::PROVIDER_RETRYABLE,
            || {
                agent
                    .post(&url)
                    .set("x-api-key", self.api_key.expose())
                    .set("anthropic-version", "2023-06-01")
                    .set("content-type", "application/json")
                    // never wait past the current request's time budget
                    .timeout(crate::budget::cap_timeout(self.timeout))
                    .send_json(body.clone())
                    .map_err(Box::new)
            },
        )
        // On a non-2xx, ureq returns Status(code, response); the Anthropic
        // JSON error body holds the real reason, so surface it.
        .map_err(|e| match *e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                Error::Http(redact_userinfo(&format!("HTTP {code}: {body}")))
            }
            // ureq prints the request URL, userinfo included (review
            // cluster-nlqa#18).
            other => Error::Http(redact_userinfo(&other.to_string())),
        })?;
        let resp = crate::net::refuse_redirect(resp, "the Anthropic API endpoint")?;
        let v: Value = resp.into_json().map_err(|e| Error::Json(e.to_string()))?;
        AnthropicClient::extract_text(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Message;

    #[test]
    fn request_body_includes_system_and_messages() {
        let c = AnthropicClient::new("k", "https://api.anthropic.com", "claude-opus-4-8");
        let req = LlmRequest::prompt("hi").system("be terse").max_tokens(64);
        let b = c.request_body(&req);
        assert_eq!(b["model"], "claude-opus-4-8");
        assert_eq!(b["system"], "be terse");
        assert_eq!(b["max_tokens"], 64);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"], "hi");
    }

    #[test]
    fn model_override_wins() {
        let c = AnthropicClient::new("k", "u", "default-model");
        let req = LlmRequest::prompt("x").model("claude-sonnet-4-6");
        assert_eq!(c.request_body(&req)["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn extract_text_concatenates_blocks() {
        let v = json!({"content": [{"type":"text","text":"foo"},{"type":"text","text":"bar"}]});
        assert_eq!(AnthropicClient::extract_text(&v).unwrap(), "foobar");
    }

    #[test]
    fn extract_text_errors_without_text() {
        let v = json!({"content": []});
        assert!(AnthropicClient::extract_text(&v).is_err());
        // also: roundtrip a hand-built message
        let _ = Message::assistant("ok");
    }
}
