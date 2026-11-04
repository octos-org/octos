//! Bounded synchronous subprocess execution.
//!
//! R-17: external renderers (pdftoppm) were waited on with bare `.output()`,
//! so a stuck renderer parked the blocking thread forever. This runner polls
//! the child and, past the deadline, kills and reaps it.

use std::fmt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Deadline for external document renderers (pdftoppm). R-17: rendering a
/// bounded page range at fixed DPI takes seconds; two minutes covers slow
/// machines while guaranteeing a stuck renderer is killed instead of parking
/// the caller's blocking thread forever.
pub const RENDERER_DEADLINE: Duration = Duration::from_secs(120);

/// Failure modes of [`run_with_deadline`]. `Io` preserves the original
/// `io::Error` so callers can still special-case `NotFound` (missing binary).
#[derive(Debug)]
pub enum BoundedRunError {
    Io(std::io::Error),
    TimedOut { program: String, timeout: Duration },
}

impl fmt::Display for BoundedRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoundedRunError::Io(e) => write!(f, "{e}"),
            BoundedRunError::TimedOut { program, timeout } => {
                write!(f, "{program} timed out after {}s", timeout.as_secs())
            }
        }
    }
}

impl std::error::Error for BoundedRunError {}

/// Spawn `cmd` and wait at most `timeout`; past the deadline the child is
/// killed and reaped instead of being left running. Stdout is discarded;
/// stderr is captured for diagnostics (the callers' renderers write little
/// stderr — a pathological child that fills the pipe simply blocks until the
/// deadline kills it, which is still bounded).
pub fn run_with_deadline(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>), BoundedRunError> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(BoundedRunError::Io)?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_end(&mut stderr);
                }
                return Ok((status, stderr));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BoundedRunError::TimedOut {
                        program: cmd.get_program().to_string_lossy().into_owned(),
                        timeout,
                    });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BoundedRunError::Io(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-17: a stuck renderer must be killed past the deadline, not waited
    /// on forever.
    #[cfg(unix)]
    #[test]
    fn run_with_deadline_kills_stuck_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");

        let start = Instant::now();
        let error = run_with_deadline(&mut cmd, Duration::from_millis(200))
            .expect_err("a 30s sleep must not survive a 200ms deadline");

        assert!(
            matches!(error, BoundedRunError::TimedOut { .. }),
            "unexpected error: {error}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "deadline must kill the child promptly, took {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_deadline_returns_status_and_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo boom >&2; exit 3"]);

        let (status, stderr) =
            run_with_deadline(&mut cmd, Duration::from_secs(5)).expect("fast command must succeed");

        assert_eq!(status.code(), Some(3));
        assert!(String::from_utf8_lossy(&stderr).contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_deadline_preserves_not_found_kind() {
        let mut cmd = Command::new("definitely-not-a-real-binary-r17");
        let error = run_with_deadline(&mut cmd, Duration::from_secs(1))
            .expect_err("missing binary must surface the spawn error");
        match error {
            BoundedRunError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io error, got {other}"),
        }
    }
}
