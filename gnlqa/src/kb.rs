//! The knowledge-base client: execute SPARQL against a gStore HTTP endpoint and
//! parse SPARQL 1.1 Results JSON, plus **local SPARQL validation** using
//! gStore's own parser (`gstore::parser::sparql::parse`) — the key advantage of
//! building on gStore: we can check (and later repair) an LLM-generated query
//! for validity before sending it anywhere.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::error::{Error, Result};
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
    /// Render back to a SPARQL/Turtle surface form (`<uri>`, `"lit"`, `_:b`).
    pub fn to_term_string(&self) -> String {
        match self.kind {
            TermKind::Uri => format!("<{}>", self.value),
            TermKind::Bnode => format!("_:{}", self.value),
            TermKind::Literal => {
                if let Some(l) = &self.lang {
                    format!("\"{}\"@{}", self.value, l)
                } else if let Some(d) = &self.datatype {
                    format!("\"{}\"^^<{}>", self.value, d)
                } else {
                    format!("\"{}\"", self.value)
                }
            }
        }
    }
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

/// Parse a SPARQL 1.1 Results JSON document into a [`SparqlAnswer`].
pub fn parse_results(v: &Value) -> Result<SparqlAnswer> {
    if let Some(b) = v.get("boolean").and_then(Value::as_bool) {
        return Ok(SparqlAnswer::Boolean(b));
    }
    let vars: Vec<String> = v["head"]["vars"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let bindings = v["results"]["bindings"].as_array().ok_or_else(|| {
        let snippet: String = v.to_string().chars().take(200).collect();
        Error::GStore(format!("results JSON has neither boolean nor bindings: {snippet}"))
    })?;
    let mut rows = Vec::with_capacity(bindings.len());
    for b in bindings {
        let mut row: Vec<Option<RdfTerm>> = Vec::with_capacity(vars.len());
        for var in &vars {
            row.push(b.get(var).and_then(parse_binding));
        }
        rows.push(row);
    }
    Ok(SparqlAnswer::Select { vars, rows })
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

/// HTTP client for a gStore SPARQL endpoint.
#[derive(Debug, Clone)]
pub struct GStoreClient {
    endpoint: String,
    auth_header: Option<Secret>,
    timeout: Duration,
}

impl GStoreClient {
    pub fn new(endpoint: impl Into<String>) -> GStoreClient {
        GStoreClient { endpoint: endpoint.into(), auth_header: None, timeout: Duration::from_secs(60) }
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
        }
    }

    /// Execute a SPARQL query (POST, `application/sparql-query`), requesting
    /// SPARQL Results JSON, and parse the response.
    pub fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        let mut req = ureq::post(&self.endpoint)
            .set("Content-Type", "application/sparql-query")
            .set("Accept", "application/sparql-results+json")
            .timeout(self.timeout);
        if let Some(auth) = &self.auth_header {
            req = req.set("Authorization", auth.expose());
        }
        let resp = req.send_string(sparql).map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                Error::GStore(format!("HTTP {code}: {}", r.into_string().unwrap_or_default()))
            }
            other => Error::GStore(other.to_string()),
        })?;
        // SELECT/ASK come back as Results JSON; CONSTRUCT/DESCRIBE as an RDF
        // graph (N-Triples). Branch on the content type so a graph response is
        // returned as `Graph(..)` rather than failing JSON parsing.
        let is_json = resp.content_type().contains("json");
        if is_json {
            let v: Value = resp.into_json().map_err(|e| Error::Json(e.to_string()))?;
            parse_results(&v)
        } else {
            let body = resp.into_string().map_err(|e| Error::GStore(e.to_string()))?;
            Ok(SparqlAnswer::Graph(body))
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
    /// Execute a SPARQL query and return the parsed answer.
    fn query(&self, sparql: &str) -> Result<SparqlAnswer>;
}

impl KbClient for GStoreClient {
    fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        GStoreClient::query(self, sparql)
    }
}

/// Forward through shared handles, so a test can keep an `Arc` to inspect a mock
/// while also handing a boxed clone to the engine.
impl<T: KbClient + ?Sized> KbClient for std::sync::Arc<T> {
    fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        (**self).query(sparql)
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
        MockKb { answers, state: Mutex::new(MockKbState::default()) }
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
        out.push(if chunk.len() > 1 { T[(((b1 & 0xf) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(b2 & 0x3f) as usize] as char } else { '=' });
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
        assert_eq!(parse_results(&json!({"head":{}, "boolean": true})).unwrap(), SparqlAnswer::Boolean(true));
    }

    #[test]
    fn term_render_roundtrips_shapes() {
        let u = RdfTerm { kind: TermKind::Uri, value: "http://ex/x".into(), datatype: None, lang: None };
        assert_eq!(u.to_term_string(), "<http://ex/x>");
        let l = RdfTerm { kind: TermKind::Literal, value: "hi".into(), datatype: None, lang: Some("en".into()) };
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
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
