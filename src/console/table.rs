//! Text output in upstream gconsole's shapes: the boxed table of
//! `Util::printConsole` (upstream `src/Util/PrettyPrint.h`), result cells as
//! N-Triples terms, and indented JSON (`nlohmann::json::dump(4)`).

use crate::json::Json;

/// Upstream's `_max_column_size`: a longer cell widens its column no further
/// (it is still printed whole).
const MAX_COLUMN_WIDTH: usize = 100;

/// The terminal width of `s`: two columns for East Asian wide characters
/// (upstream counts such a character's three UTF-8 bytes as its width and
/// then pads by its two display columns — the same alignment for CJK text),
/// none for control characters, one otherwise.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7f..0xa0).contains(&u) {
        return 0;
    }
    let wide = matches!(
        u,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD
    );
    if wide {
        2
    } else {
        1
    }
}

/// `s` followed by spaces up to `width` columns.
fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    let mut out = String::with_capacity(s.len() + width.saturating_sub(w));
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(w)));
    out
}

/// The table upstream's `Util::printConsole(headers, rows)` prints: a rule
/// of dashes, the centred headers, a rule, one line per row, a rule —
/// columns separated by `|` with one space of padding. A row shorter than
/// the headers prints only its cells.
pub fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    let n = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().take(n).enumerate() {
            widths[i] = widths[i].max(display_width(cell).min(MAX_COLUMN_WIDTH));
        }
    }
    let total = n + 1 + widths.iter().map(|w| w + 2).sum::<usize>();
    let rule = "-".repeat(total);
    let mut out = String::new();
    out.push_str(&rule);
    out.push('\n');
    out.push('|');
    for (h, w) in headers.iter().zip(&widths) {
        // Upstream: `half = width / 2 - header.size() / 2` spaces, then the
        // header, left-aligned in the column.
        let half = (w / 2).saturating_sub(display_width(h) / 2);
        let centred = format!("{}{h}", " ".repeat(half));
        out.push(' ');
        out.push_str(&pad(&centred, *w));
        out.push_str(" |");
    }
    out.push('\n');
    out.push_str(&rule);
    out.push('\n');
    for row in rows {
        out.push('|');
        for (cell, w) in row.iter().take(n).zip(&widths) {
            out.push(' ');
            out.push_str(&pad(cell, *w));
            out.push_str(" |");
        }
        out.push('\n');
    }
    out.push_str(&rule);
    out.push('\n');
    out
}

/// [`render`] with owned header names.
pub fn render_owned(headers: &[String], rows: &[Vec<String>]) -> String {
    let h: Vec<&str> = headers.iter().map(String::as_str).collect();
    render(&h, rows)
}

/// A literal's lexical form escaped for one line of N-Triples.
fn escape_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// A SPARQL-JSON binding (`{"type","value"[,"datatype"][,"xml:lang"|"lang"]}`)
/// as the N-Triples term upstream's console shows (its `format=n-triple`
/// cells): `<iri>`, `_:b`, `"lex"`, `"lex"@lang`, `"lex"^^<datatype>`.
pub fn binding_term(b: &Json) -> String {
    let value = b.get("value").and_then(Json::as_str).unwrap_or_default();
    let kind = b.get("type").and_then(Json::as_str).unwrap_or_default();
    match kind {
        "uri" => format!("<{value}>"),
        "bnode" if value.starts_with("_:") => value.to_string(),
        "bnode" => format!("_:{value}"),
        _ => {
            let lang = b
                .get("xml:lang")
                .or_else(|| b.get("lang"))
                .and_then(Json::as_str)
                .filter(|l| !l.is_empty());
            let datatype = b.get("datatype").and_then(Json::as_str);
            let lex = escape_literal(value);
            match (lang, datatype) {
                (Some(l), _) => format!("\"{lex}\"@{l}"),
                (None, Some(dt)) => format!("\"{lex}\"^^<{dt}>"),
                (None, None) => format!("\"{lex}\""),
            }
        }
    }
}

/// `j` indented like `nlohmann::json::dump(4)`: four spaces per level,
/// `"key": value`, one array element or object entry per line, `[]` / `{}`
/// for empty ones. Object keys keep their order (sort first for upstream's
/// key order).
pub fn pretty_json(j: &Json) -> String {
    let mut out = String::new();
    write_pretty(j, 0, &mut out);
    out
}

fn write_pretty(j: &Json, level: usize, out: &mut String) {
    let indent = |out: &mut String, level: usize| out.extend(std::iter::repeat_n(' ', 4 * level));
    match j {
        Json::Arr(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                indent(out, level + 1);
                write_pretty(item, level + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, level);
            out.push(']');
        }
        Json::Obj(entries) if !entries.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in entries.iter().enumerate() {
                indent(out, level + 1);
                crate::json::write_str(k, out);
                out.push_str(": ");
                write_pretty(v, level + 1, out);
                if i + 1 < entries.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, level);
            out.push('}');
        }
        other => other.write_to(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_match_upstream_pretty_print() {
        let t = render(
            &["name", "value"],
            &[
                vec!["database".into(), "lubm".into()],
                vec!["triple_num".into(), "25".into()],
            ],
        );
        assert_eq!(
            t,
            "----------------------\n\
             |    name    | value |\n\
             ----------------------\n\
             | database   | lubm  |\n\
             | triple_num | 25    |\n\
             ----------------------\n"
        );
    }

    #[test]
    fn empty_tables_and_wide_characters() {
        assert_eq!(render(&["?x"], &[]), "------\n| ?x |\n------\n------\n");
        // A CJK cell is two columns per character.
        let t = render(&["?p"], &[vec!["<关注>".into()], vec!["<a>".into()]]);
        assert_eq!(
            t,
            "----------\n|   ?p   |\n----------\n| <关注> |\n| <a>    |\n----------\n"
        );
        // Long cells widen the column only up to 100 columns.
        let long = "x".repeat(150);
        let t = render(&["c"], &[vec![long.clone()]]);
        assert!(t.starts_with(&format!("{}\n", "-".repeat(104))));
        assert!(t.contains(&format!("| {long} |")));
    }

    #[test]
    fn terms_and_json() {
        let b = |s: &str| crate::json::parse(s).unwrap();
        assert_eq!(
            binding_term(&b(r#"{"type":"uri","value":"http://e/a"}"#)),
            "<http://e/a>"
        );
        assert_eq!(
            binding_term(&b(r#"{"type":"bnode","value":"_:b1"}"#)),
            "_:b1"
        );
        assert_eq!(binding_term(&b(r#"{"type":"bnode","value":"b1"}"#)), "_:b1");
        assert_eq!(
            binding_term(&b(r#"{"type":"literal","value":"a \"q\"\nb","lang":"en"}"#)),
            "\"a \\\"q\\\"\\nb\"@en"
        );
        assert_eq!(
            binding_term(&b(
                r#"{"type":"typed-literal","value":"1","datatype":"http://www.w3.org/2001/XMLSchema#integer"}"#
            )),
            "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
        assert_eq!(
            binding_term(&b(r#"{"type":"literal","value":"x"}"#)),
            "\"x\""
        );
        assert_eq!(
            pretty_json(&b(r#"{"paths":[{"src":"<a>","len":2}],"e":[],"o":{}}"#)),
            "{\n    \"paths\": [\n        {\n            \"src\": \"<a>\",\n            \"len\": 2\n        }\n    ],\n    \"e\": [],\n    \"o\": {}\n}"
        );
    }
}
