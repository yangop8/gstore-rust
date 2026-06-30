//! # gNLQA — LLM + gStore natural-language question answering
//!
//! An LLM front-end over the [`gstore`] graph database: it turns a natural-language
//! question into a validated SPARQL query (or a graph-analytics / GraphRAG plan),
//! executes it against gStore, and returns a grounded, cited answer.
//!
//! See `docs/NLQA_DESIGN.md` for the full design. This crate is intentionally a
//! separate workspace member so its heavier dependencies (an HTTPS client for the
//! LLM API, JSON) stay out of the lean `gstore` crate.
//!
//! ## Layout (built incrementally; see `.omc/GNLQA_PLAN.md`)
//! * [`config`] — runtime configuration sourced from the environment
//! * [`error`]  — the crate error type
//! * [`llm`]    — the [`llm::LlmClient`] trait + an Anthropic (Claude) client and a mock

pub mod config;
pub mod error;
pub mod llm;

pub use config::Config;
pub use error::{Error, Result};
pub use llm::{LlmClient, LlmRequest, Message, Role};
