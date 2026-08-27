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

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipeWriter;

    impl io::Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct OtherErrorWriter;

    impl io::Write for OtherErrorWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn serve_console_write_line_broken_pipe_returns_ok() {
        let mut w = BrokenPipeWriter;
        let result = write_line(&mut w, "test");
        assert!(result.is_ok());
    }

    #[test]
    fn serve_console_write_line_other_error_returns_ok() {
        let mut w = OtherErrorWriter;
        let result = write_line(&mut w, "test");
        assert!(result.is_ok());
    }

    #[test]
    fn serve_console_write_line_normal_writer_verbatim() {
        let mut buf: Vec<u8> = Vec::new();
        let result = write_line(&mut buf, "hello");
        assert!(result.is_ok());
        assert_eq!(buf, b"hello\n");
    }

    #[test]
    fn serve_console_print_stdout_delegates_to_write_line() {
        // print_stdout is a thin wrapper around write_line with stdout lock.
        // We verify it returns Ok and does not panic under normal conditions.
        let result = print_stdout("test");
        assert!(result.is_ok());
    }
}
