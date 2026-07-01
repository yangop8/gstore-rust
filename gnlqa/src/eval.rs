//! Evaluation harness (C20). Loads KGQA benchmark datasets (QALD, LC-QuAD) and
//! scores gnlqa's answers with set-based precision / recall / F1 (the QALD
//! convention), macro-averaged over the questions.
//!
//! Two gold-answer sources are supported: answers embedded in the dataset (QALD
//! ships them) and answers obtained by executing the gold SPARQL against the KB
//! (LC-QuAD ships only the query). The predicted answer set is what
//! [`SolveEngine::ask`] returns for the question.

use std::collections::HashSet;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::solve::SolveEngine;

/// Precision / recall / F1 for one comparison (all in `[0, 1]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

/// Set-based P/R/F1 with the QALD empty-set convention: both empty ⇒ perfect;
/// exactly one empty ⇒ zero; otherwise overlap-based.
pub fn prf1(pred: &HashSet<String>, gold: &HashSet<String>) -> Metrics {
    if gold.is_empty() && pred.is_empty() {
        return Metrics { precision: 1.0, recall: 1.0, f1: 1.0 };
    }
    if gold.is_empty() || pred.is_empty() {
        return Metrics { precision: 0.0, recall: 0.0, f1: 0.0 };
    }
    let tp = pred.iter().filter(|x| gold.contains(*x)).count() as f64;
    let p = tp / pred.len() as f64;
    let r = tp / gold.len() as f64;
    let f1 = if p + r == 0.0 { 0.0 } else { 2.0 * p * r / (p + r) };
    Metrics { precision: p, recall: r, f1 }
}

/// One benchmark question with its gold answer(s) and/or gold query.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalQuestion {
    pub id: String,
    pub question: String,
    /// Gold answers if the dataset ships them (QALD); `None` for query-only sets.
    pub gold_answers: Option<HashSet<String>>,
    /// Gold SPARQL, if provided — used to derive gold answers via the KB.
    pub gold_sparql: Option<String>,
}

/// Normalize an answer surface form so `<http://x>`, `"lit"@en`, and bare values
/// compare equal across the prediction and the gold set.
pub fn normalize_answer(s: &str) -> String {
    let mut t = s.trim();
    if let Some(inner) = t.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
        t = inner;
    }
    // Drop a trailing language/datatype tag on a quoted literal, then the quotes.
    if t.starts_with('"') {
        if let Some(end) = t.rfind('"') {
            if end > 0 {
                t = &t[1..end];
            }
        }
    }
    t.trim().to_string()
}

/// Load a QALD-format dataset (`{"questions":[{id, question:[{language,string}],
/// query:{sparql}, answers:[...]}]}`). Extracts the English question string, the
/// gold SPARQL, and the gold answer set from the embedded answers.
pub fn load_qald(json: &str) -> Result<Vec<EvalQuestion>> {
    let v: Value = serde_json::from_str(json)?;
    let arr = v
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Json("QALD dataset: missing 'questions' array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for q in arr {
        let question = qald_question_text(q).unwrap_or_default();
        if question.is_empty() {
            continue; // no usable English string
        }
        let id = q.get("id").map(value_to_string).unwrap_or_default();
        let gold_sparql = q.pointer("/query/sparql").and_then(Value::as_str).map(str::to_string);
        let gold_answers = qald_answers(q);
        out.push(EvalQuestion { id, question, gold_answers, gold_sparql });
    }
    Ok(out)
}

