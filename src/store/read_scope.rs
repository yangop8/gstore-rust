//! Per-query read errors (contract C2).
//!
//! The [`TripleSource`](super::TripleSource) read methods return plain lists:
//! they cannot carry an I/O error. A backend that can fail on a read (an
//! on-disk store, an out-of-core dictionary, RocksDB) reports the failure with
//! [`note_read_error`] instead, and the statement that caused the read learns
//! of it through the [`ReadScope`] it runs in:
//!
//! ```
//! use gstore::store::read_scope::{note_read_error, ReadScope};
//!
//! let scope = ReadScope::enter();
//! // ... evaluate a query; a failing backend calls:
//! note_read_error("page 7: checksum mismatch");
//! // ... and the statement fails instead of returning a partial answer:
//! assert_eq!(scope.take_error().as_deref(), Some("page 7: checksum mismatch"));
//! ```
//!
//! The rules:
//!
//! * A scope is per thread and **nestable**: an error is recorded in the
//!   innermost scope entered on the current thread. The guard is `!Send`, so
//!   it cannot leave that thread.
//! * Every query, UPDATE WHERE evaluation, CSR build, export and snapshot query
//!   runs inside a scope; an error recorded there makes the statement fail,
//!   and its result is never cached.
//! * With no scope active the error is counted in a process-wide counter
//!   ([`unscoped_read_errors`]) and logged.
//! * Backends only ever *report* through [`note_read_error`]; they never
//!   clear a shared error slot. One statement's failure therefore never
//!   leaks into another statement, on this thread or any other.
//! * Evaluation is single-threaded. A future parallel evaluator must hand the
//!   scope across threads explicitly (enter a scope on each worker and
//!   forward what it records).

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

/// One scope's record: the first error noted in it, and how many were noted.
#[derive(Default)]
struct Slot {
    first: Option<String>,
    count: u64,
}

thread_local! {
    /// The scopes entered on this thread, innermost last.
    static SCOPES: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
}

/// Read errors noted while no scope was active on the noting thread.
static UNSCOPED: AtomicU64 = AtomicU64::new(0);

/// A per-thread read-error scope (see the module docs). Created by
/// [`enter`](Self::enter); leaving it (dropping the guard) discards whatever
/// it recorded and was not [taken](Self::take_error).
#[must_use = "a read scope records errors only while the guard is alive"]
pub struct ReadScope {
    /// This scope's position in the thread's stack.
    depth: usize,
    /// `!Send` and `!Sync`: the scope belongs to the thread that entered it.
    _not_send: PhantomData<*const ()>,
}

impl ReadScope {
    /// Enter a new innermost scope on the current thread.
    pub fn enter() -> ReadScope {
        let depth = SCOPES.with(|s| {
            let mut s = s.borrow_mut();
            s.push(Slot::default());
            s.len() - 1
        });
        ReadScope {
            depth,
            _not_send: PhantomData,
        }
    }

    /// Take the first error recorded in this scope (it is cleared, so a
    /// second call returns `None` unless another error was noted since).
    pub fn take_error(&self) -> Option<String> {
        SCOPES.with(|s| {
            let mut s = s.borrow_mut();
            let slot = s.get_mut(self.depth)?;
            slot.count = 0;
            slot.first.take()
        })
    }

    /// Whether an error has been recorded in this scope (and not taken).
    pub fn has_error(&self) -> bool {
        SCOPES.with(|s| {
            s.borrow()
                .get(self.depth)
                .is_some_and(|slot| slot.count > 0)
        })
    }

    /// How many errors were recorded in this scope since it was entered or
    /// last [taken](Self::take_error).
    pub fn error_count(&self) -> u64 {
        SCOPES.with(|s| s.borrow().get(self.depth).map_or(0, |slot| slot.count))
    }
}

impl Drop for ReadScope {
    fn drop(&mut self) {
        // Pop this scope (and any inner one whose guard was leaked).
        let _ = SCOPES.try_with(|s| s.borrow_mut().truncate(self.depth));
    }
}

impl std::fmt::Debug for ReadScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadScope")
            .field("depth", &self.depth)
            .field("errors", &self.error_count())
            .finish()
    }
}

/// Report a read error. It is recorded in the innermost [`ReadScope`] of the
/// current thread (the first message is kept, later ones are counted); with
/// no scope active it bumps [`unscoped_read_errors`] and is logged.
pub fn note_read_error(msg: impl std::fmt::Display) {
    let scoped = SCOPES
        .try_with(|s| {
            let mut s = s.borrow_mut();
            match s.last_mut() {
                Some(slot) => {
                    if slot.first.is_none() {
                        slot.first = Some(msg.to_string());
                    }
                    slot.count += 1;
                    true
                }
                None => false,
            }
        })
        .unwrap_or(false);
    if !scoped {
        UNSCOPED.fetch_add(1, Ordering::Relaxed);
        crate::gs_error!("read error outside a statement: {msg}");
    }
}

/// The number of read errors noted, process-wide, while no scope was active
/// on the noting thread.
pub fn unscoped_read_errors() -> u64 {
    UNSCOPED.load(Ordering::Relaxed)
}

/// Whether a scope is active on the current thread.
pub fn in_scope() -> bool {
    SCOPES.try_with(|s| !s.borrow().is_empty()).unwrap_or(false)
}

/// Run `f` (an existence check) in a scope of its own: if it recorded a read
/// error, answer `true` — never "absent" when in doubt — and report the error
/// again to the enclosing scope, so the statement still fails.
pub(crate) fn true_on_read_error(f: impl FnOnce() -> bool) -> bool {
    let scope = ReadScope::enter();
    let answer = f();
    let error = scope.take_error();
    drop(scope);
    match error {
        Some(e) => {
            note_read_error(e);
            true
        }
        None => answer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn innermost_scope_records_and_take_clears() {
        let outer = ReadScope::enter();
        {
            let inner = ReadScope::enter();
            note_read_error("inner");
            note_read_error("second");
            assert_eq!(inner.error_count(), 2);
            assert!(!outer.has_error());
            assert_eq!(inner.take_error().as_deref(), Some("inner"));
            assert_eq!(inner.take_error(), None);
        }
        note_read_error("outer");
        assert_eq!(outer.take_error().as_deref(), Some("outer"));
    }

    #[test]
    fn leaked_inner_guard_is_popped_by_the_outer_one() {
        let outer = ReadScope::enter();
        std::mem::forget(ReadScope::enter());
        drop(outer);
        assert!(!in_scope());
    }

    #[test]
    fn existence_checks_answer_true_on_error_and_forward_it() {
        let scope = ReadScope::enter();
        assert!(!true_on_read_error(|| false));
        assert!(true_on_read_error(|| {
            note_read_error("disk");
            false
        }));
        assert_eq!(scope.take_error().as_deref(), Some("disk"));
    }
}
