//! SPARQL 1.1 Query Results serializers — the content-negotiation half of the
//! [`crate::server`] endpoint.
//!
//! A SELECT [`ResultSet`] (or an ASK boolean) is rendered into one of four
//! standard result formats:
//!
//! | [`ResultFormat`] | media type | spec |
//! |------------------|-----------|------|
//! | `Json` | `application/sparql-results+json` | SPARQL 1.1 Query Results JSON |
//! | `Xml`  | `application/sparql-results+xml`  | SPARQL Query Results XML Format |
//! | `Csv`  | `text/csv`                        | SPARQL 1.1 Query Results CSV |
//! | `Tsv`  | `text/tab-separated-values`       | SPARQL 1.1 Query Results TSV |
//!
//! Every writer streams row by row through a [`Write`] sink (one `write_all` per
//! row) so the same code backs both the buffered (`Content-Length`) and the
//! chunked (`Transfer-Encoding: chunked`) response paths in `server.rs`.

use std::io::{self, Write};

use crate::model::Term;
use crate::parser::ntriples::parse_term;
use crate::query::ResultSet;

/// A SPARQL Query Results serialization format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFormat {
    /// SPARQL 1.1 Query Results JSON (the default).
    Json,
    /// SPARQL Query Results XML Format.
    Xml,
    /// SPARQL 1.1 Query Results CSV.
    Csv,
    /// SPARQL 1.1 Query Results TSV.
    Tsv,
}

impl ResultFormat {
    /// Choose a format from a `format=` query parameter (highest precedence),
    /// then an HTTP `Accept` header, falling back to [`ResultFormat::Json`] when
    /// neither names a recognized format (including `*/*` / absent).
    pub fn negotiate(format_param: Option<&str>, accept: Option<&str>) -> ResultFormat {
        if let Some(token) = format_param {
            if let Some(fmt) = ResultFormat::from_token(token) {
                return fmt;
            }
        }
        if let Some(accept) = accept {
            for media in accept.split(',') {
                // Drop any `;q=…` / parameters and normalize case.
                let m = media.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                if let Some(fmt) = ResultFormat::from_media_type(&m) {
                    return fmt;
                }
            }
        }
        ResultFormat::Json
    }

    /// The `Content-Type` to advertise for this format.
    pub fn content_type(self) -> &'static str {
        match self {
            ResultFormat::Json => "application/sparql-results+json",
            ResultFormat::Xml => "application/sparql-results+xml",
            ResultFormat::Csv => "text/csv; charset=utf-8",
            ResultFormat::Tsv => "text/tab-separated-values; charset=utf-8",
        }
    }

    /// A short `format=` token (`json` / `xml` / `csv` / `tsv`) or a full media
    /// type.
    fn from_token(t: &str) -> Option<ResultFormat> {
        match t.trim().to_ascii_lowercase().as_str() {
            "json" | "application/sparql-results+json" | "application/json" => {
                Some(ResultFormat::Json)
            }
            "xml" | "application/sparql-results+xml" | "application/xml" | "text/xml" => {
                Some(ResultFormat::Xml)
            }
            "csv" | "text/csv" => Some(ResultFormat::Csv),
            "tsv" | "tab-separated-values" | "text/tab-separated-values" => Some(ResultFormat::Tsv),
            _ => None,
        }
    }

    /// A single (already lowercased) `Accept` media type.
    fn from_media_type(m: &str) -> Option<ResultFormat> {
        match m {
            "application/sparql-results+json" | "application/json" => Some(ResultFormat::Json),
            "application/sparql-results+xml" | "application/xml" | "text/xml" => {
                Some(ResultFormat::Xml)
            }
            "text/csv" => Some(ResultFormat::Csv),
            "text/tab-separated-values" => Some(ResultFormat::Tsv),
            _ => None,
        }
    }
}

/// Serialize a SELECT [`ResultSet`] in `fmt`, streaming one `write_all` per row.
pub fn write_select<W: Write>(fmt: ResultFormat, rs: &ResultSet, w: &mut W) -> io::Result<()> {
    match fmt {
        ResultFormat::Json => write_select_json(rs, w),
        ResultFormat::Xml => write_select_xml(rs, w),
        ResultFormat::Csv => write_select_csv(rs, w),
        ResultFormat::Tsv => write_select_tsv(rs, w),
    }
}

