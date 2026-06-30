//! A blocking Anthropic (Claude) Messages-API client over HTTPS (`ureq` + rustls).

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::secret::Secret;

use super::{LlmClient, LlmRequest};

/// Client for the Anthropic Messages API (`POST {base}/v1/messages`).
#[derive(Debug, Clone)]
pub struct AnthropicClient {
    api_key: Secret,
    base_url: String,
    default_model: String,
    timeout: Duration,
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
        }
    }

    /// Override the per-request network timeout (builder-style).
    pub fn with_timeout(mut self, timeout: Duration) -> AnthropicClient {
        self.timeout = timeout;
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
        })
    }

    /// Serialize a request into the Messages-API JSON body (also used by tests).
    pub fn request_body(&self, req: &LlmRequest) -> Value {
        let model = req.model.clone().unwrap_or_else(|| self.default_model.clone());
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| json!({ "role": m.role.as_str(), "content": m.content }))
            .collect();
        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
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
        let resp = ureq::post(&url)
            .set("x-api-key", self.api_key.expose())
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .timeout(self.timeout)
            .send_json(body)
            // On a non-2xx, ureq returns Status(code, response); the Anthropic
            // JSON error body holds the real reason, so surface it.
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let body = resp.into_string().unwrap_or_default();
                    Error::Http(format!("HTTP {code}: {body}"))
                }
                other => Error::Http(other.to_string()),
            })?;
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
