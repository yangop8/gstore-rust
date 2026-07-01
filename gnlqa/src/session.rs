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
    let cleaned = clean_rewrite(&raw);
    // If the model returned nothing usable, fall back to the raw question rather
    // than sending an empty query downstream.
    if cleaned.is_empty() {
        Ok(question.to_string())
    } else {
        Ok(cleaned)
    }
}

/// The label some models prefix onto the rewritten question.
const REWRITE_LABEL: &str = "standalone question:";

/// Extract the rewritten question from the model's raw output. Robust to:
/// code fences; a `Standalone question:` label whether inline or **on its own
/// line** (that line strips to empty and we keep scanning — otherwise the label
/// would be mistaken for the answer and the rewrite silently lost); and a
/// *balanced* surrounding quote pair (embedded/dangling quotes are left intact,
/// so `"Blade Runner" director?` is not mangled).
fn clean_rewrite(raw: &str) -> String {
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let body = strip_label(line);
        if body.is_empty() {
            continue; // the line was only the label — keep scanning
        }
        let body = if body.len() >= 2 && body.starts_with('"') && body.ends_with('"') {
            body[1..body.len() - 1].trim()
        } else {
            body
        };
        return body.to_string();
    }
    String::new()
}

/// Case-insensitively strip a leading [`REWRITE_LABEL`] (and following spaces).
/// Uses `get(..)` so a multibyte char at the boundary can't panic.
fn strip_label(s: &str) -> &str {
    match s.get(..REWRITE_LABEL.len()) {
        Some(head) if head.eq_ignore_ascii_case(REWRITE_LABEL) => s[REWRITE_LABEL.len()..].trim(),
        _ => s,
    }
}

/// A bounded, rewrite-friendly snapshot of an answer to fold into later prompts.
/// Long list answers are truncated (the rewrite prompt input is otherwise
/// uncapped); non-answers (abstained / no results) become empty so they don't
/// mislead pronoun resolution.
fn summarize_answer(a: &Answer) -> String {
    if a.abstained {
        return String::new();
    }
    let t = a.text.trim();
    if t.is_empty() || t == "(no results)" {
        return String::new();
    }
    const CAP: usize = 240;
    if t.chars().count() > CAP {
        let s: String = t.chars().take(CAP).collect();
        format!("{s}…")
    } else {
        t.to_string()
    }
}

/// Hard cap on retained turns. Only the last [`HISTORY_WINDOW`] feed the
/// rewriter, but we keep a bit more for [`Session::history`] introspection
/// without growing unbounded over a long-running chat.
const MAX_TURNS: usize = 64;

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
    /// We record the **resolved** standalone question (so multi-hop ellipsis
    /// stays resolved) plus a bounded snapshot of the answer, so the rewriter
    /// gets stable grounding without unbounded prompt growth.
    pub fn ask(&mut self, question: &str) -> Result<Answer> {
        let standalone = self.engine.rewrite_followup_question(&self.history, question);
        let answer = self.engine.ask(&standalone)?;
        self.history.push((standalone, summarize_answer(&answer)));
        self.trim_history();
        Ok(answer)
    }

    /// Drop the oldest turns once retention exceeds [`MAX_TURNS`].
    fn trim_history(&mut self) {
        let n = self.history.len();
        if n > MAX_TURNS {
            self.history.drain(0..n - MAX_TURNS);
        }
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
    fn label_on_its_own_line_is_not_mistaken_for_the_answer() {
        // MEDIUM fix: the label alone strips to empty; scanning must continue.
        let llm = MockLlm::fixed("Standalone question:\nWho directed Alien?");
        let history = vec![("What is Alien?".into(), "a film".into())];
        let out = rewrite_followup(&llm, &history, "who directed it?", None).unwrap();
        assert_eq!(out, "Who directed Alien?");
    }

    #[test]
    fn does_not_mangle_leading_quoted_phrase() {
        // Only a *balanced* wrapping quote pair is stripped; a leading quote with
        // a trailing '?' must be left intact.
        assert_eq!(clean_rewrite("\"Blade Runner\" director?"), "\"Blade Runner\" director?");
        assert_eq!(clean_rewrite("\"Who directed Alien?\""), "Who directed Alien?");
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
        assert_eq!(sess.history()[0].0, "first?"); // empty history → verbatim
        // Turn 2 records the *resolved* standalone question, not the raw one.
        assert!(!sess.history()[1].0.is_empty());
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
