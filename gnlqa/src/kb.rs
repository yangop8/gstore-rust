//! The knowledge-base client: execute SPARQL against a gStore HTTP endpoint and
//! parse SPARQL 1.1 Results JSON, plus **local SPARQL validation** using
//! gStore's own parser (`gstore::parser::sparql::parse`) — the key advantage of
//! building on gStore: we can check (and later repair) an LLM-generated query
//! for validity before sending it anywhere.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::error::{redact_userinfo, Error, Result};
use crate::secret::Secret;

/// The kind of an RDF term in a result binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Uri,
    Literal,
    Bnode,
}

/// One bound value in a SPARQL result row.
#[derive(Debug, Clone, PartialEq)]
pub struct RdfTerm {
    pub kind: TermKind,
    pub value: String,
    pub datatype: Option<String>,
    pub lang: Option<String>,
}

impl RdfTerm {
    /// Render back to a SPARQL/Turtle surface form (`<uri>`, `"lit"`, `_:b`),
    /// escaping literal contents so embedded quotes/backslashes/newlines can't
    /// break (or inject into) the query.
    pub fn to_term_string(&self) -> String {
        match self.kind {
            TermKind::Uri => format!("<{}>", self.value),
            TermKind::Bnode => format!("_:{}", self.value),
            TermKind::Literal => {
                let v = sparql_escape_literal(&self.value);
                if let Some(l) = &self.lang {
                    format!("\"{v}\"@{l}")
                } else if let Some(d) = &self.datatype {
                    format!("\"{v}\"^^<{d}>")
                } else {
                    format!("\"{v}\"")
                }
            }
        }
    }
}

/// Escape a string for use inside a SPARQL double-quoted literal (per the
/// SPARQL 1.1 STRING_LITERAL grammar).
pub fn sparql_escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// The result of a SPARQL query.
#[derive(Debug, Clone, PartialEq)]
pub enum SparqlAnswer {
    /// SELECT: variable names + rows aligned to `vars` (`None` = unbound).
    Select {
        vars: Vec<String>,
        rows: Vec<Vec<Option<RdfTerm>>>,
    },
    /// ASK.
    Boolean(bool),
    /// CONSTRUCT/DESCRIBE: the raw RDF graph the server returned (N-Triples).
    Graph(String),
}

impl SparqlAnswer {
    /// Number of rows (0 for ASK/Graph).
    pub fn row_count(&self) -> usize {
        match self {
            SparqlAnswer::Select { rows, .. } => rows.len(),
            SparqlAnswer::Boolean(_) | SparqlAnswer::Graph(_) => 0,
        }
    }

    /// The distinct string values bound to `var` across all rows, in order.
    pub fn column_values(&self, var: &str) -> Vec<String> {
        let SparqlAnswer::Select { vars, rows } = self else {
            return Vec::new();
        };
        let Some(idx) = vars.iter().position(|v| v == var) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for r in rows {
            if let Some(Some(t)) = r.get(idx) {
                if seen.insert(t.value.clone()) {
                    out.push(t.value.clone());
                }
            }
        }
        out
    }
}

/// Cap on a KB response body (bytes) — bounds memory against a wildcard/huge
/// result set.
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
/// Cap on the rows parsed from one SELECT result. A result with more rows is
/// never silently cut (review cluster-nlqa#17): [`KbClient::query`] fails
/// with an error that asks for a `LIMIT`, and [`KbClient::query_capped`]
/// returns the first rows *with* a truncation flag.
pub const MAX_RESULT_ROWS: usize = 100_000;

/// Parse a SPARQL 1.1 Results JSON document into a [`SparqlAnswer`]. A SELECT
/// result with more than [`MAX_RESULT_ROWS`] rows is an error (see
/// [`parse_results_capped`] to keep the first rows instead).
pub fn parse_results(v: &Value) -> Result<SparqlAnswer> {
    match parse_results_capped(v, MAX_RESULT_ROWS)? {
        (_, true) => Err(too_many_rows()),
        (ans, false) => Ok(ans),
    }
}

fn too_many_rows() -> Error {
    Error::GStore(format!(
        "the result has more than {MAX_RESULT_ROWS} rows; add a LIMIT or narrow the query"
    ))
}

