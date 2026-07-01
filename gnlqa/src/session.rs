//! Multi-turn conversation support (C15). A follow-up question is often
//! elliptical ("and its capital?", "who directed *it*?") — it can't be answered
//! on its own. [`rewrite_followup`] asks the LLM to fold the conversation so far
//! into a single **standalone** question, which then flows through the normal
//! [`SolveEngine`](crate::solve::SolveEngine) pipeline unchanged. [`Session`] is
//! a thin stateful wrapper that tracks the `(question, answer)` history for you.

use crate::error::Result;
use crate::llm::{LlmClient, LlmRequest};
use crate::pipeline::Answer;
use crate::solve::SolveEngine;

/// System prompt for the rewrite step: resolve pronouns/ellipsis against the
/// conversation, output nothing but the rewritten question.
const SYS_REWRITE: &str = "\
You rewrite a user's follow-up question into a fully self-contained question, \
resolving any pronouns or omitted subjects using the conversation so far. \
Keep the original language. Do NOT answer it. Output ONLY the rewritten \
question on a single line, with no quotes, prefix, or explanation. If the \
follow-up is already self-contained, output it unchanged.";

/// How many prior turns to feed the rewriter. Older context rarely helps resolve
/// the *current* ellipsis and only inflates the prompt.
const HISTORY_WINDOW: usize = 6;

