//! Compile a [`RuleDef`] to the SPARQL statements of upstream gStore's
//! `ReasonHelper::compileReasonRule` (`src/Reason/Reason.cpp`).
//!
//! The text is upstream's, byte for byte (its spacing included — clients
//! print it): for a rule with conditions `C1 … Cn` the WHERE clause is
//!
//! ```text
//! C = "{" + (" " + s + " " + p + " " + o + ".")*  + [" filter(" + "  " + f1 + " " + (" && " + fi + " ")* + ")."] + "}"
//! W = C1 [" union "] C2 … Cn        (" union " only when logic = 0)
//! ```
//!
//! and, with `L` the label (inferred triples use the predicate `<Rule:L>`):
//!
//! | statement | property rule (`type` 0) | relation rule (`type` 1) |
//! |---|---|---|
//! | `insert_sparql` | `insert {S <Rule:L> V. } where W'` | `insert { S <Rule:L> T. } where W'` |
//! | `delete_sparql` | `delete where {?x <Rule:L> V.}` | `delete where {?x <Rule:L> ?y.}` |
//! | `select_sparql` | `select ?s ?p ?o where { bind(<Rule:L> as ?p). bind(V as ?o). ?s <Rule:L> V. }` | `select ?s ?p ?o where { bind(<Rule:L> as ?p). ?s <Rule:L> ?o. }` |
//! | `check_sparql` | `select (count(*) as ?result) where W'` | same |
//!
//! where `W'` is `W` for one condition and `"{ " + W + " }"` for several.
//!
//! **One deliberate difference:** when a condition has several filters and
//! one of them has a top-level `||`, that filter is wrapped in parentheses.
//! Upstream joins the filters with `&&` as they are, so `["?a=1 || ?b=2",
//! "?c=3"]` would mean `?a=1 || (?b=2 && ?c=3)` instead of the intended
//! conjunction of the two filters.
//!
//! Every piece was validated by [`RuleDef::from_json`]; the four statements
//! are parsed once more here and their shape checked (one `INSERT … WHERE`
//! with a single template triple, one `DELETE WHERE` of a single triple, two
//! `SELECT`s), so a rule can only ever produce these statements.

use super::rules::{has_top_level_or, RuleDef, RuleType};
use crate::parser::sparql::{self, ast::Query, ast::UpdateOp};

/// The compiled statements of a rule (the rule file's `insert_sparql`,
/// `delete_sparql`, `check_sparql` and `select_sparql`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRule {
    /// Infers the rule's triples.
    pub insert_sparql: String,
    /// Upstream's retraction statement (every triple with the rule's
    /// predicate — and, for a property rule, its value). Reported to clients;
    /// retraction here removes exactly the rule's own triples (see
    /// `super::manage`).
    pub delete_sparql: String,
    /// Counts the rule's matches.
    pub check_sparql: String,
    /// Lists the triples with the rule's predicate.
    pub select_sparql: String,
}

/// The `<Rule:label>` predicate a rule's inferred triples use, as an IRI
/// (without angle brackets).
pub fn rule_predicate(label: &str) -> String {
    format!("Rule:{label}")
}

/// The WHERE clause `W` (see the module docs).
fn where_clause(def: &RuleDef) -> String {
    let n = def.conditions.len();
    let mut out = String::new();
    for (i, cond) in def.conditions.iter().enumerate() {
        let mut sub = String::new();
        for p in &cond.patterns {
            sub.push(' ');
            sub.push_str(&p.subject);
            sub.push(' ');
            sub.push_str(&p.predicate);
            sub.push(' ');
            sub.push_str(&p.object);
            sub.push('.');
        }
        let several = cond.filters.len() > 1;
        let mut filter = String::new();
        for (j, f) in cond.filters.iter().enumerate() {
            let f = if several && has_top_level_or(f) {
                format!("({f})")
            } else {
                f.clone()
            };
            if j > 0 {
                filter.push_str(" && ");
            } else {
                filter.push_str("  ");
            }
            filter.push_str(&f);
            filter.push(' ');
        }
        if !cond.filters.is_empty() {
            filter = format!(" filter({filter}).");
        }
        out.push('{');
        out.push_str(&sub);
        out.push_str(&filter);
        out.push('}');
        if n > 1 && i < n - 1 && def.union {
            out.push_str(" union ");
        }
    }
    out
}

