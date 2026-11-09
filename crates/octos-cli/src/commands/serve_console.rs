//! Fallible console output for long-running `octos serve`.
//!
//! Replaces `println!`/`eprintln!` in the serve startup/shutdown path so that
//! `ErrorKind::BrokenPipe` (observer closed the pipe) does not panic and does
//! not interrupt gateway cleanup. Other I/O errors are downgraded to tracing
//! warnings and never override the shutdown result.

use std::io;

/// Core fallible write: append `msg` + newline to any `io::Write` without
/// panicking. `BrokenPipe` is swallowed (observer left); other errors are
/// logged as warnings. Always returns `Ok(())`.
pub fn write_line(w: &mut impl io::Write, msg: &str) -> io::Result<()> {
    match w
        .write_all(msg.as_bytes())
        .and_then(|()| w.write_all(b"\n"))
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            tracing::debug!("console write: observer closed pipe (BrokenPipe), continuing");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "console write failed");
            Ok(())
        }
    }
}

/// Thin wrapper: write a line to stdout via [`write_line`].
pub fn print_stdout(msg: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write_line(&mut out, msg)
}

/// Thin wrapper: write a line to stderr via [`write_line`].
pub fn print_stderr(msg: &str) -> io::Result<()> {
    let mut out = io::stderr().lock();
    write_line(&mut out, msg)
}
