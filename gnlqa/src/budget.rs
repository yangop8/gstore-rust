//! Per-request cost limits (review cluster-nlqa#13 / sec#18).
//!
//! One question can fan out into many billed LLM calls (intent, generation,
//! up to `candidates × max_rounds` repairs, GraphRAG, explanation) and many KB
//! queries. A server surface must bound that work per request, not only per
//! connection. [`with_limits`] installs a budget for the current thread; every
//! LLM call and KB query that a [`SolveEngine`](crate::solve::SolveEngine)
//! makes on that thread is charged against it, and the real HTTP clients cap
//! their network timeouts (and retry back-off) by the time that is left.
//!
//! The budget is thread-local because each server request runs on its own
//! thread (HTTP: one thread per connection; MCP: the stdio loop). Code that
//! runs without a budget installed — the library, the CLI, `eval` — is
//! unlimited, exactly as before.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::kb::{KbClient, SparqlAnswer};
use crate::llm::{LlmClient, LlmRequest};

/// Bounds on the work one request may cause. `None` = unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CostLimits {
    /// Maximum number of LLM completions (each retry of one call counts once).
    pub max_llm_calls: Option<usize>,
    /// Maximum number of result rows fetched from the KB, summed over every
    /// query of the request.
    pub max_kb_rows: Option<usize>,
    /// Wall-clock budget for the whole request (LLM + KB work).
    pub timeout: Option<Duration>,
}

impl CostLimits {
    /// No limits at all.
    pub fn unlimited() -> CostLimits {
        CostLimits::default()
    }
}

/// What a request has used so far (see [`usage`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub llm_calls: usize,
    pub kb_rows: usize,
}

#[derive(Debug, Clone, Copy)]
struct State {
    limits: CostLimits,
    deadline: Option<Instant>,
    used: Usage,
}