/// Parse a SPARQL 1.1 Results JSON document, keeping at most `max_rows` rows
/// of a SELECT result; the flag says whether rows were dropped.
pub fn parse_results_capped(v: &Value, max_rows: usize) -> Result<(SparqlAnswer, bool)> {
    if let Some(b) = v.get("boolean").and_then(Value::as_bool) {
        return Ok((SparqlAnswer::Boolean(b), false));
    }
    let vars: Vec<String> = v["head"]["vars"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let bindings = v["results"]["bindings"].as_array().ok_or_else(|| {
        let snippet: String = v.to_string().chars().take(200).collect();
        Error::GStore(format!(
            "results JSON has neither boolean nor bindings: {snippet}"
        ))
    })?;
    let mut rows = Vec::with_capacity(bindings.len().min(max_rows));
    for b in bindings.iter().take(max_rows) {
        let mut row: Vec<Option<RdfTerm>> = Vec::with_capacity(vars.len());
        for var in &vars {
            row.push(b.get(var).and_then(parse_binding));
        }
        rows.push(row);
    }
    let truncated = bindings.len() > max_rows;
    Ok((SparqlAnswer::Select { vars, rows }, truncated))
}

/// Parse one `{"type":..,"value":..,"datatype"?,"xml:lang"?}` binding cell.
fn parse_binding(b: &Value) -> Option<RdfTerm> {
    let value = b.get("value")?.as_str()?.to_string();
    // Be explicit: an absent/null/unknown `type` is a malformed cell, not a
    // literal — coercing it would silently produce wrong SPARQL downstream.
    let kind = match b.get("type").and_then(Value::as_str) {
        Some("uri") => TermKind::Uri,
        Some("bnode") => TermKind::Bnode,
        Some("literal") | Some("typed-literal") => TermKind::Literal,
        _ => return None,
    };
    Some(RdfTerm {
        kind,
        value,
        datatype: b.get("datatype").and_then(Value::as_str).map(String::from),
        lang: b.get("xml:lang").and_then(Value::as_str).map(String::from),
    })
}

/// Validate a SPARQL string with gStore's own parser. `Ok(())` ⇒ syntactically
/// valid; the error message is suitable to feed back to the LLM for repair.
pub fn validate_sparql(sparql: &str) -> Result<()> {
    gstore::parser::sparql::parse(sparql)
        .map(|_| ())
        .map_err(|e| Error::Sparql(e.to_string()))
}

/// Like [`validate_sparql`], but also enforces that the query is **read-only and
/// answer-shaped**: only `SELECT`/`ASK` are accepted. "Parser-valid" is NOT
/// "safe to execute" — the shared parser also accepts SPARQL UPDATE
/// (`INSERT`/`DELETE`/`DROP`/`LOAD`/`CLEAR`/…), which would mutate the store. Any
/// generated or self-repaired candidate must pass this before it is executed, so
/// an adversarial/hallucinated `DELETE WHERE { ?s ?p ?o }` can never run.
///
/// A `SERVICE` clause (federation) is refused as well, wherever it appears
/// — nested groups, `OPTIONAL`, `EXISTS`, sub-`SELECT`s, projection and
/// `HAVING` / `ORDER BY` expressions (review cluster-nlqa#23): question
/// answering never needs it, and it would make the store fetch an
/// LLM-chosen or prompt-injected URL (SSRF), so the guard does not depend on
/// the gStore server's own outbound policy.
pub fn validate_readonly_sparql(sparql: &str) -> Result<()> {
    use gstore::parser::sparql::ast::Query;
    match gstore::parser::sparql::parse(sparql).map_err(|e| Error::Sparql(e.to_string()))? {
        Query::Select(s) if select_uses_service(&s) => Err(service_refused()),
        Query::Ask(a) if pattern_uses_service(&a.pattern) => Err(service_refused()),
        Query::Select(_) | Query::Ask(_) => Ok(()),
        Query::Construct(_) | Query::Describe(_) => Err(Error::Sparql(
            "only SELECT/ASK queries are allowed for question answering".into(),
        )),
        Query::Update(_) => Err(Error::Sparql(
            "refusing to execute a SPARQL UPDATE — question answering is read-only".into(),
        )),
    }
}

fn service_refused() -> Error {
    Error::Sparql(
        "refusing a SERVICE (federated) query — question answering only reads the configured \
         store"
            .into(),
    )
}

/// Whether a SELECT (pattern, projection, grouping, HAVING, ORDER BY) contains
/// a `SERVICE` pattern anywhere.
fn select_uses_service(s: &gstore::parser::sparql::ast::SelectQuery) -> bool {
    use gstore::parser::sparql::ast::{Projection, SelectItem};
    pattern_uses_service(&s.pattern)
        || match &s.projection {
            Projection::All => false,
            Projection::Items(items) => items.iter().any(|i| match i {
                SelectItem::Var(_) => false,
                SelectItem::Expr(e, _) => expr_uses_service(e),
            }),
        }
        || s.group_by.iter().any(expr_uses_service)
        || s.having.iter().any(expr_uses_service)
        || s.order_by.iter().any(|o| expr_uses_service(&o.expr))
}

/// Whether a graph pattern contains a `SERVICE` pattern anywhere (the match
/// is exhaustive, so a new pattern kind must be classified here).
fn pattern_uses_service(p: &gstore::parser::sparql::ast::GraphPattern) -> bool {
    use gstore::parser::sparql::ast::GraphPattern as G;
    match p {
        G::Service { .. } => true,
        G::Empty | G::Bgp(_) | G::Path(_) | G::Values(_, _) => false,
        G::Join(a, b) | G::Union(a, b) | G::Minus(a, b) => {
            pattern_uses_service(a) || pattern_uses_service(b)
        }
        G::LeftJoin(a, b, filters) => {
            pattern_uses_service(a)
                || pattern_uses_service(b)
                || filters.iter().any(expr_uses_service)
        }
        G::Filter(filters, inner) => {
            filters.iter().any(expr_uses_service) || pattern_uses_service(inner)
        }
        G::Extend(inner, _, e) => pattern_uses_service(inner) || expr_uses_service(e),
        G::SubSelect(s) => select_uses_service(s),
        G::Graph(_, inner) => pattern_uses_service(inner),
    }
}

/// Whether an expression contains an `EXISTS { … SERVICE … }`.
fn expr_uses_service(e: &gstore::parser::sparql::ast::Expr) -> bool {
    use gstore::parser::sparql::ast::Expr as E;
    match e {
        E::Exists(_, p) => pattern_uses_service(p),
        E::Var(_) | E::Const(_) | E::GraphFn { .. } | E::Pfn { .. } => false,
        E::Or(a, b) | E::And(a, b) | E::Compare(_, a, b) | E::Arith(_, a, b) => {
            expr_uses_service(a) || expr_uses_service(b)
        }
        E::Not(a) | E::Unary(_, a) => expr_uses_service(a),
        E::In { expr, list, .. } => expr_uses_service(expr) || list.iter().any(expr_uses_service),
        E::Builtin(_, args) | E::IriCall { args, .. } => args.iter().any(expr_uses_service),
        E::Aggregate { arg, .. } => arg.as_deref().is_some_and(expr_uses_service),
    }
}

/// HTTP client for a gStore SPARQL endpoint.
#[derive(Debug, Clone)]
pub struct GStoreClient {
    endpoint: String,
    auth_header: Option<Secret>,
    timeout: Duration,
    /// Retries of transient endpoint failures (429/502/503/504; review
    /// cluster-nlqa#16).
    retry: crate::net::RetryPolicy,
}

impl GStoreClient {
    pub fn new(endpoint: impl Into<String>) -> GStoreClient {
        GStoreClient {
            endpoint: endpoint.into(),
            auth_header: None,
            timeout: Duration::from_secs(60),
            retry: crate::net::RetryPolicy::default(),
        }
    }

    /// How transient endpoint failures (429, 502, 503, 504) are retried.
    pub fn with_retry(mut self, retry: crate::net::RetryPolicy) -> GStoreClient {
        self.retry = retry;
        self
    }

    pub fn with_timeout(mut self, t: Duration) -> GStoreClient {
        self.timeout = t;
        self
    }

    /// Build from [`Config`], wiring optional Basic-auth credentials.
    pub fn from_config(cfg: &Config) -> GStoreClient {
        let auth_header = match (&cfg.gstore_user, &cfg.gstore_password) {
            (Some(u), Some(p)) => {
                let token = base64_encode(format!("{u}:{}", p.expose()).as_bytes());
                Some(Secret::new(format!("Basic {token}")))
            }
            _ => None,
        };
        GStoreClient {
            endpoint: cfg.gstore_endpoint.clone(),
            auth_header,
            timeout: Duration::from_secs(cfg.timeout_secs),
            retry: cfg.retry_policy(),
        }
    }

    /// Execute a SPARQL query (POST, `application/sparql-query`), requesting
    /// SPARQL Results JSON, and parse the response. A SELECT result with more
    /// than [`MAX_RESULT_ROWS`] rows is an error (never silently truncated).
    pub fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        match self.query_capped(sparql)? {
            (_, true) => Err(too_many_rows()),
            (ans, false) => Ok(ans),
        }
    }

    /// Like [`query`](Self::query), but a SELECT result with more than
    /// [`MAX_RESULT_ROWS`] rows yields its first rows and `true`.
    pub fn query_capped(&self, sparql: &str) -> Result<(SparqlAnswer, bool)> {
        // No redirects: the Basic credential must only reach the configured
        // endpoint (review cluster-nlqa#15).
        let agent = crate::net::agent();
        let send = || {
            let mut req = agent
                .post(&self.endpoint)
                .set("Content-Type", "application/sparql-query")
                .set("Accept", "application/sparql-results+json")
                // never wait past the current request's time budget
                .timeout(crate::budget::cap_timeout(self.timeout));
            if let Some(auth) = &self.auth_header {
                req = req.set("Authorization", auth.expose());
            }
            req.send_string(sparql).map_err(Box::new)
        };
        let resp = crate::net::send_with_retry(
            &self.retry,
            "the gStore endpoint",
            crate::net::KB_RETRYABLE,
            send,
        )
        .map_err(|e| match *e {
            // An HTTP status (4xx/5xx) is a *query/server* error — repairable, so
            // keep it as `GStore`. A transport failure (connection refused, DNS,
            // TLS, timeout) means the endpoint is unreachable — a distinct
            // `Http` error so callers can fail fast instead of "repairing" a dead
            // socket (see solve::best_of / repair::solve_with_repair).
            ureq::Error::Status(code, r) => Error::GStore(redact_userinfo(&format!(
                "HTTP {code}: {}",
                r.into_string().unwrap_or_default()
            ))),
            // ureq prints the request URL, userinfo included (review
            // cluster-nlqa#18).
            other => Error::Http(redact_userinfo(&format!(
                "gStore endpoint unreachable: {other}"
            ))),
        })?;
        let resp = crate::net::refuse_redirect(resp, "the gStore endpoint")?;
        // SELECT/ASK come back as Results JSON; CONSTRUCT/DESCRIBE as an RDF
        // graph (N-Triples). Branch on the content type so a graph response is
        // returned as `Graph(..)` rather than failing JSON parsing.
        let is_json = resp.content_type().contains("json");
        // Bound the response body so a wildcard/huge result can't OOM the client
        // (ureq's into_json/into_string read the whole body unbounded).
        use std::io::Read;
        let mut buf = Vec::new();
        // Read one past the cap so we can tell "hit the limit" from "exactly at
        // it", and fail rather than silently returning a truncated result.
        resp.into_reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::GStore(e.to_string()))?;
        if buf.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(Error::GStore(format!(
                "response exceeds the {MAX_RESPONSE_BYTES}-byte limit"
            )));
        }
        if is_json {
            let v: Value = serde_json::from_slice(&buf).map_err(|e| Error::Json(e.to_string()))?;
            parse_results_capped(&v, MAX_RESULT_ROWS)
        } else {
            Ok((
                SparqlAnswer::Graph(String::from_utf8_lossy(&buf).into_owned()),
                false,
            ))
        }
    }

    /// Validate then execute (fails fast on invalid SPARQL without a round-trip).
    pub fn validate_and_query(&self, sparql: &str) -> Result<SparqlAnswer> {
        validate_sparql(sparql)?;
        self.query(sparql)
    }
}

