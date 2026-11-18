//! Helpers shared by the integration tests (review tests-a#16, tests-a#8,
//! tests-b#11, tests-b#16): one copy of what about 45 test files used to
//! copy-paste, with the conventions the copies had drifted apart on fixed
//! once.
//!
//! * [`tmp`]: unique temporary paths (`<temp_dir>/gstore_<tag>_<pid>_<n>`,
//!   never a fixed name two concurrent runs could share), a [`tmp::TempDir`]
//!   removed when dropped and the thread-scoped registry the `TmpDirs` copies
//!   implemented — both remove their paths also when the test panics.
//! * [`http`]: an HTTP/1.1 client whose every request has one overall
//!   deadline (connect, write and read): a server that hangs fails the test
//!   with the request line instead of stalling the suite until the CI job's
//!   timeout.
//! * [`server`]: an in-process server running on its own thread, shut down
//!   and joined when its guard is dropped (within a bound: a server that does
//!   not stop fails the test instead of hanging it).
//! * [`globals`]: guards that put process-wide state (the diagnostic logger,
//!   the default open options) back when dropped, also on failure.
//!
//! `tests/test_helpers_fail_instead_of_hanging.rs` checks the helpers
//! themselves (a deliberately stuck server and a silent peer).
//!
//! Include it with `mod common;` (Cargo does not build `tests/common/` as a
//! test target of its own).

#![allow(dead_code)] // each test target uses a subset

pub mod globals;
pub mod http;
pub mod server;
pub mod tmp;
