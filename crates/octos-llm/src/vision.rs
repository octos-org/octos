//! Vision support: base64 image encoding for LLM providers.

use std::path::Path;

use base64::Engine;
use eyre::{Result, WrapErr};

/// Encode an image file as base64 and return (mime_type, base64_data).
pub fn encode_image(path: &str) -> Result<(String, String)> {
    let bytes = std::fs::read(path).wrap_err_with(|| format!("failed to read image: {path}"))?;

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
}
