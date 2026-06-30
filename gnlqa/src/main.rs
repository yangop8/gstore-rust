//! gNLQA command-line entry point.
//!
//! `gnlqa "<question>"` answers a question by translating it to SPARQL (via the
//! configured Claude model), validating it with gStore's parser, executing it
//! against the configured gStore endpoint, and printing the answer. With no
//! arguments it just reports configuration/readiness.

use gnlqa::{AnthropicClient, AskEngine, Config, GStoreClient};

fn main() {
    let cfg = Config::from_env();
    let question = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    if question.trim().is_empty() {
        eprintln!("gNLQA — usage: gnlqa \"<your question>\"");
        eprintln!("  model  : {}", cfg.model);
        eprintln!("  gstore : {}", cfg.gstore_endpoint);
        if !cfg.has_api_key() {
            eprintln!("  warning: ANTHROPIC_API_KEY not set — live answers will fail.");
        }
        return;
    }

    let llm = match AnthropicClient::from_config(&cfg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let kb = GStoreClient::from_config(&cfg);
    let engine = AskEngine::new(Box::new(llm), Box::new(kb));

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
