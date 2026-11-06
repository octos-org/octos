//! Bounded capture of child-process output.
//!
//! R-23: hooks and plugins collected stdout/stderr with unbounded
//! `read_to_end` and only truncated for display afterwards, so a misbehaving
//! child could exhaust process memory within the (time-only) timeout. This
//! helper caps what is retained while still draining the pipe to EOF — the
//! drain keeps the child unblocked on its side of the pipe so a flood exits
//! promptly instead of wedging until the outer timeout.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Drain a child's stdout/stderr concurrently with `wait`, each stream
/// capped at `cap` bytes (R-23). Returns `(status, stdout, stdout_overflow,
/// stderr, stderr_overflow)`; read errors degrade to empty non-overflow
/// captures exactly like the pre-cap `let _ = read_to_end(...)` handling.
pub async fn drain_child_capped(
    child: &mut tokio::process::Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    cap: usize,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>, bool, Vec<u8>, bool)> {
    async fn capped<R: AsyncRead + Unpin>(handle: Option<R>, cap: usize) -> (Vec<u8>, bool) {
        match handle {
            Some(mut h) => read_to_end_capped(&mut h, cap)
                .await
                .unwrap_or_else(|_| (Vec::new(), false)),
            None => (Vec::new(), false),
        }
    }
    let stdout_read = capped(stdout, cap);
    let stderr_read = capped(stderr, cap);
    let (status, (out_buf, out_overflow), (err_buf, err_overflow)) =
        tokio::join!(child.wait(), stdout_read, stderr_read);
    status.map(|status| (status, out_buf, out_overflow, err_buf, err_overflow))
}

/// Cap on retained hook stdout/stderr (R-23). Hook diagnostics surface to
/// the model after a 2000-char display truncation, so 256 KiB retained is
/// already far beyond any useful payload while bounding memory.
pub const HOOK_CAPTURE_CAP_BYTES: usize = 256 * 1024;

/// Cap on retained plugin stdout (the parsed JSON result) and stderr
/// (progress/error text) (R-23). Plugin results legitimately carry larger
/// payloads than hooks, so stdout gets 4 MiB; stderr is progress chatter and
/// gets 256 KiB.
pub const PLUGIN_STDOUT_CAP_BYTES: usize = 4 * 1024 * 1024;
pub const PLUGIN_STDERR_CAP_BYTES: usize = 256 * 1024;

/// Read `handle` to EOF, retaining at most `cap` bytes. Returns the retained
/// prefix and whether the stream overflowed the cap. The pipe is ALWAYS
/// drained to EOF even after overflow: stopping early would leave the child
/// blocked on a full pipe and defer the failure to the outer timeout.
pub async fn read_to_end_capped<R: AsyncRead + Unpin>(
    handle: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut overflowed = false;
    loop {
        let n = handle.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let room = cap.saturating_sub(buf.len());
        if room > 0 {
            buf.extend_from_slice(&chunk[..n.min(room)]);
        }
        if n > room {
            overflowed = true;
        }
    }
    Ok((buf, overflowed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// R-23: streams past the cap are truncated to the cap and flagged, and
    /// the pipe is still fully drained (a writer must not block).
    #[tokio::test]
    async fn read_to_end_capped_truncates_and_drains_overflow() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; 10_000])
                .await
                .expect("writer must never block past the cap");
        });
        let (buf, overflowed) = read_to_end_capped(&mut reader, 2048)
            .await
            .expect("read succeeds");
        write.await.expect("writer task joins");
        assert!(overflowed);
        assert_eq!(buf.len(), 2048);
    }

    /// R-23: an in-cap stream is retained whole and not flagged.
    #[tokio::test]
    async fn read_to_end_capped_keeps_in_cap_stream() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            writer.write_all(b"hello\nworld\n").await.unwrap();
        });
        let (buf, overflowed) = read_to_end_capped(&mut reader, 2048)
            .await
            .expect("read succeeds");
        assert!(!overflowed);
        assert_eq!(buf, b"hello\nworld\n");
    }

    /// R-23: a stream exactly at the cap is NOT overflow.
    #[tokio::test]
    async fn read_to_end_capped_exact_cap_is_not_overflow() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let payload = vec![b'y'; 2048];
        let expected = payload.clone();
        tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
        });
        let (buf, overflowed) = read_to_end_capped(&mut reader, 2048)
            .await
            .expect("read succeeds");
        assert!(!overflowed);
        assert_eq!(buf, expected);
    }
}
