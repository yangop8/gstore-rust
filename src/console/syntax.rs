//! The console's input syntax: where a statement ends, which command it is,
//! its arguments and its output redirection (upstream
//! `src/Main/gconsole.cpp`: `execute_line`, `find_command`,
//! `parse_arguments`, the readline `;` binding).
//!
//! * **Statements end with `;`** and may span lines. Upstream ends a
//!   statement at the first `;` typed anywhere; here a `;` ends one only
//!   where it can: in SPARQL text, outside `{…}` groups, string literals,
//!   `<IRI>`s and `#` comments (so `?s :p ?o ; :q ?z` and `"a;b"` work, and
//!   SPARQL Update's `;`-separated operations stay one statement inside a
//!   group); in a console command, outside `"…"` arguments and comments.
//!   Several statements may share a line.
//! * **Comments** start with `#` and run to the end of the line (in a
//!   console command: a `#` at the start of a word). A statement made only
//!   of comments is skipped.
//! * **Redirection**: `<statement> > file` and `>> file` (append), found as
//!   upstream does — the last `>` of the statement not followed by a `"`,
//!   `}` or `)` — except that a `>` closing a trailing `<IRI>` (`LOAD
//!   <file.nt>`) is not one, nor is a `>` with no file name after it.

/// The SPARQL keywords that start a query or update typed directly at the
/// prompt (upstream: `select`, `insert`, `delete`, `prefix`, `base`).
const SPARQL_KEYWORDS: &[&str] = &[
    "select", "ask", "construct", "describe", "insert", "delete", "prefix", "base", "load", "with",
    "add", "move", "copy",
];

/// What the scan of the buffered input found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// A complete statement (without its `;`, trimmed); the first
    /// `consumed` bytes of the buffer are used up.
    Statement { text: String, consumed: usize },
    /// Only whitespace and comments: the first `consumed` bytes can go.
    Blank { consumed: usize },
    /// A statement has started but its `;` has not been typed yet.
    Incomplete,
}

/// The next statement of `buf`. With `at_eof` (no more input will come) a
/// final statement without `;` is complete too.
pub fn next_statement(buf: &str, at_eof: bool) -> Next {
    let start = skip_blank(buf);
    if start >= buf.len() {
        return Next::Blank { consumed: buf.len() };
    }
    let body = &buf[start..];
    let end = if is_sparql(body) {
        find_sparql_end(body)
    } else {
        find_command_end(body)
    };
    match end {
        Some(semi) => {
            let text = body[..semi].trim().to_string();
            let consumed = start + semi + 1;
            if text.is_empty() {
                Next::Blank { consumed }
            } else {
                Next::Statement { text, consumed }
            }
        }
        None if at_eof => Next::Statement {
            text: body.trim().to_string(),
            consumed: buf.len(),
        },
        None => Next::Incomplete,
    }
}

/// Split all of `text` into statements (a final one may lack its `;`):
/// `-e` commands and the statements of a `sparql <file>`.
pub fn split_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        match next_statement(rest, true) {
            Next::Statement { text, consumed } => {
                out.push(text);
                rest = &rest[consumed..];
            }
            Next::Blank { consumed } => {
                if consumed >= rest.len() {
                    break;
                }
                rest = &rest[consumed..];
            }
            Next::Incomplete => break,
        }
    }
    out
}

/// The byte offset of the first character of `buf` that is neither
/// whitespace nor inside a `#` comment line.
fn skip_blank(buf: &str) -> usize {
    let bytes = buf.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b if b.is_ascii_whitespace() => i += 1,
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    i
}

/// The first word of `text`: its leading run of non-whitespace characters.
pub fn first_word(text: &str) -> &str {
    let t = text.trim_start();
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    &t[..end]
}

