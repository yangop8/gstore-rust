//! A minimal Model Context Protocol (MCP) server (C18), exposing gnlqa to any
//! MCP client (Claude Desktop, IDE agents, …) as four tools:
//!
//! * `ask_kg`          — natural-language question → grounded answer
//! * `run_sparql`      — raw SPARQL query → results
//! * `link_entity`     — surface mention → linked KG candidates
//! * `graph_analytics` — analytics question (shortest path / centrality / …)
//!
//! It speaks JSON-RPC 2.0 over stdio (newline-delimited messages), implementing
//! the `initialize` / `tools/list` / `tools/call` handshake by hand — no MCP SDK,
//! only serde_json. [`McpServer::handle_message`] is the pure, testable core;
//! [`McpServer::serve_stdio`] is the transport loop.

use std::io::{BufRead, Read, Write};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::analytics::run_analytics;
use crate::intent::{Mention, MentionKind};
use crate::kb::SparqlAnswer;
use crate::solve::SolveEngine;

/// MCP protocol version this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Max bytes for one JSON-RPC message over stdio — bounds memory against a
/// client that sends a huge line with no newline.
const MAX_MSG_BYTES: usize = 1 << 20;

/// An MCP server backed by a [`SolveEngine`].
pub struct McpServer {
    engine: Arc<SolveEngine>,
}

impl McpServer {
    pub fn new(engine: Arc<SolveEngine>) -> McpServer {
        McpServer { engine }
    }

