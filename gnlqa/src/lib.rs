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

pub mod analytics;
pub mod config;
pub mod embed;
pub mod error;
pub mod generate;
pub mod graphrag;
pub mod ground;
pub mod http_server;
pub mod intent;
pub mod kb;
pub mod link;
pub mod llm;
pub mod pipeline;
pub mod repair;
pub mod schema;
pub mod secret;
pub mod session;
pub mod solve;

pub use analytics::{run_analytics, AnalyticsCfg, AnalyticsOp, AnalyticsResult};
pub use config::Config;
pub use embed::{Embedder, HashEmbedder, HttpEmbedder, Scored, VectorIndex};
pub use error::{Error, Result};
pub use generate::{generate_candidates, parse_candidates};
pub use graphrag::{
    answer_from_subgraph, is_dont_know, render_subgraph, retrieve_subgraph, RetrievalCfg, Triple,
};
pub use ground::{explain, gather_citations, Citation};
pub use http_server::HttpServer;
pub use intent::{
    extract_intent, AggOp, Aggregation, Mention, MentionKind, Order, QType, QuestionIntent,
    RelationPhrase,
};
pub use kb::{
    sparql_escape_literal, validate_sparql, GStoreClient, KbClient, MockKb, RdfTerm, SparqlAnswer,
    TermKind,
};
pub use link::{local_name, Candidate, LinkKind, Linker};
pub use schema::{entity_has_predicate, EntitySchema, PredicateSchema, SchemaContext};
pub use llm::{AnthropicClient, LlmClient, LlmRequest, Message, MockLlm, Role};
pub use pipeline::{Answer, AskEngine};
pub use repair::{solve_with_repair, RepairOutcome};
pub use session::{rewrite_followup, Session};
pub use solve::{best_of, score_outcome, SolveEngine};
pub use secret::Secret;