/// Load an LC-QuAD v1-format dataset (a top-level array of
/// `{corrected_question, sparql_query}`). No gold answers are embedded, so
/// [`run_eval`] must resolve them via the KB (`resolve_gold_via_kb`).
pub fn load_lcquad(json: &str) -> Result<Vec<EvalQuestion>> {
    let v: Value = serde_json::from_str(json)?;
    let arr = v
        .as_array()
        .ok_or_else(|| Error::Json("LC-QuAD dataset: expected a top-level array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, q) in arr.iter().enumerate() {
        let question = q
            .get("corrected_question")
            .or_else(|| q.get("question"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if question.is_empty() {
            continue;
        }
        let id = q
            .get("_id")
            .or_else(|| q.get("id"))
            .map(value_to_string)
            .unwrap_or_else(|| i.to_string());
        let gold_sparql = q
            .get("sparql_query")
            .or_else(|| q.get("sparql"))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push(EvalQuestion { id, question, gold_answers: None, gold_sparql });
    }
    Ok(out)
}

/// The English question string from a QALD question (falls back to the first
/// available string).
fn qald_question_text(q: &Value) -> Option<String> {
    let arr = q.get("question").and_then(Value::as_array)?;
    let pick = |lang: &str| {
        arr.iter().find(|e| e.get("language").and_then(Value::as_str) == Some(lang))
    };
    let entry = pick("en").or_else(|| arr.first())?;
    entry.get("string").and_then(Value::as_str).map(str::to_string)
}

/// The gold answer set embedded in a QALD question (`None` if absent/empty).
/// QALD answer entries are SPARQL Results JSON, so we parse them with the same
/// `parse_results` + `answer_values` path predictions use — keeping gold and
/// prediction extraction symmetric (one bound cell per row; booleans as
/// true/false), rather than collecting every binding cell.
fn qald_answers(q: &Value) -> Option<HashSet<String>> {
    let answers = q.get("answers").and_then(Value::as_array)?;
    let mut set = HashSet::new();
    for a in answers {
        if let Ok(ans) = crate::kb::parse_results(a) {
            for v in crate::pipeline::answer_values(&ans) {
                set.insert(normalize_answer(&v));
            }
        }
    }
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Options controlling an evaluation run.
#[derive(Debug, Clone, Default)]
pub struct EvalOptions {
    /// Evaluate at most this many questions (`None` = all).
    pub limit: Option<usize>,
    /// Derive gold answers by executing the gold SPARQL against the KB (needed
    /// for query-only datasets like LC-QuAD).
    pub resolve_gold_via_kb: bool,
}

/// One question's score line.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionScore {
    pub id: String,
    pub metrics: Metrics,
}

/// The aggregate outcome of an evaluation run.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    /// Questions considered this run: `min(limit, dataset size)` (dataset size
    /// already excludes rows dropped at load time for an empty question string).
    pub total: usize,
    /// Questions actually scored (gold available).
    pub evaluated: usize,
    /// Questions skipped for lack of a gold answer set.
    pub skipped: usize,
    /// Macro-averaged metrics over the evaluated questions.
    pub macro_metrics: Metrics,
    pub per_question: Vec<QuestionScore>,
}

impl EvalReport {
    /// A compact human-readable summary.
    pub fn summary(&self) -> String {
        format!(
            "evaluated {}/{} (skipped {}) — macro P={:.3} R={:.3} F1={:.3}",
            self.evaluated,
            self.total,
            self.skipped,
            self.macro_metrics.precision,
            self.macro_metrics.recall,
            self.macro_metrics.f1,
        )
    }
}

/// Resolve the gold answer set for a question, or `None` if unavailable.
fn resolve_gold(engine: &SolveEngine, q: &EvalQuestion, opts: &EvalOptions) -> Option<HashSet<String>> {
    if let Some(g) = &q.gold_answers {
        return Some(g.iter().map(|v| normalize_answer(v)).collect());
    }
    if opts.resolve_gold_via_kb {
        if let Some(sq) = &q.gold_sparql {
            if let Ok(ans) = engine.kb().query(sq) {
                // Extract gold the SAME way predictions are (answer_values: one
                // bound cell per row, booleans as true/false) so pred and gold
                // are symmetric. An empty gold set means the query matched
                // nothing in this KB → unresolvable, so skip rather than let the
                // both-empty convention score a spurious 1.0.
                let s: HashSet<String> = crate::pipeline::answer_values(&ans)
                    .iter()
                    .map(|v| normalize_answer(v))
                    .collect();
                return if s.is_empty() { None } else { Some(s) };
            }
        }
    }
    None
}

/// Run gnlqa over `questions` and score it. Questions without a resolvable gold
/// set are skipped (counted in the report), not scored as failures.
pub fn run_eval(engine: &SolveEngine, questions: &[EvalQuestion], opts: &EvalOptions) -> EvalReport {
    let take = opts.limit.unwrap_or(questions.len());
    let mut per_question = Vec::new();
    let mut skipped = 0usize;
    let (mut sp, mut sr, mut sf) = (0.0f64, 0.0f64, 0.0f64);
    for q in questions.iter().take(take) {
        let Some(gold) = resolve_gold(engine, q, opts) else {
            skipped += 1;
            continue;
        };
        let pred: HashSet<String> = match engine.ask(&q.question) {
            Ok(a) => a.values.iter().map(|v| normalize_answer(v)).collect(),
            Err(_) => HashSet::new(), // a failed answer is an empty prediction
        };
        let m = prf1(&pred, &gold);
        sp += m.precision;
        sr += m.recall;
        sf += m.f1;
        per_question.push(QuestionScore { id: q.id.clone(), metrics: m });
    }
    let n = per_question.len();
    let macro_metrics = if n == 0 {
        Metrics { precision: 0.0, recall: 0.0, f1: 0.0 }
    } else {
        let d = n as f64;
        Metrics { precision: sp / d, recall: sr / d, f1: sf / d }
    };
    EvalReport {
        total: take.min(questions.len()),
        evaluated: n,
        skipped,
        macro_metrics,
        per_question,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prf1_conventions() {
        // perfect
        let m = prf1(&set(&["a", "b"]), &set(&["a", "b"]));
        assert!((m.f1 - 1.0).abs() < 1e-9);
        // both empty → perfect (QALD convention)
        assert_eq!(prf1(&set(&[]), &set(&[])).f1, 1.0);
        // one empty → zero
        assert_eq!(prf1(&set(&["a"]), &set(&[])).f1, 0.0);
        assert_eq!(prf1(&set(&[]), &set(&["a"])).f1, 0.0);
        // partial: pred {a,x}, gold {a,b} → p=1/2, r=1/2, f1=0.5
        let m = prf1(&set(&["a", "x"]), &set(&["a", "b"]));
        assert!((m.precision - 0.5).abs() < 1e-9);
        assert!((m.recall - 0.5).abs() < 1e-9);
        assert!((m.f1 - 0.5).abs() < 1e-9);
        // disjoint → 0
        assert_eq!(prf1(&set(&["x"]), &set(&["a"])).f1, 0.0);
    }

    #[test]
    fn normalize_strips_brackets_and_quotes() {
        assert_eq!(normalize_answer("<http://ex/a>"), "http://ex/a");
        assert_eq!(normalize_answer("\"Berlin\""), "Berlin");
        assert_eq!(normalize_answer("\"Berlin\"@en"), "Berlin");
        assert_eq!(normalize_answer("  bare  "), "bare");
    }

    #[test]
    fn load_qald_extracts_question_gold_and_sparql() {
        let json = r#"{"questions":[{
            "id":"7",
            "question":[{"language":"de","string":"Wer?"},{"language":"en","string":"Who directed Alien?"}],
            "query":{"sparql":"SELECT ?x WHERE { ?x a ?y }"},
            "answers":[{"head":{"vars":["uri"]},"results":{"bindings":[
                {"uri":{"type":"uri","value":"http://dbpedia.org/resource/Ridley_Scott"}}]}}]
        }]}"#;
        let qs = load_qald(json).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].id, "7");
        assert_eq!(qs[0].question, "Who directed Alien?"); // en preferred
        assert!(qs[0].gold_sparql.as_deref().unwrap().contains("SELECT"));
        assert!(qs[0].gold_answers.as_ref().unwrap().contains("http://dbpedia.org/resource/Ridley_Scott"));
    }

    #[test]
    fn load_qald_boolean_answer() {
        let json = r#"{"questions":[{"id":"1","question":[{"language":"en","string":"Is X Y?"}],
            "answers":[{"boolean":true}]}]}"#;
        let qs = load_qald(json).unwrap();
        assert!(qs[0].gold_answers.as_ref().unwrap().contains("true"));
    }

    #[test]
    fn load_lcquad_query_only() {
        let json = r#"[{"_id":"42","corrected_question":"What is the capital of France?",
            "sparql_query":"SELECT ?u WHERE { ?u a ?t }"}]"#;
        let qs = load_lcquad(json).unwrap();
        assert_eq!(qs[0].id, "42");
        assert_eq!(qs[0].question, "What is the capital of France?");
        assert!(qs[0].gold_answers.is_none());
        assert!(qs[0].gold_sparql.as_deref().unwrap().contains("SELECT"));
    }

    #[test]
    fn bad_dataset_shapes_error() {
        assert!(load_qald("{}").is_err()); // no questions
        assert!(load_lcquad("{}").is_err()); // not an array
    }

    fn answering_engine() -> SolveEngine {
        // intent, then a SPARQL array; KB returns the row http://ex/a.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![vec![Some(RdfTerm { kind: TermKind::Uri, value: "http://ex/a".into(), datatype: None, lang: None })]],
        }]);
        SolveEngine::new(Box::new(llm), Box::new(kb)).with_citations(false)
    }

    #[test]
    fn run_eval_scores_a_matching_answer() {
        let engine = answering_engine();
        let questions = vec![EvalQuestion {
            id: "q1".into(),
            question: "which x?".into(),
            gold_answers: Some(set(&["http://ex/a"])),
            gold_sparql: None,
        }];
        let report = run_eval(&engine, &questions, &EvalOptions::default());
        assert_eq!(report.evaluated, 1);
        assert_eq!(report.skipped, 0);
        assert!((report.macro_metrics.f1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn run_eval_skips_questions_without_gold() {
        let engine = answering_engine();
        // gold_answers None and resolve_gold_via_kb false → skipped (not scored 0)
        let questions = vec![EvalQuestion {
            id: "q1".into(),
            question: "which x?".into(),
            gold_answers: None,
            gold_sparql: Some("SELECT ?x WHERE { ?x ?y ?z }".into()),
        }];
        let report = run_eval(&engine, &questions, &EvalOptions::default());
        assert_eq!(report.evaluated, 0);
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn kb_resolved_empty_gold_is_skipped_not_perfect() {
        // gold SPARQL executes but matches nothing → skip, NOT a spurious 1.0.
        let llm = MockLlm::fixed("unused"); // ask never runs for a skipped question
        let kb = MockKb::new(vec![SparqlAnswer::Select { vars: vec!["x".into()], rows: vec![] }]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb));
        let questions = vec![EvalQuestion {
            id: "q1".into(),
            question: "which x?".into(),
            gold_answers: None,
            gold_sparql: Some("SELECT ?x WHERE { ?x <http://ex/p> ?o }".into()),
        }];
        let report =
            run_eval(&engine, &questions, &EvalOptions { limit: None, resolve_gold_via_kb: true });
        assert_eq!(report.evaluated, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.macro_metrics.f1, 0.0); // not inflated
    }

    #[test]
    fn kb_resolved_gold_scores_matching_prediction() {
        let engine = answering_engine();
        let questions = vec![EvalQuestion {
            id: "q1".into(),
            question: "which x?".into(),
            gold_answers: None,
            gold_sparql: Some("SELECT ?x WHERE { ?x <http://ex/p> ?o }".into()),
        }];
        let report =
            run_eval(&engine, &questions, &EvalOptions { limit: None, resolve_gold_via_kb: true });
        assert_eq!(report.evaluated, 1);
        assert!((report.macro_metrics.f1 - 1.0).abs() < 1e-9);
    }
}
