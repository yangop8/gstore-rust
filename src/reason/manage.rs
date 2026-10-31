//! The rule lifecycle of upstream gStore's `reasonManage` (types 1-8), over a
//! [`RuleStore`] and a [`Database`].
//!
//! # States
//!
//! A rule file's `status` is upstream's string:
//!
//! | operation | from | to | refused when |
//! |---|---|---|---|
//! | 1 add | — | `新建` | the name exists |
//! | 3 compile | any but executed | `已编译` | **executed** (Rust: upstream recompiles and loses track of the inferred triples) |
//! | 4 execute | compiled (`insert_sparql` present) | `已执行` | not compiled; the rule changed since it was compiled; a transaction is open |
//! | 5 disable | compiled | `已失效` | not compiled; a transaction is open |
//! | 8 check | compiled | `已校验` (an executed rule stays `已执行`, Rust) | not compiled |
//! | 7 delete | any but executed | — (file removed) | **executed** (`该规则已经执行，请先让该规则失效后再执行`) |
//! | 2 list, 6 show | — | — | — |
//!
//! A rule is *executed* while its status is `已执行` or it has a ledger (see
//! below).
//!
//! # Inferred triples, and retraction
//!
//! Executing a rule runs its compiled `INSERT … WHERE` through the
//! database's normal update path (query cache, CSR, write-through, update
//! and redo logs, commit point — as any SPARQL update). Inferred triples use
//! the predicate `<Rule:label>`, as upstream, so they never coincide with a
//! fact asserted with an ordinary predicate.
//!
//! Upstream retracts with `delete where {?x <Rule:label> ?y.}` — every
//! triple with the predicate, including those another rule with the same
//! label inferred, and any a user inserted with that predicate himself.
//! Here each execution records a **ledger** of the triples the rule owns
//! (`reason_rule_files/inferred/<rulename>.nt`):
//!
//! * a triple the rule derived that did not exist before is inserted and
//!   recorded;
//! * a triple the rule derived that already existed is recorded only when it
//!   is in another rule's ledger (the two rules then share it); otherwise it
//!   was **asserted** (inserted by a user / loaded) and the rule does not own
//!   it.
//!
//! Disabling a rule deletes exactly the triples of its ledger that no other
//! rule's ledger holds, through the same update path (`DELETE DATA`), and
//! drops the ledger. So: a fact asserted before the rule ran survives its
//! retraction; a fact two rules inferred survives until both are disabled.
//! A fact asserted *after* the rule inferred it (re-inserting an existing
//! triple changes nothing in an RDF store) is indistinguishable from the
//! inferred one and is retracted with it. A rule marked executed without a
//! ledger (executed by upstream gStore before the database was moved here)
//! is retracted with upstream's `delete_sparql`.
//!
//! The `delete_sparql` reported by compile / disable is upstream's text.
//!
//! # Concurrency
//!
//! Each operation holds the rule directory's process-wide lock
//! ([`RuleStore::lock`]) throughout; the caller holds the database (the
//! server: its per-database lock) for operations that touch data.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::compile::{compile as compile_rule, CompiledRule};
use super::rules::{status, validate_rule_name, RuleDef, MAX_RULES_PER_DB, MAX_RULE_FILE_BYTES};
use super::store::{RuleStore, StoreError, NO_DB_DIR};
use crate::json::{self, Json};
use crate::query::QueryResult;
use crate::Database;

/// Why a `reasonManage` operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonErrorKind {
    /// A missing or malformed request parameter (upstream `1003`).
    Param,
    /// The operation failed or was refused (upstream `1005`).
    Failed,
    /// Refused because a transaction is open on the database (`1004`, as a
    /// non-transactional update then is).
    Conflict,
}

/// A failed `reasonManage` operation: its kind and upstream-style message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonError {
    pub kind: ReasonErrorKind,
    pub message: String,
}

impl std::fmt::Display for ReasonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReasonError {}

impl From<ReasonError> for crate::GStoreError {
    fn from(e: ReasonError) -> crate::GStoreError {
        crate::GStoreError::Database(e.message)
    }
}

impl From<StoreError> for ReasonError {
    fn from(e: StoreError) -> ReasonError {
        failed(e.0)
    }
}

fn failed(msg: impl Into<String>) -> ReasonError {
    ReasonError {
        kind: ReasonErrorKind::Failed,
        message: msg.into(),
    }
}

