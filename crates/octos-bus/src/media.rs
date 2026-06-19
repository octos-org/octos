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

/// [`download_media`] with an explicit byte cap (for deterministic testing).
pub async fn download_media_with_cap(
    client: &Client,
    url: &str,
    headers: &[(&str, &str)],
    dest_dir: &Path,
    filename: &str,
    cap: u64,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)
        .wrap_err_with(|| format!("failed to create media dir: {}", dest_dir.display()))?;

    let dest = dest_dir.join(filename);

    let mut req = client.get(url);
    for &(key, value) in headers {
        req = req.header(key, value);
    }

    let response = req
        .send()
        .await
        .wrap_err_with(|| format!("failed to download: {url}"))?;

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

/// Check if a file path looks like an image file.
pub fn is_image(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_max_media_bytes_is_50_mib() {
        assert_eq!(DEFAULT_MAX_MEDIA_BYTES, 50 * 1024 * 1024);
    }

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

    #[test]
    fn test_is_audio_supported_extensions() {
        assert!(is_audio("voice.ogg"));
        assert!(is_audio("song.mp3"));
        assert!(is_audio("memo.m4a"));
        assert!(is_audio("sound.wav"));
        assert!(is_audio("clip.oga"));
        assert!(is_audio("voice.opus"));
    }

    #[test]
    fn test_is_audio_case_insensitive() {
        assert!(is_audio("file.MP3"));
        assert!(is_audio("file.Wav"));
        assert!(is_audio("file.OGG"));
    }

    #[test]
    fn test_is_audio_rejects_non_audio() {
        assert!(!is_audio("photo.jpg"));
        assert!(!is_audio("doc.pdf"));
        assert!(!is_audio("code.rs"));
        assert!(!is_audio("noext"));
        assert!(!is_audio(""));
    }

    #[test]
    fn test_is_audio_with_path() {
        assert!(is_audio("/tmp/media/voice.ogg"));
        assert!(is_audio("relative/path/song.mp3"));
    }

    #[test]
    fn test_is_image_supported_extensions() {
        assert!(is_image("photo.jpg"));
        assert!(is_image("photo.jpeg"));
        assert!(is_image("icon.png"));
        assert!(is_image("anim.gif"));
        assert!(is_image("modern.webp"));
    }

    #[test]
    fn test_is_image_case_insensitive() {
        assert!(is_image("file.JPG"));
        assert!(is_image("file.Png"));
        assert!(is_image("file.WEBP"));
    }

    #[test]
    fn test_is_image_rejects_non_image() {
        assert!(!is_image("voice.ogg"));
        assert!(!is_image("doc.pdf"));
        assert!(!is_image("code.rs"));
        assert!(!is_image("noext"));
        assert!(!is_image(""));
    }

    #[test]
    fn test_is_image_with_path() {
        assert!(is_image("/tmp/photos/shot.png"));
        assert!(is_image("uploads/avatar.jpg"));
    }
}
