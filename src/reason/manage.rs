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
//! A rule is *executed* while its status is `已执行` or its ledger (see
//! below) holds a triple.
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
//! Here each rule has a **ledger** of the triples it owns
//! (`reason_rule_files/inferred/<rulename>.nt`; add creates it empty, each
//! execution records into it):
//!
//! * a triple the rule derived that did not exist before is inserted and
//!   recorded;
//! * a triple the rule derived that already existed is recorded only when it
//!   is in another rule's ledger (the two rules then share it); otherwise it
//!   was **asserted** (inserted by a user / loaded) and the rule does not own
//!   it.
//!
//! The rule's WHERE clause is evaluated once per execution and exactly the
//! triples that evaluation derived are inserted, so a filter that is not
//! deterministic (`RAND()`, `NOW()`) cannot insert a triple the ledger
//! misses. An execution that fails having inserted nothing leaves the rule
//! as it was.
//!
//! Disabling a rule deletes exactly the triples of its ledger that no other
//! rule's ledger holds, through the same update path (`DELETE DATA`), and
//! empties the ledger. So: a fact asserted before the rule ran survives its
//! retraction; a fact two rules inferred survives until both are disabled.
//! A fact asserted *after* the rule inferred it (re-inserting an existing
//! triple changes nothing in an RDF store) is indistinguishable from the
//! inferred one and is retracted with it.
//!
//! The ledger stays true across failures and crashes: execute records the
//! triples it is about to insert before inserting them (a ledger line for a
//! triple that is not there is harmless; a triple no ledger names would be
//! orphaned), disable makes the retraction durable (`persist`) before it
//! empties the ledger, and a rule's ledger is created before its file and
//! removed after it ([`RuleStore::remove`]). Each line reads back exactly
//! ([`ledger_line`]); one that does not fails the disable.
//!
//! A rule file **without a ledger** was written by upstream gStore (the
//! database was moved here): whatever its status — upstream's check and
//! compile overwrite `已执行` without retracting anything — it may have
//! inferred triples nobody recorded. Disabling it retracts upstream's way,
//! with its `delete_sparql` (built from the validated label and value only,
//! so a rule whose conditions this implementation refuses can still be
//! disabled); executing it records the triples it derives that are already
//! there as its own. Either gives it a ledger. Names of existing rule files
//! are only checked to stay inside the rule directory (upstream accepts any
//! name; add applies the stricter [`validate_rule_name`]).
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

use super::compile::{compile as compile_rule, retraction_statement, CompiledRule};
use super::rules::{
    status, validate_existing_rule_name, validate_rule_name, RuleDef, MAX_RULES_PER_DB,
    MAX_RULE_FILE_BYTES,
};
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

/// A triple's ledger line: its three terms in N-Triples form, each followed
/// by a tab, then `.`. Every term reads back exactly ([`parse_ledger_line`])
/// — whatever a reader let into the database — and holds no tab or line
/// break, so the line splits back into its terms:
///
/// * an IRI (or a literal's datatype IRI) has every character IRIREF
///   excludes (`>`, `\`, a space, a tab, a newline…) as a `\uXXXX` escape,
///   as the exporters write it;
/// * a literal's value is escaped as in N-Triples (`\\`, `\"`, `\n`, `\r`,
///   `\t`);
/// * a blank node's label and a language tag have `\`, a space and every
///   control character as `\uXXXX`.
///
/// For the terms N-Triples can write (most), this is their N-Triples form.
pub fn ledger_line(t: &crate::model::Triple) -> String {
    let mut line = String::new();
    for term in [&t.subject, &t.predicate, &t.object] {
        push_ledger_term(&mut line, term);
        line.push('\t');
    }
    line.push('.');
    line
}

/// Append `term`'s ledger form (see [`ledger_line`]) to `out`.
fn push_ledger_term(out: &mut String, term: &crate::model::Term) {
    use crate::model::term::{escape_iriref, escape_literal};
    use crate::model::Term;
    match term {
        Term::Iri(iri) => {
            out.push('<');
            escape_iriref(iri, out);
            out.push('>');
        }
        Term::Blank(label) => {
            out.push_str("_:");
            push_escaped_name(out, label);
        }
        Term::Literal {
            value,
            datatype,
            lang,
        } => {
            out.push('"');
            out.push_str(&escape_literal(value));
            out.push('"');
            if let Some(dt) = datatype {
                out.push_str("^^<");
                escape_iriref(dt, out);
                out.push('>');
            } else if let Some(lang) = lang {
                out.push('@');
                push_escaped_name(out, lang);
            }
        }
    }
}

