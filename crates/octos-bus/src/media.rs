//! Shared media download helper for channels.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use reqwest::Client;
use tracing::debug;

/// Download a file from a URL to the media directory.
/// Returns the absolute path of the saved file.
pub async fn download_media(
    client: &Client,
    url: &str,
    headers: &[(&str, &str)],
    dest_dir: &Path,
    filename: &str,
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

    let bytes = response
        .bytes()
        .await
        .wrap_err("failed to read download body")?;
    std::fs::write(&dest, &bytes)
        .wrap_err_with(|| format!("failed to write: {}", dest.display()))?;

    debug!(path = %dest.display(), bytes = bytes.len(), "media downloaded");
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
