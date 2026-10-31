//! The reasoning-rule JSON model of upstream gStore's `reasonManage`
//! (`src/Reason/Reason.cpp`, `src/Server/ApiReasonOperation.cpp`), and its
//! validation.
//!
//! A rule is the JSON object a client sends as `ruleinfo` (upstream
//! `API_USAGE.md`, "推理引擎"):
//!
//! ```json
//! {"rulename": "舅舅", "description": "母亲的男性同胞是舅舅", "isenable": 1,
//!  "type": 1, "logic": 1,
//!  "conditions": [{"patterns": [{"subject": "?x", "predicate": "<母亲>", "object": "?y"}, …],
//!                  "filters": [], "count_info": {}}],
//!  "return": {"source": "?x", "target": "?k", "label": "舅舅"}}
//! ```
//!
//! * `type`: `0` = property (attribute) inference — infers
//!   `source <Rule:label> value`; anything else (upstream's `1`) = relation
//!   inference — infers `source <Rule:label> target`.
//! * `logic`: `0` = the conditions are alternatives (`UNION`); anything else
//!   (upstream's `1`) = all must hold (a join of the condition groups).
//! * Each condition is a group of triple patterns plus `filters`, SPARQL
//!   expressions that must all hold (`FILTER(f1 && f2 …)`).
//! * `isenable`, `description`, `count_info` and any other field are kept as
//!   given and not interpreted (as upstream).
//!
//! Upstream splices every one of these strings verbatim into SPARQL text.
//! Here each is **validated first** ([`RuleDef::from_json`]) with the
//! crate's own SPARQL lexer and parser, so that a rule can never inject
//! SPARQL:
//!
//! * a pattern position or a `return` term must be exactly **one** SPARQL
//!   term token (a variable, a full `<IRI>`, a literal, a number, a boolean,
//!   `a` as a predicate, a `_:label` in a WHERE pattern) — optionally
//!   surrounded by spaces / tabs, which are kept so the compiled text matches
//!   upstream's byte for byte. Comments, a second token, prefixed names (a
//!   rule has no `PREFIX` declarations) and control characters are refused;
//! * a filter must be one complete SPARQL expression whose brackets
//!   (`()`, `{}`, `[]`) balance — so it cannot close the `FILTER(` it is
//!   placed in — and which parses inside `FILTER( … )`;
//! * the label must make `<Rule:label>` exactly one IRI token;
//! * a variable in `return` must occur in the conditions' patterns.
//!
//! The compiled statements are then parsed again and their shape checked
//! (see `compile`).

use std::collections::BTreeSet;

use crate::json::Json;
use crate::parser::sparql::lexer::{tokenize_with_spans, Token};

/// The directory, inside a database directory, that holds its rules
/// (upstream's `<db>/reason_rule_files/`).
pub const RULE_DIR: &str = "reason_rule_files";

/// The most characters a rule name may have.
pub const MAX_RULE_NAME_CHARS: usize = 128;
/// The most bytes (UTF-8) a rule name may have, so that `<name>.json` stays
/// within the usual 255-byte file-name limit.
pub const MAX_RULE_NAME_BYTES: usize = 240;
/// The largest rule file (its JSON text) accepted, in bytes.
pub const MAX_RULE_FILE_BYTES: usize = 1024 * 1024;
/// The most rules one database may hold.
pub const MAX_RULES_PER_DB: usize = 1000;
/// The most conditions one rule may have.
pub const MAX_CONDITIONS: usize = 64;
/// The most triple patterns one condition may have.
pub const MAX_PATTERNS: usize = 256;
/// The most filters one condition may have.
pub const MAX_FILTERS: usize = 64;
/// The longest pattern / `return` term or label, in bytes.
pub const MAX_TERM_BYTES: usize = 4096;
/// The longest filter expression, in bytes.
pub const MAX_FILTER_BYTES: usize = 16 * 1024;

