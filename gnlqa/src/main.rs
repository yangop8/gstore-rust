//! gNLQA command-line entry point.
//!
//! For now this is a skeleton that loads configuration and reports readiness.
//! Subsequent commits add the `/ask` pipeline and an HTTP/MCP server.

use gnlqa::Config;

fn main() {
    let cfg = Config::from_env();

    eprintln!("gNLQA starting…");
    eprintln!("  model       : {}", cfg.model);
    eprintln!("  fast_model  : {}", cfg.fast_model);
    eprintln!("  gstore      : {}", cfg.gstore_endpoint);
    if !cfg.has_api_key() {
        eprintln!(
            "  warning     : ANTHROPIC_API_KEY not set — live LLM calls will fail \
             (tests use a mock client and still work)."
        );
    }
    eprintln!("gNLQA skeleton ready.");
}