/// The leading run of ASCII letters of `text` (after whitespace),
/// lowercased: the keyword a SPARQL statement starts with (`SELECT*` and
/// `select?x` start with `select`).
fn leading_keyword(text: &str) -> String {
    text.trim_start()
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Whether `text` (a statement, or the start of one) is SPARQL typed at the
/// prompt rather than a console command. Besides the query and update
/// keywords, `drop` / `create` are SPARQL when followed by `GRAPH` or
/// `SILENT` (and `drop` by `DEFAULT`, `NAMED`, `ALL`), and `clear` when it
/// has an argument (the console's `clear` takes none).
pub fn is_sparql(text: &str) -> bool {
    let kw = leading_keyword(text);
    if kw.is_empty() {
        return false;
    }
    let after = &text.trim_start()[kw.len()..];
    // `selectx` is not `select`: the keyword must end there.
    if after
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return false;
    }
    if SPARQL_KEYWORDS.contains(&kw.as_str()) {
        return true;
    }
    let second = leading_keyword(after);
    let second_ends = after
        .trim_start()
        .get(second.len()..)
        .and_then(|s| s.chars().next())
        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
    let second = if second_ends { second } else { String::new() };
    match kw.as_str() {
        "drop" => matches!(
            second.as_str(),
            "graph" | "silent" | "default" | "named" | "all"
        ),
        "create" => matches!(second.as_str(), "graph" | "silent"),
        "clear" => !second.is_empty(),
        _ => false,
    }
}

/// The length of an `<IRI>` starting at `bytes[i]` (`'<'`), if one does:
/// SPARQL's IRIREF — no whitespace, control characters or ``<>"{}|^`\``
/// before the closing `>`.
fn iri_len(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'>' => return Some(j + 1 - i),
            b'<' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\' => return None,
            b if b <= b' ' => return None,
            _ => j += 1,
        }
    }
    None
}

/// The offset of the `;` ending the SPARQL statement at the start of `text`.
fn find_sparql_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            q @ (b'"' | b'\'') => {
                i = skip_sparql_string(bytes, i, q);
                continue;
            }
            b'<' => {
                if let Some(n) = iri_len(bytes, i) {
                    i += n;
                    continue;
                }
            }
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The offset just past the SPARQL string literal opened by quote `q` at
/// `bytes[i]`: a long (`"""` / `'''`) string runs to its closing triple
/// quote, a short one to its closing quote or — unterminated — the end of
/// the line. Backslash escapes are skipped.
fn skip_sparql_string(bytes: &[u8], i: usize, q: u8) -> usize {
    let long = bytes.len() >= i + 3 && bytes[i + 1] == q && bytes[i + 2] == q;
    let mut j = if long { i + 3 } else { i + 1 };
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'\n' if !long => return j,
            b if b == q => {
                if !long {
                    return j + 1;
                }
                if bytes.len() >= j + 3 && bytes[j + 1] == q && bytes[j + 2] == q {
                    return j + 3;
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    bytes.len()
}

/// The offset of the `;` ending the console command at the start of `text`:
/// the first one outside a `"…"` argument and a comment.
fn find_command_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b';' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// `text` without its `#` comments (a `#` at the start of a word, outside
/// `"…"`): what a console command's arguments are parsed from.
pub fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_quote = false;
    let mut prev_ws = true;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '#' && !in_quote && prev_ws {
            for c in chars.by_ref() {
                if c == '\n' {
                    out.push('\n');
                    break;
                }
            }
            prev_ws = true;
            continue;
        }
        if c == '"' {
            in_quote = !in_quote;
        }
        if c == '\n' {
            in_quote = false;
        }
        prev_ws = c.is_whitespace();
        out.push(c);
    }
    out
}

/// An output redirection: `> path` (truncate) or `>> path` (append).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub path: String,
    pub append: bool,
}

/// Split a trailing `> file` / `>> file` off `stmt` (upstream's
/// `execute_line`: scanning back from the end, stop at `"`, `}` or `)`;
/// the first `>` met is the redirection), except that a `>` closing a
/// trailing `<IRI>` is not one, nor a `>` with nothing after it.
pub fn split_redirect(stmt: &str) -> (String, Option<Redirect>) {
    let bytes = stmt.as_bytes();
    let mut j = bytes.len();
    while j > 0 {
        j -= 1;
        match bytes[j] {
            b'"' | b'}' | b')' => break,
            b'>' => {
                if closes_iri(bytes, j) {
                    break;
                }
                let path = stmt[j + 1..].trim();
                if path.is_empty() {
                    break;
                }
                let append = j > 0 && bytes[j - 1] == b'>';
                let cut = if append { j - 1 } else { j };
                return (
                    stmt[..cut].trim_end().to_string(),
                    Some(Redirect {
                        path: path.to_string(),
                        append,
                    }),
                );
            }
            _ => {}
        }
    }
    (stmt.to_string(), None)
}