/// The `status` values of a rule file — upstream's exact (Chinese) strings.
pub mod status {
    /// Added, not compiled yet (`type` 1).
    pub const NEW: &str = "新建";
    /// Compiled (`type` 3).
    pub const COMPILED: &str = "已编译";
    /// Executed: its inferred triples are in the database (`type` 4).
    pub const EXECUTED: &str = "已执行";
    /// Disabled: its inferred triples were retracted (`type` 5).
    pub const DISABLED: &str = "已失效";
    /// Checked: its match count was computed (`type` 8).
    pub const CHECKED: &str = "已校验";
}

/// Validate a rule name, which names the rule's file
/// (`reason_rule_files/<name>.json`): non-empty, at most
/// [`MAX_RULE_NAME_CHARS`] characters / [`MAX_RULE_NAME_BYTES`] bytes, no
/// path separator (`/`, `\`), no `..`, no leading `.`, no control character
/// (NUL included).
pub fn validate_rule_name(name: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("the rule name {name:?} is not allowed: {why}"));
    if name.is_empty() {
        return Err("the value of rulename can not be empty!".to_string());
    }
    if name.chars().count() > MAX_RULE_NAME_CHARS || name.len() > MAX_RULE_NAME_BYTES {
        return bad(&format!(
            "it is longer than {MAX_RULE_NAME_CHARS} characters ({MAX_RULE_NAME_BYTES} bytes)"
        ));
    }
    if name.contains(['/', '\\']) {
        return bad("it contains a path separator");
    }
    if name.contains("..") {
        return bad("it contains '..'");
    }
    if name.starts_with('.') {
        return bad("it starts with '.'");
    }
    if name.chars().any(char::is_control) {
        return bad("it contains a control character");
    }
    Ok(())
}

/// One triple pattern of a condition, as the raw texts the rule gives (kept
/// verbatim — whitespace included — for the compiled SPARQL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// One condition: a group of triple patterns and filter expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub patterns: Vec<Pattern>,
    pub filters: Vec<String>,
}

/// What a rule infers (`type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleType {
    /// `type` 0 — property (attribute) inference: `source <Rule:label> value`.
    Property,
    /// Any other `type` (upstream documents `1`) — relation inference:
    /// `source <Rule:label> target`.
    Relation,
}

/// A validated rule definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDef {
    pub rule_type: RuleType,
    /// `logic` 0: the conditions are alternatives (`UNION`); otherwise all
    /// must hold.
    pub union: bool,
    pub conditions: Vec<Condition>,
    /// The `return` fields, raw (validated) texts; `target` / `value` are
    /// empty when the rule type does not use them.
    pub source: String,
    pub target: String,
    pub label: String,
    pub value: String,
}

/// Where a term appears (decides which kinds of term are allowed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pos {
    Subject,
    Predicate,
    Object,
    Source,
    Target,
    Value,
}

/// A validated single term: the variable name when it is a variable.
struct CheckedTerm {
    var: Option<String>,
}