/// Compile `def` (see the module docs). Fails only if a generated statement
/// does not parse to the expected shape — which validation should already
/// have ruled out.
pub fn compile(def: &RuleDef) -> Result<CompiledRule, String> {
    let w = where_clause(def);
    let several = def.conditions.len() > 1;
    let w_full = if several { format!("{{ {w} }}") } else { w };
    let label = &def.label;
    let (insert_sparql, delete_sparql, select_sparql) = match def.rule_type {
        RuleType::Property => {
            let (source, value) = (&def.source, &def.value);
            (
                format!("insert {{{source} <Rule:{label}> {value}. }} where {w_full}"),
                format!("delete where {{?x <Rule:{label}> {value}.}}"),
                format!(
                    "select ?s ?p ?o where {{ bind(<Rule:{label}> as ?p). bind({value} as ?o). \
                     ?s <Rule:{label}> {value}. }}"
                ),
            )
        }
        RuleType::Relation => {
            let (source, target) = (&def.source, &def.target);
            (
                format!("insert {{ {source} <Rule:{label}> {target}. }} where {w_full}"),
                format!("delete where {{?x <Rule:{label}> ?y.}}"),
                format!(
                    "select ?s ?p ?o where {{ bind(<Rule:{label}> as ?p). ?s <Rule:{label}> ?o. }}"
                ),
            )
        }
    };
    let check_sparql = format!("select (count(*) as ?result) where {w_full}");
    let compiled = CompiledRule {
        insert_sparql,
        delete_sparql,
        check_sparql,
        select_sparql,
    };
    verify(&compiled)?;
    Ok(compiled)
}

