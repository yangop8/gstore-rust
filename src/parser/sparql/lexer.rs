//! SPARQL tokenizer.
//!
//! Produces a flat [`Token`] stream for the recursive-descent parser. The one
//! subtlety is `<`: it begins both an IRI reference (`<http://…>`) and the
//! less-than operators (`<`, `<=`). We resolve it by attempting an IRIREF scan
//! (read up to a closing `>` with no whitespace/illegal chars); on failure we
//! fall back to the comparison operator.

use crate::error::{GStoreError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// An IRI reference, contents only (no angle brackets).
    Iri(String),
    /// A prefixed name `prefix:local` (default prefix → empty `prefix`).
    PName(String, String),
    /// A `?x` / `$x` variable (name only).
    Var(String),
    /// A blank node label `_:x`.
    Blank(String),
    /// A string literal with optional language tag / datatype.
    Str {
        value: String,
        lang: Option<String>,
        /// Datatype as either an IRI or a prefixed name, resolved later.
        datatype: Option<Box<Token>>,
    },
    /// An integer literal.
    Integer(String),
    /// A decimal literal (has a `.`, no exponent).
    Decimal(String),
    /// A double literal (has an exponent).
    Double(String),
    /// A bareword: keyword or function name. Compared case-insensitively.
    Word(String),

    // punctuation / operators
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Semicolon,
    Star,
    Plus,
    Minus,
    Slash,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Not,
    Eof,
}

