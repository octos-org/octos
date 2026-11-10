//! Shared media download helper for channels.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use futures::StreamExt;
use reqwest::Client;
use tracing::debug;

/// Default cap on a single inbound media download (50 MiB). A malicious or
/// misbehaving homeserver could otherwise stream an unbounded body and
/// exhaust memory/disk. Overridable via `OCTOS_MAX_MEDIA_BYTES`.
pub const DEFAULT_MAX_MEDIA_BYTES: u64 = 50 * 1024 * 1024;

/// Whole-download deadline: fits the default cap on a link of about 1 Mbit/s.
const MEDIA_DOWNLOAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// Resolve the inbound-media download cap, honoring `OCTOS_MAX_MEDIA_BYTES`.
pub fn max_media_bytes() -> u64 {
    std::env::var("OCTOS_MAX_MEDIA_BYTES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MAX_MEDIA_BYTES)
}

/// Download a file from a URL to the media directory.
/// Returns the absolute path of the saved file.
///
/// The body is bounded by [`max_media_bytes`]: a `Content-Length` over the cap
/// is rejected before reading, and the streamed body is aborted (and the
/// partial file removed) if it exceeds the cap mid-stream. This keeps an
/// untrusted/oversized response from exhausting memory or disk.
pub async fn download_media(
    client: &Client,
    url: &str,
    headers: &[(&str, &str)],
    dest_dir: &Path,
    filename: &str,
) -> Result<PathBuf> {
    download_media_with_cap(client, url, headers, dest_dir, filename, max_media_bytes()).await
}

/// Create a media directory owner-only (0700 on Unix): inbound attachments
/// are private session data, so the default (0777 & ~umask) would expose
/// them to every local user. A pre-existing directory with loose group/other
/// bits (e.g. from an older release) is tightened on the way through; that
/// tightening is best-effort, since a chmod failure must not turn a usable
/// download into a hard error. A directory owned by another account is
/// refused outright: at 0700 the mode check alone passes, yet every byte
/// written there would land in the other owner's hands. The owner check is
/// Unix-only (the non-Unix target has no uid equivalent and just creates the
/// directory). (#2426, #2802)
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .wrap_err_with(|| format!("failed to create private dir: {}", path.display()))?;
        let euid = rustix::process::geteuid().as_raw();
        // The final component is judged without following symlinks: a link
        // planted by another user must be refused by its own ownership, not
        // the target's (#2802).
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            check_private_dir_owner(path, &meta, euid)?;
        }
        if let Ok(meta) = std::fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)
        .wrap_err_with(|| format!("failed to create private dir: {}", path.display()))
}

/// The ownership half of [`ensure_private_dir`]: refuse a directory that
/// belongs to another account, whatever its mode bits. Split out as a pure
/// decision so the refusal (unreachable from an unprivileged test fixture)
/// still gets a regression pin. (#2802)
#[cfg(unix)]
fn check_private_dir_owner(path: &Path, meta: &std::fs::Metadata, euid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if meta.uid() != euid {
        eyre::bail!(
            "private dir {} is owned by uid {}, but this process runs as uid {euid}; \
             refusing to write private data into a directory we do not control",
            path.display(),
            meta.uid(),
        );
    }
    Ok(())
}