/// Parse the compiled statements and check their shape.
fn verify(c: &CompiledRule) -> Result<(), String> {
    let parse = |s: &str| sparql::parse(s).map_err(|e| format!("{e} (in: {s})"));
    match parse(&c.insert_sparql)? {
        Query::Update(ops) => match ops.as_slice() {
            [UpdateOp::Modify {
                with: None,
                delete,
                insert,
                using,
                using_named,
                ..
            }] if delete.is_empty()
                && insert.len() == 1
                && insert[0].graph.is_none()
                && using.is_empty()
                && using_named.is_empty() => {}
            _ => return Err("the compiled insert_sparql is not a single INSERT … WHERE".into()),
        },
        _ => return Err("the compiled insert_sparql is not an update".into()),
    }
    match parse(&c.delete_sparql)? {
        Query::Update(ops) => match ops.as_slice() {
            [UpdateOp::Modify {
                with: None,
                delete,
                insert,
                ..
            }] if insert.is_empty() && delete.len() == 1 => {}
            _ => return Err("the compiled delete_sparql is not a single DELETE WHERE".into()),
        },
        _ => return Err("the compiled delete_sparql is not an update".into()),
    }
    for s in [&c.check_sparql, &c.select_sparql] {
        if !matches!(parse(s)?, Query::Select(_)) {
            return Err(format!("the compiled statement is not a SELECT: {s}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn compiled(text: &str) -> CompiledRule {
        let def = RuleDef::from_json(&json::parse(text).unwrap()).unwrap();
        compile(&def).unwrap()
    }

    const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

    /// Upstream `API_USAGE.md`'s relation example, compiled as
    /// `Reason.cpp` (1.4) does.
    #[test]
    fn upstream_relation_example() {
        let c = compiled(
            r#"{"rulename":"舅舅","type":1,"logic":1,"conditions":[{"patterns":[
                {"subject":"?x ","predicate":"<母亲>","object":"?y"},
                {"subject":"?y ","predicate":"<父亲>","object":"?z"},
                {"subject":"?k","predicate":"<父亲>","object":"?z"},
                {"subject":"?k","predicate":"<性别>","object":"\"男\"^^<http://www.w3.org/2001/XMLSchema#string>"}],
                "filters":[],"count_info":{}}],
                "return":{"source":"?x","target":"?k","label":"舅舅"}}"#,
        );
        let w = format!(
            "{{ ?x  <母亲> ?y. ?y  <父亲> ?z. ?k <父亲> ?z. ?k <性别> \"男\"^^<{XSD_STRING}>.}}"
        );
        assert_eq!(
            c.insert_sparql,
            format!("insert {{ ?x <Rule:舅舅> ?k. }} where {w}")
        );
        assert_eq!(c.delete_sparql, "delete where {?x <Rule:舅舅> ?y.}");
        assert_eq!(
            c.select_sparql,
            "select ?s ?p ?o where { bind(<Rule:舅舅> as ?p). ?s <Rule:舅舅> ?o. }"
        );
        assert_eq!(
            c.check_sparql,
            format!("select (count(*) as ?result) where {w}")
        );
    }

    /// Upstream `API_USAGE.md`'s property example.
    #[test]
    fn upstream_property_example() {
        let c = compiled(
            r#"{"rulename":"孤儿","type":0,"logic":1,"conditions":[{"patterns":[
                {"subject":"?x ","predicate":"<父亲>","object":"?y"},
                {"subject":"?x","predicate":"<母亲>","object":"?z"},
                {"subject":"?y","predicate":"<状态>","object":"\"去世\"^^<http://www.w3.org/2001/XMLSchema#string>"},
                {"subject":"?z","predicate":"<状态>","object":"\"去世\"^^<http://www.w3.org/2001/XMLSchema#string>"}],
                "filters":[],"count_info":{}}],
                "return":{"source":"?x","label":"状态","value":"\"孤儿\"^^<http://www.w3.org/2001/XMLSchema#string>"}}"#,
        );
        let v = format!("\"孤儿\"^^<{XSD_STRING}>");
        let d = format!("\"去世\"^^<{XSD_STRING}>");
        let w = format!("{{ ?x  <父亲> ?y. ?x <母亲> ?z. ?y <状态> {d}. ?z <状态> {d}.}}");
        assert_eq!(
            c.insert_sparql,
            format!("insert {{?x <Rule:状态> {v}. }} where {w}")
        );
        assert_eq!(
            c.delete_sparql,
            format!("delete where {{?x <Rule:状态> {v}.}}")
        );
        assert_eq!(
            c.select_sparql,
            format!(
                "select ?s ?p ?o where {{ bind(<Rule:状态> as ?p). bind({v} as ?o). ?s <Rule:状态> {v}. }}"
            )
        );
        assert_eq!(
            c.check_sparql,
            format!("select (count(*) as ?result) where {w}")
        );
    }

    #[test]
    fn several_conditions_union_and_filters() {
        let rule = |logic: i64| {
            format!(
                r#"{{"type":1,"logic":{logic},"conditions":[
                    {{"patterns":[{{"subject":"?a","predicate":"<p>","object":"?b"}}],"filters":["?b != <x>"]}},
                    {{"patterns":[{{"subject":"?a","predicate":"<q>","object":"?b"}}],"filters":["?b != <y>","isIRI(?b)"]}}],
                    "return":{{"source":"?a","target":"?b","label":"r"}}}}"#
            )
        };
        let c1 = "{ ?a <p> ?b. filter(  ?b != <x> ).}";
        let c2 = "{ ?a <q> ?b. filter(  ?b != <y>  && isIRI(?b) ).}";
        let union = compiled(&rule(0));
        assert_eq!(
            union.insert_sparql,
            format!("insert {{ ?a <Rule:r> ?b. }} where {{ {c1} union {c2} }}")
        );
        assert_eq!(
            union.check_sparql,
            format!("select (count(*) as ?result) where {{ {c1} union {c2} }}")
        );
        let and = compiled(&rule(1));
        assert_eq!(
            and.insert_sparql,
            format!("insert {{ ?a <Rule:r> ?b. }} where {{ {c1}{c2} }}")
        );
    }

    #[test]
    fn a_top_level_or_filter_is_parenthesized_among_several() {
        let c = compiled(
            r#"{"type":1,"logic":1,"conditions":[{"patterns":[{"subject":"?a","predicate":"<p>","object":"?b"}],
                "filters":["?b = <x> || ?b = <y>","isIRI(?b)"]}],
                "return":{"source":"?a","target":"?b","label":"r"}}"#,
        );
        assert!(
            c.insert_sparql
                .contains("filter(  (?b = <x> || ?b = <y>)  && isIRI(?b) )."),
            "{}",
            c.insert_sparql
        );
        // Alone, it is kept as is (upstream's text).
        let c = compiled(
            r#"{"type":1,"logic":1,"conditions":[{"patterns":[{"subject":"?a","predicate":"<p>","object":"?b"}],
                "filters":["?b = <x> || ?b = <y>"]}],
                "return":{"source":"?a","target":"?b","label":"r"}}"#,
        );
        assert!(c.insert_sparql.contains("filter(  ?b = <x> || ?b = <y> )."));
    }
}
