//! The query engine: turn a parsed SPARQL query into results.
//!
//! Corresponds to gStore's `src/Query` plus the `Executor`/`Join`/`Optimizer`
//! pieces of `src/Database`. See [`engine::Evaluator`] for the evaluation
//! pipeline and `docs/DESIGN.md` §6.

pub mod candidates;
pub mod engine;
pub mod optimizer;
pub mod planner;
pub mod results;
pub mod value;

pub use engine::Evaluator;
pub use results::{QueryResult, ResultSet};
pub use value::Value;