    /// Serve MCP over stdio until EOF: one JSON-RPC message per line.
    pub fn serve_stdio(&self) {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut stdout = std::io::stdout();
        let mut consecutive_errors = 0u32;
        loop {
            let mut buf = Vec::new();
            // Read one message, bounded so a huge line (no newline) can't OOM us.
            match (&mut reader).take(MAX_MSG_BYTES as u64 + 1).read_until(b'\n', &mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => consecutive_errors = 0,
                Err(_) => {
                    // Skip a transient/bad read; bail only if persistently broken.
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        break;
                    }
                    continue;
                }
            }
            // Hit the cap without a terminating newline ⇒ oversize message. Drain
            // the rest of the line (in bounded chunks) and reply with a parse
            // error rather than processing a partial message.
            if buf.len() > MAX_MSG_BYTES && buf.last() != Some(&b'\n') {
                loop {
                    let mut sink = Vec::new();
                    match (&mut reader).take(MAX_MSG_BYTES as u64).read_until(b'\n', &mut sink) {
                        Ok(0) => break,
                        Ok(_) if sink.last() == Some(&b'\n') => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                let resp = err_response(&Value::Null, -32700, "message exceeds maximum size");
                if writeln!(stdout, "{resp}").is_err() {
                    break;
                }
                let _ = stdout.flush();
                continue;
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(resp) = self.handle_message(line) {
                if writeln!(stdout, "{resp}").is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }

    /// Handle one JSON-RPC message. Returns the response JSON, or `None` for a
    /// notification (no `id`, no reply expected).
    pub fn handle_message(&self, line: &str) -> Option<String> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(err_response(&Value::Null, -32700, &format!("parse error: {e}"))),
        };
        // Valid JSON that isn't a request/notification object (a batch array, or
        // a bare scalar) is an Invalid Request — reply so a client isn't left
        // waiting on silence. (We don't implement JSON-RPC batching.)
        if !req.is_object() {
            return Some(err_response(&Value::Null, -32600, "invalid request: expected a JSON-RPC object"));
        }
        // A message without `id` is a JSON-RPC notification — never respond, even
        // to a known method (`?` returns None here). A present-but-null id is
        // still a request.
        let id = req.get("id").cloned()?;
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            return Some(err_response(&id, -32600, "invalid request: missing method"));
        };
        match method {
            "initialize" => Some(ok_response(&id, self.initialize_result())),
            "ping" => Some(ok_response(&id, json!({}))),
            "tools/list" => Some(ok_response(&id, tools_list())),
            "tools/call" => Some(self.tools_call(&id, req.get("params"))),
            other => Some(err_response(&id, -32601, &format!("method not found: {other}"))),
        }
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "gnlqa", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    fn tools_call(&self, id: &Value, params: Option<&Value>) -> String {
        let Some(params) = params else {
            return err_response(id, -32602, "missing params");
        };
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let result = match name {
            "ask_kg" => self.tool_ask_kg(&args),
            "run_sparql" => self.tool_run_sparql(&args),
            "link_entity" => self.tool_link_entity(&args),
            "graph_analytics" => self.tool_graph_analytics(&args),
            other => Err(format!("unknown tool: {other}")),
        };
        // MCP convention: tool-level failures are a normal result with isError,
        // not a JSON-RPC error (which is reserved for protocol faults).
        match result {
            Ok(text) => ok_response(id, tool_content(&text, false)),
            Err(e) => ok_response(id, tool_content(&e, true)),
        }
    }

    fn tool_ask_kg(&self, args: &Value) -> Result<String, String> {
        let q = str_arg(args, "question")?;
        let a = self.engine.ask(q).map_err(|e| e.to_string())?;
        let mut out = a.text;
        if let Some(s) = a.sparql {
            out.push_str(&format!("\n\n[sparql] {s}"));
        }
        out.push_str(&format!("\n[confidence] {:.2}", a.confidence));
        out.push_str(&format!("  [provided by {}]", a.provenance.label()));
        if a.abstained {
            out.push_str(" [abstained]");
        }
        Ok(out)
    }

    fn tool_run_sparql(&self, args: &Value) -> Result<String, String> {
        let q = str_arg(args, "query")?;
        let ans = self.engine.kb().query(q).map_err(|e| e.to_string())?;
        Ok(render_sparql_answer(&ans))
    }

    fn tool_link_entity(&self, args: &Value) -> Result<String, String> {
        let mention = str_arg(args, "mention")?;
        let kind = args.get("kind").and_then(Value::as_str).map(parse_kind).unwrap_or(MentionKind::Entity);
        let k = args.get("k").and_then(Value::as_u64).unwrap_or(5).max(1) as usize;
        let linker = self.engine.linker().ok_or("no entity linker is configured on this server")?;
        let m = Mention { text: mention.to_string(), kind };
        let cands = linker.link_mention(&m, k).map_err(|e| e.to_string())?;
        if cands.is_empty() {
            return Ok("(no candidates)".to_string());
        }
        Ok(cands
            .iter()
            .map(|c| format!("{} ({}) score={:.3}", c.uri, c.label, c.score))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn tool_graph_analytics(&self, args: &Value) -> Result<String, String> {
        let q = str_arg(args, "question")?;
        let seeds: Vec<String> = args
            .get("seeds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let res =
            run_analytics(self.engine.kb(), q, &seeds, self.engine.analytics_cfg()).map_err(|e| e.to_string())?;
        Ok(res.text)
    }
}

/// Extract a required string argument.
fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string argument '{key}'"))
}

fn parse_kind(s: &str) -> MentionKind {
    match s.to_lowercase().as_str() {
        "type" => MentionKind::Type,
        "literal" => MentionKind::Literal,
        _ => MentionKind::Entity,
    }
}

/// The `tools/list` catalogue with JSON-Schema input shapes.
fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "ask_kg",
                "description": "Answer a natural-language question over the knowledge graph (full NL→SPARQL→answer pipeline, grounded and cited).",
                "inputSchema": {
                    "type": "object",
                    "properties": { "question": { "type": "string", "description": "The natural-language question." } },
                    "required": ["question"]
                }
            },
            {
                "name": "run_sparql",
                "description": "Execute a raw SPARQL query against the graph store and return the results.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "query": { "type": "string", "description": "A SPARQL 1.1 query." } },
                    "required": ["query"]
                }
            },
            {
                "name": "link_entity",
                "description": "Link a surface mention to candidate KG entities/types (requires a configured linker).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "mention": { "type": "string", "description": "The surface text to link." },
                        "kind": { "type": "string", "enum": ["entity", "type", "literal"], "description": "Mention kind (default entity)." },
                        "k": { "type": "integer", "description": "Max candidates (default 5)." }
                    },
                    "required": ["mention"]
                }
            },
            {
                "name": "graph_analytics",
                "description": "Run a graph-analytics query (shortest path, centrality, PageRank, communities, triangles).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "The analytics question." },
                        "seeds": { "type": "array", "items": { "type": "string" }, "description": "Entity IRIs (for shortest path)." }
                    },
                    "required": ["question"]
                }
            }
        ]
    })
}

