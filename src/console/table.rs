//! Text output in upstream gconsole's shapes: the boxed table of
//! `Util::printConsole` (upstream `src/Util/PrettyPrint.h`), result cells as
//! N-Triples terms, and indented JSON (`nlohmann::json::dump(4)`).

use std::borrow::Cow;

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

/// Whether a terminal acts on `c` rather than showing it: C0 controls, DEL
/// and C1 controls (escape sequences retitle the window, move the cursor,
/// write the clipboard …).
fn is_control(c: char) -> bool {
    let u = c as u32;
    u < 0x20 || (0x7f..0xa0).contains(&u)
}

/// `s` as text to print: every control character ([`is_control`]) written
/// as an escape — `\n` / `\r` / `\t`, else `\uXXXX` — so what a query
/// answer holds cannot drive the operator's terminal. With `keep_layout`, a
/// line break and a tab stay as they are (multi-line text).
pub fn printable(s: &str, keep_layout: bool) -> Cow<'_, str> {
    let escaped = |c: char| is_control(c) && !(keep_layout && matches!(c, '\n' | '\t'));
    if !s.chars().any(escaped) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            c if !escaped(c) => out.push(c),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push_str(&format!("\\u{:04X}", c as u32)),
        }
    }
    Cow::Owned(out)
}

/// The table upstream's `Util::printConsole(headers, rows)` prints: a rule
/// of dashes, the centred headers, a rule, one line per row, a rule —
/// columns separated by `|` with one space of padding. A row shorter than
/// the headers prints only its cells. Control characters in a header or a
/// cell are escaped ([`printable`]).
pub fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    let headers: Vec<Cow<'_, str>> = headers.iter().map(|h| printable(h, false)).collect();
    let rows: Vec<Vec<Cow<'_, str>>> = rows
        .iter()
        .map(|r| r.iter().map(|c| printable(c, false)).collect())
        .collect();
    let n = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in &rows {
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
    for row in &rows {
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

/// A literal's lexical form as upstream's console shows it in a table cell:
/// unescaped, as upstream keeps it in a result set and `format=n-triple`
/// sends it (so a graph function's JSON value reads as JSON, `"{"paths":
/// …}"`), except that a line break or tab is written `\n` / `\r` / `\t`
/// — upstream would print it raw and break the table's lines — and any
/// other control character `\uXXXX` ([`printable`]).
fn cell_literal(value: &str) -> Cow<'_, str> {
    printable(value, false)
}

/// A SPARQL-JSON binding (`{"type","value"[,"datatype"][,"xml:lang"|"lang"]}`)
/// as upstream's console shows it (its `format=n-triple` cells): `<iri>`,
/// `_:b`, `"lex"`, `"lex"@lang`, `"lex"^^<datatype>`, the lexical form
/// unescaped ([`cell_literal`]).
pub fn binding_term(b: &Json) -> String {
    let value = b.get("value").and_then(Json::as_str).unwrap_or_default();
    let kind = b.get("type").and_then(Json::as_str).unwrap_or_default();
    match kind {
        "uri" => format!("<{}>", printable(value, false)),
        "bnode" if value.starts_with("_:") => printable(value, false).into_owned(),
        "bnode" => format!("_:{}", printable(value, false)),
        _ => {
            let lang = b
                .get("xml:lang")
                .or_else(|| b.get("lang"))
                .and_then(Json::as_str)
                .filter(|l| !l.is_empty())
                .map(|l| printable(l, false));
            let datatype = b
                .get("datatype")
                .and_then(Json::as_str)
                .map(|d| printable(d, false));
            let lex = cell_literal(value);
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
/// key order). No control character is printed raw ([`printable`]).
pub fn pretty_json(j: &Json) -> String {
    let mut out = String::new();
    write_pretty(j, 0, &mut out);
    printable(&out, true).into_owned()
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
    fn control_characters_never_reach_the_terminal() {
        let b = |s: &str| crate::json::parse(s).unwrap();
        // OSC (retitle the window), BEL, CSI (colours) and a C1 CSI.
        let cell = binding_term(&b(
            r#"{"type":"literal","value":"pre\u001b]0;PWNED\u0007\u001b[31mRED\u009b"}"#,
        ));
        assert_eq!(cell, "\"pre\\u001B]0;PWNED\\u0007\\u001B[31mRED\\u009B\"");
        assert_eq!(
            binding_term(&b(r#"{"type":"uri","value":"http://e/\u001b[2J"}"#)),
            "<http://e/\\u001B[2J>"
        );
        assert_eq!(
            binding_term(&b(r#"{"type":"bnode","value":"_:b\u007f"}"#)),
            "_:b\\u007F"
        );
        // Whatever reaches a table (the n-triple format's cells, headers).
        let t = render(&["?x\u{1b}"], &[vec!["a\u{1b}[2Jb\u{9b}".into()]]);
        assert!(!t.contains(['\u{1b}', '\u{9b}']), "{t:?}");
        assert!(t.contains("a\\u001B[2Jb\\u009B"), "{t:?}");
        // Multi-line text keeps its line breaks and tabs, nothing else.
        assert_eq!(printable("a\nb\tc\r\u{7f}", true), "a\nb\tc\\r\\u007F");
        assert_eq!(printable("plain", false), "plain");
        assert!(!pretty_json(&b(r#"{"k":"x\u0085y"}"#)).contains('\u{85}'));
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
        // Upstream's cell: the lexical form unescaped (a line break kept on
        // the row's line).
        assert_eq!(
            binding_term(&b(
                r#"{"type":"literal","value":"a \"q\"\nb\\c","lang":"en"}"#
            )),
            "\"a \"q\"\\nb\\c\"@en"
        );
        assert_eq!(
            binding_term(&b(
                r#"{"type":"literal","value":"{\"paths\":[{\"src\":\"<a>\",\"length\":2}]}"}"#
            )),
            r#""{"paths":[{"src":"<a>","length":2}]}""#
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
