//! Query deadlines (review sec#10, robust#5): a statement runs under a
//! cancel flag (`Arc<AtomicBool>`) that the evaluator polls cooperatively;
//! [`arm`] makes one process-wide timer thread raise that flag once the
//! statement's time is up, and records that it was the deadline (not a
//! `/cancel`) that raised it ([`Deadline::fired`]), so the caller can answer
//! "query timed out". Dropping the [`Deadline`] disarms it.
//!
//! [`arm_linked`] gives a statement its own flag that is raised by its
//! deadline **or** by a shared parent flag (the single-database server's
//! shutdown flag): the timer thread checks the parents of such deadlines
//! every [`LINK_POLL`] while any is armed.
//!
//! The timer keeps its deadlines ordered in a map (a disarmed one is removed
//! at once, so a long `query_timeout` costs no memory after its statement
//! ended) and sleeps until the earliest one. It is started by the first
//! [`arm`] and lives for the rest of the process.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

/// How often the timer looks at the parent flags of [`arm_linked`]
/// deadlines while any is armed.
pub const LINK_POLL: Duration = Duration::from_millis(50);

/// One armed deadline.
struct Entry {
    cancel: Weak<AtomicBool>,
    fired: Arc<AtomicBool>,
    /// For [`arm_linked`]: the flag whose raising raises `cancel` too.
    parent: Option<Weak<AtomicBool>>,
}

#[derive(Default)]
struct TimerState {
    deadlines: BTreeMap<(Instant, u64), Entry>,
    /// How many armed deadlines have a parent.
    linked: usize,
    /// Whether the timer thread runs (or failed to start).
    started: bool,
}

struct Timer {
    state: Mutex<TimerState>,
    changed: Condvar,
    next_id: AtomicU64,
}

fn timer() -> &'static Timer {
    static TIMER: OnceLock<Timer> = OnceLock::new();
    TIMER.get_or_init(|| Timer {
        state: Mutex::default(),
        changed: Condvar::new(),
        next_id: AtomicU64::new(0),
    })
}

