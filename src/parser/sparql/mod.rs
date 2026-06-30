//! SPARQL parser: tokens → [`ast::Query`].
//!
//! A hand-written recursive-descent parser (mirroring gStore's hand-written
//! `SPARQLParser`). Supported: prologue (`PREFIX`/`BASE`); `SELECT` (with
//! `DISTINCT`, projected `(expr AS ?v)`, `GROUP BY`/`HAVING`, aggregates),
//! `ASK`, `CONSTRUCT`; graph patterns with `FILTER`, `OPTIONAL`, `UNION`,
//! `MINUS`, `BIND`, `VALUES`, sub-`SELECT`, `EXISTS`/`NOT EXISTS`, and property
//! paths (`/ ^ | * + ?`, `!`); solution modifiers (`ORDER BY`/`LIMIT`/`OFFSET`);
//! `ASK`/`CONSTRUCT`/`DESCRIBE`; and UPDATE (`INSERT/DELETE DATA`,
//! `DELETE/INSERT … WHERE`, `DELETE WHERE`, `LOAD`, `CLEAR`/`DROP`/`CREATE`,
//! `;`-separated sequences). Not yet supported: `GRAPH`/`SERVICE` (named graphs).

pub mod ast;
pub mod lexer;

use std::collections::HashMap;

use crate::error::{GStoreError, Result};
use crate::model::Term;

use ast::*;
use lexer::{tokenize, Token};

/// Parse a SPARQL query or update string.
pub fn parse(input: &str) -> Result<Query> {
    let tokens = tokenize(input)?;
    Parser::new(tokens).parse_request()
}

/// A parsed predicate position: a plain term, or a property path.
enum VerbKind {
    Simple(PatternTerm),
    Path(PathExpr),
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    prefixes: HashMap<String, String>,
    base: Option<String>,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Parser {
        Parser {
            tokens,
            pos: 0,
            prefixes: HashMap::new(),
            base: None,
        }
    }

    // ---- token cursor -----------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// Look `n` tokens ahead (clamped to the trailing `Eof`).
    fn peek_at(&self, n: usize) -> &Token {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i]
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn err(&self, msg: impl Into<String>) -> GStoreError {
        GStoreError::SparqlParse(msg.into())
    }

