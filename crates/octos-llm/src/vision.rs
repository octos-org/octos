//! Vision support: base64 image encoding for LLM providers.

use std::path::Path;

use base64::Engine;
use eyre::{Result, WrapErr};

/// The most a media file may weigh to be encoded inline: base64 grows it by
/// a third and every provider caps a request well under this.
pub const MAX_MEDIA_BYTES: u64 = 20 * 1024 * 1024;

/// Read a media file for encoding without following a symlink and without
/// reading past [`MAX_MEDIA_BYTES`]. A tool validates a file when it runs;
/// this request may be built much later, and a sibling tool or a background
/// writer could have replaced the file with a link to anything readable
/// since — so the same refusals apply again here, at the moment the bytes
/// leave the machine.
pub fn read_media_no_follow(path: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to read media: {path}"))?;
    if meta.file_type().is_symlink() {
        eyre::bail!("refusing to read media through a symlink: {path}");
    }
    if meta.len() > MAX_MEDIA_BYTES {
        eyre::bail!("media file is over {MAX_MEDIA_BYTES} bytes: {path}");
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .wrap_err_with(|| format!("failed to read media: {path}"))?
    };
    #[cfg(not(unix))]
    let file =
        std::fs::File::open(path).wrap_err_with(|| format!("failed to read media: {path}"))?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_MEDIA_BYTES + 1)
        .read_to_end(&mut bytes)
        .wrap_err_with(|| format!("failed to read media: {path}"))?;
    if bytes.len() as u64 > MAX_MEDIA_BYTES {
        eyre::bail!("media file is over {MAX_MEDIA_BYTES} bytes: {path}");
    }
    Ok(bytes)
}

/// Encode an image file as base64 and return (mime_type, base64_data).
pub fn encode_image(path: &str) -> Result<(String, String)> {
    let bytes = read_media_no_follow(path)?;

    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_lowercase();

    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    };

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((mime.to_string(), encoded))
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

/// Check if a file path looks like a video file a multimodal model can take
/// inline (MP4, MOV, MKV, WebM). Which providers actually accept one is
/// decided per wire protocol: OpenAI-compatible endpoints take a
/// `video_url` part (GLM, Kimi), Gemini takes inline data, Anthropic's
/// protocol has no video block, and a text-only model rejects the part —
/// see the retry in each provider.
pub fn is_video(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".mp4")
        || lower.ends_with(".m4v")
        || lower.ends_with(".mov")
        || lower.ends_with(".mkv")
        || lower.ends_with(".webm")
}

/// Encode a video file as base64 and return (mime_type, base64_data).
pub fn encode_video(path: &str) -> Result<(String, String)> {
    let bytes = read_media_no_follow(path)?;
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4")
        .to_lowercase();
    let mime = match ext.as_str() {
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        _ => "video/mp4",
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((mime.to_string(), encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_image_supported_extensions() {
        assert!(is_image("photo.jpg"));
        assert!(is_image("photo.jpeg"));
        assert!(is_image("photo.png"));
        assert!(is_image("photo.gif"));
        assert!(is_image("photo.webp"));
    }

    #[test]
    fn test_is_image_case_insensitive() {
        assert!(is_image("PHOTO.JPG"));
        assert!(is_image("Photo.PNG"));
        assert!(is_image("test.WebP"));
    }

    #[test]
    fn test_is_image_unsupported() {
        assert!(!is_image("file.txt"));
        assert!(!is_image("file.pdf"));
        assert!(!is_image("file.svg"));
        assert!(!is_image("file.bmp"));
        assert!(!is_image("file.mp4"));
        assert!(!is_image(""));
    }

    #[test]
    fn test_is_image_with_path() {
        assert!(is_image("/home/user/photos/sunset.jpg"));
        assert!(is_image("./relative/path/img.png"));
        assert!(!is_image("/usr/bin/program"));
    }

    #[test]
    fn test_encode_image_real_file() {
        // Create a minimal 1x1 PNG
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG header
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
            0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63,
            0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC, 0x33, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.png");
        std::fs::write(&path, &png_bytes).unwrap();

        let (mime, data) = encode_image(path.to_str().unwrap()).unwrap();
        assert_eq!(mime, "image/png");
        assert!(!data.is_empty());

        // Verify base64 roundtrips
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .unwrap();
        assert_eq!(decoded, png_bytes);
    }

    #[test]
    fn test_encode_image_mime_types() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![0u8; 4];

        for (ext, expected_mime) in [
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("png", "image/png"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
            ("unknown", "image/jpeg"), // fallback
        ] {
            let path = dir.path().join(format!("test.{ext}"));
            std::fs::write(&path, &data).unwrap();
            let (mime, _) = encode_image(path.to_str().unwrap()).unwrap();
            assert_eq!(mime, expected_mime, "wrong MIME for .{ext}");
        }
    }

    #[test]
    fn test_encode_image_nonexistent_file() {
        let result = encode_image("/nonexistent/path/image.png");
        assert!(result.is_err());
    }

    #[test]
    fn should_recognise_video_containers_and_nothing_else() {
        for p in ["clip.mp4", "clip.M4V", "clip.mov", "clip.mkv", "clip.webm"] {
            assert!(is_video(p), "{p}");
            assert!(!is_image(p), "{p} is not an image");
        }
        for p in ["photo.png", "notes.txt", "song.mp3", "clip.avi"] {
            assert!(!is_video(p), "{p}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_a_symlink_and_an_oversized_file_at_encode_time() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.png");
        std::fs::write(&real, b"\x89PNG").unwrap();
        let link = dir.path().join("link.png");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            encode_image(link.to_str().unwrap()).is_err(),
            "a symlink must not be read"
        );
        assert!(encode_image(real.to_str().unwrap()).is_ok());
        let big = dir.path().join("big.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_MEDIA_BYTES + 1).unwrap();
        assert!(
            encode_image(big.to_str().unwrap()).is_err(),
            "over the size cap"
        );
    }

    #[test]
    fn should_encode_video_with_the_container_mime() {
        let dir = tempfile::tempdir().unwrap();
        for (ext, mime) in [
            ("mp4", "video/mp4"),
            ("m4v", "video/mp4"),
            ("mov", "video/quicktime"),
            ("mkv", "video/x-matroska"),
            ("webm", "video/webm"),
        ] {
            let path = dir.path().join(format!("clip.{ext}"));
            std::fs::write(&path, b"\x00\x00\x00\x18ftypisom").unwrap();
            let (got, data) = encode_video(path.to_str().unwrap()).unwrap();
            assert_eq!(got, mime, ".{ext}");
            assert!(!data.is_empty());
        }
        assert!(encode_video("/nonexistent/clip.mp4").is_err());
    }
}
