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
///
/// `scope_root` is the workspace root the path was validated against at
/// tool time (`ChatConfig::media_scope_root`; only set for workspace-scoped
/// tools — host-scope reads skipped the ancestor walk at tool time and get
/// the leaf-only guard here too). Workspace paths re-walk every ancestor up
/// to the root. Paths outside the root in a workspace-scoped transcript —
/// upload and profile handles, which resolve to canonical paths in the
/// data/temp dirs — were walked without a stop at tool time, so render
/// walks them the same way: canonical paths pass by construction, a
/// symlink-ancestor swap cannot.
pub fn read_media_no_follow(path: &str, scope_root: Option<&Path>) -> Result<Vec<u8>> {
    use std::io::Read;
    if let Some(root) = scope_root {
        let resolved = Path::new(path);
        if resolved.starts_with(root) {
            reject_symlink_ancestors(resolved, root)
                .wrap_err_with(|| format!("failed to read media: {path}"))?;
        } else {
            reject_symlink_ancestors(resolved, Path::new("/"))
                .wrap_err_with(|| format!("failed to read media: {path}"))?;
        }
    }
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

/// Walk every ancestor of `resolved` (including `resolved` itself) and
/// refuse if any one is a symlink or Windows reparse point. Shared with the
/// tool-time read (`coding_tools::read_image_header_no_follow`) so the
/// render-time re-validation in [`read_media_no_follow`] enforces the exact
/// same rule the tool enforced. Stops at `workspace_root` (inclusive) so we
/// never recurse into system roots — pass `/` for the tool-time behaviour on
/// paths that live outside the workspace (upload handles), where the stop
/// never matches and the walk runs to the filesystem root. Returns `Ok(())`
/// when none of the inspected entries are symlinks; returns
/// `PermissionDenied` with a descriptive message when any are.
///
/// Safety properties:
///
/// * Uses `symlink_metadata`, which does NOT follow the link, so a
///   symlinked ancestor is correctly classified.
/// * When `resolved` does not live under `workspace_root` the stop never
///   matches and the walk runs to `/`, inspecting system directories —
///   canonical paths (the only kind out-of-root validation produces)
///   contain no symlinks and pass, but a non-canonical path under a
///   symlinked system prefix (macOS `/tmp`, `/var`) would be refused.
///   Callers choose the stop deliberately.
/// * Hard-bounded by `Path::ancestors`, which is finite.
pub fn reject_symlink_ancestors(resolved: &Path, workspace_root: &Path) -> std::io::Result<()> {
    for ancestor in resolved.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "refusing to follow symlink ancestor: {}",
                            ancestor.display()
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // The leaf may not exist yet — keep walking up so a
                // symlinked PARENT still gets caught. The actual
                // open below will surface NotFound for the leaf.
            }
            Err(err) => return Err(err),
        }
        // Stop walking once we hit (and have inspected) the
        // configured workspace root. Going further would inspect
        // system directories that the caller has no jurisdiction
        // over.
        if ancestor == workspace_root {
            break;
        }
    }
    Ok(())
}

/// Whether the bytes open like one of the raster formats the request build
/// actually ships (PNG / JPEG / GIF / WEBP — the extensions `is_image`
/// accepts). Mirrors the raster arms of the tool-time
/// `detect_image_format`: BMP and SVG are refused at tool time and never
/// reach a request. The extension only decides the MIME label; this sniff
/// decides whether arbitrary bytes may leave under an image label at all.
fn is_sniffed_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP")
}