    /// The current token as an uppercased keyword, if it is a bareword.
    fn keyword(&self) -> Option<String> {
        match self.peek() {
            Token::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }

    /// Consume the current token if it is the keyword `kw` (case-insensitive).
    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.keyword().as_deref() == Some(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Token, what: &str) -> Result<()> {
        if self.peek() == t {
            self.bump();
            Ok(())
        } else {
            Err(self.err(format!("expected {what}, found {:?}", self.peek())))
        }
    }

    // ---- top level --------------------------------------------------------

    fn parse_request(&mut self) -> Result<Query> {
        self.parse_prologue()?;
        let kw = self.keyword().ok_or_else(|| {
            self.err(format!("expected a query keyword, found {:?}", self.peek()))
        })?;
        let query = match kw.as_str() {
            "SELECT" => Query::Select(self.parse_select()?),
            "ASK" => Query::Ask(self.parse_ask()?),
            "CONSTRUCT" => Query::Construct(self.parse_construct()?),
            "DESCRIBE" => Query::Describe(self.parse_describe()?),
            "INSERT" | "DELETE" | "LOAD" | "CLEAR" | "DROP" | "CREATE" | "WITH" => {
                Query::Update(self.parse_update_sequence()?)
            }
            other => return Err(self.err(format!("unsupported query form '{other}'"))),
        };
        Ok(query)
    }

    fn parse_prologue(&mut self) -> Result<()> {
        loop {
            match self.keyword().as_deref() {
                Some("PREFIX") => {
                    self.bump();
                    let (pfx, _) = match self.bump() {
                        Token::PName(p, l) if l.is_empty() => (p, l),
                        // Lexer emits `foaf:` as PName("foaf","")
                        Token::PName(p, l) => (p, l),
                        other => {
                            return Err(self.err(format!("expected 'prefix:' , found {other:?}")))
                        }
                    };
                    let iri = match self.bump() {
                        Token::Iri(s) => s,
                        other => return Err(self.err(format!("expected <IRI>, found {other:?}"))),
                    };
                    self.prefixes.insert(pfx, iri);
                }
                Some("BASE") => {
                    self.bump();
                    match self.bump() {
                        Token::Iri(s) => self.base = Some(s),
                        other => return Err(self.err(format!("expected <IRI>, found {other:?}"))),
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    // ---- SELECT -----------------------------------------------------------

    fn parse_select(&mut self) -> Result<SelectQuery> {
        self.expect_keyword("SELECT")?;
        let distinct = self.eat_keyword("DISTINCT") || {
            // REDUCED behaves like a hint; treat as DISTINCT-less for the trunk.
            self.eat_keyword("REDUCED");
            false
        };
        let projection = self.parse_projection()?;

        self.skip_dataset_clauses()?;
        self.eat_keyword("WHERE"); // optional
        let pattern = self.parse_group_graph_pattern()?;
        let (group_by, having) = self.parse_group_having()?;
        let (order_by, limit, offset) = self.parse_solution_modifiers()?;

        Ok(SelectQuery {
            distinct,
            projection,
            pattern,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// `*` or a list of `?v` / `(expr AS ?v)` projection items.
    fn parse_projection(&mut self) -> Result<Projection> {
        if matches!(self.peek(), Token::Star) {
            self.bump();
            return Ok(Projection::All);
        }
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Token::Var(v) => {
                    items.push(SelectItem::Var(v.clone()));
                    self.bump();
                }
                Token::LParen => {
                    // (expr AS ?v)
                    self.bump();
                    let expr = self.parse_expr()?;
                    self.expect_keyword("AS")?;
                    let var = self.expect_var()?;
                    self.expect(&Token::RParen, "')'")?;
                    items.push(SelectItem::Expr(expr, var));
                }
                _ => break,
            }
        }
        if items.is_empty() {
            return Err(self.err("SELECT must list variables, (expr AS ?v), or '*'"));
        }
        Ok(Projection::Items(items))
    }

    /// `GROUP BY …` and `HAVING …` clauses (both optional, GROUP before HAVING).
    fn parse_group_having(&mut self) -> Result<(Vec<Expr>, Vec<Expr>)> {
        let mut group_by = Vec::new();
        if self.keyword().as_deref() == Some("GROUP") {
            self.bump();
            self.expect_keyword("BY")?;
            loop {
                match self.peek() {
                    Token::Var(v) => {
                        group_by.push(Expr::Var(v.clone()));
                        self.bump();
                    }
                    Token::LParen => {
                        self.bump();
                        let e = self.parse_expr()?;
                        // optional AS ?v (the bound var is ignored for grouping keys)
                        self.eat_keyword("AS");
                        if let Token::Var(_) = self.peek() {
                            self.bump();
                        }
                        self.expect(&Token::RParen, "')'")?;
                        group_by.push(e);
                    }
                    _ => break,
                }
            }
        }
        let mut having = Vec::new();
        if self.keyword().as_deref() == Some("HAVING") {
            self.bump();
            having.push(self.parse_constraint()?);
            // each additional constraint must be preceded by another HAVING keyword
            while self.eat_keyword("HAVING") {
                having.push(self.parse_constraint()?);
            }
        }
        Ok((group_by, having))
    }

    fn expect_var(&mut self) -> Result<String> {
        match self.bump() {
            Token::Var(v) => Ok(v),
            other => Err(self.err(format!("expected a ?variable, found {other:?}"))),
        }
    }

    fn parse_describe(&mut self) -> Result<DescribeQuery> {
        self.expect_keyword("DESCRIBE")?;
        let mut targets = Vec::new();
        let mut all = false;
        if matches!(self.peek(), Token::Star) {
            self.bump();
            all = true;
        } else {
            loop {
                match self.peek() {
                    Token::Var(v) => {
                        targets.push(PatternTerm::Var(v.clone()));
                        self.bump();
                    }
                    Token::Iri(_) | Token::PName(_, _) => {
                        let tok = self.peek().clone();
                        let term = self.token_to_term(&tok)?;
                        self.bump();
                        targets.push(PatternTerm::Term(term));
                    }
                    _ => break,
                }
            }
            if targets.is_empty() {
                return Err(self.err("DESCRIBE expects '*' or one or more ?var / <iri> targets"));
            }
        }
        self.skip_dataset_clauses()?;
        let pattern = if self.eat_keyword("WHERE") || matches!(self.peek(), Token::LBrace) {
            Some(self.parse_group_graph_pattern()?)
        } else {
            None
        };
        // DESCRIBE ignores solution modifiers for our purposes; consume any.
        self.parse_solution_modifiers()?;
        Ok(DescribeQuery {
            targets,
            all,
            pattern,
        })
    }

    fn parse_ask(&mut self) -> Result<AskQuery> {
        self.expect_keyword("ASK")?;
        self.skip_dataset_clauses()?;
        self.eat_keyword("WHERE");
        let pattern = self.parse_group_graph_pattern()?;
        // ASK ignores solution modifiers for our purposes; consume any present.
        self.parse_solution_modifiers()?;
        Ok(AskQuery { pattern })
    }

    /// Skip `FROM <iri>` / `FROM NAMED <iri>` dataset clauses (not modeled).
    fn skip_dataset_clauses(&mut self) -> Result<()> {
        while self.eat_keyword("FROM") {
            self.eat_keyword("NAMED");
            match self.bump() {
                Token::Iri(_) | Token::PName(_, _) => {}
                other => {
                    return Err(self.err(format!("expected <IRI> after FROM, found {other:?}")))
                }
            }
        }
        Ok(())
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(self.err(format!("expected keyword {kw}, found {:?}", self.peek())))
        }
    }

    // ---- group graph pattern ---------------------------------------------

    /// Parse `{ … }`, returning the graph-pattern algebra for its contents.
    ///
    /// Triple patterns accumulate into BGPs; group-level constructs
    /// (`OPTIONAL`, `MINUS`, `BIND`, sub-groups, `UNION`, sub-`SELECT`,
    /// `VALUES`) fold into the running conjunction in textual order. All
    /// `FILTER`s in the group apply to the whole group (wrapped at the end).
    fn parse_group_graph_pattern(&mut self) -> Result<GraphPattern> {
        self.expect(&Token::LBrace, "'{'")?;
        let mut current = GraphPattern::Empty;
        let mut filters: Vec<Expr> = Vec::new();
        let mut pending: Vec<TriplePattern> = Vec::new();
        let mut pending_paths: Vec<GraphPattern> = Vec::new();

        // Flush accumulated triples (as one BGP) and any path patterns into
        // `current`, joined on.
        macro_rules! flush {
            () => {
                if !pending.is_empty() {
                    let bgp = GraphPattern::Bgp(std::mem::take(&mut pending));
                    current = join_opt(current, bgp);
                }
                for p in std::mem::take(&mut pending_paths) {
                    current = join_opt(current, p);
                }
            };
        }

        loop {
            match self.peek() {
                Token::RBrace => {
                    self.bump();
                    break;
                }
                Token::Dot => {
                    self.bump();
                }
                Token::Word(w) if w.eq_ignore_ascii_case("FILTER") => {
                    self.bump();
                    filters.push(self.parse_constraint()?);
                }
                Token::Word(w) if w.eq_ignore_ascii_case("OPTIONAL") => {
                    self.bump();
                    flush!();
                    let group = self.parse_group_graph_pattern()?;
                    // A FILTER directly inside OPTIONAL becomes the left-join
                    // condition; otherwise the right side is the group as-is.
                    let (right, lj_filters) = match group {
                        GraphPattern::Filter(fs, inner) => (*inner, fs),
                        other => (other, Vec::new()),
                    };
                    current =
                        GraphPattern::LeftJoin(Box::new(current), Box::new(right), lj_filters);
                }
                Token::Word(w) if w.eq_ignore_ascii_case("MINUS") => {
                    self.bump();
                    flush!();
                    let right = self.parse_group_graph_pattern()?;
                    current = GraphPattern::Minus(Box::new(current), Box::new(right));
                }
                Token::Word(w) if w.eq_ignore_ascii_case("BIND") => {
                    self.bump();
                    flush!();
                    self.expect(&Token::LParen, "'('")?;
                    let expr = self.parse_expr()?;
                    self.expect_keyword("AS")?;
                    let var = self.expect_var()?;
                    self.expect(&Token::RParen, "')'")?;
                    current = GraphPattern::Extend(Box::new(current), var, expr);
                }
                Token::Word(w) if w.eq_ignore_ascii_case("VALUES") => {
                    self.bump();
                    flush!();
                    let values = self.parse_inline_values()?;
                    current = join_opt(current, values);
                }
                Token::Word(w) if w.eq_ignore_ascii_case("GRAPH") => {
                    self.bump();
                    flush!();
                    let gterm = self.parse_graph_term()?;
                    let inner = self.parse_group_graph_pattern()?;
                    current = join_opt(current, GraphPattern::Graph(gterm, Box::new(inner)));
                }
                Token::Word(w) if w.eq_ignore_ascii_case("SERVICE") => {
                    return Err(self.err(
                        "SERVICE (federated query) is not supported — no network access",
                    ));
                }
                Token::LBrace => {
                    flush!();
                    // `{ SELECT … }` is a sub-query; otherwise a group/UNION.
                    let node = if self.brace_starts_subselect() {
                        self.parse_subselect_group()?
                    } else {
                        self.parse_group_or_union()?
                    };
                    current = join_opt(current, node);
                }
                Token::Eof => return Err(self.err("unexpected end of query inside '{ }'")),
                _ => {
                    self.parse_triples_same_subject(&mut pending, &mut pending_paths)?;
                }
            }
        }
        flush!();

        if !filters.is_empty() {
            current = GraphPattern::Filter(filters, Box::new(current));
        }
        Ok(current)
    }

    /// `{…} ( UNION {…} )*` — a group, optionally unioned with more groups.
    fn parse_group_or_union(&mut self) -> Result<GraphPattern> {
        let mut left = self.parse_group_graph_pattern()?;
        while self.eat_keyword("UNION") {
            let right = self.parse_group_graph_pattern()?;
            left = GraphPattern::Union(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Does the upcoming `{` open a sub-`SELECT`? (peek past the brace.)
    fn brace_starts_subselect(&self) -> bool {
        matches!(self.peek(), Token::LBrace)
            && matches!(self.peek_at(1), Token::Word(w) if w.eq_ignore_ascii_case("SELECT"))
    }

    /// Parse `{ SELECT … }` into a [`GraphPattern::SubSelect`].
    fn parse_subselect_group(&mut self) -> Result<GraphPattern> {
        self.expect(&Token::LBrace, "'{'")?;
        let sq = self.parse_select()?;
        self.expect(&Token::RBrace, "'}'")?;
        Ok(GraphPattern::SubSelect(Box::new(sq)))
    }

    /// Parse an inline `VALUES` block: `VALUES ?v { … }` or
    /// `VALUES (?a ?b) { (..) (..) }`. `UNDEF` becomes `None`.
    fn parse_inline_values(&mut self) -> Result<GraphPattern> {
        let mut vars = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.bump();
            while let Token::Var(v) = self.peek() {
                vars.push(v.clone());
                self.bump();
            }
            self.expect(&Token::RParen, "')'")?;
            self.expect(&Token::LBrace, "'{'")?;
            let mut rows = Vec::new();
            while matches!(self.peek(), Token::LParen) {
                self.bump();
                let mut row = Vec::with_capacity(vars.len());
                while !matches!(self.peek(), Token::RParen) {
                    row.push(self.parse_values_cell()?);
                }
                self.bump(); // ')'
                rows.push(row);
            }
            self.expect(&Token::RBrace, "'}'")?;
            Ok(GraphPattern::Values(vars, rows))
        } else {
            // single-variable shorthand
            let v = self.expect_var()?;
            vars.push(v);
            self.expect(&Token::LBrace, "'{'")?;
            let mut rows = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                rows.push(vec![self.parse_values_cell()?]);
            }
            self.expect(&Token::RBrace, "'}'")?;
            Ok(GraphPattern::Values(vars, rows))
        }
    }

    /// One `VALUES` cell: a term or `UNDEF`.
    fn parse_values_cell(&mut self) -> Result<Option<Term>> {
        if self.keyword().as_deref() == Some("UNDEF") {
            self.bump();
            return Ok(None);
        }
        let tok = self.peek().clone();
        let term = self.token_to_term(&tok)?;
        self.bump();
        Ok(Some(term))
    }

    // ---- CONSTRUCT --------------------------------------------------------

    fn parse_construct(&mut self) -> Result<ConstructQuery> {
        self.expect_keyword("CONSTRUCT")?;
        // CONSTRUCT { template } WHERE { … }
        let mut template = Vec::new();
        let mut paths = Vec::new();
        self.expect(&Token::LBrace, "'{'")?;
        loop {
            match self.peek() {
                Token::RBrace => {
                    self.bump();
                    break;
                }
                Token::Dot => {
                    self.bump();
                }
                Token::Eof => return Err(self.err("unexpected end of CONSTRUCT template")),
                _ => self.parse_triples_same_subject(&mut template, &mut paths)?,
            }
        }
        if !paths.is_empty() {
            return Err(self.err("property paths are not allowed in a CONSTRUCT template"));
        }
        self.skip_dataset_clauses()?;
        self.eat_keyword("WHERE");
        let pattern = self.parse_group_graph_pattern()?;
        let (order_by, limit, offset) = self.parse_solution_modifiers()?;
        Ok(ConstructQuery {
            template,
            pattern,
            order_by,
            limit,
            offset,
        })
    }

    /// `subject  verb objlist (';' verb objlist)*`. Plain-predicate triples are
    /// pushed to `bgp`; property-path predicates emit [`GraphPattern::Path`] into
    /// `paths`.
    fn parse_triples_same_subject(
        &mut self,
        bgp: &mut Vec<TriplePattern>,
        paths: &mut Vec<GraphPattern>,
    ) -> Result<()> {
        let subject = self.parse_pattern_term()?;
        loop {
            let verb = self.parse_path_or_verb()?;
            // object list
            loop {
                let object = self.parse_pattern_term()?;
                match &verb {
                    VerbKind::Simple(p) => bgp.push(TriplePattern {
                        subject: subject.clone(),
                        predicate: p.clone(),
                        object,
                    }),
                    VerbKind::Path(path) => paths.push(GraphPattern::Path(PathPattern {
                        subject: subject.clone(),
                        path: path.clone(),
                        object,
                    })),
                }
                if matches!(self.peek(), Token::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            if matches!(self.peek(), Token::Semicolon) {
                self.bump();
                if self.starts_verb() {
                    continue;
                }
                break;
            }
            break;
        }
        Ok(())
    }

    /// Could the current token begin a verb (predicate or path)?
    fn starts_verb(&self) -> bool {
        match self.peek() {
            Token::Var(_)
            | Token::Iri(_)
            | Token::PName(_, _)
            | Token::Caret
            | Token::Not
            | Token::LParen => true,
            Token::Word(w) => w == "a",
            _ => false,
        }
    }

    /// A predicate that is either a plain term or a property path.
    fn parse_path_or_verb(&mut self) -> Result<VerbKind> {
        // A variable predicate is always a plain verb (no paths over variables).
        if let Token::Var(v) = self.peek() {
            let v = v.clone();
            self.bump();
            return Ok(VerbKind::Simple(PatternTerm::Var(v)));
        }
        let path = self.parse_path()?;
        // A bare single predicate stays a plain BGP triple (fast path).
        Ok(match path {
            PathExpr::Pred(iri) => VerbKind::Simple(PatternTerm::Term(Term::iri(iri))),
            other => VerbKind::Path(other),
        })
    }

    // ---- property paths ---------------------------------------------------

    fn parse_path(&mut self) -> Result<PathExpr> {
        // alternative: seq ( '|' seq )*
        let mut left = self.parse_path_seq()?;
        while matches!(self.peek(), Token::Pipe) {
            self.bump();
            let right = self.parse_path_seq()?;
            left = PathExpr::Alt(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_path_seq(&mut self) -> Result<PathExpr> {
        // sequence: eltOrInverse ( '/' eltOrInverse )*
        let mut left = self.parse_path_elt_or_inverse()?;
        while matches!(self.peek(), Token::Slash) {
            self.bump();
            let right = self.parse_path_elt_or_inverse()?;
            left = PathExpr::Seq(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_path_elt_or_inverse(&mut self) -> Result<PathExpr> {
        if matches!(self.peek(), Token::Caret) {
            self.bump();
            Ok(PathExpr::Inverse(Box::new(self.parse_path_elt()?)))
        } else {
            self.parse_path_elt()
        }
    }

    fn parse_path_elt(&mut self) -> Result<PathExpr> {
        let mut p = self.parse_path_primary()?;
        // postfix `*` / `+` / `?` modifiers (the lexer emits `?` with no name as
        // [`Token::Question`], so it no longer collides with the variable sigil).
        loop {
            match self.peek() {
                Token::Star => {
                    self.bump();
                    p = PathExpr::ZeroOrMore(Box::new(p));
                }
                Token::Plus => {
                    self.bump();
                    p = PathExpr::OneOrMore(Box::new(p));
                }
                Token::Question => {
                    self.bump();
                    p = PathExpr::ZeroOrOne(Box::new(p));
                }
                _ => break,
            }
        }
        Ok(p)
    }

    fn parse_path_primary(&mut self) -> Result<PathExpr> {
        match self.peek().clone() {
            Token::LParen => {
                self.bump();
                let p = self.parse_path()?;
                self.expect(&Token::RParen, "')'")?;
                Ok(p)
            }
            Token::Not => {
                self.bump();
                self.parse_negated_path_set()
            }
            Token::Word(w) if w == "a" => {
                self.bump();
                Ok(PathExpr::Pred(RDF_TYPE.to_string()))
            }
            Token::Iri(_) | Token::PName(_, _) => {
                let tok = self.peek().clone();
                let iri = match self.token_to_term(&tok)? {
                    Term::Iri(s) => s,
                    other => {
                        return Err(
                            self.err(format!("path predicate must be an IRI, got {other:?}"))
                        )
                    }
                };
                self.bump();
                Ok(PathExpr::Pred(iri))
            }
            other => Err(self.err(format!("expected a path predicate, found {other:?}"))),
        }
    }

    /// `!iri`, `!^iri`, or `!( p1 | ^p2 | … )`.
    fn parse_negated_path_set(&mut self) -> Result<PathExpr> {
        let mut set = Vec::new();
        let read_one = |this: &mut Self| -> Result<(String, bool)> {
            let inverse = if matches!(this.peek(), Token::Caret) {
                this.bump();
                true
            } else {
                false
            };
            let tok = this.peek().clone();
            let iri = if matches!(tok, Token::Word(ref w) if w == "a") {
                this.bump();
                RDF_TYPE.to_string()
            } else {
                match this.token_to_term(&tok)? {
                    Term::Iri(s) => {
                        this.bump();
                        s
                    }
                    other => {
                        return Err(this.err(format!("negated path needs an IRI, got {other:?}")))
                    }
                }
            };
            Ok((iri, inverse))
        };
        if matches!(self.peek(), Token::LParen) {
            self.bump();
            if !matches!(self.peek(), Token::RParen) {
                set.push(read_one(self)?);
                while matches!(self.peek(), Token::Pipe) {
                    self.bump();
                    set.push(read_one(self)?);
                }
            }
            self.expect(&Token::RParen, "')'")?;
        } else {
            set.push(read_one(self)?);
        }
        Ok(PathExpr::NegatedSet(set))
    }

    /// A triple-pattern position: a variable or a concrete term.
    fn parse_pattern_term(&mut self) -> Result<PatternTerm> {
        if let Token::Var(v) = self.peek() {
            let v = v.clone();
            self.bump();
            return Ok(PatternTerm::Var(v));
        }
        let tok = self.peek().clone();
        let term = self.token_to_term(&tok)?;
        self.bump();
        Ok(PatternTerm::Term(term))
    }

    // ---- FILTER expressions ----------------------------------------------

    /// A FILTER constraint: `( expr )` or a bare builtin/function call.
    fn parse_constraint(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::LParen) {
            self.bump();
            let e = self.parse_expr()?;
            self.expect(&Token::RParen, "')'")?;
            Ok(e)
        } else {
            self.parse_primary()
        }
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Token::Or) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_relational()?;
        while matches!(self.peek(), Token::And) {
            self.bump();
            let rhs = self.parse_relational()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_relational(&mut self) -> Result<Expr> {
        let lhs = self.parse_additive()?;
        let op = match self.peek() {
            Token::Eq => CompareOp::Eq,
            Token::Ne => CompareOp::Ne,
            Token::Lt => CompareOp::Lt,
            Token::Gt => CompareOp::Gt,
            Token::Le => CompareOp::Le,
            Token::Ge => CompareOp::Ge,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.parse_additive()?;
        Ok(Expr::Compare(op, Box::new(lhs), Box::new(rhs)))
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => ArithOp::Add,
                Token::Minus => ArithOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Arith(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => ArithOp::Mul,
                Token::Slash => ArithOp::Div,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Expr::Arith(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        match self.peek() {
            Token::Not => {
                self.bump();
                Ok(Expr::Not(Box::new(self.parse_unary()?)))
            }
            Token::Minus => {
                self.bump();
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.parse_unary()?)))
            }
            Token::Plus => {
                self.bump();
                Ok(Expr::Unary(UnaryOp::Plus, Box::new(self.parse_unary()?)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Token::LParen => {
                self.bump();
                let e = self.parse_expr()?;
                self.expect(&Token::RParen, "')'")?;
                Ok(e)
            }
            Token::Var(v) => {
                self.bump();
                Ok(Expr::Var(v))
            }
            Token::Word(w) => {
                let upper = w.to_ascii_uppercase();
                if upper == "TRUE" {
                    self.bump();
                    Ok(Expr::Const(Term::typed_literal("true", xsd::BOOLEAN)))
                } else if upper == "FALSE" {
                    self.bump();
                    Ok(Expr::Const(Term::typed_literal("false", xsd::BOOLEAN)))
                } else if upper == "EXISTS" {
                    self.bump();
                    let pat = self.parse_group_graph_pattern()?;
                    Ok(Expr::Exists(false, Box::new(pat)))
                } else if upper == "NOT" {
                    self.bump();
                    self.expect_keyword("EXISTS")?;
                    let pat = self.parse_group_graph_pattern()?;
                    Ok(Expr::Exists(true, Box::new(pat)))
                } else if let Some(func) = agg_func(&upper) {
                    self.bump();
                    self.parse_aggregate(func)
                } else {
                    // Builtin / function call: NAME ( args )
                    self.bump();
                    let args = if matches!(self.peek(), Token::LParen) {
                        self.parse_arg_list()?
                    } else {
                        return Err(self.err(format!("unexpected word '{w}' in expression")));
                    };
                    Ok(Expr::Builtin(upper, args))
                }
            }
            Token::Iri(_)
            | Token::PName(_, _)
            | Token::Str { .. }
            | Token::Integer(_)
            | Token::Decimal(_)
            | Token::Double(_)
            | Token::Blank(_) => {
                let tok = self.peek().clone();
                let term = self.token_to_term(&tok)?;
                self.bump();
                Ok(Expr::Const(term))
            }
            other => Err(self.err(format!("unexpected token in expression: {other:?}"))),
        }
    }

    /// Parse an aggregate call body after the function name: `( [DISTINCT]
    /// (* | expr) [; SEPARATOR = "…"] )`.
    fn parse_aggregate(&mut self, func: AggFunc) -> Result<Expr> {
        self.expect(&Token::LParen, "'('")?;
        let distinct = self.eat_keyword("DISTINCT");
        let arg = if func == AggFunc::Count && matches!(self.peek(), Token::Star) {
            self.bump();
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut sep = None;
        if func == AggFunc::GroupConcat && matches!(self.peek(), Token::Semicolon) {
            self.bump();
            self.expect_keyword("SEPARATOR")?;
            self.expect(&Token::Eq, "'='")?;
            match self.bump() {
                Token::Str { value, .. } => sep = Some(value),
                other => {
                    return Err(self.err(format!("expected a string separator, found {other:?}")))
                }
            }
        }
        self.expect(&Token::RParen, "')'")?;
        Ok(Expr::Aggregate {
            func,
            distinct,
            arg,
            sep,
        })
    }

    /// `( expr , expr , … )` argument list. Handles `()` and `(*)` (→ no args).
    fn parse_arg_list(&mut self) -> Result<Vec<Expr>> {
        self.expect(&Token::LParen, "'('")?;
        let mut args = Vec::new();
        if matches!(self.peek(), Token::Star) {
            self.bump(); // COUNT(*) etc.
            self.expect(&Token::RParen, "')'")?;
            return Ok(args);
        }
        if matches!(self.peek(), Token::RParen) {
            self.bump();
            return Ok(args);
        }
        // DISTINCT inside an aggregate arg list — tolerate and ignore.
        self.eat_keyword("DISTINCT");
        loop {
            args.push(self.parse_expr()?);
            if matches!(self.peek(), Token::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(&Token::RParen, "')'")?;
        Ok(args)
    }

    // ---- solution modifiers ----------------------------------------------

    #[allow(clippy::type_complexity)]
    fn parse_solution_modifiers(
        &mut self,
    ) -> Result<(Vec<OrderCondition>, Option<usize>, Option<usize>)> {
        let mut order_by = Vec::new();
        let mut limit = None;
        let mut offset = None;
        loop {
            match self.keyword().as_deref() {
                Some("ORDER") => {
                    self.bump();
                    self.expect_keyword("BY")?;
                    order_by = self.parse_order_conditions()?;
                }
                Some("LIMIT") => {
                    self.bump();
                    limit = Some(self.parse_usize()?);
                }
                Some("OFFSET") => {
                    self.bump();
                    offset = Some(self.parse_usize()?);
                }
                _ => break,
            }
        }
        Ok((order_by, limit, offset))
    }

    fn parse_order_conditions(&mut self) -> Result<Vec<OrderCondition>> {
        let mut conds = Vec::new();
        loop {
            let descending = match self.keyword().as_deref() {
                Some("ASC") => {
                    self.bump();
                    false
                }
                Some("DESC") => {
                    self.bump();
                    true
                }
                _ => false,
            };
            // ASC/DESC take a bracketted expression; a bare var/expr also works.
            let expr = if matches!(self.peek(), Token::LParen) {
                self.bump();
                let e = self.parse_expr()?;
                self.expect(&Token::RParen, "')'")?;
                e
            } else if let Token::Var(v) = self.peek() {
                let v = v.clone();
                self.bump();
                Expr::Var(v)
            } else {
                break;
            };
            conds.push(OrderCondition { expr, descending });
            // Stop when the next token clearly isn't another order key.
            if !matches!(self.peek(), Token::Var(_) | Token::LParen)
                && !matches!(self.keyword().as_deref(), Some("ASC") | Some("DESC"))
            {
                break;
            }
        }
        Ok(conds)
    }

    fn parse_usize(&mut self) -> Result<usize> {
        match self.bump() {
            Token::Integer(s) => s
                .parse::<usize>()
                .map_err(|_| self.err(format!("invalid integer '{s}'"))),
            other => Err(self.err(format!("expected an integer, found {other:?}"))),
        }
    }

    // ---- updates ----------------------------------------------------------

    /// `Update1 ( ';' Update1 )*`, with optional prologue between operations.
    fn parse_update_sequence(&mut self) -> Result<Vec<UpdateOp>> {
        let mut ops = Vec::new();
        loop {
            self.parse_prologue()?;
            if matches!(self.peek(), Token::Eof) {
                break;
            }
            ops.push(self.parse_update_op()?);
            if matches!(self.peek(), Token::Semicolon) {
                self.bump();
                continue;
            }
            break;
        }
        if ops.is_empty() {
            return Err(self.err("empty UPDATE request"));
        }
        Ok(ops)
    }

    fn parse_update_op(&mut self) -> Result<UpdateOp> {
        match self.keyword().as_deref() {
            Some("INSERT") => {
                self.bump();
                if self.eat_keyword("DATA") {
                    Ok(UpdateOp::InsertData(self.parse_ground_block()?))
                } else {
                    let insert = self.parse_triple_template_block()?;
                    self.skip_using_clauses()?;
                    self.expect_keyword("WHERE")?;
                    let pattern = self.parse_group_graph_pattern()?;
                    Ok(UpdateOp::Modify {
                        delete: Vec::new(),
                        insert,
                        pattern,
                    })
                }
            }
            Some("DELETE") => {
                self.bump();
                if self.eat_keyword("DATA") {
                    Ok(UpdateOp::DeleteData(self.parse_ground_block()?))
                } else if self.eat_keyword("WHERE") {
                    // DELETE WHERE { triples }: the triples are both the match
                    // pattern and the delete template.
                    let triples = self.parse_triple_template_block()?;
                    let pattern = GraphPattern::Bgp(triples.clone());
                    Ok(UpdateOp::Modify {
                        delete: triples,
                        insert: Vec::new(),
                        pattern,
                    })
                } else {
                    let delete = self.parse_triple_template_block()?;
                    let insert = if self.eat_keyword("INSERT") {
                        self.parse_triple_template_block()?
                    } else {
                        Vec::new()
                    };
                    self.skip_using_clauses()?;
                    self.expect_keyword("WHERE")?;
                    let pattern = self.parse_group_graph_pattern()?;
                    Ok(UpdateOp::Modify {
                        delete,
                        insert,
                        pattern,
                    })
                }
            }
            Some("WITH") => {
                // `WITH <iri>` sets the default graph for the modify; with a
                // single graph it has no effect, so consume and recurse.
                self.bump();
                self.expect_graph_iri()?;
                self.parse_update_op()
            }
            Some("LOAD") => {
                self.bump();
                let silent = self.eat_keyword("SILENT");
                let source = self.expect_graph_iri()?;
                if self.eat_keyword("INTO") {
                    self.expect_keyword("GRAPH")?;
                    self.expect_graph_iri()?;
                }
                Ok(UpdateOp::Load { source, silent })
            }
            Some("CLEAR") => {
                self.bump();
                let silent = self.eat_keyword("SILENT");
                let target = self.parse_graph_target()?;
                Ok(UpdateOp::Clear { target, silent })
            }
            Some("DROP") => {
                self.bump();
                let silent = self.eat_keyword("SILENT");
                let target = self.parse_graph_target()?;
                Ok(UpdateOp::Drop { target, silent })
            }
            Some("CREATE") => {
                self.bump();
                let silent = self.eat_keyword("SILENT");
                self.expect_keyword("GRAPH")?;
                let name = self.expect_graph_iri()?;
                Ok(UpdateOp::Create { name, silent })
            }
            other => Err(self.err(format!("unsupported update operation {other:?}"))),
        }
    }

    /// Parse a `{ … }` block of triple templates (variables allowed, no paths).
    fn parse_triple_template_block(&mut self) -> Result<Vec<TriplePattern>> {
        let mut patterns = Vec::new();
        let mut paths = Vec::new();
        self.expect(&Token::LBrace, "'{'")?;
        loop {
            match self.peek() {
                Token::RBrace => {
                    self.bump();
                    break;
                }
                Token::Dot => {
                    self.bump();
                }
                Token::Eof => return Err(self.err("unexpected end of template block")),
                _ => self.parse_triples_same_subject(&mut patterns, &mut paths)?,
            }
        }
        if !paths.is_empty() {
            return Err(self.err("property paths are not allowed in an INSERT/DELETE template"));
        }
        Ok(patterns)
    }

    /// `(DEFAULT | NAMED | ALL | GRAPH <iri>)` for CLEAR/DROP.
    fn parse_graph_target(&mut self) -> Result<GraphTarget> {
        match self.keyword().as_deref() {
            Some("DEFAULT") => {
                self.bump();
                Ok(GraphTarget::Default)
            }
            Some("ALL") => {
                self.bump();
                Ok(GraphTarget::All)
            }
            // NAMED targets all named graphs; we keep only a default graph, so
            // an empty-named target stands for "the (empty) set of named graphs".
            Some("NAMED") => {
                self.bump();
                Ok(GraphTarget::Named(String::new()))
            }
            Some("GRAPH") => {
                self.bump();
                Ok(GraphTarget::Named(self.expect_graph_iri()?))
            }
            _ => Err(self.err("expected DEFAULT, NAMED, ALL, or GRAPH <iri>")),
        }
    }

    /// Consume and expand an `<iri>` / `prefix:name` graph reference.
    fn expect_graph_iri(&mut self) -> Result<String> {
        match self.bump() {
            Token::Iri(s) => Ok(self.resolve_iri(&s)),
            Token::PName(p, l) => self.expand_pname(&p, &l),
            other => Err(self.err(format!("expected a graph <iri>, found {other:?}"))),
        }
    }

    /// Skip `USING [NAMED] <iri>` clauses (dataset selection; not modeled).
    fn skip_using_clauses(&mut self) -> Result<()> {
        while self.eat_keyword("USING") {
            self.eat_keyword("NAMED");
            self.expect_graph_iri()?;
        }
        Ok(())
    }

    /// The graph reference of a `GRAPH` pattern: a variable or constant IRI.
    fn parse_graph_term(&mut self) -> Result<GraphTerm> {
        match self.peek() {
            Token::Var(v) => {
                let v = v.clone();
                self.bump();
                Ok(GraphTerm::Var(v))
            }
            Token::Iri(_) | Token::PName(_, _) => {
                let tok = self.peek().clone();
                match self.token_to_term(&tok)? {
                    Term::Iri(s) => {
                        self.bump();
                        Ok(GraphTerm::Iri(s))
                    }
                    other => {
                        Err(self.err(format!("GRAPH expects an IRI or variable, got {other:?}")))
                    }
                }
            }
            other => Err(self.err(format!(
                "GRAPH expects an IRI or variable, found {other:?}"
            ))),
        }
    }

    /// Parse a `{ … }` block of ground quads (no variables): bare triples go to
    /// the default graph; `GRAPH <iri> { … }` blocks tag their triples.
    fn parse_ground_block(&mut self) -> Result<Vec<GroundTriple>> {
        self.expect(&Token::LBrace, "'{'")?;
        let mut out = Vec::new();
        loop {
            match self.peek() {
                Token::RBrace => {
                    self.bump();
                    break;
                }
                Token::Dot => {
                    self.bump();
                }
                Token::Word(w) if w.eq_ignore_ascii_case("GRAPH") => {
                    self.bump();
                    let g = match self.parse_graph_term()? {
                        GraphTerm::Iri(s) => s,
                        GraphTerm::Var(v) => {
                            return Err(
                                self.err(format!("variable ?{v} graph is not allowed in DATA"))
                            )
                        }
                    };
                    self.expect(&Token::LBrace, "'{'")?;
                    loop {
                        match self.peek() {
                            Token::RBrace => {
                                self.bump();
                                break;
                            }
                            Token::Dot => {
                                self.bump();
                            }
                            Token::Eof => return Err(self.err("unterminated GRAPH block")),
                            _ => self.parse_ground_subject(&mut out, Some(g.as_str()))?,
                        }
                    }
                }
                Token::Eof => return Err(self.err("unexpected end of query in DATA block")),
                _ => self.parse_ground_subject(&mut out, None)?,
            }
        }
        Ok(out)
    }

    /// Parse one subject's triples into ground quads tagged with `graph`.
    fn parse_ground_subject(
        &mut self,
        out: &mut Vec<GroundTriple>,
        graph: Option<&str>,
    ) -> Result<()> {
        let mut patterns = Vec::new();
        let mut paths = Vec::new();
        self.parse_triples_same_subject(&mut patterns, &mut paths)?;
        if !paths.is_empty() {
            return Err(self.err("property paths are not allowed in a DATA block"));
        }
        for p in patterns {
            out.push(GroundTriple {
                subject: self.require_ground(p.subject)?,
                predicate: self.require_ground(p.predicate)?,
                object: self.require_ground(p.object)?,
                graph: graph.map(str::to_owned),
            });
        }
        Ok(())
    }

    fn require_ground(&self, pt: PatternTerm) -> Result<Term> {
        match pt {
            PatternTerm::Term(t) => Ok(t),
            PatternTerm::Var(v) => {
                Err(self.err(format!("variable ?{v} is not allowed in a DATA block")))
            }
        }
    }

    // ---- term materialization --------------------------------------------

    fn token_to_term(&self, tok: &Token) -> Result<Term> {
        match tok {
            Token::Iri(s) => Ok(Term::Iri(self.resolve_iri(s))),
            Token::PName(p, l) => Ok(Term::Iri(self.expand_pname(p, l)?)),
            Token::Blank(l) => Ok(Term::Blank(l.clone())),
            Token::Integer(s) => Ok(Term::typed_literal(s.clone(), xsd::INTEGER)),
            Token::Decimal(s) => Ok(Term::typed_literal(s.clone(), xsd::DECIMAL)),
            Token::Double(s) => Ok(Term::typed_literal(s.clone(), xsd::DOUBLE)),
            Token::Str {
                value,
                lang,
                datatype,
            } => {
                let datatype = match datatype {
                    Some(dt) => Some(self.datatype_iri(dt)?),
                    None => None,
                };
                Ok(Term::Literal {
                    value: value.clone(),
                    datatype,
                    lang: lang.clone(),
                })
            }
            Token::Word(w) if w == "a" => Ok(Term::iri(RDF_TYPE)),
            other => Err(self.err(format!("cannot use {other:?} as a term"))),
        }
    }

    fn datatype_iri(&self, tok: &Token) -> Result<String> {
        match tok {
            Token::Iri(s) => Ok(self.resolve_iri(s)),
            Token::PName(p, l) => self.expand_pname(p, l),
            other => Err(self.err(format!("invalid datatype {other:?}"))),
        }
    }

    fn expand_pname(&self, prefix: &str, local: &str) -> Result<String> {
        match self.prefixes.get(prefix) {
            Some(ns) => Ok(format!("{ns}{local}")),
            None if prefix.is_empty() => Err(self.err("default prefix ':' used but not declared")),
            None => Err(self.err(format!("undefined prefix '{prefix}:'"))),
        }
    }

    /// Resolve a (possibly relative) IRI against BASE if one is set.
    fn resolve_iri(&self, s: &str) -> String {
        match &self.base {
            Some(base) if !is_absolute_iri(s) => format!("{base}{s}"),
            _ => s.to_string(),
        }
    }
}

/// A cheap absolute-IRI check (has a scheme like `http:` / `urn:`).
/// Conjoin two patterns, dropping a leading `Empty`.
fn join_opt(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    match left {
        GraphPattern::Empty => right,
        _ => GraphPattern::Join(Box::new(left), Box::new(right)),
    }
}

/// Map an uppercased name to its aggregate function, if it is one.
fn agg_func(name: &str) -> Option<AggFunc> {
    Some(match name {
        "COUNT" => AggFunc::Count,
        "SUM" => AggFunc::Sum,
        "AVG" => AggFunc::Avg,
        "MIN" => AggFunc::Min,
        "MAX" => AggFunc::Max,
        "SAMPLE" => AggFunc::Sample,
        "GROUP_CONCAT" => AggFunc::GroupConcat,
        _ => return None,
    })
}

fn is_absolute_iri(s: &str) -> bool {
    if let Some(idx) = s.find(':') {
        let scheme = &s[..idx];
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    } else {
        false
    }
}