/// Tokenize an entire SPARQL string.
pub fn tokenize(input: &str) -> Result<Vec<Token>> {
    Lexer {
        chars: input.chars().collect(),
        pos: 0,
    }
    .run()
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn err(&self, msg: impl Into<String>) -> GStoreError {
        GStoreError::SparqlParse(msg.into())
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn run(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            self.skip_ws_and_comments();
            let Some(c) = self.peek() else {
                out.push(Token::Eof);
                return Ok(out);
            };
            let tok = match c {
                '{' => self.single(Token::LBrace),
                '}' => self.single(Token::RBrace),
                '(' => self.single(Token::LParen),
                ')' => self.single(Token::RParen),
                '[' => self.single(Token::LBracket),
                ']' => self.single(Token::RBracket),
                ',' => self.single(Token::Comma),
                ';' => self.single(Token::Semicolon),
                '*' => self.single(Token::Star),
                '+' => self.single(Token::Plus),
                '-' => self.single(Token::Minus),
                '/' => self.single(Token::Slash),
                '=' => self.single(Token::Eq),
                '!' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        Token::Ne
                    } else {
                        Token::Not
                    }
                }
                '&' => {
                    self.bump();
                    if self.bump() == Some('&') {
                        Token::And
                    } else {
                        return Err(self.err("expected '&&'"));
                    }
                }
                '|' => {
                    self.bump();
                    if self.bump() == Some('|') {
                        Token::Or
                    } else {
                        return Err(self.err("expected '||'"));
                    }
                }
                '>' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        Token::Ge
                    } else {
                        Token::Gt
                    }
                }
                '<' => self.lex_lt_or_iri()?,
                '?' | '$' => self.lex_var()?,
                '"' | '\'' => self.lex_string()?,
                '_' => self.lex_blank()?,
                '.' => {
                    // '.' may start a decimal like `.5`, otherwise it's a dot.
                    if self.peek2().is_some_and(|d| d.is_ascii_digit()) {
                        self.lex_number()?
                    } else {
                        self.single(Token::Dot)
                    }
                }
                c if c.is_ascii_digit() => self.lex_number()?,
                c if is_name_start(c) => self.lex_word_or_pname()?,
                ':' => self.lex_word_or_pname()?, // default-prefix name like `:foo`
                other => return Err(self.err(format!("unexpected character '{other}'"))),
            };
            out.push(tok);
        }
    }

    fn single(&mut self, t: Token) -> Token {
        self.bump();
        t
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.pos += 1;
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    /// `<` — try an IRIREF, else a `<`/`<=` operator.
    fn lex_lt_or_iri(&mut self) -> Result<Token> {
        let start = self.pos;
        self.bump(); // consume '<'
        let mut iri = String::new();
        loop {
            match self.peek() {
                Some('>') => {
                    self.bump();
                    return Ok(Token::Iri(iri));
                }
                // Characters illegal inside an IRIREF ⇒ this was an operator.
                Some(c)
                    if c.is_whitespace()
                        || matches!(c, '<' | '"' | '{' | '}' | '|' | '^' | '`') =>
                {
                    break;
                }
                Some('\\') => {
                    // \uXXXX / \UXXXXXXXX inside IRI.
                    self.bump();
                    match self.bump() {
                        Some('u') => self.read_hex_into(&mut iri, 4)?,
                        Some('U') => self.read_hex_into(&mut iri, 8)?,
                        _ => break,
                    }
                }
                Some(c) => {
                    iri.push(c);
                    self.pos += 1;
                }
                None => break,
            }
        }
        // Not an IRIREF — rewind and emit the comparison operator.
        self.pos = start;
        self.bump(); // '<'
        if self.peek() == Some('=') {
            self.bump();
            Ok(Token::Le)
        } else {
            Ok(Token::Lt)
        }
    }

    fn lex_var(&mut self) -> Result<Token> {
        self.bump(); // '?' or '$'
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                name.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if name.is_empty() {
            return Err(self.err("empty variable name"));
        }
        Ok(Token::Var(name))
    }

    fn lex_blank(&mut self) -> Result<Token> {
        // peek2 distinguishes `_:` blank node from a word starting with '_'.
        if self.peek2() == Some(':') {
            self.bump(); // '_'
            self.bump(); // ':'
            let mut label = String::new();
            while let Some(c) = self.peek() {
                if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                    label.push(c);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            Ok(Token::Blank(label))
        } else {
            self.lex_word_or_pname()
        }
    }

    /// A quoted string, possibly triple-quoted, with optional `@lang`/`^^dt`.
    fn lex_string(&mut self) -> Result<Token> {
        let quote = self.bump().unwrap(); // '"' or '\''
        let triple = self.peek() == Some(quote) && self.peek2() == Some(quote);
        if triple {
            self.bump();
            self.bump();
        }
        let mut value = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string literal")),
                Some('\\') => {
                    self.bump();
                    self.read_string_escape(&mut value)?;
                }
                Some(c) if c == quote => {
                    if triple {
                        if self.peek2() == Some(quote)
                            && self.chars.get(self.pos + 2).copied() == Some(quote)
                        {
                            self.bump();
                            self.bump();
                            self.bump();
                            break;
                        }
                        value.push(c);
                        self.pos += 1;
                    } else {
                        self.bump();
                        break;
                    }
                }
                Some(c) => {
                    value.push(c);
                    self.pos += 1;
                }
            }
        }
        // optional language tag or datatype
        let (lang, datatype) = if self.peek() == Some('@') {
            self.bump();
            let mut tag = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_alphanumeric() || c == '-' {
                    tag.push(c);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            (Some(tag), None)
        } else if self.peek() == Some('^') && self.peek2() == Some('^') {
            self.bump();
            self.bump();
            // datatype is an IRIREF or a prefixed name
            let dt = match self.peek() {
                Some('<') => self.lex_lt_or_iri()?,
                _ => self.lex_word_or_pname()?,
            };
            (None, Some(Box::new(dt)))
        } else {
            (None, None)
        };
        Ok(Token::Str {
            value,
            lang,
            datatype,
        })
    }

    fn read_string_escape(&mut self, out: &mut String) -> Result<()> {
        match self.bump() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            Some('u') => self.read_hex_into(out, 4)?,
            Some('U') => self.read_hex_into(out, 8)?,
            Some(c) => return Err(self.err(format!("invalid string escape '\\{c}'"))),
            None => return Err(self.err("dangling backslash in string")),
        }
        Ok(())
    }

    fn read_hex_into(&mut self, out: &mut String, n: usize) -> Result<()> {
        let mut code = 0u32;
        for _ in 0..n {
            let c = self
                .bump()
                .ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| self.err(format!("bad hex digit '{c}'")))?;
            code = code * 16 + d;
        }
        out.push(char::from_u32(code).ok_or_else(|| self.err("invalid code point"))?);
        Ok(())
    }

    /// Numbers: integer, decimal (`.`), or double (exponent).
    fn lex_number(&mut self) -> Result<Token> {
        let mut s = String::new();
        let mut is_decimal = false;
        let mut is_double = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.pos += 1;
            } else if c == '.' {
                // Only consume '.' as part of the number if a digit follows, so
                // `3 .` (number then statement dot) is not mis-lexed.
                if self.peek2().is_some_and(|d| d.is_ascii_digit()) || !is_decimal {
                    is_decimal = true;
                    s.push(c);
                    self.pos += 1;
                } else {
                    break;
                }
            } else if c == 'e' || c == 'E' {
                is_double = true;
                s.push(c);
                self.pos += 1;
                if matches!(self.peek(), Some('+') | Some('-')) {
                    s.push(self.bump().unwrap());
                }
            } else {
                break;
            }
        }
        if is_double {
            Ok(Token::Double(s))
        } else if is_decimal {
            Ok(Token::Decimal(s))
        } else {
            Ok(Token::Integer(s))
        }
    }

    /// A bareword keyword/function, or a prefixed name `pfx:local`.
    fn lex_word_or_pname(&mut self) -> Result<Token> {
        let mut head = String::new();
        while let Some(c) = self.peek() {
            if is_name_char(c) {
                head.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some(':') {
            self.bump();
            let mut local = String::new();
            while let Some(c) = self.peek() {
                // PN_LOCAL: letters, digits, '_', '-', '.', '%' (we keep it simple).
                if is_name_char(c) || c == '-' || c == '.' || c == '%' {
                    local.push(c);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            // A trailing '.' belongs to the statement, not the local name.
            while local.ends_with('.') {
                local.pop();
                self.pos -= 1;
            }
            Ok(Token::PName(head, local))
        } else {
            if head.is_empty() {
                return Err(self.err("expected a name"));
            }
            Ok(Token::Word(head))
        }
    }
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(s: &str) -> Vec<Token> {
        tokenize(s).unwrap()
    }

    #[test]
    fn lexes_iriref() {
        assert_eq!(lex("<http://a/b>")[0], Token::Iri("http://a/b".into()));
    }

    #[test]
    fn distinguishes_lt_from_iriref() {
        // `?x < ?y` → Var, Lt, Var
        let t = lex("?x < ?y");
        assert_eq!(t[0], Token::Var("x".into()));
        assert_eq!(t[1], Token::Lt);
        assert_eq!(t[2], Token::Var("y".into()));
    }

    #[test]
    fn lexes_le_and_ge() {
        assert_eq!(lex("?a <= ?b")[1], Token::Le);
        assert_eq!(lex("?a >= ?b")[1], Token::Ge);
    }

    #[test]
    fn lexes_prefixed_name() {
        assert_eq!(
            lex("foaf:knows")[0],
            Token::PName("foaf".into(), "knows".into())
        );
        assert_eq!(lex(":foo")[0], Token::PName("".into(), "foo".into()));
    }

    #[test]
    fn pname_stops_before_statement_dot() {
        // `foaf:knows.` — the '.' is the statement terminator.
        let t = lex("foaf:knows .");
        assert_eq!(t[0], Token::PName("foaf".into(), "knows".into()));
        assert_eq!(t[1], Token::Dot);
    }

    #[test]
    fn lexes_typed_literal() {
        let t = lex("\"2500\"^^xsd:integer");
        match &t[0] {
            Token::Str {
                value, datatype, ..
            } => {
                assert_eq!(value, "2500");
                assert_eq!(
                    **datatype.as_ref().unwrap(),
                    Token::PName("xsd".into(), "integer".into())
                );
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn lexes_lang_literal() {
        let t = lex("\"chat\"@fr");
        match &t[0] {
            Token::Str { value, lang, .. } => {
                assert_eq!(value, "chat");
                assert_eq!(lang.as_deref(), Some("fr"));
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn lexes_numbers() {
        assert_eq!(lex("2500")[0], Token::Integer("2500".into()));
        assert_eq!(lex("170.0")[0], Token::Decimal("170.0".into()));
        assert_eq!(lex("1e6")[0], Token::Double("1e6".into()));
    }

    #[test]
    fn lexes_boolean_operators() {
        let t = lex("&& || !");
        assert_eq!(t[0], Token::And);
        assert_eq!(t[1], Token::Or);
        assert_eq!(t[2], Token::Not);
    }

    #[test]
    fn lexes_keywords_as_words() {
        let t = lex("SELECT DISTINCT WHERE");
        assert_eq!(t[0], Token::Word("SELECT".into()));
        assert_eq!(t[1], Token::Word("DISTINCT".into()));
        assert_eq!(t[2], Token::Word("WHERE".into()));
    }

    #[test]
    fn skips_comments() {
        let t = lex("?x # comment here\n?y");
        assert_eq!(t[0], Token::Var("x".into()));
        assert_eq!(t[1], Token::Var("y".into()));
    }
}
