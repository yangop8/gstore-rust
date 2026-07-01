//! gNLQA command-line entry point.
//!
//! * `gnlqa "<question>"`        — answer one question (full pipeline) and print it.
//! * `gnlqa chat`                — interactive multi-turn conversation (REPL).
//! * `gnlqa serve [addr]`        — run the HTTP server (default 127.0.0.1:9100).
//! * `gnlqa mcp`                 — run the MCP server over stdio.
//! * `gnlqa eval <qald|lcquad> <file>` — score against a benchmark dataset.
//! * `gnlqa` (no args)           — print configuration/readiness.

use std::io::{self, Write};
use std::sync::Arc;

use gnlqa::{
    load_lcquad, load_qald, run_eval, AnthropicClient, Config, EvalOptions, GStoreClient,
    HashEmbedder, HttpServer, Linker, LlmClient, McpServer, OpenAiClient, Session, SolveEngine,
};

fn build_engine(cfg: &Config) -> SolveEngine {
    // The KB client is always available; the LLM client errors at call time if
    // no API key is set, so build it leniently here. Pick the backend by provider.
    let timeout = std::time::Duration::from_secs(cfg.timeout_secs);
    let llm: Box<dyn LlmClient> = if cfg.llm_provider == "openai" {
        Box::new(
            OpenAiClient::new(
                cfg.openai_api_key.as_ref().map(|s| s.expose().to_string()).unwrap_or_default(),
                cfg.openai_base_url.clone(),
                cfg.model.clone(),
            )
            .with_timeout(timeout),
        )
    } else {
        Box::new(
            AnthropicClient::new(
                cfg.anthropic_api_key.as_ref().map(|s| s.expose().to_string()).unwrap_or_default(),
                cfg.anthropic_base_url.clone(),
                cfg.model.clone(),
            )
            .with_timeout(timeout),
        )
    };
    // Build a linker from the KB so generation is grounded in the store's ACTUAL
    // vocabulary — without it the LLM guesses URIs (e.g. DBpedia) and misses.
    // HashEmbedder is offline (no embedding API needed); swap for HttpEmbedder
    // for stronger semantic linking. Best-effort: if the KB is unreachable at
    // startup, degrade to no linker.
    let linker = Linker::build_from_kb(
        &GStoreClient::from_config(cfg),
        Box::new(HashEmbedder::new(256)),
        0.0,
        Some(10_000),
    )
    .ok();
    let mut engine = SolveEngine::new(llm, Box::new(GStoreClient::from_config(cfg)))
        .with_model(cfg.model.clone())
        .with_fast_model(cfg.fast_model.clone())
        .with_cache(256);
    if let Some(l) = linker {
        engine = engine.with_linker(l);
    }
    engine
}

/// Interactive multi-turn REPL: each line is answered in the context of the
/// conversation so far (via [`Session`]). `:reset` starts a fresh topic,
/// `:quit`/`:q`/EOF exits.
fn run_chat(cfg: &Config) {
    let engine = build_engine(cfg);
    let mut sess = Session::new(&engine);
    eprintln!("gNLQA chat — ask a question; :reset to clear context, :quit to exit.");
    if !cfg.has_api_key() {
        eprintln!("warning: ANTHROPIC_API_KEY not set — live answers will fail.");
    }
    let stdin = io::stdin();
    let mut consecutive_errors = 0u32;
    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => consecutive_errors = 0,
            Err(e) => {
                // A single bad line (e.g. non-UTF-8) shouldn't tear down the
                // chat — skip it. Bail only if the stream is persistently broken.
                eprintln!("input error: {e}");
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    break;
                }
                continue;
            }
        }
        let q = line.trim();
        match q {
            "" => continue,
            ":quit" | ":q" | ":exit" => break,
            ":reset" => {
                sess.clear();
                eprintln!("(context cleared)");
                continue;
            }
            _ => {}
        }
        match sess.ask(q) {
            Ok(a) => {
                println!("{}", a.text);
                if let Some(s) = &a.sparql {
                    eprintln!("[sparql] {s}");
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }
}

/// `gnlqa eval <qald|lcquad> <file>`: load a benchmark dataset and score gnlqa.
/// LC-QuAD ships only gold queries, so gold answers are resolved via the KB.
fn run_eval_cmd(cfg: &Config, args: &[String]) {
    let (Some(fmt), Some(path)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: gnlqa eval <qald|lcquad> <file.json>");
        std::process::exit(2);
    };
    let json = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            std::process::exit(1);
        }
    };
    let (questions, resolve_gold_via_kb) = match fmt.as_str() {
        "qald" => (load_qald(&json), false),
        "lcquad" => (load_lcquad(&json), true),
        other => {
            eprintln!("error: unknown dataset format '{other}' (expected qald or lcquad)");
            std::process::exit(2);
        }
    };
    let questions = match questions {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let engine = build_engine(cfg);
    let opts = EvalOptions { limit: None, resolve_gold_via_kb };
    let report = run_eval(&engine, &questions, &opts);
    println!("{}", report.summary());
}

fn main() {
    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        None => {
            eprintln!("gNLQA — usage:");
            eprintln!("  gnlqa \"<question>\"     answer one question");
            eprintln!("  gnlqa chat              interactive multi-turn conversation");
            eprintln!("  gnlqa mcp               run the MCP server over stdio");
            eprintln!("  gnlqa eval <qald|lcquad> <file>   score against a benchmark dataset");
            eprintln!("  gnlqa serve [addr]      run the HTTP server (default 127.0.0.1:9100)");
            eprintln!("  model={}  gstore={}", cfg.model, cfg.gstore_endpoint);
            if !cfg.has_api_key() {
                eprintln!("  warning: ANTHROPIC_API_KEY not set — live answers will fail.");
            }
        }
        Some("chat") => run_chat(&cfg),
        Some("mcp") => {
            let engine = Arc::new(build_engine(&cfg));
            eprintln!("gNLQA MCP server on stdio (tools: ask_kg, run_sparql, link_entity, graph_analytics)");
            McpServer::new(engine).serve_stdio();
        }
        Some("eval") => run_eval_cmd(&cfg, &args),
        Some("serve") => {
            let addr = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:9100".to_string());
            let engine = Arc::new(build_engine(&cfg));
            match HttpServer::bind(engine, &addr) {
                Ok(server) => {
                    eprintln!("gNLQA serving on http://{addr}  (POST /ask, /gSolve; GET /health)");
                    server.serve_forever();
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(_) => {
            let question = args.join(" ");
            let engine = build_engine(&cfg);
            match engine.ask(&question) {
                Ok(a) => {
                    println!("{}", a.text);
                    if let Some(s) = &a.sparql {
                        eprintln!("[sparql] {s}");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
