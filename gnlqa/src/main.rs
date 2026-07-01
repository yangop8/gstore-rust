//! gNLQA command-line entry point.
//!
//! * `gnlqa "<question>"`        — answer one question (full pipeline) and print it.
//! * `gnlqa chat`                — interactive multi-turn conversation (REPL).
//! * `gnlqa serve [addr]`        — run the HTTP server (default 127.0.0.1:9100).
//! * `gnlqa` (no args)           — print configuration/readiness.

use std::io::{self, Write};
use std::sync::Arc;

use gnlqa::{AnthropicClient, Config, GStoreClient, HttpServer, Session, SolveEngine};

fn build_engine(cfg: &Config) -> SolveEngine {
    // The KB client is always available; the LLM client errors at call time if
    // no API key is set, so build it leniently here.
    let llm = AnthropicClient::new(
        cfg.anthropic_api_key.as_ref().map(|s| s.expose().to_string()).unwrap_or_default(),
        cfg.anthropic_base_url.clone(),
        cfg.model.clone(),
    )
    .with_timeout(std::time::Duration::from_secs(cfg.timeout_secs));
    let kb = GStoreClient::from_config(cfg);
    SolveEngine::new(Box::new(llm), Box::new(kb))
        .with_model(cfg.model.clone())
        .with_fast_model(cfg.fast_model.clone())
        .with_cache(256)
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
    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("input error: {e}");
                break;
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

fn main() {
    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        None => {
            eprintln!("gNLQA — usage:");
            eprintln!("  gnlqa \"<question>\"     answer one question");
            eprintln!("  gnlqa chat              interactive multi-turn conversation");
            eprintln!("  gnlqa serve [addr]      run the HTTP server (default 127.0.0.1:9100)");
            eprintln!("  model={}  gstore={}", cfg.model, cfg.gstore_endpoint);
            if !cfg.has_api_key() {
                eprintln!("  warning: ANTHROPIC_API_KEY not set — live answers will fail.");
            }
        }
        Some("chat") => run_chat(&cfg),
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