fn param(msg: impl Into<String>) -> ReasonError {
    ReasonError {
        kind: ReasonErrorKind::Param,
        message: msg.into(),
    }
}

/// Upstream's refusal to delete an executed rule (also used here to refuse
/// recompiling one).
pub const EXECUTED_REFUSAL: &str = "该规则已经执行，请先让该规则失效后再执行";
/// Upstream's message when a rule has not been compiled.
pub const NOT_COMPILED: &str = "The sparql of the reason rule is not exist! ";
/// Upstream's message (compile / execute / disable / check) for a missing rule.
pub const NOT_EXIST: &str = "the reason file is not exist";
/// Upstream's message (show / delete) for a missing rule.
pub const NOT_EXISTS: &str = "the reason file is not exists";
/// Upstream's message for a rule file that is not a JSON object.
pub const NOT_JSON: &str = "the reason file is not fit the json format";
/// Refusal to execute a rule edited after it was compiled.
pub const STALE: &str =
    "the reason rule was changed after it was compiled; compile it again before running it";

/// The fields a client may not set: they are produced by compile / check.
const COMPILED_FIELDS: [&str; 6] = [
    "insert_sparql",
    "delete_sparql",
    "check_sparql",
    "select_sparql",
    "effectNum",
    "checkResult",
];

/// What [`Database::reason_infer`] reports to [`execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InferReport {
    /// Every distinct triple the rule derived (a ledger line, see
    /// [`ledger_line`]) and whether it existed before the insert.
    pub derived: Vec<(String, bool)>,
    /// The derived triples the insert added.
    pub inserted: Vec<String>,
    /// The update's outcome: the number of triples it added, or its error
    /// (the triples added before the error stay — see
    /// [`Database::query`]).
    pub result: Result<usize, String>,
}

/// A triple's ledger line: N-Triples with tab separators (a term's
/// N-Triples form never holds a raw tab, so the line splits back into its
/// three terms exactly).
pub fn ledger_line(t: &crate::model::Triple) -> String {
    format!("{}\t{}\t{}\t.", t.subject, t.predicate, t.object)
}

/// The triple of a ledger line (`None` for a malformed one).
pub fn parse_ledger_line(line: &str) -> Option<crate::model::Triple> {
    let mut parts = line.split('\t');
    let mut term = || crate::parser::ntriples::parse_term(parts.next()?).ok();
    let (s, p, o) = (term()?, term()?, term()?);
    Some(crate::model::Triple::new(s, p, o))
}

/// The answer of `type` 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    /// The rule file's path, as shown in messages.
    pub path: String,
}

/// The answer of `type` 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    pub insert_sparql: String,
    pub select_sparql: String,
    /// The triples this execution added (upstream's `AnsNum`: existing ones
    /// are not counted).
    pub inserted: usize,
    /// The triples the rule owns after it (its ledger).
    pub owned: usize,
}

/// The answer of `type` 5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disabled {
    pub delete_sparql: String,
    /// The triples retracted (`AnsNum`).
    pub removed: usize,
}

/// The answer of `type` 8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub check_sparql: String,
    /// The number of matches of the rule's conditions (`effectNum`).
    pub effect_num: i64,
}

/// The name check of every operation but add (`1003` when it fails).
fn check_name(name: &str) -> Result<(), ReasonError> {
    validate_rule_name(name).map_err(param)
}

/// Read and parse rule `name`'s file; `missing` is the message when it does
/// not exist.
fn read_doc(store: &RuleStore<'_>, name: &str, missing: &str) -> Result<Json, ReasonError> {
    let Some(text) = store.read(name)? else {
        return Err(failed(missing));
    };
    match json::parse(&text) {
        Ok(doc) if doc.as_object().is_some() => Ok(doc),
        _ => Err(failed(NOT_JSON)),
    }
}

/// Write rule `name`'s file: compact, keys sorted (upstream's `dump()`).
fn write_doc(store: &mut RuleStore<'_>, name: &str, doc: &Json) -> Result<(), ReasonError> {
    let text = doc.sorted().to_string();
    if text.len() > MAX_RULE_FILE_BYTES {
        return Err(failed(format!(
            "the rule is larger than {MAX_RULE_FILE_BYTES} bytes"
        )));
    }
    store.write(name, &text)?;
    Ok(())
}