/// An integer field read like upstream's `JsonUtil::jsonParam(json, key, 0)`:
/// a JSON integer, or a string holding one; absent ⇒ `0`.
fn int_field(doc: &Json, key: &str) -> Result<i64, String> {
    match doc.get(key) {
        None | Some(Json::Null) => Ok(0),
        Some(n @ Json::Num(_)) => Ok(n.as_i64().unwrap_or(0)),
        Some(Json::Str(s)) if s.trim().is_empty() => Ok(0),
        Some(Json::Str(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("the value of {key} must be an integer, not {s:?}")),
        Some(_) => Err(format!("the value of {key} must be an integer")),
    }
}

/// A string field read like upstream's `JsonUtil::jsonParam(json, key)`: a
/// JSON string; absent or another type ⇒ empty.
fn str_field(doc: &Json, key: &str) -> String {
    doc.get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

impl RuleDef {
    /// Read and validate a rule from its JSON object (a rule file, or the
    /// `ruleinfo` of an add request). The error messages for a missing
    /// `conditions` / `return` are upstream's.
    pub fn from_json(doc: &Json) -> Result<RuleDef, String> {
        if doc.as_object().is_none() {
            return Err("the rule information must be a JSON object".to_string());
        }
        let Some(conditions) = doc.get("conditions") else {
            // Upstream's text (sic).
            return Err("the reason file has not the conditons information".to_string());
        };
        let Some(conditions) = conditions.as_array() else {
            return Err("the conditions of the rule must be an array".to_string());
        };
        if conditions.is_empty() {
            return Err("the rule has no conditions".to_string());
        }
        if conditions.len() > MAX_CONDITIONS {
            return Err(format!(
                "the rule has {} conditions, at most {MAX_CONDITIONS} are allowed",
                conditions.len()
            ));
        }
        let union = int_field(doc, "logic")? == 0;
        let rule_type = if int_field(doc, "type")? == 0 {
            RuleType::Property
        } else {
            RuleType::Relation
        };
        let mut bound: BTreeSet<String> = BTreeSet::new();
        let mut conds = Vec::with_capacity(conditions.len());
        for (i, c) in conditions.iter().enumerate() {
            conds.push(read_condition(c, i + 1, &mut bound)?);
        }
        let Some(ret) = doc.get("return") else {
            return Err("the reason file has not the return information".to_string());
        };
        if ret.as_object().is_none() {
            return Err("the return information of the rule must be an object".to_string());
        }
        let source = str_field(ret, "source");
        let label = str_field(ret, "label");
        let (target, value) = match rule_type {
            RuleType::Property => (String::new(), str_field(ret, "value")),
            RuleType::Relation => (str_field(ret, "target"), String::new()),
        };
        let mut uses = Vec::new();
        uses.push(check_term(
            &source,
            Pos::Source,
            "the source of the return information",
        )?);
        match rule_type {
            RuleType::Property => uses.push(check_term(
                &value,
                Pos::Value,
                "the value of the return information (a property rule, type 0)",
            )?),
            RuleType::Relation => uses.push(check_term(
                &target,
                Pos::Target,
                "the target of the return information (a relation rule, type 1)",
            )?),
        }
        for v in uses.into_iter().filter_map(|t| t.var) {
            if !bound.contains(&v) {
                return Err(format!(
                    "the variable ?{v} of the return information does not occur in any pattern \
                     of the conditions"
                ));
            }
        }
        check_label(&label)?;
        Ok(RuleDef {
            rule_type,
            union,
            conditions: conds,
            source,
            target,
            label,
            value,
        })
    }
}

/// Read condition number `n` (1-based), adding the variables its patterns
/// bind to `bound`.
fn read_condition(c: &Json, n: usize, bound: &mut BTreeSet<String>) -> Result<Condition, String> {
    if c.as_object().is_none() {
        return Err(format!("condition {n} must be a JSON object"));
    }
    // The shape upstream's list / show examples print, `{"condition": {…},
    // "logic": 1}`, is read too (a Rust extension; upstream's compiler only
    // reads `patterns` / `filters` of the condition itself).
    let c = match c.get("condition") {
        Some(inner) if c.get("patterns").is_none() && c.get("filters").is_none() => inner,
        _ => c,
    };
    let mut patterns = Vec::new();
    if let Some(ps) = c.get("patterns") {
        let Some(ps) = ps.as_array() else {
            return Err(format!("the patterns of condition {n} must be an array"));
        };
        if ps.len() > MAX_PATTERNS {
            return Err(format!(
                "condition {n} has {} patterns, at most {MAX_PATTERNS} are allowed",
                ps.len()
            ));
        }
        for (j, p) in ps.iter().enumerate() {
            let at = format!("condition {n}, pattern {}", j + 1);
            let field = |key: &str| -> Result<String, String> {
                p.get(key)
                    .and_then(Json::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("{at}: the {key} must be a string"))
            };
            let pattern = Pattern {
                subject: field("subject")?,
                predicate: field("predicate")?,
                object: field("object")?,
            };
            for (raw, pos, key) in [
                (&pattern.subject, Pos::Subject, "subject"),
                (&pattern.predicate, Pos::Predicate, "predicate"),
                (&pattern.object, Pos::Object, "object"),
            ] {
                if let Some(v) = check_term(raw, pos, &format!("{at}: the {key}"))?.var {
                    bound.insert(v);
                }
            }
            patterns.push(pattern);
        }
    }
    let mut filters = Vec::new();
    if let Some(fs) = c.get("filters") {
        let Some(fs) = fs.as_array() else {
            return Err(format!("the filters of condition {n} must be an array"));
        };
        if fs.len() > MAX_FILTERS {
            return Err(format!(
                "condition {n} has {} filters, at most {MAX_FILTERS} are allowed",
                fs.len()
            ));
        }
        for (j, f) in fs.iter().enumerate() {
            let at = format!("condition {n}, filter {}", j + 1);
            let Some(f) = f.as_str() else {
                return Err(format!("{at}: a filter must be a string"));
            };
            check_filter(f).map_err(|e| format!("{at}: {e}"))?;
            filters.push(f.to_string());
        }
    }
    Ok(Condition { patterns, filters })
}

/// Tokenize `text` and require that every character outside the tokens is
/// one of `allowed_ws` (so there is no comment — the lexer skips `# …` — and
/// no stray character). Returns the tokens with their spans, `Eof` dropped.
fn tokens_covering(
    text: &str,
    allowed_ws: &[char],
) -> Result<Vec<(Token, std::ops::Range<usize>)>, String> {
    let (tokens, spans) =
        tokenize_with_spans(text).map_err(|e| format!("is not valid SPARQL ({e})"))?;
    let mut covered = 0usize;
    let mut out = Vec::with_capacity(tokens.len());
    for (tok, span) in tokens.into_iter().zip(spans) {
        if !text[covered..span.start]
            .chars()
            .all(|c| allowed_ws.contains(&c))
        {
            return Err(
                "contains a comment or a character that is not part of a SPARQL token".to_string(),
            );
        }
        covered = span.end;
        if tok != Token::Eof {
            out.push((tok, span));
        }
    }
    if !text[covered..].chars().all(|c| allowed_ws.contains(&c)) {
        return Err(
            "contains a comment or a character that is not part of a SPARQL token".to_string(),
        );
    }
    Ok(out)
}

/// Validate one term (see the module docs) at position `pos`; `what` names
/// it in the error message.
fn check_term(raw: &str, pos: Pos, what: &str) -> Result<CheckedTerm, String> {
    let fail = |why: String| Err(format!("{what} {raw:?} {why}"));
    if raw.trim_matches([' ', '\t']).is_empty() {
        return Err(format!("{what} is empty"));
    }
    if raw.len() > MAX_TERM_BYTES {
        return fail(format!("is longer than {MAX_TERM_BYTES} bytes"));
    }
    if raw.chars().any(|c| c.is_control() && c != '\t') {
        return fail("contains a control character".to_string());
    }
    let toks = match tokens_covering(raw, &[' ', '\t']) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let numeric = |t: &Token| matches!(t, Token::Integer(_) | Token::Decimal(_) | Token::Double(_));
    let literal_ok = matches!(pos, Pos::Object | Pos::Target | Pos::Value);
    let kind =
        match toks.as_slice() {
            [(Token::Var(v), _)] => {
                return Ok(CheckedTerm {
                    var: Some(v.clone()),
                })
            }
            [(Token::Iri(_), _)] => return Ok(CheckedTerm { var: None }),
            [(Token::Blank(_), _)] if matches!(pos, Pos::Subject | Pos::Object) => "blank",
            [(Token::Word(w), _)] if w == "a" && pos == Pos::Predicate => "a",
            [(Token::Word(w), _)] if (w == "true" || w == "false") && literal_ok => "boolean",
            [(Token::Str { datatype, .. }, _)] if literal_ok => match datatype.as_deref() {
                None | Some(Token::Iri(_)) => "literal",
                Some(_) => return fail(
                    "uses a prefixed datatype name; a rule has no PREFIX declarations, write the \
                     full <IRI>"
                        .to_string(),
                ),
            },
            [(t, _)] if numeric(t) && literal_ok => "number",
            [(Token::Plus | Token::Minus, a), (t, b)]
                if numeric(t) && literal_ok && a.end == b.start =>
            {
                "number"
            }
            [(Token::PName(..), _)] => {
                return fail(
                    "is a prefixed name; a rule has no PREFIX declarations, write the full <IRI>"
                        .to_string(),
                )
            }
            [] => return Err(format!("{what} is empty")),
            [_] => "",
            _ => return fail("is not a single SPARQL term".to_string()),
        };
    if kind.is_empty() {
        let allowed = match pos {
            Pos::Subject => "a variable, an <IRI> or a _:blank node",
            Pos::Predicate => "a variable, an <IRI> or 'a'",
            Pos::Object => "a variable, an <IRI>, a _:blank node or a literal",
            Pos::Source => "a variable or an <IRI>",
            Pos::Target | Pos::Value => "a variable, an <IRI> or a literal",
        };
        return fail(format!("is not {allowed}"));
    }
    Ok(CheckedTerm { var: None })
}

/// Validate the `return` label: non-empty, and `<Rule:label>` must be
/// exactly one IRI token.
fn check_label(label: &str) -> Result<(), String> {
    if label.is_empty() {
        return Err("the label of the return information is empty".to_string());
    }
    if label.len() > MAX_TERM_BYTES {
        return Err(format!(
            "the label of the return information is longer than {MAX_TERM_BYTES} bytes"
        ));
    }
    let bad = || {
        Err(format!(
            "the label of the return information {label:?} cannot be part of an IRI \
             (no spaces, control characters or any of <>\"{{}}|^`\\)"
        ))
    };
    if label.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return bad();
    }
    let iri = format!("<Rule:{label}>");
    match tokens_covering(&iri, &[]) {
        Ok(toks) if matches!(toks.as_slice(), [(Token::Iri(_), s)] if *s == (0..iri.len())) => {
            Ok(())
        }
        _ => bad(),
    }
}

/// Whether a (validated) filter has a top-level `||`, which binds looser
/// than the `&&` that joins a condition's filters.
pub(crate) fn has_top_level_or(filter: &str) -> bool {
    let Ok(toks) = tokens_covering(filter, &[' ', '\t', '\n', '\r']) else {
        return false;
    };
    let mut depth = 0i64;
    for (t, _) in toks {
        match t {
            Token::LParen | Token::LBrace | Token::LBracket => depth += 1,
            Token::RParen | Token::RBrace | Token::RBracket => depth -= 1,
            Token::Or if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Validate one filter expression (see the module docs).
fn check_filter(f: &str) -> Result<(), String> {
    if f.trim().is_empty() {
        return Err("the filter is empty".to_string());
    }
    if f.len() > MAX_FILTER_BYTES {
        return Err(format!(
            "the filter is longer than {MAX_FILTER_BYTES} bytes"
        ));
    }
    if f.chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Err(format!("the filter {f:?} contains a control character"));
    }
    let toks = tokens_covering(f, &[' ', '\t', '\n', '\r'])
        .map_err(|e| format!("the filter {f:?} {e}"))?;
    // Brackets must nest within the filter: it can then never close the
    // `FILTER(` (or the group) it is placed in.
    let mut stack: Vec<Token> = Vec::new();
    for (t, _) in &toks {
        let close = match t {
            Token::LParen | Token::LBrace | Token::LBracket => {
                stack.push(t.clone());
                continue;
            }
            Token::RParen => Token::LParen,
            Token::RBrace => Token::LBrace,
            Token::RBracket => Token::LBracket,
            Token::PName(..) => {
                return Err(format!(
                    "the filter {f:?} uses a prefixed name; a rule has no PREFIX declarations, \
                     write the full <IRI>"
                ))
            }
            _ => continue,
        };
        if stack.pop() != Some(close) {
            return Err(format!("the filter {f:?} has unbalanced brackets"));
        }
    }
    if !stack.is_empty() {
        return Err(format!("the filter {f:?} has unbalanced brackets"));
    }
    // It must be one complete expression.
    let probe = format!("SELECT * WHERE {{ FILTER( {f} ) }}");
    crate::parser::sparql::parse(&probe)
        .map(|_| ())
        .map_err(|e| format!("the filter {f:?} is not a valid SPARQL expression ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn rule(text: &str) -> Result<RuleDef, String> {
        RuleDef::from_json(&json::parse(text).unwrap())
    }

    const UNCLE: &str = r#"{"rulename":"舅舅","description":"母亲的男性同胞是舅舅","isenable":1,"type":1,"logic":1,
        "conditions":[{"patterns":[
            {"subject":"?x ","predicate":"<母亲>","object":"?y"},
            {"subject":"?y ","predicate":"<父亲>","object":"?z"},
            {"subject":"?k","predicate":"<父亲>","object":"?z"},
            {"subject":"?k","predicate":"<性别>","object":"\"男\"^^<http://www.w3.org/2001/XMLSchema#string>"}],
            "filters":[],"count_info":{}}],
        "return":{"source":"?x","target":"?k","label":"舅舅"}}"#;

    #[test]
    fn the_upstream_example_validates() {
        let r = rule(UNCLE).unwrap();
        assert_eq!(r.rule_type, RuleType::Relation);
        assert!(!r.union);
        assert_eq!(r.conditions.len(), 1);
        assert_eq!(r.conditions[0].patterns.len(), 4);
        // Raw texts are kept, whitespace included.
        assert_eq!(r.conditions[0].patterns[0].subject, "?x ");
        assert_eq!(r.source, "?x");
        assert_eq!(r.target, "?k");
        assert_eq!(r.label, "舅舅");
    }

    #[test]
    fn rule_names_are_safe_file_names() {
        for ok in ["孤儿", "rule-1", "a.b", "x y", "uncle_2"] {
            assert!(validate_rule_name(ok).is_ok(), "{ok}");
        }
        let long = "x".repeat(MAX_RULE_NAME_CHARS + 1);
        let wide = "舅".repeat(81); // 243 bytes
        for bad in [
            "",
            "../x",
            "a/b",
            "a\\b",
            "..",
            "a..b",
            ".hidden",
            "a\0b",
            "a\nb",
            long.as_str(),
            wide.as_str(),
        ] {
            assert!(validate_rule_name(bad).is_err(), "{bad:?}");
        }
    }

    fn with_pattern(s: &str, p: &str, o: &str) -> String {
        format!(
            r#"{{"type":1,"logic":1,"conditions":[{{"patterns":[{{"subject":{},"predicate":{},"object":{}}}]}}],
               "return":{{"source":"?x","target":"?x","label":"L"}}}}"#,
            json::quote(s),
            json::quote(p),
            json::quote(o)
        )
    }

    #[test]
    fn terms_must_be_single_tokens() {
        assert!(rule(&with_pattern("?x", "<p>", "?y")).is_ok());
        assert!(rule(&with_pattern(" ?x\t", "a", "?y")).is_ok());
        for (s, p, o) in [
            ("?x", "<p>", "?y . ?a ?b ?c"),
            ("?x", "<p> ?q", "?y"),
            ("?x", "<p>", "?y }"),
            ("?x", "<p>", "?y } ; DROP ALL ; INSERT DATA { <a> <b> <c> }"),
            ("?x #", "<p>", "?y"),
            ("?x", "<p>", "?y #"),
            ("?x", "ex:p", "?y"),
            ("\"lit\"", "<p>", "?y"),
            ("?x", "\"p\"", "?y"),
            ("?x", "<p>", "\"v\"^^xsd:string"),
            ("?x", "<p>", ""),
            ("?x\n", "<p>", "?y"),
            ("?x", "<p", "?y"),
            ("?x", "<p>", "?y\u{0}"),
        ] {
            let r = rule(&with_pattern(s, p, o));
            let err = r.expect_err(&format!("accepted {s:?} {p:?} {o:?}"));
            assert!(err.contains("pattern 1"), "{err}");
        }
        // Literals, numbers and booleans are objects.
        for o in [
            "\"v\"",
            "\"v\"@en",
            "\"1\"^^<http://www.w3.org/2001/XMLSchema#int>",
            "12",
            "-1.5",
            "true",
            "_:b",
        ] {
            let r = rule(&with_pattern("?x", "<p>", o));
            assert!(r.is_ok(), "{o}: {r:?}");
        }
    }

    #[test]
    fn return_terms_are_checked_and_bound() {
        let base = |ret: &str| {
            format!(
                r#"{{"type":1,"logic":1,"conditions":[{{"patterns":[{{"subject":"?x","predicate":"<p>","object":"?y"}}]}}],
                   "return":{ret}}}"#
            )
        };
        assert!(rule(&base(r#"{"source":"?x","target":"?y","label":"L"}"#)).is_ok());
        let unbound = rule(&base(r#"{"source":"?x","target":"?zz","label":"L"}"#)).unwrap_err();
        assert!(unbound.contains("?zz"), "{unbound}");
        assert!(rule(&base(r#"{"source":"","target":"?y","label":"L"}"#)).is_err());
        assert!(
            rule(&base(r#"{"source":"?x","label":"L"}"#)).is_err(),
            "no target"
        );
        assert!(
            rule(&base(r#"{"source":"?x","target":"?y"}"#)).is_err(),
            "no label"
        );
        for label in ["a b", "a>b", "a>  <x", "x}", "\"", "a\nb"] {
            let ret = format!(
                r#"{{"source":"?x","target":"?y","label":{}}}"#,
                json::quote(label)
            );
            assert!(rule(&base(&ret)).is_err(), "label {label:?}");
        }
        // A property rule needs a value, and ignores target.
        let prop = r#"{"type":0,"logic":1,"conditions":[{"patterns":[{"subject":"?x","predicate":"<p>","object":"?y"}]}],
                       "return":{"source":"?x","label":"L","value":"\"v\""}}"#;
        assert_eq!(rule(prop).unwrap().rule_type, RuleType::Property);
        let no_value = prop.replace(r#","value":"\"v\"""#, "");
        assert!(rule(&no_value).is_err());
    }

    #[test]
    fn filters_must_be_confined_expressions() {
        let with_filter = |f: &str| {
            format!(
                r#"{{"type":1,"logic":1,"conditions":[{{"patterns":[{{"subject":"?x","predicate":"<p>","object":"?y"}}],"filters":[{}]}}],
                   "return":{{"source":"?x","target":"?y","label":"L"}}}}"#,
                json::quote(f)
            )
        };
        for ok in [
            "?y > 1",
            "?y != <a> || ?y = <b>",
            "regex(str(?y), \"^a\")",
            "NOT EXISTS { ?y <q> ?x }",
        ] {
            assert!(rule(&with_filter(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "?y > 1 ) || ( true",
            "1) } ; DROP ALL ; INSERT DATA { <a> <b> <c> . FILTER((true",
            "true # comment",
            "?y > ",
            "ex:f(?y)",
            "?y > 1 ?x",
            "EXISTS { ?y <q> ?x ",
        ] {
            assert!(rule(&with_filter(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn structure_errors_use_upstream_messages() {
        assert_eq!(
            rule(r#"{"type":1,"return":{}}"#).unwrap_err(),
            "the reason file has not the conditons information"
        );
        assert_eq!(
            rule(r#"{"type":1,"conditions":[{"patterns":[{"subject":"?x","predicate":"<p>","object":"?y"}]}]}"#)
                .unwrap_err(),
            "the reason file has not the return information"
        );
        assert!(rule(r#"{"conditions":[],"return":{}}"#).is_err());
        assert!(rule(r#"{"conditions":{},"return":{}}"#).is_err());
        assert!(rule(r#"{"logic":"x","conditions":[{}],"return":{}}"#).is_err());
        assert!(rule(r#"[1]"#).is_err());
    }

    #[test]
    fn the_listed_condition_shape_is_read() {
        let r = rule(
            r#"{"type":1,"logic":1,"conditions":[{"condition":{"patterns":[{"subject":"?x","predicate":"<p>","object":"?y"}],"filters":[]},"logic":1}],
                "return":{"source":"?x","target":"?y","label":"L"}}"#,
        )
        .unwrap();
        assert_eq!(r.conditions[0].patterns.len(), 1);
    }

    #[test]
    fn top_level_or_is_detected() {
        assert!(has_top_level_or("?a = 1 || ?b = 2"));
        assert!(!has_top_level_or("(?a = 1 || ?b = 2)"));
        assert!(!has_top_level_or("?a = 1 && ?b = 2"));
        assert!(!has_top_level_or("EXISTS { ?a <p> ?b FILTER(?a || ?b) }"));
    }
}
