//! SPARQL parser: tokens → [`ast::Query`].
//!
//! A hand-written recursive-descent parser (mirroring gStore's hand-written
//! `SPARQLParser`) for the supported subset: prologue (`PREFIX`/`BASE`),
//! `SELECT`/`ASK`, a flat basic graph pattern with `FILTER`, solution modifiers
//! (`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`), and `INSERT DATA`/`DELETE DATA`.

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
        let projection = if matches!(self.peek(), Token::Star) {
            self.bump();
            Projection::All
        } else {
            let mut vars = Vec::new();
            while let Token::Var(v) = self.peek() {
                vars.push(v.clone());
                self.bump();
            }
            if vars.is_empty() {
                return Err(self.err("SELECT must list variables or '*' (projected expressions are not yet supported)"));
            }
            Projection::Vars(vars)
        };

        self.skip_dataset_clauses()?;
        self.eat_keyword("WHERE"); // optional
        let pattern = self.parse_group_graph_pattern()?;
        let (order_by, limit, offset) = self.parse_solution_modifiers()?;

        Ok(SelectQuery {
            distinct,
            projection,
            pattern,
            order_by,
            limit,
            offset,
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
    fn parse_group_graph_pattern(&mut self) -> Result<GraphPattern> {
        self.expect(&Token::LBrace, "'{'")?;
        let mut elements: Vec<GraphPattern> = Vec::new();
        let mut filters: Vec<Expr> = Vec::new();
        let mut pending: Vec<TriplePattern> = Vec::new();

        // Flush accumulated triple patterns into a single BGP element.
        macro_rules! flush {
            () => {
                if !pending.is_empty() {
                    elements.push(GraphPattern::Bgp(std::mem::take(&mut pending)));
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
                Token::Word(w)
                    if matches!(
                        w.to_ascii_uppercase().as_str(),
                        "OPTIONAL" | "MINUS" | "GRAPH" | "SERVICE" | "BIND" | "VALUES"
                    ) =>
                {
                    return Err(self.err(format!(
                        "'{w}' is not supported yet (see REFACTOR_BACKLOG item D)"
                    )));
                }
                Token::LBrace => {
                    flush!();
                    elements.push(self.parse_group_or_union()?);
                }
                Token::Eof => return Err(self.err("unexpected end of query inside '{ }'")),
                _ => {
                    self.parse_triples_same_subject(&mut pending)?;
                }
            }
        }
        flush!();

        // Conjoin elements left-to-right, then wrap in a FILTER if any.
        let mut combined = elements
            .into_iter()
            .reduce(|a, b| GraphPattern::Join(Box::new(a), Box::new(b)))
            .unwrap_or(GraphPattern::Empty);
        if !filters.is_empty() {
            combined = GraphPattern::Filter(filters, Box::new(combined));
        }
        Ok(combined)
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

    /// `subject  verb objlist (';' verb objlist)*` flattened into patterns.
    fn parse_triples_same_subject(&mut self, out: &mut Vec<TriplePattern>) -> Result<()> {
        let subject = self.parse_pattern_term()?;
        loop {
            let predicate = self.parse_verb()?;
            // object list
            loop {
                let object = self.parse_pattern_term()?;
                out.push(TriplePattern {
                    subject: subject.clone(),
                    predicate: predicate.clone(),
                    object,
                });
                if matches!(self.peek(), Token::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            if matches!(self.peek(), Token::Semicolon) {
                self.bump();
                // trailing ';' before end of block or '.' is allowed
                if self.starts_verb() {
                    continue;
                }
                break;
            }
            break;
        }
        Ok(())
    }

    /// Could the current token begin a verb (predicate)?
    fn starts_verb(&self) -> bool {
        match self.peek() {
            Token::Var(_) | Token::Iri(_) | Token::PName(_, _) => true,
            Token::Word(w) => w == "a",
            _ => false,
        }
    }

    /// A predicate: a variable, an IRI/prefixed name, or `a` (= rdf:type).
    fn parse_verb(&mut self) -> Result<PatternTerm> {
        if let Token::Word(w) = self.peek() {
            if w == "a" {
                self.bump();
                return Ok(PatternTerm::Term(Term::iri(RDF_TYPE)));
            }
        }
        self.parse_pattern_term()
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
                _ => self.parse_triples_same_subject(&mut patterns)?,
            }
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
        assert_eq!(q.projection, Projection::Vars(vec!["X".into()]));
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
    fn unsupported_optional_errors_clearly() {
        let e = parse("SELECT * WHERE { ?s <p> ?o OPTIONAL { ?s <q> ?z } }").unwrap_err();
        match e {
            GStoreError::SparqlParse(m) => assert!(m.contains("OPTIONAL")),
            other => panic!("got {other:?}"),
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
    fn unsupported_minus_errors_clearly() {
        let e = parse("SELECT * WHERE { ?s <p> ?o MINUS { ?s <q> ?z } }").unwrap_err();
        match e {
            GStoreError::SparqlParse(m) => assert!(m.contains("MINUS")),
            other => panic!("got {other:?}"),
        }
    }
}