fn status_of(doc: &Json) -> &str {
    doc.get("status").and_then(Json::as_str).unwrap_or_default()
}

/// Whether the rule currently owns inferred triples.
fn is_executed(doc: &Json, ledger: Option<&BTreeSet<String>>) -> bool {
    ledger.is_some() || status_of(doc) == status::EXECUTED
}

/// The rule definition and its compiled statements.
fn recompile(doc: &Json) -> Result<CompiledRule, ReasonError> {
    let def = RuleDef::from_json(doc).map_err(failed)?;
    compile_rule(&def).map_err(failed)
}

/// The lines of `candidates` that another rule's ledger (not `name`'s) holds.
fn held_by_others(
    store: &RuleStore<'_>,
    name: &str,
    candidates: &BTreeSet<String>,
) -> Result<BTreeSet<String>, ReasonError> {
    let mut held = BTreeSet::new();
    if candidates.is_empty() {
        return Ok(held);
    }
    for other in store.names()? {
        if other == name {
            continue;
        }
        if let Some(lines) = store.ledger(&other)? {
            held.extend(lines.intersection(candidates).cloned());
        }
    }
    Ok(held)
}

fn refuse_in_transaction(db: &Database) -> Result<(), ReasonError> {
    if db.in_transaction() {
        return Err(ReasonError {
            kind: ReasonErrorKind::Conflict,
            message: "a transaction is open on the database; reasoning rules can only be \
                      executed or disabled outside a transaction"
                .to_string(),
        });
    }
    Ok(())
}

/// The stored compiled statement `field` (`insert_sparql`, `delete_sparql`,
/// `check_sparql`) of rule `name`, with the errors upstream reports for a
/// missing / unreadable / uncompiled rule — what a server checks (and logs)
/// before it takes the database for execute / disable / check.
pub fn compiled_statement(
    store: &RuleStore<'_>,
    name: &str,
    field: &str,
) -> Result<String, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    let doc = read_doc(store, name, NOT_EXIST)?;
    doc.get(field)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| failed(NOT_COMPILED))
}

/// `type` 1 — add a rule from its JSON (`ruleinfo`), as `新建` with
/// `createtime`. The rule is validated (and compiled, to be sure it can be)
/// first; client-supplied compiled fields are dropped. Refused when the name
/// exists or the database holds [`MAX_RULES_PER_DB`] rules.
pub fn add(store: &mut RuleStore<'_>, ruleinfo: &Json, createtime: &str) -> Result<Added, ReasonError> {
    let fail = |m: String| failed(format!("Add Reason Rule Fail. {m}"));
    if ruleinfo.as_object().is_none() {
        return Err(fail("the rule information must be a JSON object".into()));
    }
    let name = ruleinfo
        .get("rulename")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    check_name(&name)?;
    let _lock = store.lock();
    if !store.db_exists() {
        return Err(fail(NO_DB_DIR.into()));
    }
    let def = RuleDef::from_json(ruleinfo).map_err(fail)?;
    compile_rule(&def).map_err(fail)?;
    let mut doc = ruleinfo.clone();
    for f in COMPILED_FIELDS {
        while doc.remove(f).is_some() {}
    }
    while doc.remove("status").is_some() {}
    while doc.remove("createtime").is_some() {}
    doc.set("status", status::NEW);
    doc.set("createtime", createtime);
    let text = doc.sorted().to_string();
    if text.len() > MAX_RULE_FILE_BYTES {
        return Err(fail(format!(
            "the rule is larger than {MAX_RULE_FILE_BYTES} bytes"
        )));
    }
    if store.names()?.len() >= MAX_RULES_PER_DB {
        return Err(fail(format!(
            "the database already holds {MAX_RULES_PER_DB} reasoning rules, the most allowed"
        )));
    }
    if !store.create(&name, &text)? {
        return Err(fail(
            "The rule file has been exist, please remove it before!".into(),
        ));
    }
    Ok(Added {
        path: store.shown_path(&name),
    })
}

