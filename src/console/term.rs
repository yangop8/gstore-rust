//! Terminal plumbing for the console, without extra crates: whether stdin
//! is a terminal, Ctrl-C (`SIGINT`) as a counter instead of process death,
//! reading lines from stdin so that a Ctrl-C interrupts the read, and
//! turning the terminal's echo off for passwords (upstream's
//! `HideStdinDisplay`).
//!
//! Ctrl-C: [`install_sigint`] replaces `SIGINT`'s disposition with a handler
//! that only bumps an atomic counter (async-signal-safe), and asks the C
//! library not to restart system calls it interrupts (`siginterrupt`), so a
//! blocking `read` of stdin returns `EINTR`: [`LineReader`] reports that as
//! [`ReadOutcome::Interrupted`]. Code waiting on something else (a request
//! to the server) polls [`interrupts`].

use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

static INTERRUPTS: AtomicU64 = AtomicU64::new(0);

/// How many Ctrl-Cs the process has received since [`install_sigint`].
pub fn interrupts() -> u64 {
    INTERRUPTS.load(Ordering::SeqCst)
}

/// Count a Ctrl-C as if one had arrived (tests, and editors that read the
/// key themselves).
pub fn note_interrupt() {
    INTERRUPTS.fetch_add(1, Ordering::SeqCst);
}

/// Write `text` to stdout and flush it, ignoring errors: a closed pipe
/// (`gconsole … | head`) must not end the console with a panic.
pub fn show(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Whether stdin is a terminal.
pub fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

/// Whether stdout is a terminal.
pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Make Ctrl-C bump [`interrupts`] instead of ending the process, and
/// interrupt blocking reads (see the module docs). `false` (and nothing
/// changed) where unsupported or on failure.
pub fn install_sigint() -> bool {
    #[cfg(unix)]
    {
        sig::install()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
mod sig {
    use std::os::raw::c_int;

    const SIGINT: c_int = 2;
    const SIG_ERR: usize = usize::MAX;

    extern "C" {
        /// `sighandler_t signal(int, sighandler_t)`, the handler passed as a
        /// pointer-sized integer.
        fn signal(signum: c_int, handler: usize) -> usize;
        /// `int siginterrupt(int sig, int flag)`: with `flag` 1, system
        /// calls interrupted by `sig` fail with `EINTR` instead of being
        /// restarted.
        fn siginterrupt(sig: c_int, flag: c_int) -> c_int;
    }

    extern "C" fn on_sigint(_signum: c_int) {
        super::INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn install() -> bool {
        let handler = on_sigint as extern "C" fn(c_int) as usize;
        // SAFETY: both are the C library's; `on_sigint` has the C handler
        // signature and only performs a lock-free atomic add.
        unsafe { signal(SIGINT, handler) != SIG_ERR && siginterrupt(SIGINT, 1) == 0 }
    }
}

/// What a [`LineReader::read_line`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// A line, without its end-of-line characters.
    Line(String),
    /// Ctrl-C while waiting (the partial line, if any, is dropped).
    Interrupted,
    /// End of input (Ctrl-D on an empty line, or the end of a script).
    Eof,
}

/// Lines from stdin, one `read` at a time so that a Ctrl-C (with
/// [`install_sigint`]) interrupts the wait. Every read of stdin in the
/// console goes through one reader, so no input is lost to a buffer.
#[derive(Debug, Default)]
pub struct LineReader {
    buf: Vec<u8>,
    eof: bool,
}

impl LineReader {
    pub fn new() -> LineReader {
        LineReader::default()
    }

    /// The next line of stdin.
    pub fn read_line(&mut self) -> ReadOutcome {
        let mark = interrupts();
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return ReadOutcome::Line(String::from_utf8_lossy(&line).into_owned());
            }
            if self.eof {
                if self.buf.is_empty() {
                    return ReadOutcome::Eof;
                }
                let line = std::mem::take(&mut self.buf);
                return ReadOutcome::Line(String::from_utf8_lossy(&line).into_owned());
            }
            let mut chunk = [0u8; 8192];
            match io::stdin().read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    if interrupts() != mark {
                        // The terminal discards the line being typed.
                        self.buf.clear();
                        return ReadOutcome::Interrupted;
                    }
                }
                Err(_) => self.eof = true,
            }
        }
    }
}

/// Terminal echo off while alive (upstream `HideStdinDisplay`); a no-op
/// when stdin is not a terminal.
pub struct EchoOff {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    saved: Option<termios::Termios>,
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    stty: bool,
}

impl EchoOff {
    pub fn new() -> EchoOff {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            EchoOff {
                saved: if stdin_is_tty() {
                    termios::echo_off()
                } else {
                    None
                },
            }
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            let stty = stdin_is_tty()
                && std::process::Command::new("stty")
                    .arg("-echo")
                    .stdin(std::process::Stdio::inherit())
                    .status()
                    .is_ok_and(|s| s.success());
            EchoOff { stty }
        }
        #[cfg(not(unix))]
        {
            EchoOff {}
        }
    }
}

impl Default for EchoOff {
    fn default() -> Self {
        EchoOff::new()
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(saved) = &self.saved {
            termios::restore(saved);
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        if self.stty {
            let _ = std::process::Command::new("stty")
                .arg("echo")
                .stdin(std::process::Stdio::inherit())
                .status();
        }
    }
}

/// `tcgetattr` / `tcsetattr` with Linux's `struct termios` layout.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod termios {
    use std::os::raw::{c_int, c_uint};

    /// Linux's `struct termios` as glibc and musl expose it: four
    /// `tcflag_t`, `c_line`, `c_cc[32]`, two speeds. Only `c_lflag` is
    /// touched — at offset 12 with `ECHO` = `0o10` on every Linux
    /// architecture —, and spare room follows, so an architecture whose
    /// struct is laid out differently or larger is still safe.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        c_iflag: c_uint,
        c_oflag: c_uint,
        c_cflag: c_uint,
        c_lflag: c_uint,
        c_line: u8,
        c_cc: [u8; 32],
        c_ispeed: c_uint,
        c_ospeed: c_uint,
        _spare: [u8; 64],
    }

    const ECHO: c_uint = 0o10;
    const TCSANOW: c_int = 0;

    extern "C" {
        fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
        fn tcsetattr(fd: c_int, optional_actions: c_int, termios: *const Termios) -> c_int;
    }

    /// Turn echo off on stdin; the previous settings, to restore.
    pub fn echo_off() -> Option<Termios> {
        let mut t = Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 32],
            c_ispeed: 0,
            c_ospeed: 0,
            _spare: [0; 64],
        };
        // SAFETY: `t` is a valid, writable `struct termios`.
        if unsafe { tcgetattr(0, &mut t) } != 0 {
            return None;
        }
        let saved = t;
        t.c_lflag &= !ECHO;
        // SAFETY: `t` is a valid `struct termios` read by `tcgetattr`.
        if unsafe { tcsetattr(0, TCSANOW, &t) } != 0 {
            return None;
        }
        Some(saved)
    }

    /// Put back settings [`echo_off`] saved.
    pub fn restore(saved: &Termios) {
        // SAFETY: `saved` is a valid `struct termios` read by `tcgetattr`.
        unsafe {
            tcsetattr(0, TCSANOW, saved);
        }
    }
}
