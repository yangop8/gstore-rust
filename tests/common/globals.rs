//! Guards for process-wide state a test installs (review tests-b#16).
//!
//! A test that installs the diagnostic logger ([`gstore::logging::init`]) or
//! the default open options ([`gstore::db::set_default_open_options`]) and
//! puts them back only on its last line leaves them installed when an
//! assertion fails half-way: every other test of the binary then logs into a
//! removed scratch directory, or opens databases with the wrong mode. These
//! guards put the defaults back when dropped — also while a failing test
//! unwinds.

use gstore::db::{set_default_open_options, OpenOptions};

/// Resets the process-wide diagnostic logger ([`gstore::logging::reset`]:
/// standard error at `info`, no file) when dropped: `let _log =
/// LoggerReset;` before the test installs one.
pub struct LoggerReset;

impl Drop for LoggerReset {
    fn drop(&mut self) {
        gstore::logging::reset();
    }
}

/// Installs default open options and puts the built-in defaults
/// ([`OpenOptions::default`]) back when dropped.
#[must_use = "the defaults are restored when the guard is dropped"]
pub struct DefaultOpenOptions;

impl DefaultOpenOptions {
    /// Install `options` as the process's defaults until the guard is dropped.
    pub fn install(options: OpenOptions) -> DefaultOpenOptions {
        set_default_open_options(options);
        DefaultOpenOptions
    }
}

impl Drop for DefaultOpenOptions {
    fn drop(&mut self) {
        set_default_open_options(OpenOptions::default());
    }
}