thread_local! {
    static CURRENT: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Restores the previous budget when dropped (also on unwind), so a nested
/// [`with_limits`] / [`unmetered`] never leaks its state to the caller.
struct Restore(Option<State>);

impl Drop for Restore {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// Run `f` with `limits` charged for every metered LLM call and KB query it
/// makes on this thread. The budget is removed again when `f` returns or
/// panics; an enclosing budget (if any) is restored.
pub fn with_limits<R>(limits: CostLimits, f: impl FnOnce() -> R) -> R {
    let state = State {
        limits,
        deadline: limits.timeout.map(|t| Instant::now() + t),
        used: Usage::default(),
    };
    let prev = CURRENT.with(|c| c.borrow_mut().replace(state));
    let _restore = Restore(prev);
    f()
}

/// Run `f` with no budget installed (for work that is not caused by the
/// current request, e.g. a lazy linker rebuild that serves every later one).
pub fn unmetered<R>(f: impl FnOnce() -> R) -> R {
    let prev = CURRENT.with(|c| c.borrow_mut().take());
    let _restore = Restore(prev);
    f()
}

/// The current request's usage, if a budget is installed.
pub fn usage() -> Option<Usage> {
    CURRENT.with(|c| c.borrow().map(|s| s.used))
}

/// Time left before the current request's deadline (`None` = no deadline or
/// no budget installed). Zero once the deadline has passed.
pub fn remaining_time() -> Option<Duration> {
    CURRENT.with(|c| {
        c.borrow()
            .and_then(|s| s.deadline)
            .map(|d| d.saturating_duration_since(Instant::now()))
    })
}

/// `timeout` capped by the time the current request has left (never below
/// 1 ms, so a client call still fails fast rather than blocking forever on a
/// zero timeout, which some transports treat as "no timeout").
pub fn cap_timeout(timeout: Duration) -> Duration {
    match remaining_time() {
        Some(left) => timeout.min(left).max(Duration::from_millis(1)),
        None => timeout,
    }
}

fn deadline_error(limits: &CostLimits) -> Error {
    let secs = limits.timeout.map(|t| t.as_secs_f64()).unwrap_or(0.0);
    Error::Limit(format!(
        "the request ran past its {secs:.0}-second time budget"
    ))
}

/// Fail once the current request's deadline has passed.
pub fn check_deadline() -> Result<()> {
    CURRENT.with(|c| match *c.borrow() {
        Some(s) if s.deadline.is_some_and(|d| Instant::now() >= d) => {
            Err(deadline_error(&s.limits))
        }
        _ => Ok(()),
    })
}

/// Charge one LLM call (checked before the call is made).
pub fn charge_llm_call() -> Result<()> {
    check_deadline()?;
    CURRENT.with(|c| {
        let mut b = c.borrow_mut();
        let Some(s) = b.as_mut() else { return Ok(()) };
        if let Some(max) = s.limits.max_llm_calls {
            if s.used.llm_calls >= max {
                return Err(Error::Limit(format!(
                    "the request needed more than its budget of {max} LLM call(s)"
                )));
            }
        }
        s.used.llm_calls += 1;
        Ok(())
    })
}

/// A transport failure ([`Error::Http`]) of a call that ended at or after
/// the request's deadline is reported as the exhausted time budget: the
/// real clients cap their network timeout by the time left, so such a
/// failure is usually that cap firing, not an outage of a healthy
/// provider or KB (which would answer 500 "upstream unavailable" and blame
/// it). Any other error is returned unchanged.
fn attribute_to_deadline(e: Error) -> Error {
    if matches!(e, Error::Http(_)) {
        if let Err(limit) = check_deadline() {
            eprintln!("gnlqa: a call ran into the request's time budget: {e}");
            return limit;
        }
    }
    e
}

/// Charge `rows` KB result rows (checked after the query returned).
pub fn charge_kb_rows(rows: usize) -> Result<()> {
    CURRENT.with(|c| {
        let mut b = c.borrow_mut();
        let Some(s) = b.as_mut() else { return Ok(()) };
        s.used.kb_rows = s.used.kb_rows.saturating_add(rows);
        if let Some(max) = s.limits.max_kb_rows {
            if s.used.kb_rows > max {
                return Err(Error::Limit(format!(
                    "the request fetched more than its budget of {max} knowledge-base row(s)"
                )));
            }
        }
        Ok(())
    })
}

/// An [`LlmClient`] that charges every call to the current budget before
/// making it. [`SolveEngine`](crate::solve::SolveEngine) wraps its client in
/// one, so every surface that reaches the LLM through the engine is metered.
pub(crate) struct MeteredLlm(pub(crate) Box<dyn LlmClient>);

impl LlmClient for MeteredLlm {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        charge_llm_call()?;
        self.0.complete(req).map_err(attribute_to_deadline)
    }
}

/// A [`KbClient`] that checks the deadline before each query and charges the
/// rows it returned afterwards (the engine's KB, also used by MCP
/// `run_sparql` and analytics through [`SolveEngine::kb`](crate::solve::SolveEngine::kb)).
pub(crate) struct MeteredKb(pub(crate) Box<dyn KbClient>);

impl KbClient for MeteredKb {
    fn query(&self, sparql: &str) -> Result<SparqlAnswer> {
        check_deadline()?;
        let ans = self.0.query(sparql).map_err(attribute_to_deadline)?;
        charge_kb_rows(ans.row_count())?;
        Ok(ans)
    }
    fn query_capped(&self, sparql: &str) -> Result<(SparqlAnswer, bool)> {
        check_deadline()?;
        let (ans, truncated) = self.0.query_capped(sparql).map_err(attribute_to_deadline)?;
        charge_kb_rows(ans.row_count())?;
        Ok((ans, truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_budget_means_unlimited() {
        for _ in 0..100 {
            charge_llm_call().unwrap();
        }
        charge_kb_rows(usize::MAX).unwrap();
        assert!(usage().is_none());
        assert!(remaining_time().is_none());
        assert_eq!(cap_timeout(Duration::from_secs(5)), Duration::from_secs(5));
    }

    #[test]
    fn llm_calls_and_rows_are_capped_and_restored() {
        let limits = CostLimits {
            max_llm_calls: Some(2),
            max_kb_rows: Some(10),
            timeout: None,
        };
        with_limits(limits, || {
            charge_llm_call().unwrap();
            charge_llm_call().unwrap();
            assert!(matches!(charge_llm_call(), Err(Error::Limit(_))));
            charge_kb_rows(10).unwrap();
            assert!(matches!(charge_kb_rows(1), Err(Error::Limit(_))));
            // unmetered work is not charged and sees no budget
            unmetered(|| {
                assert!(usage().is_none());
                charge_llm_call().unwrap();
            });
            assert_eq!(usage().unwrap().llm_calls, 2);
        });
        assert!(usage().is_none(), "the budget must not outlive with_limits");
    }

    #[test]
    fn expired_deadline_fails_every_charge() {
        let limits = CostLimits {
            max_llm_calls: None,
            max_kb_rows: None,
            timeout: Some(Duration::ZERO),
        };
        with_limits(limits, || {
            assert!(matches!(check_deadline(), Err(Error::Limit(_))));
            assert!(matches!(charge_llm_call(), Err(Error::Limit(_))));
            assert_eq!(remaining_time(), Some(Duration::ZERO));
            assert_eq!(
                cap_timeout(Duration::from_secs(60)),
                Duration::from_millis(1)
            );
        });
    }

    #[test]
    fn a_transport_failure_at_the_deadline_is_the_time_budget() {
        // review (r5-gnlqa reviewer): a call cut short by the budget-capped
        // network timeout reports the budget, not an upstream outage.
        let limits = CostLimits {
            timeout: Some(Duration::ZERO),
            ..CostLimits::unlimited()
        };
        with_limits(limits, || {
            let e = attribute_to_deadline(Error::Http("timed out".into()));
            assert!(matches!(e, Error::Limit(_)), "{e:?}");
            // other errors keep their kind
            let e = attribute_to_deadline(Error::GStore("HTTP 400: bad query".into()));
            assert!(matches!(e, Error::GStore(_)), "{e:?}");
        });
        // before the deadline (or without a budget) an outage is an outage
        let e = attribute_to_deadline(Error::Http("connection refused".into()));
        assert!(matches!(e, Error::Http(_)), "{e:?}");
    }

    #[test]
    fn budget_is_removed_on_panic() {
        let r = std::panic::catch_unwind(|| {
            with_limits(CostLimits::unlimited(), || panic!("boom"));
        });
        assert!(r.is_err());
        assert!(usage().is_none());
    }
}
