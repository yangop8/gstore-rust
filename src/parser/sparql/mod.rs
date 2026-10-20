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
//! `;`-separated sequences); named graphs (`GRAPH`); and federated query
//! (`SERVICE [SILENT] (<iri> | ?var) { … }`).
//!
//! ## gStore advanced functions
//!
//! Every path/analytics alternative of upstream gStore 1.4's `builtInCall`
//! rule (`SPARQL.g4`) parses into [`Expr::GraphFn`] with its exact positional
//! shape ([`GFnName::params`]): `varOrIri`, `booleanLiteral`, signed
//! `integerLiteral`, unsigned `num_integer`, `numericLiteral`, `predSet`
//! (`{}`, `{<p>, pfx:q}`, `!<p>` exclusions) and the named `alpha=`/`maxIter=`/
//! `tol=`/`increase=` arguments. Keywords are case-insensitive. As in
//! upstream's `QueryParser`, a call is accepted as the whole expression of a
//! SELECT projection `(f(…) AS ?v)`; only `simpleCycleBoolean`,
//! `cycleBoolean`, `shortestPathLen` and `kHopReachable` may also be nested in
//! other expressions (FILTER/HAVING/BIND/…). `PFN.name({…})` parses into
//! [`Expr::Pfn`].
//!
//! The pre-upstream gstore-rust graph built-ins live in the IRI namespace
//! [`RS_NAMESPACE`] behind the predeclared prefix `rs:` (a query's own
//! `PREFIX rs:` overrides it) and are called like any IRI function,
//! `rs:SSSP(<a>, <b>)`, anywhere an expression is allowed ([`Expr::IriCall`]).
//! A bare call that collides with an upstream function name but has the
//! legacy shape (`SSSP(?a, ?b)`) is a parse error that points to `rs:`.

pub mod ast;
pub mod builtins;
pub mod lexer;

use std::collections::HashMap;
use std::ops::Range;

use crate::error::{GStoreError, Result};
use crate::model::Term;

use ast::*;
use lexer::{tokenize_with_spans, Token};

/// The IRI namespace of the legacy gstore-rust graph built-ins, predeclared
/// as the `rs:` prefix.
pub const RS_NAMESPACE: &str = "urn:gstore-rust:fn:";

/// The legacy gstore-rust graph built-ins reachable as `rs:NAME(…)` (upper
/// case; the local name is matched case-insensitively). Each keeps the
/// argument count it has in [`builtins::BUILTINS`]. `CLUSTERINGCOEFF`,
/// `KHOPENUMERATEPATH` and `SIMILARITY` do not collide with an upstream
/// advanced function, so their bare names also keep working.
pub const RS_FUNCTIONS: &[&str] = &[
    "BETWEENNESSCENTRALITY",
    "BFSCOUNT",
    "CLOSENESSCENTRALITY",
    "CLUSTERINGCOEFF",
    "CYCLEBOOLEAN",
    "DEGREECORRELATION",
    "DIAMETERESTIMATION",
    "JACCARDSIMILARITY",
    "KHOPALLNEIGHBORS",
    "KHOPCOUNT",
    "KHOPENUMERATEPATH",
    "KHOPNEIGHBOR",
    "KHOPREACHABLE",
    "LABELPROP",
    "LOUVAIN",
    "PPR",
    "SHORTESTPATHLEN",
    "SHORTESTPATHS",
    "SIMILARITY",
    "SIMPLECYCLEBOOLEAN",
    "SSSP",
    "SSSPLEN",
    "TRIANGLECOUNTING",
    "WCC",
];

/// The legacy built-in (upper-case name, as in [`RS_FUNCTIONS`]) that `iri`
/// names, if it is `urn:gstore-rust:fn:NAME` for a known `NAME` (any case).
pub fn rs_function_name(iri: &str) -> Option<&'static str> {
    let local = iri.strip_prefix(RS_NAMESPACE)?;
    RS_FUNCTIONS
        .iter()
        .copied()
        .find(|n| n.eq_ignore_ascii_case(local))
}

/// The legacy built-in whose bare name collides with upstream function `f`.
fn legacy_collision(f: GFnName) -> Option<&'static str> {
    RS_FUNCTIONS.iter().copied().find(|n| *n == f.keyword())
}

/// Parse a SPARQL query or update string.
pub fn parse(input: &str) -> Result<Query> {
    let (tokens, spans) = tokenize_with_spans(input)?;
    Parser::new(input, tokens, spans).parse_request()
}

/// A parsed predicate position: a plain term, or a property path.
enum VerbKind {
    Simple(PatternTerm),
    Path(PathExpr),
}

struct Parser<'s> {
    /// The request text, for constructs passed through verbatim (PFN's
    /// `jsonVars`) and for quoting the user's arguments in error messages.
    src: &'s str,
    tokens: Vec<Token>,
    /// `spans[i]`: byte range of `tokens[i]` in `src`.
    spans: Vec<Range<usize>>,
    pos: usize,
    prefixes: HashMap<String, String>,
    base: Option<String>,
}

