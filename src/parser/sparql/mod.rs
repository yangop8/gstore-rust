//! SPARQL parser: tokens → [`ast::Query`].
//!
//! A hand-written recursive-descent parser (mirroring gStore's hand-written
//! `SPARQLParser`). Supported: prologue (`PREFIX`/`BASE`); `SELECT` (with
//! `DISTINCT`, projected `(expr AS ?v)`, `GROUP BY`/`HAVING`, aggregates),
//! `ASK`, `CONSTRUCT`; graph patterns with `FILTER`, `OPTIONAL`, `UNION`,
//! `MINUS`, `BIND`, `VALUES`, sub-`SELECT`, and property paths (`/ ^ | * + !`);
//! solution modifiers (`ORDER BY`/`LIMIT`/`OFFSET`); and `INSERT/DELETE DATA`.
//! Not yet supported: `GRAPH`/`SERVICE`, the `?` path modifier (lexer treats
//! `?` as a variable sigil), `DESCRIBE`.

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
            "INSERT" => self.parse_insert_data()?,
            "DELETE" => self.parse_delete_data()?,
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
            // allow multiple bracketed HAVING constraints
            while matches!(self.peek(), Token::LParen) {
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
                Token::Word(w)
                    if matches!(w.to_ascii_uppercase().as_str(), "GRAPH" | "SERVICE") =>
                {
                    return Err(self.err(format!(
                        "'{w}' is not supported yet (see REFACTOR_BACKLOG item D)"
                    )));
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
        // postfix `*` / `+` modifiers (zero-or-one `?` is not supported: see
        // REFACTOR_BACKLOG item D — the lexer treats `?` as a variable sigil).
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

    fn parse_insert_data(&mut self) -> Result<Query> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("DATA")?;
        Ok(Query::InsertData(self.parse_ground_block()?))
    }

    fn parse_delete_data(&mut self) -> Result<Query> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword("DATA")?;
        Ok(Query::DeleteData(self.parse_ground_block()?))
    }

    /// Parse a `{ … }` block of ground triples (no variables allowed).
    fn parse_ground_block(&mut self) -> Result<Vec<GroundTriple>> {
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
                Token::Eof => return Err(self.err("unexpected end of query in DATA block")),
                _ => self.parse_triples_same_subject(&mut patterns, &mut paths)?,
            }
        }
        if !paths.is_empty() {
            return Err(self.err("property paths are not allowed in a DATA block"));
        }
        // Convert to ground triples, rejecting variables.
        patterns
            .into_iter()
            .map(|p| {
                Ok(GroundTriple {
                    subject: self.require_ground(p.subject)?,
                    predicate: self.require_ground(p.predicate)?,
                    object: self.require_ground(p.object)?,
                })
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(q: &str) -> SelectQuery {
        match parse(q).unwrap() {
            Query::Select(s) => s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    /// All triple patterns in a graph pattern (textual order).
    fn triples(gp: &GraphPattern) -> Vec<&TriplePattern> {
        let mut v = Vec::new();
        gp.collect_triples(&mut v);
        v
    }

    /// The FILTER expressions wrapping the top-level group, if any.
    fn top_filters(gp: &GraphPattern) -> &[Expr] {
        match gp {
            GraphPattern::Filter(fs, _) => fs,
            _ => &[],
        }
    }

    #[test]
    fn parses_prefix_and_simple_bgp() {
        let q = sel("PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
             PREFIX ub: <http://ex/ub#>
             SELECT ?X WHERE {
                ?X rdf:type ub:GraduateStudent .
                ?X ub:takesCourse <http://ex/c0> .
             }");
        assert_eq!(
            q.projection,
            Projection::Items(vec![SelectItem::Var("X".into())])
        );
        let tps = triples(&q.pattern);
        assert_eq!(tps.len(), 2);
        // rdf:type expanded
        assert_eq!(tps[0].predicate, PatternTerm::Term(Term::iri(RDF_TYPE)));
        assert_eq!(
            tps[0].object,
            PatternTerm::Term(Term::iri("http://ex/ub#GraduateStudent"))
        );
    }

    #[test]
    fn keyword_a_expands_to_rdf_type() {
        let q = sel("SELECT ?x WHERE { ?x a <http://ex/T> }");
        assert_eq!(
            triples(&q.pattern)[0].predicate,
            PatternTerm::Term(Term::iri(RDF_TYPE))
        );
    }

    #[test]
    fn select_star_and_distinct() {
        let q = sel("SELECT DISTINCT * WHERE { ?s ?p ?o }");
        assert!(q.distinct);
        assert_eq!(q.projection, Projection::All);
        assert_eq!(triples(&q.pattern).len(), 1);
    }

    #[test]
    fn predicate_object_list_abbreviations() {
        let q = sel("SELECT * WHERE { ?s <p1> <o1> , <o2> ; <p2> <o3> . }");
        let tps = triples(&q.pattern);
        assert_eq!(tps.len(), 3);
        // all share subject ?s
        for p in &tps {
            assert_eq!(p.subject, PatternTerm::Var("s".into()));
        }
    }

    #[test]
    fn filter_with_arithmetic_and_logic() {
        // mirrors data/num/num1.sql
        let q = sel("PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
             SELECT ?sx ?sy WHERE {
                ?x <salary> ?sx . ?y <salary> ?sy .
                FILTER(?sx < ?sy && abs(?sx - ?sy) < \"3000\"^^xsd:integer)
             }");
        assert_eq!(top_filters(&q.pattern).len(), 1);
        match &top_filters(&q.pattern)[0] {
            Expr::And(l, r) => {
                assert!(matches!(**l, Expr::Compare(CompareOp::Lt, _, _)));
                match &**r {
                    Expr::Compare(CompareOp::Lt, lhs, _) => {
                        assert!(matches!(**lhs, Expr::Builtin(ref n, _) if n == "ABS"));
                    }
                    other => panic!("unexpected rhs: {other:?}"),
                }
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn filter_nested_or_precedence() {
        // mirrors data/num/num2.sql: ?sx > ?sy && (?hx > ?hy || ?hx >= "170.0"^^xsd:float)
        let q = sel(
            "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
             SELECT * WHERE {
                ?x <a> ?sx . ?x <b> ?sy . ?x <c> ?hx . ?x <d> ?hy .
                FILTER(?sx > ?sy && (?hx > ?hy || ?hx >= \"170.0\"^^<http://www.w3.org/2001/XMLSchema#float>))
             }",
        );
        assert!(matches!(top_filters(&q.pattern)[0], Expr::And(_, _)));
    }

    #[test]
    fn order_by_limit_offset() {
        let q = sel("SELECT ?x WHERE { ?x <p> ?y } ORDER BY DESC(?y) LIMIT 10 OFFSET 5");
        assert_eq!(q.order_by.len(), 1);
        assert!(q.order_by[0].descending);
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.offset, Some(5));
    }

    #[test]
    fn numeric_literals_become_typed() {
        let q = sel("SELECT * WHERE { ?s <p> ?o . FILTER(?o > 5) }");
        match &top_filters(&q.pattern)[0] {
            Expr::Compare(CompareOp::Gt, _, rhs) => {
                assert_eq!(**rhs, Expr::Const(Term::typed_literal("5", xsd::INTEGER)));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parses_insert_data() {
        match parse("INSERT DATA { <a> <p> <b> . <a> <p> \"v\" . }").unwrap() {
            Query::InsertData(ts) => {
                assert_eq!(ts.len(), 2);
                assert_eq!(ts[0].subject, Term::iri("a"));
                assert_eq!(ts[1].object, Term::plain_literal("v"));
            }
            other => panic!("expected InsertData, got {other:?}"),
        }
    }

    #[test]
    fn parses_delete_data() {
        match parse("DELETE DATA { <a> <p> <b> }").unwrap() {
            Query::DeleteData(ts) => assert_eq!(ts.len(), 1),
            other => panic!("expected DeleteData, got {other:?}"),
        }
    }

    #[test]
    fn ask_query() {
        match parse("ASK { ?s <p> <o> }").unwrap() {
            Query::Ask(a) => assert_eq!(triples(&a.pattern).len(), 1),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn variable_in_data_block_is_rejected() {
        let e = parse("INSERT DATA { ?s <p> <o> }").unwrap_err();
        assert!(matches!(e, GStoreError::SparqlParse(_)));
    }

    #[test]
    fn optional_builds_left_join() {
        let q = sel("SELECT * WHERE { ?s <p> ?o OPTIONAL { ?s <q> ?z } }");
        assert!(matches!(q.pattern, GraphPattern::LeftJoin(_, _, _)));
        assert_eq!(triples(&q.pattern).len(), 2);
    }

    #[test]
    fn optional_with_inner_filter() {
        let q = sel("SELECT * WHERE { ?s <p> ?o OPTIONAL { ?s <q> ?z FILTER(?z > 5) } }");
        match q.pattern {
            GraphPattern::LeftJoin(_, _, fs) => assert_eq!(fs.len(), 1),
            other => panic!("expected LeftJoin, got {other:?}"),
        }
    }

    #[test]
    fn minus_builds_minus_node() {
        let q = sel("SELECT * WHERE { ?s <p> ?o MINUS { ?s <q> ?z } }");
        assert!(matches!(q.pattern, GraphPattern::Minus(_, _)));
    }

    #[test]
    fn bind_builds_extend() {
        let q = sel("SELECT * WHERE { ?s <p> ?o . BIND(?o + 1 AS ?x) }");
        // Extend wraps the BGP.
        assert!(matches!(q.pattern, GraphPattern::Extend(_, _, _)));
    }

    #[test]
    fn values_block_multi_var() {
        let q = sel("SELECT * WHERE { VALUES (?a ?b) { (<x> 1) (<y> UNDEF) } }");
        match &q.pattern {
            GraphPattern::Values(vars, rows) => {
                assert_eq!(vars, &vec!["a".to_string(), "b".to_string()]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[1][1], None); // UNDEF
            }
            other => panic!("expected Values, got {other:?}"),
        }
    }

    #[test]
    fn subselect_parses() {
        let q = sel("SELECT ?s WHERE { { SELECT ?s WHERE { ?s <p> ?o } LIMIT 5 } }");
        assert!(matches!(q.pattern, GraphPattern::SubSelect(_)));
    }

    #[test]
    fn aggregate_projection_and_group_by() {
        let q = sel(
            "SELECT ?s (COUNT(?o) AS ?c) WHERE { ?s <p> ?o } GROUP BY ?s HAVING(COUNT(?o) > 1)",
        );
        assert_eq!(q.group_by.len(), 1);
        assert_eq!(q.having.len(), 1);
        match &q.projection {
            Projection::Items(items) => {
                assert!(matches!(items[0], SelectItem::Var(_)));
                assert!(matches!(
                    items[1],
                    SelectItem::Expr(Expr::Aggregate { .. }, _)
                ));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn construct_parses() {
        match parse("CONSTRUCT { ?s <knows> ?o } WHERE { ?s <p> ?o }").unwrap() {
            Query::Construct(c) => {
                assert_eq!(c.template.len(), 1);
                assert_eq!(triples(&c.pattern).len(), 1);
            }
            other => panic!("expected Construct, got {other:?}"),
        }
    }

    #[test]
    fn undefined_prefix_errors() {
        let e = parse("SELECT * WHERE { ?s foaf:knows ?o }").unwrap_err();
        match e {
            GStoreError::SparqlParse(m) => assert!(m.contains("foaf")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn union_builds_union_node() {
        // mirrors lubm q6
        let q = sel("SELECT ?X WHERE { { ?X <p> <A> } UNION { ?X <p> <B> } }");
        assert!(matches!(q.pattern, GraphPattern::Union(_, _)));
        assert_eq!(triples(&q.pattern).len(), 2);
    }

    #[test]
    fn union_joined_with_trailing_pattern() {
        // mirrors lubm q5: {A} UNION {B} . ?X memberOf <dept>
        let q = sel("SELECT ?X WHERE {
                { ?X <type> <Undergrad> } UNION { ?X <type> <Grad> }
                ?X <memberOf> <dept>
             }");
        // top level is a Join(Union(...), Bgp(...))
        assert!(matches!(q.pattern, GraphPattern::Join(_, _)));
        assert_eq!(triples(&q.pattern).len(), 3);
    }

    #[test]
    fn chained_union() {
        // mirrors lubm q13's long UNION chain
        let q = sel("SELECT ?X WHERE { { ?X <a> <A> } UNION { ?X <a> <B> } UNION { ?X <a> <C> } }");
        // left-associative: Union(Union(A,B), C)
        match &q.pattern {
            GraphPattern::Union(left, _) => {
                assert!(matches!(**left, GraphPattern::Union(_, _)));
            }
            other => panic!("expected Union, got {other:?}"),
        }
        assert_eq!(triples(&q.pattern).len(), 3);
    }

    #[test]
    fn unsupported_graph_errors_clearly() {
        let e = parse("SELECT * WHERE { GRAPH ?g { ?s <p> ?o } }").unwrap_err();
        match e {
            GStoreError::SparqlParse(m) => assert!(m.contains("GRAPH")),
            other => panic!("got {other:?}"),
        }
    }
}