/// Whether the `>` at `bytes[j]` closes an `<IRI>`.
fn closes_iri(bytes: &[u8], j: usize) -> bool {
    let mut i = j;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'<' => return iri_len(bytes, i) == Some(j + 1 - i),
            b'>' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\' => return false,
            b if b <= b' ' => return false,
            _ => {}
        }
    }
    false
}

/// Why [`parse_arguments`] refused a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidArguments;

/// The arguments of a console command (upstream `parse_arguments`): words
/// separated by whitespace; a word starting with `"` runs to the next `"`,
/// which must end the word (the quotes are removed — upstream keeps them,
/// which no command can use).
pub fn parse_arguments(text: &str) -> Result<Vec<String>, InvalidArguments> {
    let mut args = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if chars[i] == '"' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j] != '"' {
                j += 1;
            }
            let closed = j < chars.len();
            let ends_word = chars.get(j + 1).is_none_or(|c| c.is_whitespace());
            if !closed || !ends_word {
                return Err(InvalidArguments);
            }
            args.push(chars[start..j].iter().collect());
            i = j + 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        args.push(chars[start..i].iter().collect());
    }
    Ok(args)
}

/// A command statement split into its (lowercased) command word and its
/// arguments, comments removed.
pub fn parse_command(stmt: &str) -> Result<(String, Vec<String>), InvalidArguments> {
    let text = strip_comments(stmt);
    let word = first_word(&text).to_string();
    let rest = &text.trim_start()[word.len()..];
    let args = parse_arguments(rest)?;
    Ok((word.to_ascii_lowercase(), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(buf: &str) -> Option<String> {
        match next_statement(buf, false) {
            Next::Statement { text, .. } => Some(text),
            _ => None,
        }
    }

    #[test]
    fn statements_end_at_semicolons_outside_groups_strings_and_iris() {
        assert_eq!(stmt("use lubm;\n").as_deref(), Some("use lubm"));
        assert_eq!(stmt("use lubm\n"), None);
        assert_eq!(
            stmt("select * where { ?s <p> ?o ; <q> ?z };").as_deref(),
            Some("select * where { ?s <p> ?o ; <q> ?z }")
        );
        assert_eq!(
            next_statement("select * where {\n ?s ?p ?o ;\n", false),
            Next::Incomplete
        );
        assert_eq!(
            stmt("SELECT ?x WHERE { ?x <p> \"a;b\" } ;").as_deref(),
            Some("SELECT ?x WHERE { ?x <p> \"a;b\" }")
        );
        assert_eq!(
            stmt("select ?x where { ?x <http://e/a;b> 'it''s;' };").as_deref(),
            Some("select ?x where { ?x <http://e/a;b> 'it''s;' }")
        );
        // A comment hides a `;` (and the `}` of `# }`).
        assert_eq!(
            next_statement("select * where { # x; }\n", false),
            Next::Incomplete
        );
        // `<` as an operator is not an IRI.
        assert_eq!(
            stmt("select * where { ?s <p> ?o FILTER(?o < 3) };").as_deref(),
            Some("select * where { ?s <p> ?o FILTER(?o < 3) }")
        );
        // Long strings span lines and may hold quotes.
        assert_eq!(
            stmt("insert data { <a> <b> \"\"\"x\n\"y\"; \"\"\" };").as_deref(),
            Some("insert data { <a> <b> \"\"\"x\n\"y\"; \"\"\" }")
        );
        // Update operations separated by `;` at the top level: one each.
        assert_eq!(
            stmt("insert data { <a> <b> <c> } ; delete data { <a> <b> <c> };").as_deref(),
            Some("insert data { <a> <b> <c> }")
        );
    }

    #[test]
    fn several_statements_and_comment_lines() {
        let buf = "# a comment; not a statement\n  use lubm; show;\n";
        let Next::Statement { text, consumed } = next_statement(buf, false) else {
            panic!()
        };
        assert_eq!(text, "use lubm");
        assert_eq!(stmt(&buf[consumed..]).as_deref(), Some("show"));
        assert_eq!(
            next_statement("# only a comment\n  \n", false),
            Next::Blank { consumed: 20 }
        );
        assert_eq!(next_statement(" ; ", false), Next::Blank { consumed: 2 });
        assert_eq!(
            split_statements("use a; select * where {?s ?p ?o}\n; pdb"),
            vec!["use a", "select * where {?s ?p ?o}", "pdb"]
        );
        assert_eq!(split_statements("  # nothing\n"), Vec::<String>::new());
        // Commands: `;` inside a quoted argument does not end them; an
        // apostrophe is just a character.
        assert_eq!(
            stmt("create \"a;b\" x.nt;").as_deref(),
            Some("create \"a;b\" x.nt")
        );
        assert_eq!(stmt("addusr o'neil;").as_deref(), Some("addusr o'neil"));
        assert_eq!(stmt("show lubm # c; still a comment\n"), None);
    }

    #[test]
    fn sparql_is_told_from_commands() {
        for s in [
            "select * where {?s ?p ?o}",
            "SELECT*{?s ?p ?o}",
            "Ask { ?s ?p ?o }",
            "construct where {?s ?p ?o}",
            "DESCRIBE <a>",
            "insert data { <a> <b> <c> }",
            "DELETE WHERE { ?s ?p ?o }",
            "PREFIX ex: <http://e/> SELECT * {?s ?p ?o}",
            "BASE <http://e/> SELECT * {?s ?p ?o}",
            "LOAD <file:///x.nt>",
            "CLEAR DEFAULT",
            "clear graph <g>",
            "DROP GRAPH <g>",
            "drop silent graph <g>",
            "DROP ALL",
            "CREATE GRAPH <g>",
            "WITH <g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }",
            "ADD <a> TO <b>",
        ] {
            assert!(is_sparql(s), "{s}");
        }
        // `clear;` followed by more input is still the console's command.
        assert_eq!(stmt("clear;\nshow;\n").as_deref(), Some("clear"));
        for s in [
            "clear;\nshow;",
            "clear > out.txt",
            "use lubm",
            "drop lubm",
            "drop graphs",
            "create lubm data.nt",
            "clear",
            "show",
            "selection",
            "addusr bob",
            "help select",
            "quit",
        ] {
            assert!(!is_sparql(s), "{s}");
        }
    }

    #[test]
    fn redirects_are_found_like_upstream() {
        assert_eq!(
            split_redirect("select * where {?s ?p ?o} > out.txt"),
            (
                "select * where {?s ?p ?o}".to_string(),
                Some(Redirect {
                    path: "out.txt".into(),
                    append: false
                })
            )
        );
        assert_eq!(
            split_redirect("showdbs >> logs/dbs.txt"),
            (
                "showdbs".to_string(),
                Some(Redirect {
                    path: "logs/dbs.txt".into(),
                    append: true
                })
            )
        );
        // `>` inside the group / a filter is not a redirection.
        let q = "select * where {?s ?p ?o FILTER(?o > 3)}";
        assert_eq!(split_redirect(q), (q.to_string(), None));
        // Nor is the `>` closing a trailing IRI, nor one without a file.
        let q = "LOAD <file:///data/x.nt>";
        assert_eq!(split_redirect(q), (q.to_string(), None));
        let q = "show >";
        assert_eq!(split_redirect(q), (q.to_string(), None));
        // A redirect after a trailing IRI still is one.
        assert_eq!(
            split_redirect("describe <a> > d.txt").1,
            Some(Redirect {
                path: "d.txt".into(),
                append: false
            })
        );
    }

    #[test]
    fn arguments_and_comments() {
        assert_eq!(
            parse_command("CREATE lubm data/lubm.nt").unwrap(),
            ("create".to_string(), vec!["lubm".into(), "data/lubm.nt".into()])
        );
        assert_eq!(
            parse_command("create \"my db\"  \"a b.nt\"").unwrap().1,
            vec!["my db".to_string(), "a b.nt".to_string()]
        );
        assert_eq!(parse_command("create \"my db").unwrap_err(), InvalidArguments);
        assert_eq!(parse_command("create \"my\"db").unwrap_err(), InvalidArguments);
        assert_eq!(
            parse_command("show lubm # the lubm db\n").unwrap().1,
            vec!["lubm".to_string()]
        );
        assert_eq!(
            parse_command("batchinsert data#1.nt").unwrap().1,
            vec!["data#1.nt".to_string()]
        );
        assert_eq!(parse_command("pdb").unwrap(), ("pdb".to_string(), vec![]));
        assert_eq!(
            strip_comments("a # b\nc \"#d\" #e"),
            "a \nc \"#d\" ".to_string()
        );
    }
}