/// Serialize an ASK boolean in `fmt`.
pub fn write_ask<W: Write>(fmt: ResultFormat, value: bool, w: &mut W) -> io::Result<()> {
    match fmt {
        ResultFormat::Json => w.write_all(format!("{{\"head\":{{}},\"boolean\":{value}}}").as_bytes()),
        ResultFormat::Xml => w.write_all(
            format!(
                "<?xml version=\"1.0\"?>\n\
                 <sparql xmlns=\"http://www.w3.org/2005/sparql-results#\">\n  \
                 <head></head>\n  <boolean>{value}</boolean>\n</sparql>\n"
            )
            .as_bytes(),
        ),
        ResultFormat::Csv => w.write_all(format!("{value}\r\n").as_bytes()),
        ResultFormat::Tsv => w.write_all(format!("{value}\n").as_bytes()),
    }
}

// --- JSON --------------------------------------------------------------------

/// SPARQL 1.1 Query Results JSON for a SELECT table.
fn write_select_json<W: Write>(rs: &ResultSet, w: &mut W) -> io::Result<()> {
    let mut head = String::from("{\"head\":{\"vars\":[");
    for (i, v) in rs.vars.iter().enumerate() {
        if i > 0 {
            head.push(',');
        }
        head.push_str(&json_str(v));
    }
    head.push_str("]},\"results\":{\"bindings\":[");
    w.write_all(head.as_bytes())?;

    for (ri, row) in rs.rows.iter().enumerate() {
        let mut s = String::new();
        if ri > 0 {
            s.push(',');
        }
        s.push('{');
        let mut first = true;
        for (ci, cell) in row.iter().enumerate() {
            if let Some(val) = cell {
                if !first {
                    s.push(',');
                }
                first = false;
                s.push_str(&json_str(&rs.vars[ci]));
                s.push(':');
                s.push_str(&binding_json(val));
            }
        }
        s.push('}');
        w.write_all(s.as_bytes())?;
    }
    w.write_all(b"]}}")
}

/// One result cell (an N-Triples surface term) as an RDF-term JSON object.
fn binding_json(cell: &str) -> String {
    match parse_term(cell) {
        Ok(Term::Iri(s)) => format!("{{\"type\":\"uri\",\"value\":{}}}", json_str(&s)),
        Ok(Term::Blank(l)) => format!("{{\"type\":\"bnode\",\"value\":{}}}", json_str(&l)),
        Ok(Term::Literal {
            value,
            datatype,
            lang,
        }) => {
            if let Some(l) = lang {
                format!(
                    "{{\"type\":\"literal\",\"value\":{},\"xml:lang\":{}}}",
                    json_str(&value),
                    json_str(&l)
                )
            } else if let Some(dt) = datatype {
                format!(
                    "{{\"type\":\"literal\",\"value\":{},\"datatype\":{}}}",
                    json_str(&value),
                    json_str(&dt)
                )
            } else {
                format!("{{\"type\":\"literal\",\"value\":{}}}", json_str(&value))
            }
        }
        Err(_) => format!("{{\"type\":\"literal\",\"value\":{}}}", json_str(cell)),
    }
}

/// Quote + escape a string as a JSON string literal (shared with `server.rs`).
pub(crate) fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- XML ---------------------------------------------------------------------

/// SPARQL Query Results XML Format for a SELECT table.
fn write_select_xml<W: Write>(rs: &ResultSet, w: &mut W) -> io::Result<()> {
    let mut head = String::from(
        "<?xml version=\"1.0\"?>\n\
         <sparql xmlns=\"http://www.w3.org/2005/sparql-results#\">\n  <head>\n",
    );
    for v in &rs.vars {
        head.push_str(&format!("    <variable name={}/>\n", xml_attr(v)));
    }
    head.push_str("  </head>\n  <results>\n");
    w.write_all(head.as_bytes())?;

    for row in &rs.rows {
        let mut s = String::from("    <result>\n");
        for (ci, cell) in row.iter().enumerate() {
            if let Some(val) = cell {
                s.push_str(&format!(
                    "      <binding name={}>{}</binding>\n",
                    xml_attr(&rs.vars[ci]),
                    binding_xml(val)
                ));
            }
        }
        s.push_str("    </result>\n");
        w.write_all(s.as_bytes())?;
    }
    w.write_all(b"  </results>\n</sparql>\n")
}

