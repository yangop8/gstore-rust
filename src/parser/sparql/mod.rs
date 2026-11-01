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
//! ## SPARQL 1.1 grammar details
//!
//! * `expr IN (…)` / `expr NOT IN (…)` ([`Expr::In`]).
//! * Signed numeric literals (`-5`, `+1.5`, `-1.5e3`; the sign must touch the
//!   digits, as in the `INTEGER_NEGATIVE` & co. terminals, so `:p +7` is the
//!   object `+7` while `:p+ 7` is a path) and `true`/`false` as terms in
//!   triple patterns, `VALUES` cells and DATA blocks.
//! * Blank node property lists `[ :p :o ; :q :r ]` and collections
//!   `( 1 2 ?x )` (expanded to `rdf:first`/`rdf:rest`/`rdf:nil`) in subject
//!   and object position, nested, also as a statement of their own. In WHERE
//!   patterns and CONSTRUCT/INSERT templates their blank nodes are hidden
//!   variables ([`ANON_VAR_PREFIX`]; the engine instantiates an unbound one
//!   in a template as a fresh blank node per solution); in `INSERT DATA`
//!   they are fresh blank nodes; in `DELETE DATA`, `DELETE WHERE` and DELETE
//!   templates they are an error, as SPARQL 1.1 Update forbids blank nodes
//!   there. `[]` and `()` (`rdf:nil`) are plain terms.
//! * `ORDER BY` keys: `ASC(…)`/`DESC(…)`, `?var`, `( expr )` and bare calls
//!   (`STRLEN(?x)`, `COUNT(?x)`, `xsd:integer(?z)`). `GROUP BY` conditions:
//!   `?var`, bare calls, `( expr )` and `( expr AS ?v )`, which binds `?v`.
//!   `HAVING` takes one or more constraints (`HAVING (c1) (c2)`).
//! * `CONSTRUCT WHERE { triples }`, the short form whose template is its
//!   basic graph pattern (anything but triple patterns is an error); as in
//!   any template, its `[ … ]` / `( … )` blank nodes are fresh per solution.
//! * A trailing `VALUES` block after any query form (and inside a
//!   sub-SELECT): kept on [`SelectQuery::values`] for SELECT, joined onto the
//!   WHERE pattern for ASK/CONSTRUCT/DESCRIBE. `VALUES () { () }` and row
//!   width checks are supported.
//! * `ADD` / `MOVE` / `COPY [SILENT] (DEFAULT | [GRAPH] <g>) TO …`.
//! * Prefixed names: `PN_LOCAL` escapes (`ex:a\-b` is `ex:` + `a-b`), `%XX`
//!   kept verbatim, `:` inside local names (`ex:a:b`), `-`/`.` inside
//!   prefixes (`my-ns.v2:x`); an unescaped trailing `.` (also after a blank
//!   node label) ends the statement.
//! * gStore's `CONTAINALL(items, list [, sep])` / `CONTAINANY(…)` built-ins
//!   (`SPARQL.g4`), evaluated by the query engine.
//!
//! Known deviations: a labelled blank node `_:b` in a WHERE pattern is a
//! constant (as in upstream gStore, which reads it as the IRI `<_:b>`), not
//! a variable, and in a template or `INSERT DATA` it names the same node for
//! every solution / request; `FROM`/`FROM NAMED` are skipped; CONSTRUCT, ASK
//! and DESCRIBE accept no GROUP BY / HAVING.
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
/// advanced function, so their bare names also parse; the bare `SIMILARITY`
/// is upstream's Jaro string similarity, though, and the legacy node/string
/// `SIMILARITY` only runs as `rs:SIMILARITY` (see the query engine).
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

/// The deepest *recursive* nesting a SPARQL request may have: nested groups
/// (`{ … }`, `OPTIONAL`, `MINUS`, `GRAPH`, `SERVICE`, `EXISTS`, sub-`SELECT`),
/// sub-expressions (parenthesized, function, aggregate and `IN` list
/// arguments), unary operators, parenthesized property paths and triples
/// nodes (`[ … ]` blank node property lists and `( … )` collections, in
/// patterns, templates and DATA blocks alike). Deeper requests are refused
/// with a parse error: the parser (and the evaluator after it) recurses once
/// per level, and unbounded recursion would let one request overflow the
/// stack — which aborts the whole process, not just the request.
pub const MAX_NESTING_DEPTH: usize = 128;

/// The tallest syntax tree a SPARQL request may produce. Besides the nesting
/// of [`MAX_NESTING_DEPTH`], this counts every link of a left-deep chain —
/// `a || b || c`, `1 + 2 + 3`, `p / q / r`, `{…} UNION {…} UNION {…}`, the
/// patterns of one group, the `GROUP BY (… AS ?v)` bindings of a query or the
/// `GRAPH` blocks of a `DELETE WHERE` — each of which wraps everything before
/// it in one more node. Chains cost no parser recursion, but evaluating (and even
/// dropping) the tree recurses once per level, so they are bounded too —
/// generously, as generated queries do use long `||` or `UNION` chains.
pub const MAX_TREE_HEIGHT: usize = 1024;

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

/// How the anonymous blank nodes of the triples being parsed — `[]`,
/// `[ :p :o ]` and the cells of a collection `( … )` — are represented.
#[derive(Debug, Clone, Copy)]
enum BnodeMode {
    /// WHERE patterns and CONSTRUCT/INSERT templates: fresh hidden variables
    /// ([`ANON_VAR_PREFIX`]). In a pattern they match like variables; in a
    /// template the engine mints a fresh blank node per solution.
    Var,
    /// `INSERT DATA`: fresh blank-node terms, disjoint from the store's.
    Fresh,
    /// `DELETE DATA`, `DELETE WHERE` and DELETE templates, where SPARQL 1.1
    /// Update forbids blank nodes; the string names the context.
    Forbidden(&'static str),
}

/// Keywords that start a clause, never a function call, in the positions
/// where a bare call may appear (GROUP BY, HAVING, ORDER BY conditions).
const CLAUSE_KEYWORDS: &[&str] = &[
    "VALUES", "HAVING", "ORDER", "GROUP", "LIMIT", "OFFSET", "ASC", "DESC", "WHERE", "AS",
];

/// The keywords that open a non-triple element of a group graph pattern.
const GROUP_PATTERN_KEYWORDS: &[&str] = &[
    "FILTER", "OPTIONAL", "MINUS", "BIND", "VALUES", "GRAPH", "SERVICE", "UNION", "SELECT",
];

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
    /// Nesting levels currently open (see [`Parser::nested`]).
    depth: usize,
    /// High-water mark of the tree height reached below the innermost open
    /// [`Chain`] (absolute, i.e. including `depth`): how tall the operands
    /// parsed so far are.
    peak: usize,
    /// Counter for the hidden variables of anonymous blank nodes.
    anon: usize,
    /// How anonymous blank nodes are represented where the parser is now.
    bnode_mode: BnodeMode,
}

