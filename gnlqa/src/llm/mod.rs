//! The LLM abstraction: a small [`LlmClient`] trait plus a real Anthropic
//! (Claude) client ([`anthropic::AnthropicClient`]) and a [`mock::MockLlm`] used
//! by tests so the whole system compiles and runs without an API key.

pub mod anthropic;
pub mod mock;

pub use anthropic::AnthropicClient;
pub use mock::MockLlm;

use crate::error::Result;

/// A chat turn role. The system prompt is carried separately on [`LlmRequest`],
/// so messages are only ever `User` or `Assistant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    /// The wire string the Anthropic API expects.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// A single conversation turn.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Message {
        Message { role: Role::User, content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Message {
        Message { role: Role::Assistant, content: content.into() }
    }
}

/// A completion request. Build a simple one with [`LlmRequest::prompt`], then
/// chain the builder setters.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// Optional system prompt (instructions, schema context).
    pub system: Option<String>,
    /// Conversation so far (at least one `User` message).
    pub messages: Vec<Message>,
    /// Model override; `None` ⇒ the client's default model.
    pub model: Option<String>,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl LlmRequest {
    /// A single-user-message request with default sampling.
    pub fn prompt(user: impl Into<String>) -> LlmRequest {
        LlmRequest {
            system: None,
            messages: vec![Message::user(user)],
            model: None,
            max_tokens: 1024,
            temperature: 0.0,
        }
    }

    pub fn system(mut self, s: impl Into<String>) -> LlmRequest {
        self.system = Some(s.into());
        self
    }
    pub fn model(mut self, m: impl Into<String>) -> LlmRequest {
        self.model = Some(m.into());
        self
    }
    pub fn max_tokens(mut self, n: u32) -> LlmRequest {
        self.max_tokens = n;
        self
    }
    pub fn temperature(mut self, t: f32) -> LlmRequest {
        self.temperature = t;
        self
    }
    /// Append a turn (for multi-turn conversations / self-repair loops).
    pub fn push(mut self, m: Message) -> LlmRequest {
        self.messages.push(m);
        self
    }
}

/// Anything that can answer an [`LlmRequest`] with assistant text.
///
/// `Send + Sync` so a client can be shared across request-handling threads.
pub trait LlmClient: Send + Sync {
    /// Run one completion, returning the assistant's text.
    fn complete(&self, req: &LlmRequest) -> Result<String>;
}

/// Forward through shared handles (lets a test keep an `Arc` to inspect a mock
/// while handing a boxed clone to the engine).
impl<T: LlmClient + ?Sized> LlmClient for std::sync::Arc<T> {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        (**self).complete(req)
    }
}