/// A blank node label or language tag: `\`, a space and control characters
/// as `\uXXXX`.
fn push_escaped_name(out: &mut String, name: &str) {
    use std::fmt::Write as _;
    for c in name.chars() {
        if c == '\\' || c <= ' ' || c.is_control() {
            let _ = write!(out, "\\u{:04X}", u32::from(c));
        } else {
            out.push(c);
        }
    }
}

/// The triple of a ledger line ([`ledger_line`]; one written before its
/// escapes were added reads back too, unless one of its terms held a
/// character they escape now) — `None` for a malformed line: anything but
/// three terms and `.`, separated by tabs.
pub fn parse_ledger_line(line: &str) -> Option<crate::model::Triple> {
    let mut parts = line.split('\t');
    let mut term = || parse_ledger_term(parts.next()?);
    let (s, p, o) = (term()?, term()?, term()?);
    if parts.next()? != "." || parts.next().is_some() {
        return None;
    }
    Some(crate::model::Triple::new(s, p, o))
}

/// One term of a ledger line (see [`ledger_line`]).
fn parse_ledger_term(text: &str) -> Option<crate::model::Term> {
    use crate::model::Term;
    if let Some(iri) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
        return Some(Term::Iri(unescape_u(iri)?));
    }
    if let Some(label) = text.strip_prefix("_:") {
        return Some(Term::Blank(unescape_u(label)?));
    }
    let body = text.strip_prefix('"')?;
    let mut value = String::with_capacity(body.len());
    let mut chars = body.char_indices();
    let rest = loop {
        match chars.next()? {
            (i, '"') => break &body[i + 1..],
            (_, '\\') => match chars.next()?.1 {
                '\\' => value.push('\\'),
                '"' => value.push('"'),
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                _ => return None,
            },
            (_, c) => value.push(c),
        }
    };
    let (datatype, lang) = if rest.is_empty() {
        (None, None)
    } else if let Some(dt) = rest.strip_prefix("^^<").and_then(|r| r.strip_suffix('>')) {
        (Some(unescape_u(dt)?), None)
    } else {
        (None, Some(unescape_u(rest.strip_prefix('@')?)?))
    };
    Some(Term::Literal {
        value,
        datatype,
        lang,
    })
}

