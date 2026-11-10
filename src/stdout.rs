//! Standard output for the command-line tools, safe on a closed pipe.
//!
//! `println!` panics when standard output is a pipe whose reader has exited
//! (`gserver -S | head -1`, `gshow | head -2`): Rust ignores `SIGPIPE`, so
//! the write fails with `EPIPE` and the macro turns that error into a panic.
//! The tools print with [`outln!`](crate::outln) and [`out!`](crate::out)
//! instead. The first broken-pipe error marks standard output closed: nothing
//! more is written (later calls return at once, without formatting), and the
//! tool finishes its work and exits with its usual status, without a panic
//! message. Finishing rather than exiting at once means a reader going away
//! never cuts a mutating tool (`gbuild`, `gadd`, …) or a running server off
//! halfway.
//!
//! Any other write error still panics, exactly as `println!` does.

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static CLOSED: AtomicBool = AtomicBool::new(false);

/// Whether a write found standard output's reader gone (see the module docs).
pub fn is_closed() -> bool {
    CLOSED.load(Ordering::Relaxed)
}

/// Write `args` to standard output; what [`outln!`](crate::outln) and
/// [`out!`](crate::out) expand to.
pub fn print(args: fmt::Arguments<'_>) {
    if is_closed() {
        return;
    }
    if let Err(e) = io::stdout().lock().write_fmt(args) {
        if e.kind() == io::ErrorKind::BrokenPipe {
            CLOSED.store(true, Ordering::Relaxed);
        } else {
            panic!("failed printing to stdout: {e}");
        }
    }
}

/// For output a tool streams to standard output through [`Write`] itself:
/// a broken pipe marks standard output closed and counts as success; any
/// other result is returned unchanged.
pub fn quiet_broken_pipe(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            CLOSED.store(true, Ordering::Relaxed);
            Ok(())
        }
        other => other,
    }
}

/// `println!` for the command-line tools: a closed pipe ends the output
/// quietly instead of panicking (see [`crate::stdout`]).
#[macro_export]
macro_rules! outln {
    () => {
        $crate::stdout::print(::std::format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::stdout::print(::std::format_args!("{}\n", ::std::format_args!($($arg)*)))
    };
}

/// `print!` for the command-line tools (see [`outln!`](crate::outln)).
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::stdout::print(::std::format_args!($($arg)*))
    };
}
