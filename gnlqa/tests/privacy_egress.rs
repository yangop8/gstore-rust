//! What reaches the LLM in each answering mode, checked with a recording LLM
//! stub against the documented privacy table (README "Answering modes &
//! privacy"): `structured` keeps result rows local, `open` uses no KB data,
//! `auto` sends result data only on its GraphRAG fallback.
//!
//! review cluster-nlqa#14 (follow-up rewrite replayed KB answers) and
//! cluster-nlqa#21 (structured mode with `with_explain`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gnlqa::{
    LlmClient, LlmRequest, MockKb, Mode, Provenance, RdfTerm, Session, SolveEngine, SparqlAnswer,
    TermKind,
};

/// Records every prompt (system + messages) and replies from a script.
struct RecordingLlm {
    script: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl RecordingLlm {
    fn new(script: &[&str]) -> Arc<RecordingLlm> {
        Arc::new(RecordingLlm {
            script: Mutex::new(script.iter().map(|s| s.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
    fn any_prompt_contains(&self, needle: &str) -> bool {
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains(needle))
    }
}

impl LlmClient for RecordingLlm {
    fn complete(&self, req: &LlmRequest) -> gnlqa::Result<String> {
        let mut all = req.system.clone().unwrap_or_default();
        for m in &req.messages {
            all.push('\n');
            all.push_str(&m.content);
        }
        self.prompts.lock().unwrap().push(all);
        Ok(self.script.lock().unwrap().pop_front().unwrap_or_default())
    }
}

const SECRET: &str = "http://ex/SECRETSALARY";

fn secret_kb() -> MockKb {
    MockKb::new(vec![SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: vec![vec![Some(RdfTerm {
            kind: TermKind::Uri,
            value: SECRET.into(),
            datatype: None,
            lang: None,
        })]],
    }])
}

const INTENT: &str = r#"{"qtype":"list"}"#;
const GEN: &str = r#"["SELECT ?x WHERE { ?x <http://ex/salary> ?o }"]"#;

#[test]
fn structured_answers_are_not_replayed_in_open_mode() {
    // turn 1 (structured): intent, generation; turn 2 (open): rewrite, answer
    let llm = RecordingLlm::new(&[INTENT, GEN, "what is SPARQL?", "A query language."]);
    let engine =
        SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(secret_kb())).with_citations(false);
    let mut sess = Session::new(&engine);
    let a1 = sess
        .ask_with_mode("list the salaries", Mode::Structured)
        .unwrap();
    assert_eq!(a1.provenance, Provenance::GStore);
    assert!(a1.text.contains("SECRETSALARY"));
    let a2 = sess.ask_with_mode("what is SPARQL?", Mode::Open).unwrap();
    assert_eq!(a2.provenance, Provenance::Llm);
    assert_eq!(llm.calls(), 4, "the rewrite step ran on turn 2");
    assert!(
        !llm.any_prompt_contains("SECRETSALARY"),
        "open mode ('no KB data involved') sent a structured KB answer to the LLM"
    );
}

#[test]
fn auto_mode_follow_up_does_not_replay_locally_computed_answers() {
    // turn 1 (auto, answered locally from SPARQL rows), turn 2 (auto):
    // rewrite, intent, generation
    let llm = RecordingLlm::new(&[INTENT, GEN, "and their managers?", INTENT, GEN]);
    let engine =
        SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(secret_kb())).with_citations(false);
    let mut sess = Session::new(&engine);
    let a1 = sess.ask_with_mode("list the salaries", Mode::Auto).unwrap();
    assert_eq!(a1.provenance, Provenance::GStore);
    sess.ask_with_mode("and their managers?", Mode::Auto)
        .unwrap();
    assert!(
        llm.any_prompt_contains("and their managers?"),
        "rewrite ran"
    );
    assert!(
        !llm.any_prompt_contains("SECRETSALARY"),
        "auto mode sent result data to the LLM outside its GraphRAG fallback"
    );
}

#[test]
fn open_mode_answers_stay_available_to_later_rewrites() {
    // An LLM-provenance answer holds no KB data, so it is still replayed.
    let llm = RecordingLlm::new(&[
        "Alien is a 1979 film.",
        "Who directed Alien?",
        "Ridley Scott.",
    ]);
    let engine = SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(secret_kb()));
    let mut sess = Session::new(&engine);
    sess.ask_with_mode("what is Alien?", Mode::Open).unwrap();
    sess.ask_with_mode("who directed it?", Mode::Open).unwrap();
    assert!(llm.any_prompt_contains("A: Alien is a 1979 film."));
}