/// `text` with its `\uXXXX` / `\UXXXXXXXX` escapes decoded; `None` for any
/// other `\`, or an escape that names no character.
fn unescape_u(text: &str) -> Option<String> {
    if !text.contains('\\') {
        return Some(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits = match chars.next()? {
            'u' => 4,
            'U' => 8,
            _ => return None,
        };
        let hex: String = chars.by_ref().take(digits).collect();
        if hex.len() != digits || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
    }
    Some(out)
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

/// The name check of every operation but add (`1003` when it fails): the
/// name must name a file inside the rule directory (see
/// [`validate_existing_rule_name`]).
fn check_name(name: &str) -> Result<(), ReasonError> {
    validate_existing_rule_name(name).map_err(param)
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

/// Whether the rule currently owns inferred triples (an empty ledger owns
/// none).
fn is_executed(doc: &Json, ledger: Option<&BTreeSet<String>>) -> bool {
    ledger.is_some_and(|l| !l.is_empty()) || status_of(doc) == status::EXECUTED
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
pub fn add(
    store: &mut RuleStore<'_>,
    ruleinfo: &Json,
    createtime: &str,
) -> Result<Added, ReasonError> {
    let fail = |m: String| failed(format!("Add Reason Rule Fail. {m}"));
    if ruleinfo.as_object().is_none() {
        return Err(fail("the rule information must be a JSON object".into()));
    }
    let name = ruleinfo
        .get("rulename")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    validate_rule_name(&name).map_err(param)?;
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
    let exists = || fail("The rule file has been exist, please remove it before!".into());
    if store.exists(&name)? {
        return Err(exists());
    }
    // Its (empty) ledger marks the rule as one whose inferred triples are
    // recorded here (see the module docs). It is written before the rule
    // file, so that a crash in between never leaves a rule file without a
    // ledger — which would be taken for an upstream rule, retracted
    // upstream's way (every `<Rule:label>` triple). A ledger without its
    // rule file is ignored, and replaced when the name is added again.
    store.set_ledger(&name, Some(&BTreeSet::new()))?;
    match store.create(&name, &text) {
        Ok(true) => {}
        Ok(false) => return Err(exists()),
        Err(e) => {
            let _ = store.set_ledger(&name, None);
            return Err(e.into());
        }
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
    let recorded = store.ledger(name)?;
    // A rule file of upstream gStore (no ledger), possibly executed there:
    // the triples it derives that are already there are taken as its own,
    // as upstream's retraction would treat them.
    let legacy = recorded.is_none();
    let mut ledger = recorded.clone().unwrap_or_default();
    // Before anything is inserted (an insert is durable at once on the
    // persistent layouts), the ledger records every triple the insert may
    // add (and, for an upstream rule, those it takes as its own): a crash or
    // a failed ledger write afterwards never leaves an inferred triple no
    // rule owns. A recorded triple the insert did not add is dropped from
    // the ledger below; one left there (the final write failed) names a
    // triple that is not there, which a retraction skips.
    let mut intended = false;
    let report = db
        .reason_infer(&compiled.insert_sparql, cancel, |derived| {
            let mut intent = ledger.clone();
            intent.extend(
                derived
                    .iter()
                    .filter(|(_, existed)| legacy || !existed)
                    .map(|(line, _)| line.clone()),
            );
            if intent.len() == ledger.len() {
                return Ok(());
            }
            store.set_ledger(name, Some(&intent)).map_err(|e| e.0)?;
            intended = true;
            Ok(())
        })
        .map_err(|e| failed(e.to_string()))?;
    let existing: BTreeSet<String> = report
        .derived
        .iter()
        .filter(|(line, existed)| *existed && !ledger.contains(line))
        .map(|(line, _)| line.clone())
        .collect();
    if legacy {
        ledger.extend(existing);
    } else {
        ledger.extend(held_by_others(store, name, &existing)?);
    }
    ledger.extend(report.inserted.iter().cloned());
    // Recorded even when the insert failed part-way — what it added is
    // owned, and the rule is then executed. A failure that added nothing
    // leaves the rule as it was.
    if report.result.is_ok() || !report.inserted.is_empty() {
        store.set_ledger(name, Some(&ledger))?;
        doc.set("status", status::EXECUTED);
        write_doc(store, name, &doc)?;
    } else if intended {
        // Nothing was inserted: the ledger as it was (none, for an upstream
        // rule file).
        store.set_ledger(name, recorded.as_ref())?;
    }
    let inserted = report.result.map_err(failed)?;
    Ok(Executed {
        insert_sparql: compiled.insert_sparql,
        select_sparql: compiled.select_sparql,
        inserted,
        owned: ledger.len(),
    })
}

/// `type` 5 — disable a compiled rule: retract the triples it owns (see the
/// module docs) and set it `已失效`.
///
/// `persist` makes the retraction durable before the ledger is emptied (a
/// snapshot database is saved — upstream saves the database at this point
/// too); the persistent layouts already are at their commit point. When it
/// fails, the rule keeps its ledger and status, so disabling it again
/// retracts (and saves) what remains.
pub fn disable(
    db: &mut Database,
    store: &mut RuleStore<'_>,
    name: &str,
    cancel: Option<Arc<AtomicBool>>,
    persist: impl FnOnce(&mut Database) -> crate::Result<()>,
) -> Result<Disabled, ReasonError> {
    check_name(name)?;
    let _lock = store.lock();
    let mut doc = read_doc(store, name, NOT_EXIST)?;
    let Some(stored) = doc.get("delete_sparql").and_then(Json::as_str) else {
        return Err(failed(NOT_COMPILED));
    };
    let stored = stored.to_string();
    refuse_in_transaction(db)?;
    let removed = match store.ledger(name)? {
        Some(lines) => {
            let shared = held_by_others(store, name, &lines)?;
            let retract: Vec<String> = lines.difference(&shared).cloned().collect();
            db.reason_retract(&retract, cancel)
                .map_err(|e| failed(e.to_string()))?
        }
        // A rule file of upstream gStore (no ledger), whatever its status:
        // upstream's retraction, built from the validated label (and value)
        // only — never the stored text.
        None => {
            let delete = retraction_statement(&doc).map_err(failed)?;
            db.reason_update(&delete, cancel)
                .map_err(|e| failed(e.to_string()))?
        }
    };
    persist(db).map_err(|e| {
        failed(format!(
            "the retraction could not be saved ({e}); the rule keeps its inferred triples \
             recorded: disable it again"
        ))
    })?;
    store.set_ledger(name, Some(&BTreeSet::new()))?;
    doc.set("status", status::DISABLED);
    write_doc(store, name, &doc)?;
    let delete_sparql = recompile(&doc)
        .map(|c| c.delete_sparql)
        .or_else(|_| retraction_statement(&doc))
        .unwrap_or(stored);
    Ok(Disabled {
        delete_sparql,
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
        QueryResult::Select(rs) => count_of(
            rs.rows
                .first()
                .and_then(|r| r.first())
                .and_then(|c| c.as_deref()),
        ),
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

    /// Finding: add wrote the rule file before its ledger, and delete
    /// removed the ledger first, so a crash in between left a rule file
    /// without a ledger — an "upstream" rule whose disable deletes every
    /// `<Rule:label>` triple.
    #[test]
    fn a_crash_never_leaves_a_rule_file_without_its_ledger() {
        use super::super::store::crash;
        let rule = json::parse(
            r#"{"rulename":"u","type":1,"logic":1,
            "conditions":[{"patterns":[{"subject":"?x","predicate":"<p>","object":"?y"}]}],
            "return":{"source":"?x","target":"?y","label":"u"}}"#,
        )
        .unwrap();
        let consistent = |store: &RuleStore<'_>| {
            for name in store.names().unwrap() {
                assert!(
                    store.ledger(&name).unwrap().is_some(),
                    "rule {name} has no ledger"
                );
            }
        };
        // (Each simulated crash prints its panic message.)
        for n in 0..4 {
            let dir = std::env::temp_dir()
                .join(format!("gstore_manage_crash_{}_{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut store = RuleStore::dir(&dir);
            // A crash during add …
            crash::arm(n);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                add(&mut store, &rule, "t")
            }));
            crash::disarm();
            consistent(&store);
            // … and, the rule added, during delete.
            if store.exists("u").unwrap() {
                let _ = store.remove("u");
            }
            add(&mut store, &rule, "t").unwrap();
            crash::arm(n);
            let _ =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| delete(&mut store, "u")));
            crash::disarm();
            consistent(&store);
            let _ = std::fs::remove_dir_all(&dir);
        }
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

    /// Every term any reader may let into a database reads back exactly,
    /// on one line, and only a well-formed line reads at all.
    #[test]
    fn ledger_lines_hold_any_term_exactly() {
        use crate::model::{Term, Triple};
        let weird = [
            "a>b",
            "c\\d",
            "t\tab",
            "n\newline",
            "r\rcr",
            "s p",
            "x\\u0041",
            "<>\"{}|^`",
            "",
        ];
        let lit = |v: &str, dt: Option<&str>, lang: Option<&str>| Term::Literal {
            value: v.into(),
            datatype: dt.map(Into::into),
            lang: lang.map(Into::into),
        };
        let mut terms = Vec::new();
        for w in weird {
            terms.push(Term::iri(w));
            terms.push(Term::blank(w));
            terms.push(lit(w, None, None));
            terms.push(lit("v", Some(w), None));
            terms.push(lit(w, None, Some(w)));
        }
        for s in &terms {
            for o in &terms {
                let t = Triple::new(s.clone(), Term::iri("Rule:x"), o.clone());
                let line = ledger_line(&t);
                assert_eq!(line.matches('\t').count(), 3, "{line:?}");
                assert!(!line.contains(['\n', '\r']), "{line:?}");
                assert_eq!(parse_ledger_line(&line), Some(t), "{line:?}");
            }
        }
        // An ordinary triple is written as N-Triples (as before).
        let t = Triple::new(
            Term::iri("http://ex/a"),
            Term::iri("Rule:x"),
            lit("v", None, Some("en")),
        );
        assert_eq!(ledger_line(&t), "<http://ex/a>\t<Rule:x>\t\"v\"@en\t.");
        for bad in [
            "<a>\t<b>\t<c>",
            "<a>\t<b>\t<c>\t.\t<d>",
            "<a>\t<b>\t<c>\tx",
            "<a\\q>\t<b>\t<c>\t.",
            "<a\\u00>\t<b>\t<c>\t.",
            "\"v\\q\"\t<b>\t<c>\t.",
        ] {
            assert_eq!(parse_ledger_line(bad), None, "{bad:?}");
        }
    }
}