/// Abstraction over a SPARQL-executing backend, so the QA pipeline can run
/// against a real [`GStoreClient`] in production and a [`MockKb`] in tests.
pub trait KbClient: Send + Sync {
    /// Execute a SPARQL query and return the parsed answer. A backend that
    /// caps result rows must fail rather than return a silently truncated
    /// answer (review cluster-nlqa#17).
    fn query(&self, sparql: &str) -> Result<SparqlAnswer>;

    /// Execute a query whose caller can use a partial result: rows beyond the
    /// backend's cap are dropped and the flag is `true`. The default never
    /// truncates.
    fn query_capped(&self, sparql: &str) -> Result<(SparqlAnswer, bool)> {
        self.query(sparql).map(|a| (a, false))
    }
}

impl KbClient for GStoreClient {
    fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        GStoreClient::query(self, sparql)
    }
    fn query_capped(&self, sparql: &str) -> Result<(SparqlAnswer, bool)> {
        GStoreClient::query_capped(self, sparql)
    }
}

/// Forward through shared handles, so a test can keep an `Arc` to inspect a mock
/// while also handing a boxed clone to the engine.
impl<T: KbClient + ?Sized> KbClient for std::sync::Arc<T> {
    fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        (**self).query(sparql)
    }
    fn query_capped(&self, sparql: &str) -> Result<(SparqlAnswer, bool)> {
        (**self).query_capped(sparql)
    }
}