/// `type` 2 — every rule file's JSON, by rule name. Upstream answers
/// `pasrse rulefiles error:<the files>` when one is not JSON.
pub fn list(store: &RuleStore<'_>) -> Result<Vec<Json>, ReasonError> {
    let _lock = store.lock();
    let mut texts = Vec::new();
    for name in store.names()? {
        if let Some(t) = store.read(&name)? {
            texts.push(t);
        }
    }
    let mut out = Vec::with_capacity(texts.len());
    for t in &texts {
        match json::parse(t) {
            Ok(v) => out.push(v),
            Err(_) => {
                return Err(failed(format!(
                    "pasrse rulefiles error:[{}]",
                    texts.join(",")
                )))
            }
        }
    }
    Ok(out)
}

/// `type` 3 — compile a rule: store its four statements and set it
/// `已编译`. Refused for an executed rule (disable it first).
pub fn compile(store: &mut RuleStore<'_>, name: &str) -> Result<CompiledRule, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    let mut doc = read_doc(store, name, NOT_EXIST)?;
    if is_executed(&doc, store.ledger(name)?.as_ref()) {
        return Err(failed(EXECUTED_REFUSAL));
    }
    let compiled = recompile(&doc)?;
    doc.set("status", status::COMPILED);
    doc.set("checkResult", "");
    doc.set("insert_sparql", compiled.insert_sparql.as_str());
    doc.set("delete_sparql", compiled.delete_sparql.as_str());
    doc.set("check_sparql", compiled.check_sparql.as_str());
    doc.set("select_sparql", compiled.select_sparql.as_str());
    write_doc(store, name, &doc)?;
    Ok(compiled)
}

/// `type` 4 — execute a compiled rule against `db` (see the module docs),
/// set it `已执行`. Running it again infers what the data now implies.
pub fn execute(
    db: &mut Database,
    store: &mut RuleStore<'_>,
    name: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Executed, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    let mut doc = read_doc(store, name, NOT_EXIST)?;
    let Some(stored) = doc.get("insert_sparql").and_then(Json::as_str) else {
        return Err(failed(NOT_COMPILED));
    };
    // Only the statement compiled from the (validated) definition ever runs.
    let compiled = recompile(&doc)?;
    if compiled.insert_sparql != stored {
        return Err(failed(STALE));
    }
    refuse_in_transaction(db)?;
    let mut ledger = store.ledger(name)?.unwrap_or_default();
    let report = db
        .reason_infer(&compiled.insert_sparql, cancel)
        .map_err(|e| failed(e.to_string()))?;
    let existing: BTreeSet<String> = report
        .derived
        .iter()
        .filter(|(line, existed)| *existed && !ledger.contains(line))
        .map(|(line, _)| line.clone())
        .collect();
    ledger.extend(held_by_others(store, name, &existing)?);
    ledger.extend(report.inserted.iter().cloned());
    // Recorded even when the update failed part-way: what it added is owned.
    store.set_ledger(name, Some(&ledger))?;
    let inserted = report.result.map_err(failed)?;
    doc.set("status", status::EXECUTED);
    write_doc(store, name, &doc)?;
    Ok(Executed {
        insert_sparql: compiled.insert_sparql,
        select_sparql: compiled.select_sparql,
        inserted,
        owned: ledger.len(),
    })
}

/// `type` 5 — disable a compiled rule: retract the triples it owns (see the
/// module docs) and set it `已失效`.
pub fn disable(
    db: &mut Database,
    store: &mut RuleStore<'_>,
    name: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Disabled, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    let mut doc = read_doc(store, name, NOT_EXIST)?;
    let Some(stored) = doc.get("delete_sparql").and_then(Json::as_str) else {
        return Err(failed(NOT_COMPILED));
    };
    let stored = stored.to_string();
    refuse_in_transaction(db)?;
    let compiled = recompile(&doc);
    let removed = match store.ledger(name)? {
        Some(lines) => {
            let shared = held_by_others(store, name, &lines)?;
            let retract: Vec<String> = lines.difference(&shared).cloned().collect();
            db.reason_retract(&retract, cancel)
                .map_err(|e| failed(e.to_string()))?
        }
        // Executed by an earlier gStore (no ledger): upstream's retraction,
        // compiled from the validated definition.
        None if status_of(&doc) == status::EXECUTED => {
            let c = compiled.clone()?;
            match db.reason_update(&c.delete_sparql, cancel) {
                Ok(n) => n,
                Err(e) => return Err(failed(e.to_string())),
            }
        }
        None => 0,
    };
    store.set_ledger(name, None)?;
    doc.set("status", status::DISABLED);
    write_doc(store, name, &doc)?;
    Ok(Disabled {
        delete_sparql: compiled.map_or(stored, |c| c.delete_sparql),
        removed,
    })
}