impl<'s> Parser<'s> {
    fn new(src: &'s str, tokens: Vec<Token>, spans: Vec<Range<usize>>) -> Parser<'s> {
        // `rs:` is predeclared; a `PREFIX rs: <…>` in the prologue replaces it.
        let mut prefixes = HashMap::new();
        prefixes.insert("rs".to_string(), RS_NAMESPACE.to_string());
        Parser {
            src,
            tokens,
            spans,
            pos: 0,
            prefixes,
            base: None,
        }
    }

    /// The source text of tokens `from..to` (exclusive), whitespace between
    /// them included.
    fn source_between(&self, from: usize, to: usize) -> &'s str {
        if to <= from {
            return "";
        }
        &self.src[self.spans[from].start..self.spans[to - 1].end]
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
        // A well-formed request consumes all of its input. Reject trailing
        // tokens so a second statement smuggled after the query — e.g.
        // `SELECT … } ; DROP ALL` — is a parse error rather than being silently
        // dropped (the read-only gate in gnlqa/kb.rs and the HTTP privilege
        // check both rely on the parse faithfully reflecting the whole string).
        self.expect(&Token::Eof, "end of query")?;
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
                    let expr = self.parse_projection_expr()?;
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
                    self.bump();
                    flush!();
                    let silent = self.eat_keyword("SILENT");
                    let endpoint = self.parse_service_ref()?;
                    let inner = self.parse_group_graph_pattern()?;
                    current = join_opt(
                        current,
                        GraphPattern::Service {
                            endpoint,
                            silent,
                            pattern: Box::new(inner),
                        },
                    );
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
                } else if let Some(func) = GFnName::from_keyword(&upper)
                    .filter(|_| matches!(self.peek_at(1), Token::LParen))
                {
                    // An upstream advanced function nested in an expression.
                    self.parse_graph_fn(func, false)
                } else if self.at_pfn_call() {
                    self.parse_pfn(false)
                } else {
                    // Builtin / function call: NAME ( args )
                    self.bump();
                    let args = if matches!(self.peek(), Token::LParen) {
                        self.parse_arg_list()?
                    } else {
                        return Err(self.err(format!("unexpected word '{w}' in expression")));
                    };
                    // A known built-in called with the wrong arity (e.g. `STR()`)
                    // is a parse error naming the function and its arity.
                    builtins::check_call(&upper, &args).map_err(|m| self.err(m))?;
                    Ok(Expr::Builtin(upper, args))
                }
            }
            // `<iri>(args…)` / `pfx:name(args…)`: a function call by IRI.
            Token::Iri(_) | Token::PName(_, _) if matches!(self.peek_at(1), Token::LParen) => {
                self.parse_iri_call()
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

    // ---- IRI function calls ----------------------------------------------

    /// `iri ArgList` (SPARQL `iriOrFunction` with arguments). A call into the
    /// [`RS_NAMESPACE`] must name a legacy built-in ([`RS_FUNCTIONS`]) with an
    /// argument count it accepts.
    fn parse_iri_call(&mut self) -> Result<Expr> {
        let tok = self.bump();
        let iri = match self.token_to_term(&tok)? {
            Term::Iri(s) => s,
            other => return Err(self.err(format!("expected a function IRI, found {other:?}"))),
        };
        let written = match &tok {
            Token::PName(p, l) => format!("{p}:{l}"),
            _ => format!("<{iri}>"),
        };
        let args = self.parse_arg_list()?;
        if iri.starts_with(RS_NAMESPACE) {
            let Some(name) = rs_function_name(&iri) else {
                return Err(self.err(format!(
                    "unknown gstore-rust function {written}: the rs: namespace <{RS_NAMESPACE}> \
                     provides {}",
                    RS_FUNCTIONS.join(", ")
                )));
            };
            if let Some(spec) = builtins::lookup(name) {
                if !spec.arity.accepts(args.len()) {
                    return Err(self.err(format!(
                        "{written} expects {}, got {}",
                        spec.arity.describe(),
                        args.len()
                    )));
                }
            }
        }
        Ok(Expr::IriCall { iri, args })
    }

    // ---- upstream advanced functions -------------------------------------

    /// The expression of a SELECT `(expr AS ?v)` item. An upstream advanced
    /// function or PFN call is only accepted as the *whole* expression here,
    /// as upstream's `QueryParser::parseSelectAggregateFunction` requires.
    fn parse_projection_expr(&mut self) -> Result<Expr> {
        let call = match self.peek() {
            Token::Word(w) => {
                if let Some(func) = GFnName::from_keyword(w)
                    .filter(|_| matches!(self.peek_at(1), Token::LParen))
                {
                    let e = self.parse_graph_fn(func, true)?;
                    Some((e, func.name().to_string()))
                } else if self.at_pfn_call() {
                    let e = self.parse_pfn(true)?;
                    let shown = match &e {
                        Expr::Pfn { name, .. } => format!("PFN.{name}"),
                        _ => "PFN".to_string(),
                    };
                    Some((e, shown))
                } else {
                    None
                }
            }
            _ => None,
        };
        match call {
            None => self.parse_expr(),
            Some((e, shown)) => {
                if self.keyword().as_deref() == Some("AS") {
                    Ok(e)
                } else {
                    Err(self.err(format!(
                        "{shown}(…) must be the whole projected expression, as in \
                         ({shown}(…) AS ?var); found {} after the call",
                        describe_token(self.peek())
                    )))
                }
            }
        }
    }

    /// Parse the advanced-function call whose keyword is the current token.
    /// `whole_projection`: the call is the entire expression of a SELECT
    /// projection item (where upstream accepts every function); elsewhere only
    /// [`GFnName::filter_callable`] functions are allowed.
    ///
    /// A call that does not fit the upstream signature but has the shape of
    /// the colliding legacy gstore-rust built-in (e.g. `SSSP(?a, ?b)`) is
    /// reported with a pointer to its new `rs:` name.
    fn parse_graph_fn(&mut self, func: GFnName, whole_projection: bool) -> Result<Expr> {
        let written = match self.bump() {
            Token::Word(w) => w,
            other => return Err(self.err(format!("expected {}, found {other:?}", func.name()))),
        };
        let args_start = self.pos;
        let legacy = legacy_collision(func);
        match self.parse_gfn_args(func) {
            Ok(args) if whole_projection || func.filter_callable() => {
                Ok(Expr::GraphFn { func, args })
            }
            Ok(_) => {
                let mut msg = format!(
                    "{name}(…) is only allowed as the whole expression of a SELECT projection, \
                     e.g. SELECT ({name}(…) AS ?v); upstream gStore nests only \
                     simpleCycleBoolean, cycleBoolean, shortestPathLen and kHopReachable \
                     inside FILTER/BIND/HAVING/ORDER BY expressions",
                    name = func.name()
                );
                if legacy.is_some() {
                    let call = format!("rs:{written}(…)");
                    msg.push_str(&format!(
                        "; the legacy gstore-rust function is callable anywhere as {call}"
                    ));
                }
                Err(self.err(msg))
            }
            Err(detail) => {
                let Some(name) = legacy else {
                    return Err(self.err(detail));
                };
                // Does the call parse as the legacy built-in's argument list?
                let resume = self.pos;
                self.pos = args_start;
                let legacy_args = self.parse_arg_list().ok();
                let fits = legacy_args.as_ref().is_some_and(|a| {
                    builtins::lookup(name).is_some_and(|spec| spec.arity.accepts(a.len()))
                });
                if fits {
                    let call = format!("rs:{written}{}", self.source_between(args_start, self.pos));
                    return Err(self.err(format!(
                        "{written}(…) has the shape of the legacy gstore-rust built-in, which is \
                         now called as {call}; the bare name is upstream gStore's {}: {detail}",
                        func.name()
                    )));
                }
                self.pos = resume;
                Err(self.err(format!(
                    "{detail} (the legacy gstore-rust built-in is now called as rs:{written}(…))"
                )))
            }
        }
    }

    /// The argument list of an advanced function, slot by slot as
    /// [`GFnName::params`] lays it out. Errors are messages naming the
    /// function, its signature and the offending argument.
    fn parse_gfn_args(&mut self, func: GFnName) -> std::result::Result<Vec<GArg>, String> {
        let ctx = |m: String| format!("{} expects {}: {m}", func.name(), func.signature());
        if !matches!(self.peek(), Token::LParen) {
            return Err(ctx(format!("expected '(', found {}", describe_token(self.peek()))));
        }
        self.bump();
        let mut args = Vec::new();
        for p in func.params() {
            let first = args.is_empty();
            if p.optional {
                let present = if first {
                    self.starts_gfn_slot(p.slot)
                } else {
                    matches!(self.peek(), Token::Comma)
                };
                if !present {
                    continue;
                }
            }
            let argno = args.len() + 1;
            if !first {
                match self.peek() {
                    Token::Comma => {
                        self.bump();
                    }
                    Token::RParen => {
                        return Err(ctx(format!(
                            "missing argument {argno} ({})",
                            p.slot.describe()
                        )))
                    }
                    other => {
                        return Err(ctx(format!(
                            "expected ',' before argument {argno}, found {}",
                            describe_token(other)
                        )))
                    }
                }
            }
            if matches!(self.peek(), Token::RParen) {
                return Err(ctx(format!("missing argument {argno} ({})", p.slot.describe())));
            }
            args.push(self.parse_gfn_slot(p.slot, argno).map_err(&ctx)?);
        }
        match self.peek() {
            Token::RParen => {
                self.bump();
                Ok(args)
            }
            Token::Comma => Err(ctx(format!(
                "too many arguments: expected ')' after argument {}",
                args.len()
            ))),
            other => Err(ctx(format!(
                "expected ')' after argument {}, found {}",
                args.len(),
                describe_token(other)
            ))),
        }
    }

    /// Can the current token begin an argument of grammar slot `slot`? (Only
    /// needed for a leading optional slot: `clusterCoeff`'s `varOrIri`.)
    fn starts_gfn_slot(&self, slot: GSlot) -> bool {
        match slot {
            GSlot::Node => matches!(self.peek(), Token::Var(_) | Token::Iri(_) | Token::PName(..)),
            GSlot::Bool => matches!(self.peek(), Token::Word(w)
                if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false")),
            GSlot::Int | GSlot::UInt | GSlot::Num => matches!(
                self.peek(),
                Token::Integer(_) | Token::Decimal(_) | Token::Double(_) | Token::Plus | Token::Minus
            ),
            GSlot::PredSet => matches!(self.peek(), Token::LBrace),
            GSlot::NamedNum(k) | GSlot::NamedUInt(k) => {
                matches!(self.peek(), Token::Word(w) if w.eq_ignore_ascii_case(k))
            }
        }
    }

    /// One advanced-function argument of grammar slot `slot`.
    fn parse_gfn_slot(&mut self, slot: GSlot, argno: usize) -> std::result::Result<GArg, String> {
        let found = |t: &Token| describe_token(t);
        match slot {
            GSlot::Node => match self.peek().clone() {
                Token::Var(v) => {
                    self.bump();
                    Ok(GArg::Node(VarOrIri::Var(v)))
                }
                tok @ (Token::Iri(_) | Token::PName(..)) => {
                    let iri = self.token_iri(&tok).map_err(|e| format!("argument {argno}: {e}"))?;
                    self.bump();
                    Ok(GArg::Node(VarOrIri::Iri(iri)))
                }
                other => Err(format!(
                    "argument {argno} must be a varOrIri (a ?variable or an IRI), found {}",
                    found(&other)
                )),
            },
            GSlot::Bool => match self.peek() {
                Token::Word(w) if w.eq_ignore_ascii_case("true") => {
                    self.bump();
                    Ok(GArg::Bool(true))
                }
                Token::Word(w) if w.eq_ignore_ascii_case("false") => {
                    self.bump();
                    Ok(GArg::Bool(false))
                }
                other => Err(format!(
                    "argument {argno} must be a booleanLiteral (true or false), found {}",
                    found(other)
                )),
            },
            GSlot::Int => self
                .parse_gfn_integer(true)
                .map(GArg::Int)
                .map_err(|m| format!("argument {argno} must be an integerLiteral: {m}")),
            GSlot::UInt => self
                .parse_gfn_integer(false)
                .map(GArg::Int)
                .map_err(|m| format!("argument {argno} must be an unsigned integer: {m}")),
            GSlot::Num => self
                .parse_gfn_number()
                .map(GArg::Num)
                .map_err(|m| format!("argument {argno} must be a numericLiteral: {m}")),
            GSlot::PredSet => self
                .parse_pred_set()
                .map_err(|m| format!("argument {argno} must be a predSet: {m}")),
            GSlot::NamedNum(key) | GSlot::NamedUInt(key) => {
                let shape = slot.describe();
                match self.peek() {
                    Token::Word(w) if w.eq_ignore_ascii_case(key) => {
                        self.bump();
                    }
                    other => {
                        return Err(format!(
                            "argument {argno} must be the named argument {shape}, found {}",
                            found(other)
                        ))
                    }
                }
                if !matches!(self.peek(), Token::Eq) {
                    return Err(format!(
                        "argument {argno}: expected '=' after {key}, found {}",
                        found(self.peek())
                    ));
                }
                self.bump();
                let value = if matches!(slot, GSlot::NamedUInt(_)) {
                    self.parse_gfn_integer(false).map(GNum::Int)
                } else {
                    self.parse_gfn_number().map(GNum::Num)
                }
                .map_err(|m| format!("argument {argno} ({shape}): {m}"))?;
                Ok(GArg::Named {
                    key: key.to_string(),
                    value,
                })
            }
        }
    }

    /// An optionally signed integer (`integerLiteral`), or with `signed ==
    /// false` an unsigned one (`num_integer`), as an `i64`.
    fn parse_gfn_integer(&mut self, signed: bool) -> std::result::Result<i64, String> {
        let sign = match self.peek() {
            Token::Plus => "+",
            Token::Minus => "-",
            _ => "",
        };
        let digits_at = usize::from(!sign.is_empty());
        let Token::Integer(digits) = self.peek_at(digits_at).clone() else {
            return Err(format!("found {}", describe_token(self.peek_at(digits_at))));
        };
        if !signed && !sign.is_empty() {
            return Err(format!("found signed {sign}{digits}"));
        }
        let n = format!("{sign}{digits}")
            .parse::<i64>()
            .map_err(|_| format!("{sign}{digits} is out of the 64-bit integer range"))?;
        self.pos += digits_at;
        self.bump();
        Ok(n)
    }

    /// A `numericLiteral` (integer, decimal or double, optionally signed).
    fn parse_gfn_number(&mut self) -> std::result::Result<f64, String> {
        let sign = match self.peek() {
            Token::Plus => "+",
            Token::Minus => "-",
            _ => "",
        };
        let at = usize::from(!sign.is_empty());
        let lexical = match self.peek_at(at) {
            Token::Integer(s) | Token::Decimal(s) | Token::Double(s) => s.clone(),
            other => return Err(format!("found {}", describe_token(other))),
        };
        let v = format!("{sign}{lexical}")
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("{sign}{lexical} is out of range"))?;
        self.pos += at;
        self.bump();
        Ok(v)
    }

    /// `predSet : '{' iriOrNegIri ( ',' iriOrNegIri )* '}' | '{' '}'`, with
    /// `iriOrNegIri : iri | '!' iri`. IRIs are prefix/BASE-expanded.
    fn parse_pred_set(&mut self) -> std::result::Result<GArg, String> {
        if !matches!(self.peek(), Token::LBrace) {
            return Err(format!(
                "expected '{{' (use {{}} for all predicates), found {}",
                describe_token(self.peek())
            ));
        }
        self.bump();
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        if matches!(self.peek(), Token::RBrace) {
            self.bump();
            return Ok(GArg::PredSet { include, exclude });
        }
        loop {
            let negated = matches!(self.peek(), Token::Not);
            if negated {
                self.bump();
            }
            let tok = self.peek().clone();
            if !matches!(tok, Token::Iri(_) | Token::PName(..)) {
                return Err(format!(
                    "expected an IRI or !IRI, found {}",
                    describe_token(&tok)
                ));
            }
            let iri = self.token_iri(&tok)?;
            self.bump();
            if negated {
                exclude.push(iri);
            } else {
                include.push(iri);
            }
            match self.peek() {
                Token::Comma => {
                    self.bump();
                }
                Token::RBrace => {
                    self.bump();
                    return Ok(GArg::PredSet { include, exclude });
                }
                other => {
                    return Err(format!(
                        "expected ',' or '}}' in the predicate set, found {}",
                        describe_token(other)
                    ))
                }
            }
        }
    }

    /// The expanded IRI of an `Iri`/`PName` token (error text on failure,
    /// e.g. an undeclared prefix).
    fn token_iri(&self, tok: &Token) -> std::result::Result<String, String> {
        match self.token_to_term(tok) {
            Ok(Term::Iri(s)) => Ok(s),
            Ok(other) => Err(format!("expected an IRI, found {other:?}")),
            Err(GStoreError::SparqlParse(m)) => Err(m),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Is the cursor at an upstream `PFN.name(` (or `PFN name(`) call?
    fn at_pfn_call(&self) -> bool {
        matches!(self.peek(), Token::Word(w) if w.eq_ignore_ascii_case("PFN"))
            && match self.peek_at(1) {
                Token::Dot => matches!(self.peek_at(2), Token::Word(_)),
                Token::Word(_) => true,
                _ => false,
            }
    }

    /// `K_PFN VARNAME '(' jsonVars ')'` — upstream spells it `PFN.name({…})`
    /// in its docs; both that and the grammar's `PFN name({…})` are accepted.
    fn parse_pfn(&mut self, whole_projection: bool) -> Result<Expr> {
        self.bump(); // PFN
        if matches!(self.peek(), Token::Dot) {
            self.bump();
        }
        let name = match self.bump() {
            Token::Word(w) => w,
            other => {
                return Err(self.err(format!(
                    "expected a function name after PFN., found {}",
                    describe_token(&other)
                )))
            }
        };
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' after PFN.{name}, found {}",
                describe_token(self.peek())
            )));
        }
        self.bump();
        let json = self
            .parse_json_vars()
            .map_err(|m| self.err(format!("PFN.{name} expects one jsonVars argument: {m}")))?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "PFN.{name} expects one jsonVars argument: expected ')' after it, found {}",
                describe_token(self.peek())
            )));
        }
        self.bump();
        if !whole_projection {
            return Err(self.err(format!(
                "PFN.{name}(…) is only allowed as the whole expression of a SELECT projection, \
                 e.g. SELECT (PFN.{name}({{…}}) AS ?v)"
            )));
        }
        Ok(Expr::Pfn { name, json })
    }

    /// `jsonVars : '{' jsonVar ( ',' jsonVar )* '}' | '{' '}'` with
    /// `jsonVar : string ':' string | string ':' '[' string ( ',' string )* ']'`.
    /// Returns the tokens' raw text concatenated without the whitespace
    /// between them — what upstream passes on (`QueryParser.cpp`, which joins
    /// each `jsonVar`'s `getText()`).
    fn parse_json_vars(&mut self) -> std::result::Result<String, String> {
        let start = self.pos;
        let plain_string = |t: &Token| {
            matches!(
                t,
                Token::Str {
                    lang: None,
                    datatype: None,
                    ..
                }
            )
        };
        if !matches!(self.peek(), Token::LBrace) {
            return Err(format!(
                "expected a JSON object '{{…}}', found {}",
                describe_token(self.peek())
            ));
        }
        self.bump();
        if !matches!(self.peek(), Token::RBrace) {
            loop {
                if !plain_string(self.peek()) {
                    return Err(format!(
                        "expected a string key, found {}",
                        describe_token(self.peek())
                    ));
                }
                self.bump();
                // The lexer reads a lone ':' as an empty prefixed name.
                if !matches!(self.peek(), Token::PName(p, l) if p.is_empty() && l.is_empty()) {
                    return Err(format!(
                        "expected ':' after the key, found {}",
                        describe_token(self.peek())
                    ));
                }
                self.bump();
                if matches!(self.peek(), Token::LBracket) {
                    self.bump();
                    loop {
                        if !plain_string(self.peek()) {
                            return Err(format!(
                                "expected a string in the value list, found {}",
                                describe_token(self.peek())
                            ));
                        }
                        self.bump();
                        match self.peek() {
                            Token::Comma => {
                                self.bump();
                            }
                            Token::RBracket => {
                                self.bump();
                                break;
                            }
                            other => {
                                return Err(format!(
                                    "expected ',' or ']' in the value list, found {}",
                                    describe_token(other)
                                ))
                            }
                        }
                    }
                } else if plain_string(self.peek()) {
                    self.bump();
                } else {
                    return Err(format!(
                        "values must be strings or [string, …] lists, found {}",
                        describe_token(self.peek())
                    ));
                }
                match self.peek() {
                    Token::Comma => {
                        self.bump();
                    }
                    Token::RBrace => break,
                    other => {
                        return Err(format!(
                            "expected ',' or '}}' in the JSON object, found {}",
                            describe_token(other)
                        ))
                    }
                }
            }
        }
        self.bump(); // '}'
        Ok((start..self.pos)
            .map(|i| &self.src[self.spans[i].clone()])
            .collect())
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

    /// The endpoint reference of a `SERVICE` pattern: a variable or constant IRI.
    fn parse_service_ref(&mut self) -> Result<ServiceRef> {
        match self.peek() {
            Token::Var(v) => {
                let v = v.clone();
                self.bump();
                Ok(ServiceRef::Var(v))
            }
            Token::Iri(_) | Token::PName(_, _) => {
                let tok = self.peek().clone();
                match self.token_to_term(&tok)? {
                    Term::Iri(s) => {
                        self.bump();
                        Ok(ServiceRef::Iri(s))
                    }
                    other => Err(self.err(format!(
                        "SERVICE expects an IRI or variable, got {other:?}"
                    ))),
                }
            }
            other => Err(self.err(format!(
                "SERVICE expects an IRI or variable, found {other:?}"
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

/// A token as the user would have written it, for error messages.
fn describe_token(t: &Token) -> String {
    let s = match t {
        Token::Iri(s) => return format!("<{s}>"),
        Token::PName(p, l) if p.is_empty() && l.is_empty() => "':'",
        Token::PName(p, l) => return format!("{p}:{l}"),
        Token::Var(v) => return format!("?{v}"),
        Token::Blank(b) => return format!("_:{b}"),
        Token::Str { value, .. } => return format!("the string literal {value:?}"),
        Token::Integer(s) | Token::Decimal(s) | Token::Double(s) => return s.clone(),
        Token::Word(w) => return format!("'{w}'"),
        Token::LBrace => "'{'",
        Token::RBrace => "'}'",
        Token::LParen => "'('",
        Token::RParen => "')'",
        Token::LBracket => "'['",
        Token::RBracket => "']'",
        Token::Dot => "'.'",
        Token::Comma => "','",
        Token::Semicolon => "';'",
        Token::Star => "'*'",
        Token::Plus => "'+'",
        Token::Minus => "'-'",
        Token::Slash => "'/'",
        Token::Eq => "'='",
        Token::Ne => "'!='",
        Token::Lt => "'<'",
        Token::Gt => "'>'",
        Token::Le => "'<='",
        Token::Ge => "'>='",
        Token::And => "'&&'",
        Token::Or => "'||'",
        Token::Not => "'!'",
        Token::Caret => "'^'",
        Token::Pipe => "'|'",
        Token::Question => "'?'",
        Token::Eof => "the end of the query",
    };
    s.to_string()
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
    fn trailing_tokens_after_a_query_are_rejected() {
        // A smuggled second statement after a complete query must be a parse
        // error, not silently dropped (read-only gates rely on this).
        assert!(parse("SELECT * WHERE { ?s ?p ?o } ; DROP ALL").is_err());
        assert!(parse("ASK { ?s ?p ?o } garbage").is_err());
        // A well-formed query followed by only whitespace still parses.
        assert!(parse("SELECT * WHERE { ?s ?p ?o }\n  ").is_ok());
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

    /// `(NAME, #args)` of each projected built-in call; an `rs:` legacy call
    /// reports its canonical built-in name.
    fn builtin_calls(q: &SelectQuery) -> Vec<(String, usize)> {
        match &q.projection {
            Projection::Items(items) => items
                .iter()
                .filter_map(|it| match it {
                    SelectItem::Expr(Expr::Builtin(n, args), _) => Some((n.clone(), args.len())),
                    SelectItem::Expr(Expr::IriCall { iri, args }, _) => {
                        rs_function_name(iri).map(|n| (n.to_string(), args.len()))
                    }
                    _ => None,
                })
                .collect(),
            _ => panic!("expected projection items"),
        }
    }

    #[test]
    fn parses_new_builtin_function_names() {
        // Every new SPARQL function name must tokenize + parse as a Builtin call
        // via the generic NAME(args) path — case-insensitively, with zero-arg,
        // single-arg, and multi-arg shapes all supported. (The legacy
        // two-argument SHORTESTPATHLEN is now reached as rs:SHORTESTPATHLEN;
        // the bare name is upstream gStore's shortestPathLen.)
        let q = sel(
            "SELECT (SUBSTR(?s,1,2) AS ?a) (now() AS ?b) (md5(?s) AS ?c)
                    (rs:SHORTESTPATHLEN(<x>,<y>) AS ?d) (UUID() AS ?e) (if(true,1,2) AS ?f)
             WHERE { ?s <p> ?o }",
        );
        let got = builtin_calls(&q);
        assert_eq!(
            got,
            vec![
                ("SUBSTR".to_string(), 3),
                ("NOW".to_string(), 0),
                ("MD5".to_string(), 1),
                ("SHORTESTPATHLEN".to_string(), 2),
                ("UUID".to_string(), 0),
                ("IF".to_string(), 3),
            ]
        );
    }

    #[test]
    fn parses_graph_analytics_function_names() {
        // The legacy gstore-rust graph-analytics built-ins are reached through
        // the rs: namespace — local names case-insensitive — with zero-, one-,
        // and two-argument shapes all supported.
        let q = sel(
            "SELECT (rs:SSSP(<a>,<b>) AS ?p) (rs:khopCount(<a>,2) AS ?k)
                    (rs:TriangleCounting() AS ?t) (rs:PPR(<a>,<b>) AS ?r)
                    (rs:DIAMETERESTIMATION() AS ?d) (rs:BfsCount(<a>) AS ?c)
             WHERE { ?s <p> ?o }",
        );
        let got = builtin_calls(&q);
        assert_eq!(
            got,
            vec![
                ("SSSP".to_string(), 2),
                ("KHOPCOUNT".to_string(), 2),
                ("TRIANGLECOUNTING".to_string(), 0),
                ("PPR".to_string(), 2),
                ("DIAMETERESTIMATION".to_string(), 0),
                ("BFSCOUNT".to_string(), 1),
            ]
        );
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

    /// The single update op of a one-operation request.
    fn one_op(q: &str) -> UpdateOp {
        match parse(q).unwrap() {
            Query::Update(mut ops) => {
                assert_eq!(ops.len(), 1);
                ops.pop().unwrap()
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn parses_insert_data() {
        match one_op("INSERT DATA { <a> <p> <b> . <a> <p> \"v\" . }") {
            UpdateOp::InsertData(ts) => {
                assert_eq!(ts.len(), 2);
                assert_eq!(ts[0].subject, Term::iri("a"));
                assert_eq!(ts[1].object, Term::plain_literal("v"));
            }
            other => panic!("expected InsertData, got {other:?}"),
        }
    }

    #[test]
    fn parses_delete_data() {
        match one_op("DELETE DATA { <a> <p> <b> }") {
            UpdateOp::DeleteData(ts) => assert_eq!(ts.len(), 1),
            other => panic!("expected DeleteData, got {other:?}"),
        }
    }

    #[test]
    fn parses_delete_where() {
        match one_op("DELETE WHERE { ?s <p> ?o }") {
            UpdateOp::Modify {
                delete,
                insert,
                pattern,
            } => {
                assert_eq!(delete.len(), 1);
                assert!(insert.is_empty());
                assert!(matches!(pattern, GraphPattern::Bgp(_)));
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn parses_delete_insert_where() {
        match one_op("DELETE { ?s <old> ?o } INSERT { ?s <new> ?o } WHERE { ?s <old> ?o }") {
            UpdateOp::Modify {
                delete, insert, ..
            } => {
                assert_eq!(delete.len(), 1);
                assert_eq!(insert.len(), 1);
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn parses_insert_where_and_clear_load() {
        assert!(matches!(
            one_op("INSERT { ?s <a> <b> } WHERE { ?s <p> ?o }"),
            UpdateOp::Modify { delete, .. } if delete.is_empty()
        ));
        assert!(matches!(
            one_op("CLEAR ALL"),
            UpdateOp::Clear { target: GraphTarget::All, .. }
        ));
        assert!(matches!(
            one_op("LOAD <file:///tmp/x.ttl>"),
            UpdateOp::Load { silent: false, .. }
        ));
        assert!(matches!(
            one_op("DROP SILENT DEFAULT"),
            UpdateOp::Drop { target: GraphTarget::Default, silent: true }
        ));
    }

    #[test]
    fn parses_update_sequence() {
        match parse("INSERT DATA { <a> <p> <b> } ; DELETE DATA { <a> <p> <b> }").unwrap() {
            Query::Update(ops) => {
                assert_eq!(ops.len(), 2);
                assert!(matches!(ops[0], UpdateOp::InsertData(_)));
                assert!(matches!(ops[1], UpdateOp::DeleteData(_)));
            }
            other => panic!("expected Update, got {other:?}"),
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
    fn graph_pattern_parses() {
        let q = sel("SELECT * WHERE { GRAPH ?g { ?s <p> ?o } }");
        match &q.pattern {
            GraphPattern::Graph(GraphTerm::Var(g), inner) => {
                assert_eq!(g, "g");
                assert_eq!(triples(inner).len(), 1);
            }
            other => panic!("expected Graph, got {other:?}"),
        }
        let q2 = sel("SELECT * WHERE { GRAPH <http://ex/g> { ?s <p> ?o } }");
        assert!(matches!(
            &q2.pattern,
            GraphPattern::Graph(GraphTerm::Iri(i), _) if i == "http://ex/g"
        ));
    }

    #[test]
    fn service_parses_constant_endpoint() {
        let q = sel("SELECT * WHERE { SERVICE <http://ex/sparql> { ?s <p> ?o } }");
        match &q.pattern {
            GraphPattern::Service {
                endpoint,
                silent,
                pattern,
            } => {
                assert_eq!(*endpoint, ServiceRef::Iri("http://ex/sparql".into()));
                assert!(!silent);
                assert_eq!(triples(pattern).len(), 1);
            }
            other => panic!("expected Service, got {other:?}"),
        }
        // Variables from the inner pattern are discoverable for SELECT *.
        assert_eq!(q.result_vars(), vec!["s".to_string(), "o".to_string()]);
    }

    #[test]
    fn service_silent_and_variable_endpoint_and_join() {
        // SILENT flag with a variable endpoint.
        let q = sel("SELECT * WHERE { ?a <p> ?b SERVICE SILENT ?svc { ?b <q> ?c } }");
        // The outer BGP joins with the SERVICE node.
        let mut found = false;
        fn walk(gp: &GraphPattern, found: &mut bool) {
            match gp {
                GraphPattern::Service {
                    endpoint, silent, ..
                } => {
                    assert_eq!(*endpoint, ServiceRef::Var("svc".into()));
                    assert!(silent);
                    *found = true;
                }
                GraphPattern::Join(a, b) => {
                    walk(a, found);
                    walk(b, found);
                }
                _ => {}
            }
        }
        walk(&q.pattern, &mut found);
        assert!(found, "SERVICE node not present in {:?}", q.pattern);
    }

    #[test]
    fn insert_data_into_graph_parses() {
        match one_op("INSERT DATA { GRAPH <http://ex/g> { <a> <p> <b> } }") {
            UpdateOp::InsertData(ts) => {
                assert_eq!(ts.len(), 1);
                assert_eq!(ts[0].graph.as_deref(), Some("http://ex/g"));
            }
            other => panic!("expected InsertData, got {other:?}"),
        }
    }

    #[test]
    fn path_zero_or_one_parses() {
        let q = sel("SELECT * WHERE { ?s <http://p>? ?o }");
        match &q.pattern {
            GraphPattern::Path(p) => assert!(matches!(p.path, PathExpr::ZeroOrOne(_))),
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn path_zero_or_one_after_group() {
        let q = sel("SELECT * WHERE { ?s (<http://p>/<http://q>)? ?o }");
        match &q.pattern {
            GraphPattern::Path(p) => assert!(matches!(p.path, PathExpr::ZeroOrOne(_))),
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn exists_filter_parses() {
        let q = sel("SELECT * WHERE { ?s <p> ?o FILTER EXISTS { ?s <q> ?z } }");
        match &q.pattern {
            GraphPattern::Filter(fs, _) => assert!(matches!(fs[0], Expr::Exists(false, _))),
            other => panic!("expected Filter, got {other:?}"),
        }
    }

    #[test]
    fn not_exists_filter_parses() {
        let q = sel("SELECT * WHERE { ?s <p> ?o FILTER NOT EXISTS { ?s <q> ?z } }");
        match &q.pattern {
            GraphPattern::Filter(fs, _) => assert!(matches!(fs[0], Expr::Exists(true, _))),
            other => panic!("expected Filter, got {other:?}"),
        }
    }

    #[test]
    fn describe_targets_and_star() {
        match parse("DESCRIBE <http://a> <http://b>").unwrap() {
            Query::Describe(d) => {
                assert_eq!(d.targets.len(), 2);
                assert!(!d.all);
                assert!(d.pattern.is_none());
            }
            other => panic!("expected Describe, got {other:?}"),
        }
        match parse("DESCRIBE * WHERE { ?s <p> ?o }").unwrap() {
            Query::Describe(d) => {
                assert!(d.all);
                assert!(d.pattern.is_some());
            }
            other => panic!("expected Describe, got {other:?}"),
        }
    }
}