/// One result cell as a SPARQL Results XML `<binding>` child element.
fn binding_xml(cell: &str) -> String {
    match parse_term(cell) {
        Ok(Term::Iri(s)) => format!("<uri>{}</uri>", xml_text(&s)),
        Ok(Term::Blank(l)) => format!("<bnode>{}</bnode>", xml_text(&l)),
        Ok(Term::Literal {
            value,
            datatype,
            lang,
        }) => {
            if let Some(l) = lang {
                format!("<literal xml:lang={}>{}</literal>", xml_attr(&l), xml_text(&value))
            } else if let Some(dt) = datatype {
                format!("<literal datatype={}>{}</literal>", xml_attr(&dt), xml_text(&value))
            } else {
                format!("<literal>{}</literal>", xml_text(&value))
            }
        }
        Err(_) => format!("<literal>{}</literal>", xml_text(cell)),
    }
}

/// Escape text content for XML (`&`, `<`, `>`).
fn xml_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// A double-quoted, escaped XML attribute value (`&`, `<`, `>`, `"`).
fn xml_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- CSV ---------------------------------------------------------------------

/// SPARQL 1.1 Query Results CSV (RFC 4180 quoting, CRLF line endings).
fn write_select_csv<W: Write>(rs: &ResultSet, w: &mut W) -> io::Result<()> {
    let mut head = String::new();
    for (i, v) in rs.vars.iter().enumerate() {
        if i > 0 {
            head.push(',');
        }
        head.push_str(&csv_field(v));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;

    for row in &rs.rows {
        let mut s = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            if let Some(val) = cell {
                s.push_str(&csv_field(&csv_value(val)));
            }
        }
        s.push_str("\r\n");
        w.write_all(s.as_bytes())?;
    }
    Ok(())
}

/// A cell's bare CSV value: IRIs and literal lexical forms carry no syntax,
/// blank nodes keep their `_:` prefix.
fn csv_value(cell: &str) -> String {
    match parse_term(cell) {
        Ok(Term::Iri(s)) => s,
        Ok(Term::Blank(l)) => format!("_:{l}"),
        Ok(Term::Literal { value, .. }) => value,
        Err(_) => cell.to_string(),
    }
}

/// RFC 4180 field quoting: wrap in `"…"` and double inner quotes when the value
/// contains a quote, comma, or line break.
fn csv_field(s: &str) -> String {
    if s.contains(['"', ',', '\n', '\r']) {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        s.to_string()
    }
}

// --- TSV ---------------------------------------------------------------------

/// SPARQL 1.1 Query Results TSV: `?var`-prefixed header, Turtle-encoded terms,
/// tab-separated columns, LF rows.
fn write_select_tsv<W: Write>(rs: &ResultSet, w: &mut W) -> io::Result<()> {
    let mut head = String::new();
    for (i, v) in rs.vars.iter().enumerate() {
        if i > 0 {
            head.push('\t');
        }
        head.push('?');
        head.push_str(v);
    }
    head.push('\n');
    w.write_all(head.as_bytes())?;

    for row in &rs.rows {
        let mut s = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                s.push('\t');
            }
            if let Some(val) = cell {
                s.push_str(&tsv_value(val));
            }
        }
        s.push('\n');
        w.write_all(s.as_bytes())?;
    }
    Ok(())
}