/// A mock KB for tests: returns canned answers in order (cycling) and records
/// the SPARQL it was asked.
#[derive(Debug, Default)]
pub struct MockKb {
    answers: Vec<SparqlAnswer>,
    state: Mutex<MockKbState>,
}

#[derive(Debug, Default)]
struct MockKbState {
    next: usize,
    queries: Vec<String>,
}

impl MockKb {
    pub fn new(answers: Vec<SparqlAnswer>) -> MockKb {
        MockKb {
            answers,
            state: Mutex::new(MockKbState::default()),
        }
    }
    /// The most recent SPARQL query the pipeline sent.
    pub fn last_query(&self) -> Option<String> {
        self.state.lock().unwrap().queries.last().cloned()
    }
    pub fn query_count(&self) -> usize {
        self.state.lock().unwrap().queries.len()
    }
}

impl KbClient for MockKb {
    fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        let mut st = self.state.lock().unwrap();
        st.queries.push(sparql.to_string());
        if self.answers.is_empty() {
            return Err(Error::GStore("MockKb has no canned answers".into()));
        }
        let i = st.next % self.answers.len();
        st.next += 1;
        Ok(self.answers[i].clone())
    }
}

/// Standard base64 (with padding).
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b1 & 0xf) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_select_results() {
        let v = json!({
            "head": {"vars": ["s", "name"]},
            "results": {"bindings": [
                {"s": {"type":"uri","value":"http://ex/a"}, "name": {"type":"literal","value":"Alice","xml:lang":"en"}},
                {"s": {"type":"uri","value":"http://ex/b"}}
            ]}
        });
        let ans = parse_results(&v).unwrap();
        match ans {
            SparqlAnswer::Select { vars, rows } => {
                assert_eq!(vars, vec!["s", "name"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0].as_ref().unwrap().kind, TermKind::Uri);
                assert_eq!(rows[0][1].as_ref().unwrap().lang.as_deref(), Some("en"));
                assert!(rows[1][1].is_none()); // unbound
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn parses_ask() {
        assert_eq!(
            parse_results(&json!({"head":{}, "boolean": true})).unwrap(),
            SparqlAnswer::Boolean(true)
        );
    }

    #[test]
    fn term_render_roundtrips_shapes() {
        let u = RdfTerm {
            kind: TermKind::Uri,
            value: "http://ex/x".into(),
            datatype: None,
            lang: None,
        };
        assert_eq!(u.to_term_string(), "<http://ex/x>");
        let l = RdfTerm {
            kind: TermKind::Literal,
            value: "hi".into(),
            datatype: None,
            lang: Some("en".into()),
        };
        assert_eq!(l.to_term_string(), "\"hi\"@en");
    }

    #[test]
    fn validate_accepts_valid_and_rejects_garbage() {
        assert!(validate_sparql("SELECT ?s WHERE { ?s ?p ?o }").is_ok());
        assert!(validate_sparql("this is not sparql").is_err());
    }

    #[test]
    fn column_values_dedups() {
        let v = json!({"head":{"vars":["x"]},"results":{"bindings":[
            {"x":{"type":"uri","value":"u1"}},{"x":{"type":"uri","value":"u1"}},{"x":{"type":"uri","value":"u2"}}]}});
        let ans = parse_results(&v).unwrap();
        assert_eq!(ans.column_values("x"), vec!["u1", "u2"]);
    }

    #[test]
    fn escape_literal_neutralizes_injection() {
        assert_eq!(sparql_escape_literal("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        let t = RdfTerm {
            kind: TermKind::Literal,
            value: "x\"y".into(),
            datatype: None,
            lang: None,
        };
        assert_eq!(t.to_term_string(), "\"x\\\"y\"");
    }

    #[test]
    fn row_cap_is_an_error_or_a_flag_never_silent() {
        // review cluster-nlqa#17
        let rows: Vec<Value> = (0..5)
            .map(|i| json!({"x": {"type":"uri","value": format!("u{i}")}}))
            .collect();
        let v = json!({"head":{"vars":["x"]},"results":{"bindings": rows}});
        let (ans, cut) = parse_results_capped(&v, 3).unwrap();
        assert!(cut);
        assert_eq!(ans.row_count(), 3);
        let (ans, cut) = parse_results_capped(&v, 5).unwrap();
        assert!(!cut);
        assert_eq!(ans.row_count(), 5);
        assert!(parse_results(&v).is_ok()); // under MAX_RESULT_ROWS
                                            // a backend without a cap never reports truncation
        let kb = MockKb::new(vec![ans]);
        assert!(!kb.query_capped("q").unwrap().1);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