/// Whether the bytes open like a container the tool-time `detect_video_format`
/// also accepts: ISO BMFF (`ftyp` at offset 4: MP4 / M4V / MOV) or EBML
/// (MKV / WebM).
fn is_sniffed_video_container(bytes: &[u8]) -> bool {
    (bytes.len() >= 12 && &bytes[4..8] == b"ftyp") || bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

/// Encode an image file as base64 and return (mime_type, base64_data).
pub fn encode_image(path: &str, scope_root: Option<&Path>) -> Result<(String, String)> {
    let bytes = read_media_no_follow(path, scope_root)?;
    // The tool sniffed a recognised image header when it ran; the file may
    // have been swapped since. Re-sniff so arbitrary payload cannot leave
    // the machine labelled image/png.
    if !is_sniffed_image(&bytes) {
        eyre::bail!(
            "media file does not match a recognised image header (PNG / JPEG / GIF / WEBP): {path}"
        );
    }

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
pub fn encode_video(path: &str, scope_root: Option<&Path>) -> Result<(String, String)> {
    let bytes = read_media_no_follow(path, scope_root)?;
    if !is_sniffed_video_container(&bytes) {
        eyre::bail!(
            "media file does not match a recognised video container (MP4 / MOV / MKV / WebM): {path}"
        );
    }
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

        let (mime, data) = encode_image(path.to_str().unwrap(), None).unwrap();
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
        // Real headers: the extension only labels the MIME, the bytes must
        // still sniff as an image (#2480).
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        let png = tiny_png();
        let gif = b"GIF89a".to_vec();
        let webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();

        for (ext, bytes, expected_mime) in [
            ("jpg", jpeg.clone(), "image/jpeg"),
            ("jpeg", jpeg, "image/jpeg"),
            ("png", png.clone(), "image/png"),
            ("gif", gif, "image/gif"),
            ("webp", webp, "image/webp"),
            // Unknown extensions still encode when the bytes are an image —
            // the MIME falls back to image/jpeg.
            ("unknown", png, "image/jpeg"), // fallback
        ] {
            let path = dir.path().join(format!("test.{ext}"));
            std::fs::write(&path, &bytes).unwrap();
            let (mime, _) = encode_image(path.to_str().unwrap(), None).unwrap();
            assert_eq!(mime, expected_mime, "wrong MIME for .{ext}");
        }
    }

    #[test]
    fn test_encode_image_nonexistent_file() {
        let result = encode_image("/nonexistent/path/image.png", None);
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
        std::fs::write(&real, tiny_png()).unwrap();
        let link = dir.path().join("link.png");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            encode_image(link.to_str().unwrap(), None).is_err(),
            "a symlink must not be read"
        );
        assert!(encode_image(real.to_str().unwrap(), None).is_ok());
        let big = dir.path().join("big.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_MEDIA_BYTES + 1).unwrap();
        assert!(
            encode_image(big.to_str().unwrap(), None).is_err(),
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
            let (got, data) = encode_video(path.to_str().unwrap(), None).unwrap();
            assert_eq!(got, mime, ".{ext}");
            assert!(!data.is_empty());
        }
        assert!(encode_video("/nonexistent/clip.mp4", None).is_err());
    }

    fn tiny_png() -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    /// #2480: between the tool validating a media path and the request being
    /// built, a background writer can swap a parent directory for a symlink
    /// pointing anywhere readable. With the scope root the path was
    /// validated against, the encode must refuse; without it, today's
    /// behaviour would ship the swapped bytes as image/png.
    #[cfg(unix)]
    #[test]
    fn should_refuse_a_swapped_symlink_ancestor_when_the_scope_root_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let secret = dir.path().join("secret");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        let path = ws.join("sub").join("img.png");
        std::fs::write(&path, tiny_png()).unwrap();

        // Positive control: the untouched layout encodes fine.
        let (mime, _) = encode_image(path.to_str().unwrap(), Some(&ws)).unwrap();
        assert_eq!(mime, "image/png");

        // The swap: `sub` now points at the directory holding the payload.
        std::fs::remove_dir_all(ws.join("sub")).unwrap();
        std::fs::write(secret.join("img.png"), tiny_png()).unwrap();
        std::os::unix::fs::symlink(&secret, ws.join("sub")).unwrap();
        assert!(
            encode_image(path.to_str().unwrap(), Some(&ws)).is_err(),
            "a symlinked ancestor must not be followed at encode time"
        );
    }

    /// Out-of-root paths in a workspace-scoped transcript are the canonical
    /// upload/profile handles: tool time walked them without a stop
    /// (canonical paths pass by construction), and render walks them the
    /// same way, so a parent swapped for a symlink outside the root is
    /// still caught. Host-scope transcripts pass `None` and keep the
    /// leaf-only guard.
    #[cfg(unix)]
    #[test]
    fn should_walk_out_of_root_paths_without_a_stop_like_the_tool_did() {
        let ws = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        let file = store.path().join("upload.png");
        std::fs::write(&file, tiny_png()).unwrap();
        // Upload handles are canonical by construction (canonicalize_under);
        // the rooted walk would never match, the full one passes it.
        let canonical = std::fs::canonicalize(&file).unwrap();
        assert!(encode_image(canonical.to_str().unwrap(), Some(ws.path())).is_ok());

        // The swap: `store` now points at the directory holding the payload.
        std::fs::remove_dir_all(store.path()).unwrap();
        std::fs::write(secret.path().join("upload.png"), tiny_png()).unwrap();
        std::os::unix::fs::symlink(secret.path(), store.path()).unwrap();
        assert!(
            encode_image(canonical.to_str().unwrap(), Some(ws.path())).is_err(),
            "a swapped parent outside the root must be refused too"
        );

        // Leaf symlink stays refused regardless of the root.
        let link = ws.path().join("link.png");
        std::os::unix::fs::symlink(&canonical, &link).unwrap();
        assert!(encode_image(link.to_str().unwrap(), Some(ws.path())).is_err());
        assert!(encode_image(link.to_str().unwrap(), None).is_err());
    }

    #[test]
    fn should_refuse_non_image_bytes_arriving_with_an_image_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.png");
        std::fs::write(&path, b"definitely not an image").unwrap();
        assert!(
            encode_image(path.to_str().unwrap(), None).is_err(),
            "arbitrary bytes must not leave labelled image/png"
        );
    }

    #[test]
    fn should_refuse_non_container_bytes_arriving_with_a_video_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"garbage").unwrap();
        assert!(
            encode_video(path.to_str().unwrap(), None).is_err(),
            "arbitrary bytes must not leave labelled video/mp4"
        );
        let ok = dir.path().join("ok.mp4");
        std::fs::write(&ok, b"\x00\x00\x00\x18ftypisom").unwrap();
        assert!(encode_video(ok.to_str().unwrap(), None).is_ok());
    }
}