impl Timer {
    fn lock(&self) -> MutexGuard<'_, TimerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The timer thread: fire what is due, propagate raised parents, sleep
    /// until the next deadline (or the next parent check).
    fn run(&self) {
        let mut st = self.lock();
        loop {
            let now = Instant::now();
            while let Some(entry) = st.deadlines.first_entry() {
                if entry.key().0 > now {
                    break;
                }
                let e = entry.remove();
                if e.parent.is_some() {
                    st.linked -= 1;
                }
                e.fired.store(true, Ordering::SeqCst);
                if let Some(c) = e.cancel.upgrade() {
                    c.store(true, Ordering::SeqCst);
                }
            }
            if st.linked > 0 {
                for e in st.deadlines.values() {
                    let raised = e
                        .parent
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .is_some_and(|p| p.load(Ordering::Relaxed));
                    if raised {
                        if let Some(c) = e.cancel.upgrade() {
                            c.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            let next = st
                .deadlines
                .keys()
                .next()
                .map(|(t, _)| t.saturating_duration_since(now));
            let wait = match (next, st.linked > 0) {
                (Some(d), true) => Some(d.min(LINK_POLL)),
                (Some(d), false) => Some(d),
                (None, true) => Some(LINK_POLL),
                (None, false) => None,
            };
            st = match wait {
                Some(d) => {
                    self.changed
                        .wait_timeout(st, d)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.changed.wait(st).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }
}

/// An armed deadline (see the module docs); disarmed when dropped.
#[derive(Debug)]
pub struct Deadline {
    key: (Instant, u64),
    fired: Arc<AtomicBool>,
    timeout: Duration,
}

impl Deadline {
    /// Whether the deadline passed and raised the flag.
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// The time the statement was given.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// "query timed out after … (query_timeout)" — the message of a
    /// statement this deadline stopped.
    pub fn message(&self) -> String {
        timeout_message(self.timeout)
    }
}

/// The message of a statement stopped after `timeout`.
pub fn timeout_message(timeout: Duration) -> String {
    let secs = timeout.as_secs_f64();
    let shown = if timeout.subsec_nanos() == 0 {
        format!("{}", timeout.as_secs())
    } else {
        format!("{secs:.3}")
    };
    format!(
        "query timed out after {shown} s (query_timeout): it was stopped; narrow it down \
         (e.g. a LIMIT, fewer unconnected patterns) or raise query_timeout"
    )
}

impl Drop for Deadline {
    fn drop(&mut self) {
        let t = timer();
        let mut st = t.lock();
        if let Some(e) = st.deadlines.remove(&self.key) {
            if e.parent.is_some() {
                st.linked -= 1;
            }
        }
    }
}

/// Raise `cancel` once `timeout` has passed (`None` for a zero timeout: no
/// deadline). Keep the returned [`Deadline`] while the statement runs.
pub fn arm(cancel: &Arc<AtomicBool>, timeout: Duration) -> Option<Deadline> {
    insert(Arc::downgrade(cancel), None, timeout)
}

/// A fresh flag for one statement, raised by its deadline after `timeout`
/// or as soon as `parent` is raised (checked every [`LINK_POLL`]), and the
/// deadline. With no deadline — a zero timeout, one too large to represent,
/// or a timer that could not start — `parent` itself, so the statement
/// still stops when `parent` is raised.
pub fn arm_linked(
    parent: &Arc<AtomicBool>,
    timeout: Duration,
) -> (Arc<AtomicBool>, Option<Deadline>) {
    if timeout.is_zero() {
        return (Arc::clone(parent), None);
    }
    let flag = Arc::new(AtomicBool::new(parent.load(Ordering::SeqCst)));
    match insert(Arc::downgrade(&flag), Some(Arc::downgrade(parent)), timeout) {
        Some(deadline) => (flag, Some(deadline)),
        // Nothing would link the fresh flag to `parent`.
        None => (Arc::clone(parent), None),
    }
}

fn insert(
    cancel: Weak<AtomicBool>,
    parent: Option<Weak<AtomicBool>>,
    timeout: Duration,
) -> Option<Deadline> {
    if timeout.is_zero() {
        return None;
    }
    let t = timer();
    // A timeout too large to add means no deadline in practice.
    let at = Instant::now().checked_add(timeout)?;
    let key = (at, t.next_id.fetch_add(1, Ordering::Relaxed));
    let fired = Arc::new(AtomicBool::new(false));
    let mut st = t.lock();
    if !st.started {
        st.started = true;
        let spawned = std::thread::Builder::new()
            .name("gstore-deadline".to_string())
            .spawn(|| timer().run());
        if let Err(e) = spawned {
            // The next `arm` tries again: no deadline is kept that nothing
            // would ever fire.
            st.started = false;
            crate::gs_error!(
                "cannot start the query deadline timer: {e}; this statement runs without \
                 query_timeout"
            );
            return None;
        }
    }
    // Wake the timer when it must re-plan its sleep: a new earliest
    // deadline, or a parent to check (it may sleep with no such bound).
    let wake = parent.is_some() || st.deadlines.keys().next().is_none_or(|k| key < *k);
    if parent.is_some() {
        st.linked += 1;
    }
    st.deadlines.insert(
        key,
        Entry {
            cancel,
            fired: Arc::clone(&fired),
            parent,
        },
    );
    drop(st);
    if wake {
        t.changed.notify_all();
    }
    Some(Deadline {
        key,
        fired,
        timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Wait (a hang guard, not an assertion) until `flag` is raised.
    fn raised_within(flag: &Arc<AtomicBool>, guard: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        let f = Arc::clone(flag);
        std::thread::spawn(move || {
            while !f.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let _ = tx.send(());
        });
        rx.recv_timeout(guard).is_ok()
    }

    #[test]
    fn a_deadline_raises_the_flag_and_says_so() {
        let flag = Arc::new(AtomicBool::new(false));
        let d = arm(&flag, Duration::from_millis(1)).expect("armed");
        assert!(raised_within(&flag, Duration::from_secs(60)));
        assert!(d.fired());
        assert!(d.message().contains("query_timeout"));
    }

    #[test]
    fn a_dropped_deadline_never_fires() {
        let flag = Arc::new(AtomicBool::new(false));
        let d = arm(&flag, Duration::from_secs(3600)).expect("armed");
        let key = d.key;
        drop(d);
        assert!(!timer().lock().deadlines.contains_key(&key), "removed");
        assert!(!flag.load(Ordering::SeqCst));
        assert!(arm(&flag, Duration::ZERO).is_none(), "zero: no deadline");
    }

    #[test]
    fn a_cancel_raised_by_someone_else_is_not_a_timeout() {
        let flag = Arc::new(AtomicBool::new(false));
        let d = arm(&flag, Duration::from_secs(3600)).expect("armed");
        flag.store(true, Ordering::SeqCst);
        assert!(!d.fired());
    }

    #[test]
    fn a_linked_flag_follows_its_parent() {
        let parent = Arc::new(AtomicBool::new(false));
        let (flag, d) = arm_linked(&parent, Duration::from_secs(3600));
        let d = d.expect("armed");
        assert!(!flag.load(Ordering::SeqCst));
        parent.store(true, Ordering::SeqCst);
        assert!(raised_within(&flag, Duration::from_secs(60)));
        assert!(!d.fired(), "raised by the parent, not the deadline");
        drop(d);
        let (same, none) = arm_linked(&parent, Duration::ZERO);
        assert!(Arc::ptr_eq(&same, &parent) && none.is_none());
    }

    /// A timeout too large to be a deadline arms none — and the statement's
    /// flag is then the parent itself, so a shutdown still stops it (it
    /// used to get a fresh flag that nothing linked to the parent).
    #[test]
    fn a_timeout_too_large_for_a_deadline_still_follows_the_parent() {
        let parent = Arc::new(AtomicBool::new(false));
        let (flag, d) = arm_linked(&parent, Duration::MAX);
        assert!(d.is_none(), "no deadline");
        assert!(Arc::ptr_eq(&flag, &parent), "the parent's own flag");
    }

    #[test]
    fn a_linked_flag_fires_at_its_deadline() {
        let parent = Arc::new(AtomicBool::new(false));
        let (flag, d) = arm_linked(&parent, Duration::from_millis(1));
        let d = d.expect("armed");
        assert!(raised_within(&flag, Duration::from_secs(60)));
        assert!(d.fired());
        assert!(!parent.load(Ordering::SeqCst), "the parent is left alone");
    }
}