/// A cell as a Turtle/SPARQL term (`<iri>`, `"lit"^^<dt>` / `"lit"@lang`,
/// `_:bnode`). The [`Term`] `Display` already escapes tabs/newlines/quotes.
fn tsv_value(cell: &str) -> String {
    match parse_term(cell) {
        Ok(t) => t.to_string(),
        Err(_) => Term::plain_literal(cell).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResultSet {
        let mut rs = ResultSet::new(vec!["x".to_string(), "y".to_string()]);
        // Row 1: an IRI and a plain literal containing a comma and a quote.
        rs.rows.push(vec![
            Some("<http://ex/a>".to_string()),
            Some("\"he said \\\"hi\\\", ok\"".to_string()),
        ]);
        // Row 2: an IRI with y unbound, plus a typed literal swapped in for x.
        rs.rows
            .push(vec![Some("<http://ex/b>".to_string()), None]);
        rs
    }

    fn render(fmt: ResultFormat, rs: &ResultSet) -> String {
        let mut buf = Vec::new();
        write_select(fmt, rs, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn negotiate_param_beats_accept_and_defaults_to_json() {
        assert_eq!(ResultFormat::negotiate(Some("csv"), None), ResultFormat::Csv);
        assert_eq!(
            ResultFormat::negotiate(Some("xml"), Some("text/csv")),
            ResultFormat::Xml
        );
        assert_eq!(
            ResultFormat::negotiate(None, Some("application/sparql-results+xml")),
            ResultFormat::Xml
        );
        // Unknown leading types are skipped; the first known one wins.
        assert_eq!(
            ResultFormat::negotiate(None, Some("text/html, text/csv;q=0.9")),
            ResultFormat::Csv
        );
        assert_eq!(ResultFormat::negotiate(None, Some("*/*")), ResultFormat::Json);
        assert_eq!(ResultFormat::negotiate(None, None), ResultFormat::Json);
        assert_eq!(
            ResultFormat::negotiate(Some("garbage"), None),
            ResultFormat::Json
        );
    }

    #[test]
    fn csv_header_quotes_and_unbound() {
        let csv = render(ResultFormat::Csv, &sample());
        let mut lines = csv.split("\r\n");
        assert_eq!(lines.next().unwrap(), "x,y");
        // The literal value (lexical only) has a comma + quotes, so it is quoted.
        assert_eq!(
            lines.next().unwrap(),
            r#"http://ex/a,"he said ""hi"", ok""#
        );
        // y is unbound -> empty trailing field.
        assert_eq!(lines.next().unwrap(), "http://ex/b,");
    }

    #[test]
    fn tsv_uses_turtle_terms_and_question_header() {
        let tsv = render(ResultFormat::Tsv, &sample());
        let mut lines = tsv.split('\n');
        assert_eq!(lines.next().unwrap(), "?x\t?y");
        // IRIs keep angle brackets; literals keep quotes (inner quotes escaped).
        assert_eq!(
            lines.next().unwrap(),
            "<http://ex/a>\t\"he said \\\"hi\\\", ok\""
        );
        assert_eq!(lines.next().unwrap(), "<http://ex/b>\t");
    }

    #[test]
    fn xml_is_well_formed_with_typed_terms() {
        let mut rs = ResultSet::new(vec!["n".to_string(), "t".to_string()]);
        rs.rows.push(vec![
            Some("<http://ex/a>".to_string()),
            Some("\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_string()),
        ]);
        let xml = render(ResultFormat::Xml, &rs);
        assert!(xml.starts_with("<?xml version=\"1.0\"?>"));
        assert!(xml.contains("<variable name=\"n\"/>"));
        assert!(xml.contains("<uri>http://ex/a</uri>"));
        assert!(xml.contains(
            "<literal datatype=\"http://www.w3.org/2001/XMLSchema#integer\">42</literal>"
        ));
        assert!(xml.trim_end().ends_with("</sparql>"));
    }

    #[test]
    fn xml_escapes_markup_in_values() {
        let mut rs = ResultSet::new(vec!["v".to_string()]);
        rs.rows
            .push(vec![Some("\"a < b & c > d\"".to_string())]);
        let xml = render(ResultFormat::Xml, &rs);
        assert!(xml.contains("<literal>a &lt; b &amp; c &gt; d</literal>"));
    }

    #[test]
    fn json_matches_existing_envelope() {
        let mut rs = ResultSet::new(vec!["o".to_string()]);
        rs.rows.push(vec![Some("<http://ex/b>".to_string())]);
        let json = render(ResultFormat::Json, &rs);
        assert_eq!(
            json,
            "{\"head\":{\"vars\":[\"o\"]},\"results\":{\"bindings\":\
             [{\"o\":{\"type\":\"uri\",\"value\":\"http://ex/b\"}}]}}"
        );
    }

    #[test]
    fn ask_formats() {
        let mut buf = Vec::new();
        write_ask(ResultFormat::Json, true, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"head\":{},\"boolean\":true}");

        let mut buf = Vec::new();
        write_ask(ResultFormat::Csv, false, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "false\r\n");

        let mut buf = Vec::new();
        write_ask(ResultFormat::Xml, true, &mut buf).unwrap();
        let xml = String::from_utf8(buf).unwrap();
        assert!(xml.contains("<boolean>true</boolean>"));
    }
}