/// A left-deep chain being parsed (see [`Parser::chain_begin`]).
struct Chain {
    /// The enclosing chain's [`Parser::peak`], restored (and raised) at the end.
    saved_peak: usize,
    /// Nodes wrapped around the operands so far.
    links: usize,
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
            depth: 0,
            peak: 0,
            anon: 0,
            bnode_mode: BnodeMode::Var,
        }
    }

    // ---- nesting limit (see MAX_NESTING_DEPTH) ----------------------------

    fn too_deep(&self) -> GStoreError {
        self.err(format!(
            "the request is nested too deeply (more than {MAX_NESTING_DEPTH} nested groups, \
             sub-expressions or paths, or a syntax tree taller than {MAX_TREE_HEIGHT})"
        ))
    }

    /// Run `f` one nesting level deeper (a group, a sub-expression, a unary
    /// operator, a parenthesized path), failing past [`MAX_NESTING_DEPTH`].
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.too_deep());
        }
        self.depth += 1;
        self.peak = self.peak.max(self.depth);
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Start a left-deep chain (`a || b || …`, the patterns of a group, …).
    /// Its operands are parsed at the current depth; each
    /// [`chain_link`](Self::chain_link) wraps everything so far in one more
    /// node, so the chain is as tall as its links plus its tallest operand.
    fn chain_begin(&mut self) -> Chain {
        Chain {
            saved_peak: std::mem::replace(&mut self.peak, self.depth),
            links: 0,
        }
    }

    /// Account for one more node wrapped around the chain so far, failing
    /// once the tree would be taller than [`MAX_TREE_HEIGHT`].
    fn chain_link(&mut self, chain: &mut Chain) -> Result<()> {
        chain.links += 1;
        if self.peak + chain.links > MAX_TREE_HEIGHT {
            return Err(self.too_deep());
        }
        Ok(())
    }

    /// Finish a chain: its height counts towards the enclosing one's.
    fn chain_end(&mut self, chain: Chain) {
        self.peak = chain.saved_peak.max(self.peak + chain.links);
    }

    /// Run `f` with anonymous blank nodes represented as `mode`.
    fn with_bnode_mode<T>(
        &mut self,
        mode: BnodeMode,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let saved = std::mem::replace(&mut self.bnode_mode, mode);
        let r = f(self);
        self.bnode_mode = saved;
        r
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
            "INSERT" | "DELETE" | "LOAD" | "CLEAR" | "DROP" | "CREATE" | "WITH" | "ADD"
            | "MOVE" | "COPY" => Query::Update(self.parse_update_sequence()?),
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
        // Each `GROUP BY (expr AS ?v)` wraps the WHERE pattern in one more
        // `Extend` node: a chain on top of the pattern's own height.
        let mut chain = self.chain_begin();
        let mut pattern = self.parse_group_graph_pattern()?;
        let (group_by, having) = self.parse_group_having(&mut pattern, &mut chain)?;
        self.chain_end(chain);
        let (order_by, limit, offset) = self.parse_solution_modifiers()?;
        let values = self.parse_trailing_values()?;

        Ok(SelectQuery {
            distinct,
            projection,
            pattern,
            group_by,
            having,
            order_by,
            limit,
            offset,
            values,
        })
    }

    /// The `ValuesClause` that may close a query (or a sub-SELECT):
    /// `VALUES dataBlock`, if present.
    fn parse_trailing_values(&mut self) -> Result<Option<ValuesTable>> {
        if self.eat_keyword("VALUES") {
            Ok(Some(self.parse_data_block()?))
        } else {
            Ok(None)
        }
    }

    /// Join a trailing `VALUES` block onto the WHERE pattern of a query form
    /// without grouping (CONSTRUCT/ASK/DESCRIBE): SPARQL 1.1 §18.2.4.3 joins
    /// it with the WHERE solutions before the solution modifiers.
    fn join_trailing_values(pattern: GraphPattern, values: Option<ValuesTable>) -> GraphPattern {
        match values {
            Some((vars, rows)) => join_opt(pattern, GraphPattern::Values(vars, rows)),
            None => pattern,
        }
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
    ///
    /// A `GroupCondition` is a variable, a bare built-in or function call,
    /// `( expr )`, or `( expr AS ?v )`. The last form binds `?v` to the
    /// grouping value: `pattern` is extended with `BIND(expr AS ?v)` and the
    /// group key is `?v` (§18.2.4.1), so the query may project `?v`.
    /// `HAVING` takes one or more constraints (`HAVING (c1) (c2)`: all must
    /// hold); a repeated `HAVING` keyword is accepted too.
    fn parse_group_having(
        &mut self,
        pattern: &mut GraphPattern,
        chain: &mut Chain,
    ) -> Result<(Vec<Expr>, Vec<Expr>)> {
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
                        if self.eat_keyword("AS") {
                            let var = self.expect_var()?;
                            self.expect(&Token::RParen, "')' after GROUP BY (… AS ?var)")?;
                            let mut bound = Vec::new();
                            pattern.collect_vars(&mut bound);
                            if bound.contains(&var) {
                                return Err(self.err(format!(
                                    "GROUP BY (… AS ?{var}): ?{var} is already bound by the \
                                     WHERE clause; the variable must be new"
                                )));
                            }
                            let inner = std::mem::replace(pattern, GraphPattern::Empty);
                            *pattern = GraphPattern::Extend(Box::new(inner), var.clone(), e);
                            self.chain_link(chain)?;
                            group_by.push(Expr::Var(var));
                        } else {
                            self.expect(&Token::RParen, "')'")?;
                            group_by.push(e);
                        }
                    }
                    // GroupCondition ::= BuiltInCall | FunctionCall, e.g.
                    // `GROUP BY STR(?o)` or `GROUP BY rs:WCC(?x)`.
                    _ if self.at_call() => group_by.push(self.parse_primary()?),
                    _ => break,
                }
            }
            if group_by.is_empty() {
                return Err(self.err(format!(
                    "GROUP BY needs at least one condition (?var, (expr), (expr AS ?var) or a \
                     function call), found {}",
                    describe_token(self.peek())
                )));
            }
        }
        let mut having = Vec::new();
        while self.eat_keyword("HAVING") {
            if !self.at_constraint() {
                return Err(self.err(format!(
                    "HAVING needs a constraint, e.g. HAVING (COUNT(?x) > 1), found {}",
                    describe_token(self.peek())
                )));
            }
            while self.at_constraint() {
                having.push(self.parse_constraint()?);
            }
        }
        Ok((group_by, having))
    }

    /// Is the cursor at a bare `BuiltInCall` / `FunctionCall` — `NAME(…)`,
    /// `EXISTS {…}`, `NOT EXISTS {…}`, `<iri>(…)`, `pfx:name(…)` — rather
    /// than at a clause keyword such as `VALUES (?x) {…}`?
    fn at_call(&self) -> bool {
        if self.at_iri_call() {
            return true;
        }
        let Token::Word(w) = self.peek() else {
            return false;
        };
        let upper = w.to_ascii_uppercase();
        if CLAUSE_KEYWORDS.contains(&upper.as_str()) {
            return false;
        }
        match self.peek_at(1) {
            Token::LParen => true,
            Token::LBrace => upper == "EXISTS",
            Token::Word(n) => upper == "NOT" && n.eq_ignore_ascii_case("EXISTS"),
            _ => false,
        }
    }

    /// Can a `Constraint` (`( expr )` or a bare call) start here?
    fn at_constraint(&self) -> bool {
        matches!(self.peek(), Token::LParen) || self.at_call()
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
        let mut pattern = if self.eat_keyword("WHERE") || matches!(self.peek(), Token::LBrace) {
            Some(self.parse_group_graph_pattern()?)
        } else {
            None
        };
        // DESCRIBE ignores solution modifiers for our purposes; consume any.
        self.parse_solution_modifiers()?;
        if let Some((vars, rows)) = self.parse_trailing_values()? {
            let values = GraphPattern::Values(vars, rows);
            pattern = Some(match pattern {
                Some(p) => join_opt(p, values),
                None => values,
            });
        }
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
        let values = self.parse_trailing_values()?;
        Ok(AskQuery {
            pattern: Self::join_trailing_values(pattern, values),
        })
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
        self.nested(Self::parse_group_graph_pattern_inner)
    }

    fn parse_group_graph_pattern_inner(&mut self) -> Result<GraphPattern> {
        self.expect(&Token::LBrace, "'{'")?;
        // Every construct folded into `current` wraps it in one more node.
        let mut chain = self.chain_begin();
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
                    self.chain_link(&mut chain)?;
                }
                for p in std::mem::take(&mut pending_paths) {
                    current = join_opt(current, p);
                    self.chain_link(&mut chain)?;
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
                    self.chain_link(&mut chain)?;
                }
                Token::Word(w) if w.eq_ignore_ascii_case("MINUS") => {
                    self.bump();
                    flush!();
                    let right = self.parse_group_graph_pattern()?;
                    current = GraphPattern::Minus(Box::new(current), Box::new(right));
                    self.chain_link(&mut chain)?;
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
                    self.chain_link(&mut chain)?;
                }
                Token::Word(w) if w.eq_ignore_ascii_case("VALUES") => {
                    self.bump();
                    flush!();
                    let values = self.parse_inline_values()?;
                    current = join_opt(current, values);
                    self.chain_link(&mut chain)?;
                }
                Token::Word(w) if w.eq_ignore_ascii_case("GRAPH") => {
                    self.bump();
                    flush!();
                    let gterm = self.parse_graph_term()?;
                    let inner = self.parse_group_graph_pattern()?;
                    current = join_opt(current, GraphPattern::Graph(gterm, Box::new(inner)));
                    self.chain_link(&mut chain)?;
                    self.chain_link(&mut chain)?;
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
                    self.chain_link(&mut chain)?;
                    self.chain_link(&mut chain)?;
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
                    self.chain_link(&mut chain)?;
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
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(current)
    }

    /// `{…} ( UNION {…} )*` — a group, optionally unioned with more groups.
    fn parse_group_or_union(&mut self) -> Result<GraphPattern> {
        let mut chain = self.chain_begin();
        let mut left = self.parse_group_graph_pattern()?;
        while self.eat_keyword("UNION") {
            let right = self.parse_group_graph_pattern()?;
            left = GraphPattern::Union(Box::new(left), Box::new(right));
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(left)
    }

    /// Does the upcoming `{` open a sub-`SELECT`? (peek past the brace.)
    fn brace_starts_subselect(&self) -> bool {
        matches!(self.peek(), Token::LBrace)
            && matches!(self.peek_at(1), Token::Word(w) if w.eq_ignore_ascii_case("SELECT"))
    }

    /// Parse `{ SELECT … }` into a [`GraphPattern::SubSelect`].
    fn parse_subselect_group(&mut self) -> Result<GraphPattern> {
        self.nested(|p| {
            p.expect(&Token::LBrace, "'{'")?;
            let sq = p.parse_select()?;
            p.expect(&Token::RBrace, "'}'")?;
            Ok(GraphPattern::SubSelect(Box::new(sq)))
        })
    }

    /// Parse an inline `VALUES` block: `VALUES ?v { … }` or
    /// `VALUES (?a ?b) { (..) (..) }`. `UNDEF` becomes `None`.
    fn parse_inline_values(&mut self) -> Result<GraphPattern> {
        let (vars, rows) = self.parse_data_block()?;
        Ok(GraphPattern::Values(vars, rows))
    }

    /// `DataBlock ::= InlineDataOneVar | InlineDataFull`:
    /// `?v { value* }`, or `( ?a … ) { ( value* ) … }` where `()` is an empty
    /// variable list or an empty row. Every row must have exactly one value
    /// per variable.
    fn parse_data_block(&mut self) -> Result<ValuesTable> {
        let mut vars = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.bump();
            while let Token::Var(v) = self.peek() {
                if vars.contains(v) {
                    return Err(self.err(format!("VALUES declares ?{v} twice")));
                }
                vars.push(v.clone());
                self.bump();
            }
            self.expect(&Token::RParen, "')' closing the VALUES variable list")?;
            self.expect(&Token::LBrace, "'{'")?;
            let mut rows = Vec::new();
            loop {
                match self.peek() {
                    Token::LParen => {
                        self.bump();
                        let mut row = Vec::with_capacity(vars.len());
                        while !matches!(self.peek(), Token::RParen) {
                            if matches!(self.peek(), Token::Eof) {
                                return Err(self.err("unterminated VALUES row"));
                            }
                            row.push(self.parse_values_cell()?);
                        }
                        self.bump(); // ')'
                        if row.len() != vars.len() {
                            return Err(self.err(format!(
                                "VALUES row {} has {} value(s) but {} variable(s) are declared",
                                rows.len() + 1,
                                row.len(),
                                vars.len()
                            )));
                        }
                        rows.push(row);
                    }
                    Token::RBrace => {
                        self.bump();
                        break;
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected a VALUES row '( … )' or '}}', found {}",
                            describe_token(other)
                        )))
                    }
                }
            }
            Ok((vars, rows))
        } else {
            // single-variable shorthand
            let v = match self.bump() {
                Token::Var(v) => v,
                other => {
                    return Err(self.err(format!(
                        "VALUES expects ?var {{ … }} or (?var …) {{ ( … ) … }}, found {}",
                        describe_token(&other)
                    )))
                }
            };
            vars.push(v);
            self.expect(&Token::LBrace, "'{'")?;
            let mut rows = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                if matches!(self.peek(), Token::Eof) {
                    return Err(self.err("unterminated VALUES block"));
                }
                rows.push(vec![self.parse_values_cell()?]);
            }
            self.expect(&Token::RBrace, "'}'")?;
            Ok((vars, rows))
        }
    }

    /// One `VALUES` cell (`DataBlockValue`): an IRI, a literal (including a
    /// signed number or `true`/`false`) or `UNDEF`.
    fn parse_values_cell(&mut self) -> Result<Option<Term>> {
        if self.keyword().as_deref() == Some("UNDEF") {
            self.bump();
            return Ok(None);
        }
        match self.peek() {
            Token::Var(v) => Err(self.err(format!(
                "a VALUES cell must be an IRI, a literal or UNDEF, not the variable ?{v}"
            ))),
            Token::Blank(b) => Err(self.err(format!(
                "a VALUES cell must be an IRI, a literal or UNDEF, not the blank node _:{b}"
            ))),
            _ => self.parse_term_token().map(Some),
        }
    }

    // ---- CONSTRUCT --------------------------------------------------------

    /// `CONSTRUCT { template } DatasetClause* WhereClause SolutionModifier`,
    /// or the short form `CONSTRUCT DatasetClause* WHERE { TriplesTemplate? }
    /// SolutionModifier`, whose template is its WHERE triples (§16.2.4: the
    /// pattern must be a basic graph pattern — no FILTER, OPTIONAL, paths, …).
    /// Either may end with a `VALUES` block.
    fn parse_construct(&mut self) -> Result<ConstructQuery> {
        self.expect_keyword("CONSTRUCT")?;
        let (template, pattern) = if matches!(self.peek(), Token::LBrace) {
            let template = self.parse_triples_only_block("a CONSTRUCT template", "")?;
            self.skip_dataset_clauses()?;
            self.eat_keyword("WHERE");
            (template, self.parse_group_graph_pattern()?)
        } else {
            self.skip_dataset_clauses()?;
            if !self.eat_keyword("WHERE") {
                return Err(self.err(format!(
                    "expected a CONSTRUCT template '{{ … }}' or the short form CONSTRUCT WHERE \
                     {{ … }}, found {}",
                    describe_token(self.peek())
                )));
            }
            let triples = self.parse_triples_only_block(
                "the CONSTRUCT WHERE { … } short form",
                "; write CONSTRUCT { template } WHERE { … } for anything but triple patterns",
            )?;
            // The template is the pattern, except that a blank node of `[]` /
            // `( … )` is a fresh one per solution in a template (§16.2.1)
            // but existential in the pattern: the template gets its own
            // hidden variables, which no solution binds — as in the long
            // form `CONSTRUCT { T } WHERE { T }`.
            let mut renamed: HashMap<String, String> = HashMap::new();
            let mut fresh = |pt: &PatternTerm| match pt {
                PatternTerm::Var(v) if is_anon_var(v) => PatternTerm::Var(
                    renamed
                        .entry(v.clone())
                        .or_insert_with(|| {
                            let name = format!("{ANON_VAR_PREFIX}{}", self.anon);
                            self.anon += 1;
                            name
                        })
                        .clone(),
                ),
                other => other.clone(),
            };
            let template = triples
                .iter()
                .map(|tp| TriplePattern {
                    subject: fresh(&tp.subject),
                    predicate: fresh(&tp.predicate),
                    object: fresh(&tp.object),
                })
                .collect();
            let pattern = if triples.is_empty() {
                GraphPattern::Empty
            } else {
                GraphPattern::Bgp(triples)
            };
            (template, pattern)
        };
        let (order_by, limit, offset) = self.parse_solution_modifiers()?;
        let values = self.parse_trailing_values()?;
        Ok(ConstructQuery {
            template,
            pattern: Self::join_trailing_values(pattern, values),
            order_by,
            limit,
            offset,
        })
    }

    /// `{ TriplesTemplate? }`: triple patterns only — no property paths, no
    /// FILTER/OPTIONAL/… and no nested groups. `what` names the block and
    /// `hint` is appended to the error for anything else.
    fn parse_triples_only_block(&mut self, what: &str, hint: &str) -> Result<Vec<TriplePattern>> {
        let mut triples = Vec::new();
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
                Token::Eof => return Err(self.err(format!("unexpected end of query in {what}"))),
                Token::LBrace => {
                    return Err(self.err(format!(
                        "{what} allows only triple patterns, found a nested group '{{'{hint}"
                    )))
                }
                Token::Word(w)
                    if GROUP_PATTERN_KEYWORDS.contains(&w.to_ascii_uppercase().as_str()) =>
                {
                    return Err(self.err(format!(
                        "{what} allows only triple patterns, found {}{hint}",
                        w.to_ascii_uppercase()
                    )))
                }
                _ => self.parse_triples_same_subject(&mut triples, &mut paths)?,
            }
        }
        if !paths.is_empty() {
            return Err(self.err(format!("property paths are not allowed in {what}{hint}")));
        }
        Ok(triples)
    }

    /// `TriplesSameSubject(Path)`: `subject verb objlist (';' verb objlist)*`,
    /// where the subject (and any object) may be a triples node — a blank
    /// node property list `[ … ]` or a collection `( … )` — and a triples
    /// node subject may stand alone (`[ :p :o ] .`). Plain-predicate triples
    /// are pushed to `bgp`; property-path predicates emit
    /// [`GraphPattern::Path`] into `paths`.
    fn parse_triples_same_subject(
        &mut self,
        bgp: &mut Vec<TriplePattern>,
        paths: &mut Vec<GraphPattern>,
    ) -> Result<()> {
        // `[]` and `()` are plain terms (ANON / NIL), not triples nodes, so
        // they need a property list like any other subject.
        let triples_node = match self.peek() {
            Token::LBracket => !matches!(self.peek_at(1), Token::RBracket),
            Token::LParen => !matches!(self.peek_at(1), Token::RParen),
            _ => false,
        };
        let subject = self.parse_graph_node(bgp, paths)?;
        if triples_node && !self.starts_verb() {
            return Ok(());
        }
        self.parse_property_list(&subject, bgp, paths)
    }

    /// `PropertyList(Path)NotEmpty` for `subject`:
    /// `verb objlist (';' (verb objlist)?)*`.
    fn parse_property_list(
        &mut self,
        subject: &PatternTerm,
        bgp: &mut Vec<TriplePattern>,
        paths: &mut Vec<GraphPattern>,
    ) -> Result<()> {
        loop {
            let verb = self.parse_path_or_verb()?;
            // object list
            loop {
                // The triple linking to a triples-node object goes before the
                // node's own triples, keeping the textual variable order.
                let (bgp_mark, paths_mark) = (bgp.len(), paths.len());
                let object = self.parse_graph_node(bgp, paths)?;
                match &verb {
                    VerbKind::Simple(p) => bgp.insert(
                        bgp_mark,
                        TriplePattern {
                            subject: subject.clone(),
                            predicate: p.clone(),
                            object,
                        },
                    ),
                    VerbKind::Path(path) => paths.insert(
                        paths_mark,
                        GraphPattern::Path(PathPattern {
                            subject: subject.clone(),
                            path: path.clone(),
                            object,
                        }),
                    ),
                }
                if matches!(self.peek(), Token::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            // `;` may repeat, and may end the list (`:p :o ; .`).
            let mut semicolon = false;
            while matches!(self.peek(), Token::Semicolon) {
                self.bump();
                semicolon = true;
            }
            if semicolon && self.starts_verb() {
                continue;
            }
            return Ok(());
        }
    }

    /// A subject/object position (`GraphNode(Path)`): a variable, an RDF term,
    /// or a triples node whose triples are emitted into `bgp`/`paths`.
    fn parse_graph_node(
        &mut self,
        bgp: &mut Vec<TriplePattern>,
        paths: &mut Vec<GraphPattern>,
    ) -> Result<PatternTerm> {
        match self.peek() {
            Token::LBracket => self.parse_blank_node_property_list(bgp, paths),
            Token::LParen => self.parse_collection(bgp, paths),
            _ => self.parse_pattern_term(),
        }
    }

    /// `[]` (an anonymous blank node) or `[ PropertyListNotEmpty ]` (a blank
    /// node that is the subject of the listed triples).
    fn parse_blank_node_property_list(
        &mut self,
        bgp: &mut Vec<TriplePattern>,
        paths: &mut Vec<GraphPattern>,
    ) -> Result<PatternTerm> {
        self.expect(&Token::LBracket, "'['")?;
        if matches!(self.peek(), Token::RBracket) {
            self.bump();
            return self.fresh_anon("[]");
        }
        // One nesting level (see `MAX_NESTING_DEPTH`): the property list
        // recurses into nested triples nodes.
        self.nested(|p| {
            let node = p.fresh_anon("[ … ]")?;
            if !p.starts_verb() {
                return Err(p.err(format!(
                    "expected a predicate in the blank node property list '[ … ]', found {}",
                    describe_token(p.peek())
                )));
            }
            p.parse_property_list(&node, bgp, paths)?;
            p.expect(&Token::RBracket, "']' closing the blank node property list")?;
            Ok(node)
        })
    }

    /// `( node+ )`, an RDF collection, expanded to an `rdf:first`/`rdf:rest`
    /// chain of blank nodes ending in `rdf:nil`; `()` is `rdf:nil` itself.
    fn parse_collection(
        &mut self,
        bgp: &mut Vec<TriplePattern>,
        paths: &mut Vec<GraphPattern>,
    ) -> Result<PatternTerm> {
        let iri = |s: &str| PatternTerm::Term(Term::iri(s));
        self.expect(&Token::LParen, "'('")?;
        if matches!(self.peek(), Token::RParen) {
            self.bump();
            return Ok(iri(rdf::NIL));
        }
        // One nesting level (see `MAX_NESTING_DEPTH`): items may be nested
        // triples nodes.
        let items = self.nested(|p| {
            let mut items = Vec::new();
            while !matches!(p.peek(), Token::RParen) {
                if matches!(p.peek(), Token::Eof) {
                    return Err(p.err("unterminated collection '( …': expected ')'"));
                }
                items.push(p.parse_graph_node(bgp, paths)?);
            }
            p.bump(); // ')'
            Ok(items)
        })?;
        let head = self.fresh_anon("a collection ( … )")?;
        let mut cell = head.clone();
        let last = items.len() - 1;
        for (i, item) in items.into_iter().enumerate() {
            bgp.push(TriplePattern {
                subject: cell.clone(),
                predicate: iri(rdf::FIRST),
                object: item,
            });
            let next = if i == last {
                iri(rdf::NIL)
            } else {
                self.fresh_anon("a collection ( … )")?
            };
            bgp.push(TriplePattern {
                subject: cell,
                predicate: iri(rdf::REST),
                object: next.clone(),
            });
            cell = next;
        }
        Ok(head)
    }

    /// A fresh anonymous blank node (`what` is its syntax, for errors), as
    /// the current [`BnodeMode`] represents it.
    fn fresh_anon(&mut self, what: &str) -> Result<PatternTerm> {
        match self.bnode_mode {
            BnodeMode::Var => {
                let name = format!("{ANON_VAR_PREFIX}{}", self.anon);
                self.anon += 1;
                Ok(PatternTerm::Var(name))
            }
            BnodeMode::Fresh => Ok(PatternTerm::Term(Term::Blank(fresh_bnode_label()))),
            BnodeMode::Forbidden(ctx) => Err(self.err(format!(
                "blank nodes are not allowed in {ctx} (SPARQL 1.1 Update), but {what} \
                 denotes one"
            ))),
        }
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
        let mut chain = self.chain_begin();
        let mut left = self.parse_path_seq()?;
        while matches!(self.peek(), Token::Pipe) {
            self.bump();
            let right = self.parse_path_seq()?;
            left = PathExpr::Alt(Box::new(left), Box::new(right));
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(left)
    }

    fn parse_path_seq(&mut self) -> Result<PathExpr> {
        // sequence: eltOrInverse ( '/' eltOrInverse )*
        let mut chain = self.chain_begin();
        let mut left = self.parse_path_elt_or_inverse()?;
        while matches!(self.peek(), Token::Slash) {
            self.bump();
            let right = self.parse_path_elt_or_inverse()?;
            left = PathExpr::Seq(Box::new(left), Box::new(right));
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(left)
    }

    fn parse_path_elt_or_inverse(&mut self) -> Result<PathExpr> {
        if matches!(self.peek(), Token::Caret) {
            self.bump();
            self.nested(|p| Ok(PathExpr::Inverse(Box::new(p.parse_path_elt()?))))
        } else {
            self.parse_path_elt()
        }
    }

    fn parse_path_elt(&mut self) -> Result<PathExpr> {
        // Each postfix modifier wraps the element in one more node.
        let mut chain = self.chain_begin();
        let mut p = self.parse_path_primary()?;
        // postfix `*` / `+` / `?` modifiers (the lexer emits `?` with no name as
        // [`Token::Question`], so it no longer collides with the variable sigil).
        loop {
            match self.peek() {
                Token::Star => {
                    self.bump();
                    p = PathExpr::ZeroOrMore(Box::new(p));
                }
                // `+7` touching its digits is a signed number (the object,
                // as the tokenizer's longest match `INTEGER_POSITIVE` reads
                // `:p +7`), not the one-or-more modifier.
                Token::Plus if !self.at_signed_number() => {
                    self.bump();
                    p = PathExpr::OneOrMore(Box::new(p));
                }
                Token::Question => {
                    self.bump();
                    p = PathExpr::ZeroOrOne(Box::new(p));
                }
                _ => break,
            }
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(p)
    }

    fn parse_path_primary(&mut self) -> Result<PathExpr> {
        match self.peek().clone() {
            Token::LParen => {
                self.bump();
                let p = self.nested(Self::parse_path)?;
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

    /// A triple-pattern position (`VarOrTerm`): a variable or a concrete term.
    fn parse_pattern_term(&mut self) -> Result<PatternTerm> {
        if let Token::Var(v) = self.peek() {
            let v = v.clone();
            self.bump();
            return Ok(PatternTerm::Var(v));
        }
        Ok(PatternTerm::Term(self.parse_term_token()?))
    }

    /// Is the cursor at a sign immediately followed by a number (`-5`, `+1.5`)?
    fn at_signed_number(&self) -> bool {
        matches!(self.peek(), Token::Plus | Token::Minus)
            && matches!(
                self.peek_at(1),
                Token::Integer(_) | Token::Decimal(_) | Token::Double(_)
            )
            && self.spans[self.pos].end == self.spans[self.pos + 1].start
    }

    /// A `GraphTerm` other than `[]`/`()`: an IRI, a literal, a blank node
    /// label, a signed numeric literal (`-5`, `+1.5`, `-1.5e3`: the sign is
    /// part of the literal and must touch the number, as in the
    /// `INTEGER_NEGATIVE` & co. terminals) or a boolean (`true`/`false`).
    fn parse_term_token(&mut self) -> Result<Term> {
        match self.peek().clone() {
            Token::Plus | Token::Minus => {
                let sign = if matches!(self.peek(), Token::Plus) {
                    "+"
                } else {
                    "-"
                };
                let adjacent = self.spans[self.pos].end == self.spans[self.pos + 1].start;
                let (digits, datatype) = match self.peek_at(1) {
                    Token::Integer(d) => (d.clone(), xsd::INTEGER),
                    Token::Decimal(d) => (d.clone(), xsd::DECIMAL),
                    Token::Double(d) => (d.clone(), xsd::DOUBLE),
                    other => {
                        return Err(self.err(format!(
                            "expected a number after '{sign}', found {}",
                            describe_token(other)
                        )))
                    }
                };
                if !adjacent {
                    return Err(self.err(format!(
                        "a signed number is written without a space after its sign: \
                         {sign}{digits}"
                    )));
                }
                self.bump();
                self.bump();
                Ok(Term::typed_literal(format!("{sign}{digits}"), datatype))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false") => {
                self.bump();
                Ok(Term::typed_literal(w.to_ascii_lowercase(), xsd::BOOLEAN))
            }
            tok => {
                let term = self.token_to_term(&tok)?;
                self.bump();
                Ok(term)
            }
        }
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

    /// Every (sub-)expression — parenthesized, a function or aggregate
    /// argument, a projection, a `BIND` — is one nesting level.
    fn parse_expr(&mut self) -> Result<Expr> {
        self.nested(Self::parse_or)
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut chain = self.chain_begin();
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Token::Or) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut chain = self.chain_begin();
        let mut lhs = self.parse_relational()?;
        while matches!(self.peek(), Token::And) {
            self.bump();
            let rhs = self.parse_relational()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(lhs)
    }

    fn parse_relational(&mut self) -> Result<Expr> {
        let mut chain = self.chain_begin();
        let lhs = self.parse_additive()?;
        // `expr IN ExpressionList` / `expr NOT IN ExpressionList`.
        match self.keyword().as_deref() {
            Some("IN") => {
                self.bump();
                let list = self.parse_expression_list("IN")?;
                self.chain_link(&mut chain)?;
                self.chain_end(chain);
                return Ok(Expr::In {
                    expr: Box::new(lhs),
                    list,
                    negated: false,
                });
            }
            Some("NOT") => {
                if !matches!(self.peek_at(1), Token::Word(w) if w.eq_ignore_ascii_case("IN")) {
                    return Err(self.err(format!(
                        "expected IN after NOT in an expression (… NOT IN (…)), found {}",
                        describe_token(self.peek_at(1))
                    )));
                }
                self.bump();
                self.bump();
                let list = self.parse_expression_list("NOT IN")?;
                self.chain_link(&mut chain)?;
                self.chain_end(chain);
                return Ok(Expr::In {
                    expr: Box::new(lhs),
                    list,
                    negated: true,
                });
            }
            _ => {}
        }
        let op = match self.peek() {
            Token::Eq => CompareOp::Eq,
            Token::Ne => CompareOp::Ne,
            Token::Lt => CompareOp::Lt,
            Token::Gt => CompareOp::Gt,
            Token::Le => CompareOp::Le,
            Token::Ge => CompareOp::Ge,
            _ => {
                self.chain_end(chain);
                return Ok(lhs);
            }
        };
        self.bump();
        let rhs = self.parse_additive()?;
        self.chain_link(&mut chain)?;
        self.chain_end(chain);
        Ok(Expr::Compare(op, Box::new(lhs), Box::new(rhs)))
    }

    /// `ExpressionList ::= NIL | '(' Expression ( ',' Expression )* ')'`, the
    /// right-hand side of `op` (IN / NOT IN).
    fn parse_expression_list(&mut self, op: &str) -> Result<Vec<Expr>> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "{op} expects a parenthesized list of expressions, e.g. ?x {op} (1, 2), found {}",
                describe_token(self.peek())
            )));
        }
        self.bump();
        let mut list = Vec::new();
        if matches!(self.peek(), Token::RParen) {
            self.bump();
            return Ok(list);
        }
        loop {
            list.push(self.parse_expr()?);
            match self.peek() {
                Token::Comma => {
                    self.bump();
                }
                Token::RParen => {
                    self.bump();
                    return Ok(list);
                }
                other => {
                    return Err(self.err(format!(
                        "expected ',' or ')' in the {op} list, found {}",
                        describe_token(other)
                    )))
                }
            }
        }
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut chain = self.chain_begin();
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
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut chain = self.chain_begin();
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
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        let op = match self.peek() {
            Token::Not => None,
            Token::Minus => Some(UnaryOp::Neg),
            Token::Plus => Some(UnaryOp::Plus),
            _ => return self.parse_primary(),
        };
        self.bump();
        let operand = Box::new(self.nested(Self::parse_unary)?);
        Ok(match op {
            None => Expr::Not(operand),
            Some(op) => Expr::Unary(op, operand),
        })
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
            Token::Iri(_) | Token::PName(_, _) if self.at_iri_call() => self.parse_iri_call(),
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

    /// Is the cursor at `<iri>(` / `pfx:name(` — a function call by IRI?
    fn at_iri_call(&self) -> bool {
        matches!(self.peek(), Token::Iri(_) | Token::PName(..))
            && matches!(self.peek_at(1), Token::LParen)
    }

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
                if let Some(func) =
                    GFnName::from_keyword(w).filter(|_| matches!(self.peek_at(1), Token::LParen))
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
            return Err(ctx(format!(
                "expected '(', found {}",
                describe_token(self.peek())
            )));
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
                return Err(ctx(format!(
                    "missing argument {argno} ({})",
                    p.slot.describe()
                )));
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
            GSlot::Node => matches!(
                self.peek(),
                Token::Var(_) | Token::Iri(_) | Token::PName(..)
            ),
            GSlot::Bool => matches!(self.peek(), Token::Word(w)
                if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false")),
            GSlot::Int | GSlot::UInt | GSlot::Num => matches!(
                self.peek(),
                Token::Integer(_)
                    | Token::Decimal(_)
                    | Token::Double(_)
                    | Token::Plus
                    | Token::Minus
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
                    let iri = self
                        .token_iri(&tok)
                        .map_err(|e| format!("argument {argno}: {e}"))?;
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
                .parse_gfn_integer(true, true)
                .map(GArg::Int)
                .map_err(|m| format!("argument {argno} must be an integerLiteral: {m}")),
            GSlot::UInt => self
                .parse_gfn_integer(false, true)
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
                    // `maxIter` goes through `stof` upstream: no `int` bound.
                    self.parse_gfn_integer(false, false).map(GNum::Int)
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
    /// false` an unsigned one (`num_integer`), as an `i64`. With `int32`, the
    /// value must fit a C `int`: upstream reads every positional integer
    /// argument (`k`, `retNum`, `hopCnt`, `maximumKplex`'s `k`, `coreTruss`'s
    /// two parameters) with `std::stoi`, which throws `out_of_range` — a
    /// failed query — instead of wrapping (QueryParser.cpp:526-567).
    fn parse_gfn_integer(&mut self, signed: bool, int32: bool) -> std::result::Result<i64, String> {
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
        let n = format!("{sign}{digits}").parse::<i64>().ok();
        let n = if int32 {
            n.filter(|v| i32::try_from(*v).is_ok()).ok_or_else(|| {
                format!(
                    "{sign}{digits} is out of the 32-bit int range \
                     (upstream reads it with stoi)"
                )
            })?
        } else {
            n.ok_or_else(|| format!("{sign}{digits} is out of the 64-bit integer range"))?
        };
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

    /// `OrderCondition+`: `ASC( expr )` / `DESC( expr )` (a bare variable is
    /// accepted after ASC/DESC too), `?var`, `( expr )`, or a bare
    /// `Constraint` call — a built-in (`STRLEN(?x)`, `COUNT(?x)`, …) or a
    /// function call by IRI (`xsd:integer(?z)`, `rs:BFSCOUNT(?x)`).
    fn parse_order_conditions(&mut self) -> Result<Vec<OrderCondition>> {
        let mut conds = Vec::new();
        loop {
            let direction = match self.keyword().as_deref() {
                Some(d @ ("ASC" | "DESC")) => {
                    let d = d.to_string();
                    self.bump();
                    Some(d)
                }
                _ => None,
            };
            let expr = match self.peek() {
                Token::LParen => {
                    self.bump();
                    let e = self.parse_expr()?;
                    self.expect(&Token::RParen, "')'")?;
                    e
                }
                Token::Var(v) => {
                    let v = v.clone();
                    self.bump();
                    Expr::Var(v)
                }
                _ if direction.is_none() && self.at_call() => self.parse_primary()?,
                other => {
                    if let Some(d) = direction {
                        return Err(self.err(format!(
                            "{d} expects a bracketted expression, e.g. {d}(?x), found {}",
                            describe_token(other)
                        )));
                    }
                    break;
                }
            };
            conds.push(OrderCondition {
                expr,
                descending: direction.as_deref() == Some("DESC"),
            });
        }
        if conds.is_empty() {
            return Err(self.err(format!(
                "ORDER BY needs at least one condition (?var, ASC(…), DESC(…), (expr) or a \
                 function call), found {}",
                describe_token(self.peek())
            )));
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
                    // `[ … ]` / `( … )` insert fresh blank nodes (§3.1.1).
                    let data =
                        self.with_bnode_mode(BnodeMode::Fresh, |p| p.parse_ground_block())?;
                    Ok(UpdateOp::InsertData(data))
                } else {
                    let insert = self.parse_quad_template_block()?;
                    let (using, using_named) = self.parse_using_clauses()?;
                    self.expect_keyword("WHERE")?;
                    let pattern = self.parse_group_graph_pattern()?;
                    Ok(UpdateOp::Modify {
                        with: None,
                        delete: Vec::new(),
                        insert,
                        using,
                        using_named,
                        pattern,
                    })
                }
            }
            Some("DELETE") => {
                self.bump();
                if self.eat_keyword("DATA") {
                    let data = self.with_bnode_mode(BnodeMode::Forbidden("DELETE DATA"), |p| {
                        p.parse_ground_block()
                    })?;
                    Ok(UpdateOp::DeleteData(data))
                } else if self.eat_keyword("WHERE") {
                    // DELETE WHERE { quads }: the quads are both the match
                    // pattern and the delete template.
                    let quads = self
                        .with_bnode_mode(BnodeMode::Forbidden("DELETE WHERE"), |p| {
                            p.parse_quad_template_block()
                        })?;
                    let pattern = self.delete_where_pattern(&quads)?;
                    Ok(UpdateOp::Modify {
                        with: None,
                        delete: quads,
                        insert: Vec::new(),
                        using: Vec::new(),
                        using_named: Vec::new(),
                        pattern,
                    })
                } else {
                    let delete = self
                        .with_bnode_mode(BnodeMode::Forbidden("a DELETE template"), |p| {
                            p.parse_quad_template_block()
                        })?;
                    let insert = if self.eat_keyword("INSERT") {
                        self.parse_quad_template_block()?
                    } else {
                        Vec::new()
                    };
                    let (using, using_named) = self.parse_using_clauses()?;
                    self.expect_keyword("WHERE")?;
                    let pattern = self.parse_group_graph_pattern()?;
                    Ok(UpdateOp::Modify {
                        with: None,
                        delete,
                        insert,
                        using,
                        using_named,
                        pattern,
                    })
                }
            }
            Some("WITH") => {
                // `WITH <iri>` names the default graph of the DELETE/INSERT
                // … WHERE that follows (also accepted before DELETE WHERE).
                self.bump();
                let graph = self.expect_graph_iri()?;
                // Only DELETE/INSERT may follow (checked before recursing, so
                // `WITH <g> WITH <g> …` cannot recurse without bound).
                if !matches!(self.keyword().as_deref(), Some("DELETE" | "INSERT")) {
                    return Err(self.err(format!(
                        "WITH <iri> must be followed by DELETE/INSERT … WHERE, found {}",
                        describe_token(self.peek())
                    )));
                }
                match self.parse_update_op()? {
                    UpdateOp::Modify {
                        with: None,
                        delete,
                        insert,
                        using,
                        using_named,
                        pattern,
                    } => Ok(UpdateOp::Modify {
                        with: Some(graph),
                        delete,
                        insert,
                        using,
                        using_named,
                        pattern,
                    }),
                    _ => Err(self.err("WITH <iri> must be followed by DELETE/INSERT … WHERE")),
                }
            }
            Some("LOAD") => {
                self.bump();
                let silent = self.eat_keyword("SILENT");
                let source = self.expect_graph_iri()?;
                let into = if self.eat_keyword("INTO") {
                    self.expect_keyword("GRAPH")?;
                    Some(self.expect_graph_iri()?)
                } else {
                    None
                };
                Ok(UpdateOp::Load {
                    source,
                    silent,
                    into,
                })
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
            Some(op @ ("ADD" | "MOVE" | "COPY")) => {
                let op = op.to_string();
                self.bump();
                let silent = self.eat_keyword("SILENT");
                let from = self.parse_graph_or_default(&op)?;
                if !self.eat_keyword("TO") {
                    return Err(self.err(format!(
                        "expected TO in {op} [SILENT] (DEFAULT | [GRAPH] <iri>) TO (DEFAULT | \
                         [GRAPH] <iri>), found {}",
                        describe_token(self.peek())
                    )));
                }
                let to = self.parse_graph_or_default(&op)?;
                Ok(match op.as_str() {
                    "ADD" => UpdateOp::Add { from, to, silent },
                    "MOVE" => UpdateOp::Move { from, to, silent },
                    _ => UpdateOp::Copy { from, to, silent },
                })
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

    /// `GraphOrDefault ::= 'DEFAULT' | 'GRAPH'? iri` of ADD/MOVE/COPY:
    /// `None` for the default graph.
    fn parse_graph_or_default(&mut self, op: &str) -> Result<Option<String>> {
        if self.eat_keyword("DEFAULT") {
            return Ok(None);
        }
        self.eat_keyword("GRAPH");
        match self.peek() {
            Token::Iri(_) | Token::PName(..) => self.expect_graph_iri().map(Some),
            other => Err(self.err(format!(
                "{op} expects DEFAULT or a graph IRI ([GRAPH] <iri>), found {}",
                describe_token(other)
            ))),
        }
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
            Some("NAMED") => {
                self.bump();
                Ok(GraphTarget::AllNamed)
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

    /// `( USING [NAMED] <iri> )*`: the `USING` graphs and the `USING NAMED`
    /// graphs, each in source order.
    fn parse_using_clauses(&mut self) -> Result<(Vec<String>, Vec<String>)> {
        let (mut using, mut using_named) = (Vec::new(), Vec::new());
        while self.eat_keyword("USING") {
            if self.eat_keyword("NAMED") {
                using_named.push(self.expect_graph_iri()?);
            } else {
                using.push(self.expect_graph_iri()?);
            }
        }
        Ok((using, using_named))
    }

    /// A DELETE/INSERT template `{ … }` (SPARQL 1.1 Update `QuadPattern`):
    /// triple templates, and `GRAPH (<iri> | ?g) { … }` blocks whose triples
    /// target that graph.
    fn parse_quad_template_block(&mut self) -> Result<Vec<QuadPattern>> {
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
                    let graph = self.parse_graph_term()?;
                    for triple in self.parse_triple_template_block()? {
                        out.push(QuadPattern {
                            graph: Some(graph.clone()),
                            triple,
                        });
                    }
                }
                Token::Eof => return Err(self.err("unexpected end of template block")),
                _ => {
                    let mut patterns = Vec::new();
                    let mut paths = Vec::new();
                    self.parse_triples_same_subject(&mut patterns, &mut paths)?;
                    if !paths.is_empty() {
                        return Err(
                            self.err("property paths are not allowed in an INSERT/DELETE template")
                        );
                    }
                    out.extend(patterns.into_iter().map(QuadPattern::default_graph));
                }
            }
        }
        Ok(out)
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

    /// The match pattern of `DELETE WHERE { quads }`: the default-graph
    /// triples as one BGP, joined with a `GRAPH g { … }` BGP per run of
    /// consecutive quads in the same graph. The joins form a left-deep chain,
    /// bounded like any other by [`MAX_TREE_HEIGHT`].
    fn delete_where_pattern(&mut self, quads: &[QuadPattern]) -> Result<GraphPattern> {
        let default: Vec<TriplePattern> = quads
            .iter()
            .filter(|q| q.graph.is_none())
            .map(|q| q.triple.clone())
            .collect();
        let mut pattern = if default.is_empty() && quads.iter().any(|q| q.graph.is_some()) {
            GraphPattern::Empty
        } else {
            GraphPattern::Bgp(default)
        };
        let mut chain = self.chain_begin();
        let mut i = 0;
        while i < quads.len() {
            let Some(g) = &quads[i].graph else {
                i += 1;
                continue;
            };
            let mut triples = Vec::new();
            while i < quads.len() && quads[i].graph.as_ref() == Some(g) {
                triples.push(quads[i].triple.clone());
                i += 1;
            }
            // Each operand is `Graph(Bgp)`, two levels tall.
            self.peak = self.peak.max(self.depth + 2);
            let block = GraphPattern::Graph(g.clone(), Box::new(GraphPattern::Bgp(triples)));
            pattern = join_opt(pattern, block);
            self.chain_link(&mut chain)?;
        }
        self.chain_end(chain);
        Ok(pattern)
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
            other => Err(self.err(format!(
                "expected an RDF term (a variable, IRI, literal or blank node), found {}",
                describe_token(other)
            ))),
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

    /// Worst-case request shapes for stack depth: `n` levels of nesting (or
    /// `n` links of a left-deep chain).
    fn deep_shapes(n: usize) -> Vec<(&'static str, String)> {
        let rep = |s: &str| s.repeat(n);
        vec![
            ("parens", format!("SELECT * WHERE {{ ?s ?p ?o FILTER({}1{}) }}", rep("("), rep(")"))),
            ("unary", format!("SELECT * WHERE {{ ?s ?p ?o FILTER({}true) }}", rep("!"))),
            ("calls", format!("SELECT * WHERE {{ ?s ?p ?o FILTER({}?s{} != \"\") }}", rep("STR("), rep(")"))),
            ("or", format!(
                "SELECT * WHERE {{ ?s ?p ?o FILTER({}) }}",
                (0..n).map(|i| format!("?o = {i}")).collect::<Vec<_>>().join(" || ")
            )),
            ("sum", format!("SELECT * WHERE {{ ?s ?p ?o FILTER(1{} > 0) }}", rep(" + 1"))),
            ("groups", format!("SELECT * WHERE {}?s ?p ?o{}", rep("{ "), rep(" }"))),
            ("optional-chain", format!("SELECT * WHERE {{ ?s ?p ?o {} }}", rep("OPTIONAL { ?s ?p ?o } "))),
            ("optional-nest", format!("SELECT * WHERE {{ ?s ?p ?o {}{} }}", rep("OPTIONAL { ?s ?p ?o "), rep("} "))),
            ("union", format!("SELECT * WHERE {{ {} }}", vec!["{ ?s ?p ?o }"; n].join(" UNION "))),
            ("exists", format!("SELECT * WHERE {{ ?s ?p ?o {}{} }}", rep("FILTER EXISTS { ?s ?p ?o "), rep("} "))),
            ("path-seq", format!("SELECT * WHERE {{ ?s {} ?o }}", vec!["<http://ex/p>"; n].join("/"))),
            ("path-parens", format!("SELECT * WHERE {{ ?s {}<http://ex/p>{} ?o }}", rep("("), rep(")"))),
            ("path-postfix", format!("SELECT * WHERE {{ ?s <http://ex/p>{} ?o }}", rep("?"))),
            ("subselects", format!("SELECT * WHERE {{ {}?s ?p ?o{} }}", rep("{ SELECT * WHERE { "), rep(" } }"))),
            ("binds", format!(
                "SELECT * WHERE {{ ?s ?p ?o {} }}",
                (0..n).map(|i| format!("BIND(1 AS ?b{i}) ")).collect::<String>()
            )),
            ("path-patterns", format!("SELECT * WHERE {{ {} }}", rep("?s <http://ex/p>+ ?o . "))),
            ("chains-in-parens", {
                let mut e = String::from("1");
                for _ in 0..n.min(2000) {
                    e = format!("({e} + 1 + 1 + 1)");
                }
                format!("SELECT * WHERE {{ ?s ?p ?o FILTER({e} > 0) }}")
            }),
            ("update", format!("DELETE {{ ?s ?p ?o }} WHERE {}?s ?p ?o{}", rep("{ "), rep(" }"))),
            // Phase 2/3 grammar: graph functions, IN lists, triples nodes,
            // trailing VALUES, CONSTRUCT WHERE, GROUP BY (… AS ?v), DELETE
            // WHERE over named graphs.
            ("gfun-projection", format!(
                "SELECT ?s (shortestPathLen(?s, ?o, true, {{}}) AS ?v) WHERE {{ ?s ?p ?o FILTER({}true{}) }}",
                rep("("),
                rep(")")
            )),
            ("rs-calls", format!(
                "SELECT * WHERE {{ ?s ?p ?o FILTER({}?o{} >= 0) }}",
                rep("rs:SHORTESTPATHLEN(?s, "),
                rep(")")
            )),
            ("in-lists", format!("SELECT * WHERE {{ ?s ?p ?o FILTER({}true{}) }}", rep("?o IN ("), rep(")"))),
            ("not-in-lists", format!("SELECT * WHERE {{ ?s ?p ?o FILTER({}false{}) }}", rep("?o NOT IN ("), rep(")"))),
            ("bnodes", format!("SELECT * WHERE {{ ?s <http://ex/p> {}?o{} }}", rep("[ <http://ex/p> "), rep(" ]"))),
            ("collections", format!("SELECT * WHERE {{ ?s <http://ex/p> {}1{} }}", rep("( "), rep(" )"))),
            ("construct-where", format!("CONSTRUCT WHERE {{ ?s <http://ex/p> {}?o{} }}", rep("[ <http://ex/p> "), rep(" ]"))),
            ("construct-template", format!(
                "CONSTRUCT {{ ?s <http://ex/q> {}?o{} }} WHERE {{ ?s ?p ?o }}",
                rep("( "),
                rep(" )")
            )),
            ("insert-data", format!("INSERT DATA {{ <http://ex/x> <http://ex/q> {}1{} }}", rep("[ <http://ex/q> "), rep(" ]"))),
            ("insert-template", format!("INSERT {{ ?s <http://ex/q> {}?o{} }} WHERE {{ ?s <http://ex/z> ?o }}", rep("( "), rep(" )"))),
            ("subselect-values", format!("SELECT * WHERE {{ {}?s ?p ?o{} }}", rep("{ SELECT * WHERE { "), rep(" } VALUES ?v { 1 } }"))),
            ("group-by-as", format!(
                "SELECT ?g0 WHERE {{ ?s ?p ?o }} GROUP BY {}",
                (0..n).map(|i| format!("(1 AS ?g{i}) ")).collect::<String>()
            )),
            ("delete-where-graphs", format!(
                "DELETE WHERE {{ {} }}",
                (0..n).map(|i| format!("GRAPH <http://ex/g{}> {{ ?s ?p ?o }} ", i % 2)).collect::<String>()
            )),
        ]
    }

    /// Run `f` on a thread with the HTTP worker threads' stack.
    fn on_worker_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(crate::server::WORKER_STACK_SIZE)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    /// A request nested 100 000 levels deep is a parse error — not a stack
    /// overflow, which would abort the whole server process (no
    /// `catch_unwind` can contain it).
    #[test]
    fn absurdly_deep_requests_are_refused_not_a_stack_overflow() {
        on_worker_stack(|| {
            for (name, q) in deep_shapes(100_000) {
                match parse(&q) {
                    Err(GStoreError::SparqlParse(m)) => {
                        assert!(m.contains("nested too deeply"), "{name}: {m}")
                    }
                    other => panic!("{name}: expected a nesting error, got {other:?}"),
                }
            }
        });
    }

    /// Requests just inside the limits still work end to end — parsed,
    /// evaluated and dropped — on the server's worker stack, and long chains
    /// (generated queries do use hundreds of `||` or `UNION` alternatives)
    /// are allowed well past the nesting limit.
    #[test]
    fn requests_inside_the_limits_parse_and_evaluate() {
        on_worker_stack(|| {
            let mut db = crate::Database::build_from_str(
                "d",
                "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
            )
            .unwrap();
            // Nesting: one level is spent on the outer group / FILTER.
            let nested = MAX_NESTING_DEPTH / 2 - 2;
            for (name, q) in deep_shapes(nested) {
                db.query(&q).unwrap_or_else(|e| panic!("{name} at {nested}: {e}"));
            }
            // Chains: far longer than the nesting limit.
            let long = MAX_TREE_HEIGHT - 8;
            for (name, q) in deep_shapes(long) {
                if matches!(
                    name,
                    "or" | "sum" | "optional-chain" | "union" | "path-seq" | "path-postfix" | "binds"
                        | "path-patterns"
                        | "group-by-as"
                        | "delete-where-graphs"
                ) {
                    db.query(&q).unwrap_or_else(|e| panic!("{name} at {long}: {e}"));
                }
            }
        });
        // The limits themselves.
        assert!(parse(&deep_shapes(MAX_NESTING_DEPTH + 1)[0].1).is_err());
        let or_chain = |n: usize| {
            format!(
                "ASK {{ FILTER({}) }}",
                (0..n).map(|i| format!("{i} = 0")).collect::<Vec<_>>().join(" || ")
            )
        };
        assert!(parse(&or_chain(MAX_TREE_HEIGHT - 8)).is_ok());
        assert!(parse(&or_chain(MAX_TREE_HEIGHT + 1)).is_err());
        // Triples nodes share the global nesting limit (there is no separate,
        // larger one for `[ … ]` / `( … )`).
        let bnodes = |n: usize| {
            format!(
                "SELECT * WHERE {{ ?s <http://ex/p> {}?o{} }}",
                "[ <http://ex/p> ".repeat(n),
                " ]".repeat(n)
            )
        };
        assert!(parse(&bnodes(MAX_NESTING_DEPTH - 2)).is_ok());
        assert!(parse(&bnodes(MAX_NESTING_DEPTH + 1)).is_err());
        // A repeated WITH is an error up front, not a recursion per WITH.
        let with = format!(
            "{}DELETE {{ ?s ?p ?o }} WHERE {{ ?s ?p ?o }}",
            "WITH <http://ex/g> ".repeat(100_000)
        );
        on_worker_stack(move || match parse(&with) {
            Err(GStoreError::SparqlParse(m)) => {
                assert!(m.contains("WITH <iri> must be followed by DELETE/INSERT"), "{m}")
            }
            other => panic!("expected a parse error, got {other:?}"),
        });
    }

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
                ..
            } => {
                assert_eq!(delete.len(), 1);
                assert!(insert.is_empty());
                assert!(matches!(pattern, GraphPattern::Bgp(_)));
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn parses_quad_templates_with_and_using() {
        let q = "PREFIX : <http://ex/> WITH :w DELETE { ?s :p ?o . GRAPH ?g { ?s :q ?o } } \
                 INSERT { GRAPH :h { ?s :r ?o } ?s :t ?o } USING :u1 USING NAMED :n USING :u2 \
                 WHERE { GRAPH ?g { ?s :p ?o } }";
        match one_op(q) {
            UpdateOp::Modify {
                with,
                delete,
                insert,
                using,
                using_named,
                pattern,
            } => {
                assert_eq!(with.as_deref(), Some("http://ex/w"));
                let graphs =
                    |qs: &[QuadPattern]| qs.iter().map(|q| q.graph.clone()).collect::<Vec<_>>();
                assert_eq!(
                    graphs(&delete),
                    vec![None, Some(GraphTerm::Var("g".into()))]
                );
                assert_eq!(
                    graphs(&insert),
                    vec![Some(GraphTerm::Iri("http://ex/h".into())), None]
                );
                assert_eq!(using, vec!["http://ex/u1", "http://ex/u2"]);
                assert_eq!(using_named, vec!["http://ex/n"]);
                assert!(matches!(pattern, GraphPattern::Graph(..)));
            }
            other => panic!("expected Modify, got {other:?}"),
        }
        // DELETE WHERE with GRAPH blocks: the quads are also the pattern.
        match one_op("DELETE WHERE { <a> <p> ?o . GRAPH <g> { ?s <p> ?o } }") {
            UpdateOp::Modify {
                delete, pattern, ..
            } => {
                assert_eq!(delete.len(), 2);
                assert_eq!(delete[1].graph, Some(GraphTerm::Iri("g".into())));
                match pattern {
                    GraphPattern::Join(a, b) => {
                        assert!(matches!(*a, GraphPattern::Bgp(ref t) if t.len() == 1));
                        assert!(matches!(*b, GraphPattern::Graph(GraphTerm::Iri(_), _)));
                    }
                    other => panic!("expected a join, got {other:?}"),
                }
            }
            other => panic!("expected Modify, got {other:?}"),
        }
        match one_op("DELETE WHERE { GRAPH <g> { ?s <p> ?o } }") {
            UpdateOp::Modify { pattern, .. } => {
                assert!(matches!(pattern, GraphPattern::Graph(GraphTerm::Iri(_), _)))
            }
            other => panic!("expected Modify, got {other:?}"),
        }
        // WITH only qualifies a DELETE/INSERT … WHERE.
        assert!(parse("WITH <g> INSERT DATA { <a> <b> <c> }").is_err());
        assert!(parse("INSERT { <a> <b> <c> } USING NAMED WHERE {}").is_err());
    }

    #[test]
    fn parses_load_into_graph() {
        match one_op("LOAD SILENT <file:///tmp/x.ttl> INTO GRAPH <http://ex/g>") {
            UpdateOp::Load {
                source,
                silent,
                into,
            } => {
                assert_eq!(source, "file:///tmp/x.ttl");
                assert!(silent);
                assert_eq!(into.as_deref(), Some("http://ex/g"));
            }
            other => panic!("expected Load, got {other:?}"),
        }
        assert!(matches!(
            one_op("LOAD <file:///tmp/x.ttl>"),
            UpdateOp::Load { into: None, .. }
        ));
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
    fn select_star_lists_in_scope_vars_only() {
        let vars = |q: &str| sel(q).result_vars();
        // §18.2.1: MINUS contributes no in-scope variables (upstream
        // parser_test case 16).
        assert_eq!(
            vars("SELECT * { ?s ?p ?o MINUS { ?x ?y ?z } }"),
            vec!["s", "p", "o"]
        );
        assert_eq!(
            vars("SELECT * { ?s ?p ?o MINUS { ?s ?y ?z MINUS { ?z ?w ?v } } }"),
            vec!["s", "p", "o"]
        );
        // … but a MINUS-side variable bound on the left is listed once.
        assert_eq!(
            vars("SELECT * { ?s <p> ?o MINUS { ?s <q> ?o } }"),
            vec!["s", "o"]
        );
        // OPTIONAL, UNION, BIND, VALUES, GRAPH ?g and sub-SELECT projections
        // stay in scope; FILTER (NOT) EXISTS patterns do not.
        assert_eq!(
            vars(
                "SELECT * { ?a <p> ?b OPTIONAL { ?b <q> ?c } { ?d <r> ?e } UNION { ?f <s> ?e } \
                 BIND(1 AS ?g) VALUES ?h { 1 } GRAPH ?i { ?j <t> ?k } \
                 { SELECT ?l WHERE { ?l <u> ?m } } FILTER NOT EXISTS { ?n <v> ?o } }"
            ),
            vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]
        );
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
