//! A test hook (not for production use): park a server's background thread
//! right before it exits, and observe whoever joins it — so a test can
//! prove that a shutdown *joins* the thread (it waits for the parked
//! thread, and says so through [`ExitPark::wait_joining`]) instead of
//! leaving it running past `serve_forever`, deterministically: a thread
//! left detached is still parked when `serve_forever` returns.
//!
//! The threads that call `exiting` as their last step: the
//! idle-transaction reaper (`gstore-txn-reaper`), the asynchronous
//! operations' runners (`gstore-task`) and callback threads
//! (`gstore-callback`), and the background statistics refreshes
//! (`gstore-stats-refresh`). Their joiners call `joining` right before
//! they join. With no park armed both cost one atomic load.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

#[derive(Default)]
struct Gate {
    /// (threads parked at their exit, joins announced, released)
    state: Mutex<(u64, u64, bool)>,
    changed: Condvar,
}

/// The armed parks: thread-name prefix and gate.
static PARKS: Mutex<Vec<(String, Arc<Gate>)>> = Mutex::new(Vec::new());
static ARMED: AtomicBool = AtomicBool::new(false);

/// An armed exit park: every thread whose name starts with its prefix parks
/// right before it exits, until [`release`](Self::release)d (or the park is
/// dropped).
pub struct ExitPark {
    prefix: String,
    gate: Arc<Gate>,
}

/// Park the threads whose name starts with `prefix` at their exit.
pub fn arm(prefix: &str) -> ExitPark {
    let gate = Arc::new(Gate::default());
    let mut parks = PARKS.lock().unwrap_or_else(PoisonError::into_inner);
    parks.push((prefix.to_string(), Arc::clone(&gate)));
    ARMED.store(true, Ordering::SeqCst);
    ExitPark {
        prefix: prefix.to_string(),
        gate,
    }
}

impl ExitPark {
    fn wait_for(&self, hang_guard: Duration, done: impl Fn(&(u64, u64, bool)) -> bool) -> bool {
        let g = self
            .gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (g, _) = self
            .gate
            .changed
            .wait_timeout_while(g, hang_guard, |s| !done(s))
            .unwrap_or_else(PoisonError::into_inner);
        done(&g)
    }

    /// Wait (at most `hang_guard`) until a thread is parked at its exit.
    pub fn wait_parked(&self, hang_guard: Duration) -> bool {
        self.wait_for(hang_guard, |s| s.0 > 0)
    }

    /// Wait (at most `hang_guard`) until someone is about to join a thread
    /// this park holds.
    pub fn wait_joining(&self, hang_guard: Duration) -> bool {
        self.wait_for(hang_guard, |s| s.1 > 0)
    }

    /// Let the parked threads (and every later one) exit.
    pub fn release(&self) {
        let mut g = self
            .gate
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        g.2 = true;
        self.gate.changed.notify_all();
    }
}

impl Drop for ExitPark {
    fn drop(&mut self) {
        self.release();
        let mut parks = PARKS.lock().unwrap_or_else(PoisonError::into_inner);
        parks.retain(|(p, g)| !(p == &self.prefix && Arc::ptr_eq(g, &self.gate)));
        if parks.is_empty() {
            ARMED.store(false, Ordering::SeqCst);
        }
    }
}

/// The gate armed for a thread named `name`, if any.
fn gate_for(name: Option<&str>) -> Option<Arc<Gate>> {
    if !ARMED.load(Ordering::SeqCst) {
        return None;
    }
    let name = name?;
    let parks = PARKS.lock().unwrap_or_else(PoisonError::into_inner);
    parks
        .iter()
        .find(|(p, _)| name.starts_with(p.as_str()))
        .map(|(_, g)| Arc::clone(g))
}

/// The calling thread is about to exit: park here if a test armed its name.
pub(crate) fn exiting() {
    let Some(gate) = gate_for(std::thread::current().name()) else {
        return;
    };
    let mut g = gate.state.lock().unwrap_or_else(PoisonError::into_inner);
    g.0 += 1;
    gate.changed.notify_all();
    while !g.2 {
        g = gate.changed.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
}

/// About to join `thread`: tell a test that armed its name.
pub(crate) fn joining(thread: &std::thread::Thread) {
    if let Some(gate) = gate_for(thread.name()) {
        let mut g = gate.state.lock().unwrap_or_else(PoisonError::into_inner);
        g.1 += 1;
        gate.changed.notify_all();
    }
}