/// `type` 6 — rule `name`'s JSON.
pub fn show(store: &RuleStore<'_>, name: &str) -> Result<Json, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    if !store.db_exists() {
        return Err(failed(NO_DB_DIR));
    }
    let Some(text) = store.read(name)? else {
        return Err(failed(NOT_EXISTS));
    };
    json::parse(&text).map_err(|_| failed(format!("pasrse ruleinfo error:{text}")))
}

/// `type` 7 — delete a rule that is not executed. Returns upstream's
/// message (`the reason file has been remove successfully! file path:…`).
pub fn delete(store: &mut RuleStore<'_>, name: &str) -> Result<String, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    if !store.db_exists() {
        return Err(failed(NO_DB_DIR));
    }
    let Some(text) = store.read(name)? else {
        return Err(failed(NOT_EXISTS));
    };
    let doc = json::parse(&text).unwrap_or(Json::Null);
    if is_executed(&doc, store.ledger(name)?.as_ref()) {
        return Err(failed(EXECUTED_REFUSAL));
    }
    store.remove(name)?;
    Ok(format!(
        "the reason file has been remove successfully! file path:{}",
        store.shown_path(name)
    ))
}

/// The integer of a `count(*)` cell (`"3"^^<…#integer>` or `3`), `0` when
/// it is not one (upstream logs the error and records `0`).
fn count_of(cell: Option<&str>) -> i64 {
    let Some(raw) = cell else { return 0 };
    let lexical = match raw.find("\"^^<") {
        Some(pos) if raw.starts_with('"') => &raw[1..pos],
        _ => raw.trim_matches('"'),
    };
    lexical.trim().parse().unwrap_or(0)
}

/// `type` 8 — count the matches of a compiled rule's conditions with
/// `read` (the database's read path: the server reads an open
/// transaction's snapshot), store them as `effectNum` and set the rule
/// `已校验` (an executed rule stays `已执行`).
pub fn check(
    store: &mut RuleStore<'_>,
    name: &str,
    read: impl FnOnce(&str) -> crate::Result<QueryResult>,
) -> Result<Checked, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    let mut doc = read_doc(store, name, NOT_EXIST)?;
    let Some(stored) = doc.get("check_sparql").and_then(Json::as_str) else {
        return Err(failed(NOT_COMPILED));
    };
    let compiled = recompile(&doc)?;
    if compiled.check_sparql != stored {
        return Err(failed(STALE));
    }
    let effect_num = match read(&compiled.check_sparql).map_err(|e| failed(e.to_string()))? {
        QueryResult::Select(rs) => count_of(rs.rows.first().and_then(|r| r.first()).and_then(|c| c.as_deref())),
        _ => 0,
    };
    let executed = is_executed(&doc, store.ledger(name)?.as_ref());
    doc.set("effectNum", effect_num);
    doc.set("checkResult", "ok");
    if !executed {
        doc.set("status", status::CHECKED);
    }
    write_doc(store, name, &doc)?;
    Ok(Checked {
        check_sparql: compiled.check_sparql,
        effect_num,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_cells_parse_like_upstream() {
        assert_eq!(
            count_of(Some("\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>")),
            3
        );
        assert_eq!(count_of(Some("12")), 12);
        assert_eq!(count_of(Some("\"x\"")), 0);
        assert_eq!(count_of(None), 0);
    }

    #[test]
    fn ledger_lines_round_trip() {
        use crate::model::{Term, Triple};
        for t in [
            Triple::new(Term::iri("a"), Term::iri("Rule:舅舅"), Term::iri("b")),
            Triple::new(
                Term::blank("b1"),
                Term::iri("Rule:x"),
                Term::typed_literal("a\tb\n\"c\"", "http://www.w3.org/2001/XMLSchema#string"),
            ),
            Triple::new(
                Term::iri("s"),
                Term::iri("Rule:y"),
                Term::Literal {
                    value: "v".into(),
                    datatype: None,
                    lang: Some("en".into()),
                },
            ),
        ] {
            let line = ledger_line(&t);
            assert_eq!(line.matches('\t').count(), 3, "{line}");
            assert_eq!(parse_ledger_line(&line), Some(t));
        }
        assert_eq!(parse_ledger_line("garbage"), None);
    }
}