/// Rewrite a (possibly elliptical) `question` into a standalone one using prior
/// `(question, answer)` turns. Returns the question unchanged when there is no
/// history. Only the last [`HISTORY_WINDOW`] turns are used. `model` overrides
/// the client default — this is a cheap step, so route it to a fast model.
pub fn rewrite_followup(
    llm: &dyn LlmClient,
    history: &[(String, String)],
    question: &str,
    model: Option<&str>,
) -> Result<String> {
    if history.is_empty() {
        return Ok(question.to_string());
    }
    let start = history.len().saturating_sub(HISTORY_WINDOW);
    let convo: String = history[start..]
        .iter()
        .map(|(q, a)| format!("Q: {q}\nA: {a}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("Conversation so far:\n{convo}\n\nFollow-up: {question}\n\nStandalone question:");
    let mut req = LlmRequest::prompt(user).system(SYS_REWRITE);
    if let Some(m) = model {
        req = req.model(m);
    }
    let raw = llm.complete(&req)?;
    // Models sometimes still wrap in a fence or add a leading label — take the
    // first non-empty line and strip a surrounding pair of quotes.
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```"))
        .unwrap_or("")
        .trim();
    let cleaned = line
        .strip_prefix("Standalone question:")
        .unwrap_or(line)
        .trim()
        .trim_matches('"')
        .trim();
    // If the model returned nothing usable, fall back to the raw question rather
    // than sending an empty query downstream.
    if cleaned.is_empty() {
        Ok(question.to_string())
    } else {
        Ok(cleaned.to_string())
    }
}

/// A stateful multi-turn conversation over a [`SolveEngine`]. Each [`ask`](Self::ask)
/// rewrites the question against the accumulated history, solves it, and records
/// the turn. Borrow-only: it holds the engine by reference so one engine can back
/// many sessions.
pub struct Session<'a> {
    engine: &'a SolveEngine,
    history: Vec<(String, String)>,
}

impl<'a> Session<'a> {
    /// Start an empty conversation backed by `engine`.
    pub fn new(engine: &'a SolveEngine) -> Self {
        Session { engine, history: Vec::new() }
    }

    /// Ask the next turn: rewrite against history, solve, and record the turn.
    /// The recorded answer text is the user-facing rendering, so the rewriter
    /// sees what the user saw.
    pub fn ask(&mut self, question: &str) -> Result<Answer> {
        let answer = self.engine.ask_followup(&self.history, question)?;
        self.history.push((question.to_string(), answer.text.clone()));
        Ok(answer)
    }

    /// The accumulated `(question, answer)` turns.
    pub fn history(&self) -> &[(String, String)] {
        &self.history
    }

    /// Number of turns recorded so far.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Whether no turns have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Forget all prior turns (start a fresh topic on the same engine).
    pub fn clear(&mut self) {
        self.history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;

    #[test]
    fn empty_history_returns_question_verbatim() {
        let llm = MockLlm::fixed("SHOULD NOT BE CALLED");
        let out = rewrite_followup(&llm, &[], "who directed Alien?", None).unwrap();
        assert_eq!(out, "who directed Alien?");
        assert_eq!(llm.call_count(), 0); // no LLM call when there's no history
    }

    #[test]
    fn rewrites_followup_using_history() {
        let llm = MockLlm::fixed("Who directed Alien?");
        let history = vec![("What is Alien?".into(), "A 1979 sci-fi film.".into())];
        let out = rewrite_followup(&llm, &history, "who directed it?", None).unwrap();
        assert_eq!(out, "Who directed Alien?");
        assert_eq!(llm.call_count(), 1);
    }

    #[test]
    fn strips_fences_quotes_and_labels() {
        let llm = MockLlm::fixed("```\nStandalone question: \"Who directed Alien?\"\n```");
        let history = vec![("x".into(), "y".into())];
        let out = rewrite_followup(&llm, &history, "and the director?", None).unwrap();
        assert_eq!(out, "Who directed Alien?");
    }

    #[test]
    fn blank_rewrite_falls_back_to_original() {
        let llm = MockLlm::fixed("   \n  ");
        let history = vec![("x".into(), "y".into())];
        let out = rewrite_followup(&llm, &history, "original?", None).unwrap();
        assert_eq!(out, "original?");
    }

    #[test]
    fn history_window_caps_turns_fed() {
        // 8 turns of history, window is 6 → only the last 6 appear in the prompt.
        let llm = Arc::new(RecordingLlm::default());
        let mut history = Vec::new();
        for i in 0..8 {
            history.push((format!("q{i}"), format!("a{i}")));
        }
        let boxed: Arc<dyn LlmClient> = llm.clone();
        let _ = rewrite_followup(boxed.as_ref(), &history, "next?", None).unwrap();
        let prompt = llm.last_prompt();
        assert!(prompt.contains("q7") && prompt.contains("q2"));
        assert!(!prompt.contains("q1")); // trimmed by the window
    }

    #[test]
    fn session_tracks_history_across_turns() {
        // MockLlm returns the same string for every call; that's fine — we only
        // assert the session records turns and stays consistent.
        let llm = MockLlm::fixed(r#"SELECT ?x WHERE { ?x ?y ?z }"#);
        let ans = SparqlAnswer::Select {
            vars: vec!["x".to_string()],
            rows: vec![vec![Some(RdfTerm {
                kind: TermKind::Uri,
                value: "http://example.org/a".to_string(),
                datatype: None,
                lang: None,
            })]],
        };
        let kb = MockKb::new(vec![ans]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb));
        let mut sess = Session::new(&engine);
        assert!(sess.is_empty());
        let _ = sess.ask("first?").unwrap();
        let _ = sess.ask("and then?").unwrap();
        assert_eq!(sess.len(), 2);
        assert_eq!(sess.history()[0].0, "first?");
        assert_eq!(sess.history()[1].0, "and then?");
        sess.clear();
        assert!(sess.is_empty());
    }

    use std::sync::{Arc, Mutex};

    /// A tiny LLM that records the last user prompt it saw.
    #[derive(Default)]
    struct RecordingLlm {
        last: Mutex<String>,
    }
    impl RecordingLlm {
        fn last_prompt(&self) -> String {
            self.last.lock().unwrap().clone()
        }
    }
    impl LlmClient for RecordingLlm {
        fn complete(&self, req: &LlmRequest) -> Result<String> {
            if let Some(m) = req.messages.last() {
                *self.last.lock().unwrap() = m.content.clone();
            }
            Ok("rewritten".to_string())
        }
    }
}
