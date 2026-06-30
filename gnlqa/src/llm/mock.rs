//! A deterministic mock [`LlmClient`] for tests — no network, no API key.

use std::sync::Mutex;

use crate::error::{Error, Result};

use super::{LlmClient, LlmRequest};

/// Returns canned responses in order (cycling once exhausted), and records the
/// requests it received so tests can assert on prompts.
#[derive(Debug, Default)]
pub struct MockLlm {
    responses: Vec<String>,
    state: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    next: usize,
    seen: Vec<LlmRequest>,
}

impl MockLlm {
    /// A mock that returns `responses` in order, cycling when exhausted.
    pub fn new(responses: Vec<String>) -> MockLlm {
        MockLlm { responses, state: Mutex::new(MockState::default()) }
    }

    /// A mock that always returns the same text.
    pub fn fixed(text: impl Into<String>) -> MockLlm {
        MockLlm::new(vec![text.into()])
    }

    /// How many completions have been requested.
    pub fn call_count(&self) -> usize {
        self.state.lock().unwrap().seen.len()
    }

    /// The system prompt of the last request, if any.
    pub fn last_system(&self) -> Option<String> {
        self.state.lock().unwrap().seen.last().and_then(|r| r.system.clone())
    }

    /// The last user message content, if any.
    pub fn last_user(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .seen
            .last()
            .and_then(|r| r.messages.last().map(|m| m.content.clone()))
    }
}

impl LlmClient for MockLlm {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        let mut st = self.state.lock().unwrap();
        st.seen.push(req.clone());
        if self.responses.is_empty() {
            return Err(Error::Llm("MockLlm has no canned responses".into()));
        }
        let idx = st.next % self.responses.len();
        st.next += 1;
        Ok(self.responses[idx].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_responses_and_records_requests() {
        let m = MockLlm::new(vec!["a".into(), "b".into()]);
        assert_eq!(m.complete(&LlmRequest::prompt("q1")).unwrap(), "a");
        assert_eq!(m.complete(&LlmRequest::prompt("q2").system("sys")).unwrap(), "b");
        assert_eq!(m.complete(&LlmRequest::prompt("q3")).unwrap(), "a"); // cycles
        assert_eq!(m.call_count(), 3);
        assert_eq!(m.last_user().as_deref(), Some("q3"));
    }

    #[test]
    fn empty_mock_errors() {
        let m = MockLlm::new(vec![]);
        assert!(m.complete(&LlmRequest::prompt("q")).is_err());
    }
}