/// [`download_media`] with an explicit byte cap (for deterministic testing).
pub async fn download_media_with_cap(
    client: &Client,
    url: &str,
    headers: &[(&str, &str)],
    dest_dir: &Path,
    filename: &str,
    cap: u64,
) -> Result<PathBuf> {
    ensure_private_dir(dest_dir)?;

    let dest = dest_dir.join(filename);

    // Overrides the client's whole-request cap; stalls still hit its read timeout.
    let mut req = client.get(url).timeout(MEDIA_DOWNLOAD_DEADLINE);
    for &(key, value) in headers {
        req = req.header(key, value);
    }

    // The byte cap is not a time bound: a peer that accepts the connection and
    // then stalls (or drips bytes slowly) must not hold the inbound pipeline
    // forever (R-14). One deadline covers connect, headers, and body.
    let download = async {
        let response = req.send().await.map_err(|error| {
            if error.is_timeout() {
                eyre::eyre!("download timed out: {url}")
            } else {
                eyre::Report::new(error).wrap_err(format!("failed to download: {url}"))
            }
        })?;

        if !response.status().is_success() {
            eyre::bail!("download failed (HTTP {}): {url}", response.status());
        }

        // Fast reject when the server advertises an oversized body.
        if let Some(len) = response.content_length()
            && len > cap
        {
            eyre::bail!("media exceeds max size ({len} bytes > {cap} byte cap): {url}");
        }

        // Stream the body, enforcing the cap even when Content-Length is absent or
        // lies. Accumulate in memory up to the cap, then write once.
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.wrap_err("failed to read download body")?;
            if buf.len() as u64 + chunk.len() as u64 > cap {
                eyre::bail!("media exceeds max size ({cap} byte cap): {url}");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    };

    let buf = match tokio::time::timeout(MEDIA_DOWNLOAD_DEADLINE, download).await {
        Ok(result) => result?,
        Err(_) => eyre::bail!(
            "download timed out after {}s: {url}",
            MEDIA_DOWNLOAD_DEADLINE.as_secs()
        ),
    };

    std::fs::write(&dest, &buf).wrap_err_with(|| format!("failed to write: {}", dest.display()))?;

    debug!(path = %dest.display(), bytes = buf.len(), "media downloaded");
    Ok(dest)
}

/// Check if a file path looks like an audio file.
pub fn is_audio(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".ogg")
        || lower.ends_with(".mp3")
        || lower.ends_with(".m4a")
        || lower.ends_with(".wav")
        || lower.ends_with(".oga")
        || lower.ends_with(".opus")
        || lower.ends_with(".flac")
        || lower.ends_with(".amr")
}

pub use octos_core::media::is_image;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn download_media_with_cap_rejects_oversized_body() {
        use axum::Router;
        use axum::routing::get;

        // Mock server returns a 1 KiB body with no/large Content-Length.
        let app = Router::new().route("/big", get(|| async { vec![0u8; 1024] }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::TempDir::new().unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/big");

        // cap = 100 bytes, body = 1024 → must be rejected, no file written.
        let result = download_media_with_cap(&client, &url, &[], dir.path(), "big.bin", 100).await;
        assert!(result.is_err(), "oversized download should be rejected");
        assert!(!dir.path().join("big.bin").exists());

        // cap = 4096 bytes → same body now fits.
        let ok = download_media_with_cap(&client, &url, &[], dir.path(), "ok.bin", 4096).await;
        assert!(ok.is_ok(), "within-cap download should succeed: {ok:?}");
        assert_eq!(std::fs::metadata(ok.unwrap()).unwrap().len(), 1024);

        handle.abort();
    }

    /// A server that accepts the connection but never responds must not hold
    /// the download forever: the size cap is not a time bound (R-14).
    #[tokio::test(start_paused = true)]
    async fn download_media_with_cap_fails_when_server_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let dir = tempfile::TempDir::new().unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/stall");

        let fut = download_media_with_cap(&client, &url, &[], dir.path(), "s.bin", 4096);
        tokio::pin!(fut);
        let advance = tokio::spawn(tokio::time::advance(std::time::Duration::from_secs(1800)));
        let result = tokio::time::timeout(std::time::Duration::from_secs(1800), &mut fut).await;
        advance.await.unwrap();

        handle.abort();
        let err = result
            .expect("download must eventually produce an outcome, not hang forever")
            .expect_err("stalled server must produce an error, not success");
        assert!(
            err.to_string().contains("timed out"),
            "error must name the timeout: {err}"
        );
    }
}