/// Render a SPARQL answer as compact text (TSV for SELECT, capped).
fn render_sparql_answer(ans: &SparqlAnswer) -> String {
    match ans {
        SparqlAnswer::Boolean(b) => b.to_string(),
        SparqlAnswer::Graph(g) => g.clone(),
        SparqlAnswer::Select { vars, rows } => {
            if rows.is_empty() {
                return "(no results)".to_string();
            }
            let mut out = vars.join("\t");
            for r in rows.iter().take(200) {
                out.push('\n');
                let cells: Vec<String> = r
                    .iter()
                    .map(|c| c.as_ref().map(|t| t.to_term_string()).unwrap_or_default())
                    .collect();
                out.push_str(&cells.join("\t"));
            }
            if rows.len() > 200 {
                out.push_str(&format!("\n… ({} rows total)", rows.len()));
            }
            out
        }
    }
}

fn tool_content(text: &str, is_error: bool) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error })
}

fn ok_response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn err_response(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;

    fn nonempty() -> SparqlAnswer {
        SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![vec![Some(RdfTerm { kind: TermKind::Uri, value: "http://ex/a".into(), datatype: None, lang: None })]],
        }
    }

    /// An engine whose LLM emits an intent then a SPARQL array, KB returns a row.
    fn engine() -> Arc<SolveEngine> {
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        Arc::new(SolveEngine::new(Box::new(llm), Box::new(kb)).with_citations(false))
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn initialize_reports_server_info() {
        let srv = McpServer::new(engine());
        let resp = srv.handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        let v = parse(&resp);
        assert_eq!(v["result"]["serverInfo"]["name"], "gnlqa");
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn notifications_get_no_response() {
        let srv = McpServer::new(engine());
        assert!(srv.handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn tools_list_has_all_four() {
        let srv = McpServer::new(engine());
        let resp = srv.handle_message(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let v = parse(&resp);
        let names: Vec<&str> =
            v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ask_kg"));
        assert!(names.contains(&"run_sparql"));
        assert!(names.contains(&"link_entity"));
        assert!(names.contains(&"graph_analytics"));
    }

    #[test]
    fn tools_call_ask_kg_returns_answer() {
        let srv = McpServer::new(engine());
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ask_kg","arguments":{"question":"which x?"}}}"#;
        let v = parse(&srv.handle_message(req).unwrap());
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("http://ex/a"));
        assert!(text.contains("[sparql]"));
    }

    #[test]
    fn tools_call_run_sparql_renders_rows() {
        let srv = McpServer::new(engine());
        let req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"run_sparql","arguments":{"query":"SELECT ?x WHERE { ?x ?y ?z }"}}}"#;
        let v = parse(&srv.handle_message(req).unwrap());
        assert_eq!(v["result"]["isError"], false);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("<http://ex/a>"));
    }

    #[test]
    fn link_entity_without_linker_is_tool_error() {
        let srv = McpServer::new(engine());
        let req = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"link_entity","arguments":{"mention":"Alien"}}}"#;
        let v = parse(&srv.handle_message(req).unwrap());
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("no entity linker"));
    }

    #[test]
    fn unknown_tool_is_tool_error() {
        let srv = McpServer::new(engine());
        let req = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#;
        let v = parse(&srv.handle_message(req).unwrap());
        assert_eq!(v["result"]["isError"], true);
    }

    #[test]
    fn unknown_method_is_jsonrpc_error() {
        let srv = McpServer::new(engine());
        let resp = srv.handle_message(r#"{"jsonrpc":"2.0","id":7,"method":"frobnicate"}"#).unwrap();
        let v = parse(&resp);
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn malformed_json_is_parse_error() {
        let srv = McpServer::new(engine());
        let resp = srv.handle_message("{not json").unwrap();
        assert_eq!(parse(&resp)["error"]["code"], -32700);
    }

    #[test]
    fn non_object_and_batch_are_invalid_request() {
        let srv = McpServer::new(engine());
        // bare scalar
        assert_eq!(parse(&srv.handle_message("42").unwrap())["error"]["code"], -32600);
        // batch array (unsupported) → must reply, not hang
        let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#;
        assert_eq!(parse(&srv.handle_message(batch).unwrap())["error"]["code"], -32600);
    }

    #[test]
    fn object_without_method_is_invalid_request() {
        let srv = McpServer::new(engine());
        let resp = srv.handle_message(r#"{"jsonrpc":"2.0","id":9}"#).unwrap();
        let v = parse(&resp);
        assert_eq!(v["error"]["code"], -32600);
        assert_eq!(v["id"], 9); // id echoed back
    }

    #[test]
    fn missing_argument_is_tool_error() {
        let srv = McpServer::new(engine());
        let req = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"ask_kg","arguments":{}}}"#;
        let v = parse(&srv.handle_message(req).unwrap());
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("question"));
    }
}
