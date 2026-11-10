//! Vision support: base64 image encoding for LLM providers.

use std::path::Path;

use base64::Engine;
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};

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
/// tools — host-scope reads skipped the ancestor walk at tool time).
/// Workspace paths are opened component by
/// component relative to a trusted directory fd, with no-follow semantics at
/// every level, so a parent replacement cannot redirect the subsequent open.
/// Paths outside the root follow symlinks in their ancestors (user
/// attachments never ran through a tool, and their raw temp paths are not
/// canonical — macOS tempdirs sit under the /var symlink) but are still
/// opened fd-relative on Unix, so a parent swapped mid-read can no longer
/// redirect the bytes (#2505 residual). The bytes must still pass the
/// re-sniffed magic below.
pub fn read_media_no_follow(path: &str, scope_root: Option<&Path>) -> Result<Vec<u8>> {
    read_media_no_follow_inner(Path::new(path), scope_root, || {})
}

fn read_media_no_follow_inner<F: FnOnce()>(
    path: &Path,
    scope_root: Option<&Path>,
    before_open: F,
) -> Result<Vec<u8>> {
    if let Some(root) = scope_root.filter(|root| path.starts_with(root)) {
        read_workspace_media(path, root, before_open)
    } else {
        read_leaf_media(path, before_open)
    }
}

/// Build directory-open options without requesting directory read/list access.
/// Linux uses `O_PATH`, Apple platforms use `O_SEARCH`, and Windows uses a
/// zero desired-access mask. `read(true)` is still required by cap-primitives'
/// option validation; the OS-specific access flags override it at open time.
fn workspace_dir_options(nofollow: bool) -> cap_primitives::fs::OpenOptions {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(if nofollow {
        FollowSymlinks::No
    } else {
        FollowSymlinks::Yes
    });
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut flags = libc::O_DIRECTORY | libc::O_PATH;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        options.custom_flags(flags);
    }
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))
    ))]
    {
        let mut flags = libc::O_DIRECTORY;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        options.custom_flags(flags);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let mut flags = libc::O_SEARCH;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        options.custom_flags(flags);
    }
    #[cfg(windows)]
    {
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.access_mode(0);
        options.custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS
                | if nofollow {
                    FILE_FLAG_OPEN_REPARSE_POINT
                } else {
                    0
                },
        );
    }
    options
}

fn ensure_workspace_directory(file: &std::fs::File, path: &Path) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("media parent is not a directory: {}", path.display()),
        ));
    }
    Ok(())
}

fn ensure_nofollow_workspace_directory(file: &std::fs::File, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        let metadata = file.metadata()?;
        // A reparse handle does not necessarily report itself as a directory.
        // Classify it before the ordinary-directory check so the security
        // rejection names the actual condition instead of NotADirectory.
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("media parent is a reparse point: {}", path.display()),
            ));
        }
    }
    ensure_workspace_directory(file, path)?;
    Ok(())
}

fn open_workspace_dir(start: &std::fs::File, path: &Path) -> std::io::Result<std::fs::File> {
    let file = cap_primitives::fs::open(start, path, &workspace_dir_options(true))?;
    ensure_nofollow_workspace_directory(&file, path)?;
    Ok(file)
}

fn open_ambient_workspace_dir(path: &Path) -> std::io::Result<std::fs::File> {
    let file = cap_primitives::fs::open_ambient(
        path,
        &workspace_dir_options(false),
        cap_primitives::ambient_authority(),
    )?;
    ensure_workspace_directory(&file, path)?;
    Ok(file)
}

/// Open the configured root without following a symlink in its own final
/// component. Ancestors above the configured root remain ambient/trusted, just
/// as they were for the previous ancestor walk.
fn open_workspace_root(root: &Path) -> std::io::Result<std::fs::File> {
    if let Some(name) = root.file_name() {
        let parent = root.parent().unwrap_or_else(|| Path::new("."));
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let parent = open_ambient_workspace_dir(parent)?;
        open_workspace_dir(&parent, Path::new(name))
    } else {
        open_ambient_workspace_dir(root)
    }
}

/// Open a workspace file relative to a trusted directory descriptor. Every
/// parent component is opened with no-follow semantics before advancing, so a
/// path replacement cannot redirect the subsequent open to another ancestor.
fn read_workspace_media<F: FnOnce()>(path: &Path, root: &Path, before_open: F) -> Result<Vec<u8>> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions, open, stat};
    use std::path::Component;

    let relative = path.strip_prefix(root).wrap_err_with(|| {
        format!(
            "resolve media path {} relative to workspace {}",
            path.display(),
            root.display()
        )
    })?;
    let mut components = relative.components();
    let leaf = match components.next_back() {
        Some(Component::Normal(leaf)) => leaf,
        _ => {
            return Err(eyre::eyre!(
                "media path has no file component: {}",
                path.display()
            ));
        }
    };
    let mut directory = open_workspace_root(root).wrap_err_with(|| {
        format!(
            "open trusted workspace directory {} for media {}",
            root.display(),
            path.display()
        )
    })?;
    for component in components {
        match component {
            Component::CurDir => continue,
            Component::Normal(component) => {
                directory =
                    open_workspace_dir(&directory, Path::new(component)).wrap_err_with(|| {
                        format!(
                            "open media parent component {} in {}",
                            component.to_string_lossy(),
                            path.display()
                        )
                    })?;
            }
            _ => {
                return Err(eyre::eyre!(
                    "media path escapes its workspace through a non-normal component: {}",
                    path.display()
                ));
            }
        }
    }

    let expected = stat(&directory, Path::new(leaf), FollowSymlinks::No).wrap_err_with(|| {
        format!(
            "stat media leaf {} relative to its opened parent",
            path.display()
        )
    })?;
    if expected.is_symlink() || !expected.is_file() {
        eyre::bail!("media leaf is not a regular file: {}", path.display());
    }
    if expected.len() > MAX_MEDIA_BYTES {
        eyre::bail!(
            "media file is over {MAX_MEDIA_BYTES} bytes: {}",
            path.display()
        );
    }

    before_open();
    let mut options = OpenOptions::new();
    options.read(true);
    // O_NOFOLLOW alone is not enough for cap-primitives' manual resolver:
    // its final-component recovery follows links when `follow` remains Yes.
    options._cap_fs_ext_follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_primitives::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = open(&directory, Path::new(leaf), &options).wrap_err_with(|| {
        format!(
            "open media leaf {} relative to its opened parent",
            path.display()
        )
    })?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        let metadata = file
            .metadata()
            .wrap_err_with(|| format!("read metadata for opened media leaf {}", path.display()))?;
        eyre::ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "media leaf is a reparse point: {}",
            path.display()
        );
    }
    read_open_media(file, path)
}

/// Out-of-root media previously kept a leaf-only guard: a path-based
/// `symlink_metadata` followed by a separate path-based `open`, so a
/// same-batch writer replacing a PARENT between the two redirected the read
/// (#2505 residual, ops P2 triage 2026-11-05: LD_PRELOAD parent-swap
/// redirected the actual bytes). On Unix the read is now fd-relative like
/// the workspace path: every ancestor is opened relative to the previously
/// opened directory fd, so a later replacement cannot redirect components we
/// already pinned. Ancestors still FOLLOW symlinks — user attachments live
/// in raw temp paths that are not canonical (macOS `/var` is a symlink);
/// each symlink ancestor is resolved once and the walk restarts fd-relative
/// from the resolved path, so the final chain to the leaf is always pinned.
/// Only the leaf keeps the no-follow refusal, and all metadata comes from
/// opened fds, not a prior path walk. Its upload/snapshot lifecycle stays
/// intentionally separate from the workspace fix.
#[cfg(unix)]
fn read_leaf_media<F: FnOnce()>(path: &Path, before_open: F) -> Result<Vec<u8>> {
    // Static symlink chains (system tempdirs) resolve in one or two hops;
    // the bound matches the kernel's ELOOP ceiling so a hostile loop fails
    // closed instead of spinning.
    const MAX_MEDIA_SYMLINK_RESTARTS: usize = 40;
    let mut current = path.to_path_buf();
    let mut before_open = Some(before_open);
    for _ in 0..MAX_MEDIA_SYMLINK_RESTARTS {
        match open_leaf_with_pinned_parents(&current, before_open.take())? {
            PinnedLeaf::Opened(file) => return read_open_media(file, path),
            PinnedLeaf::Redirect(resolved) => current = resolved,
        }
    }
    eyre::bail!(
        "media path exceeds {MAX_MEDIA_SYMLINK_RESTARTS} symlink redirects: {}",
        path.display()
    );
}

#[cfg(not(unix))]
fn read_leaf_media<F: FnOnce()>(path: &Path, before_open: F) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to read media: {}", path.display()))?;
    if meta.file_type().is_symlink() {
        eyre::bail!(
            "refusing to read media through a symlink: {}",
            path.display()
        );
    }
    if meta.len() > MAX_MEDIA_BYTES {
        eyre::bail!(
            "media file is over {MAX_MEDIA_BYTES} bytes: {}",
            path.display()
        );
    }
    before_open();
    let file = std::fs::File::open(path)
        .wrap_err_with(|| format!("failed to read media: {}", path.display()))?;
    read_open_media(file, path)
}

#[cfg(unix)]
enum PinnedLeaf {
    Opened(std::fs::File),
    Redirect(std::path::PathBuf),
}

/// Open `path`'s leaf relative to directory fds pinned one component at a
/// time. A symlink ancestor is resolved via `read_link` and reported as a
/// [`PinnedLeaf::Redirect`] for a fresh fd-relative walk from the resolved
/// path — cap-primitives' sandbox resolver refuses absolute symlink targets
/// outright, so the restart is what keeps raw temp paths (macOS `/var`)
/// readable. `.` is ignored and `..` pops the already pinned absolute prefix
/// before restarting, which keeps relative symlink targets such as
/// `alias -> ../real` legal without weakening the pin. The leaf itself is
/// stat-checked and opened no-follow relative to its pinned parent;
/// `before_open` fires exactly between those two, keeping the swap-injection
/// point the tests rely on.
#[cfg(unix)]
fn open_leaf_with_pinned_parents<F: FnOnce()>(
    path: &Path,
    before_open: Option<F>,
) -> Result<PinnedLeaf> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions, open, stat};
    use std::path::Component;

    // Make relative paths absolute WITHOUT resolving symlinks: lexical `..`
    // handling below needs an absolute `resolved_prefix` it can pop. This
    // accesses only cwd; every actual filesystem component is still pinned
    // fd-relative below.
    let absolute = std::path::absolute(path)
        .wrap_err_with(|| format!("make media path absolute: {}", path.display()))?;
    let components: Vec<Component<'_>> = absolute.components().collect();
    let mut index = 1; // component zero is RootDir after `absolute`.
    let mut resolved_prefix = std::path::PathBuf::from("/");
    // Anchor at the filesystem root so every component below it is opened
    // relative to a pinned directory fd.
    let mut directory = open_ambient_workspace_dir(Path::new("/"))
        .wrap_err_with(|| format!("open the media anchor directory for {}", path.display()))?;

    while index + 1 < components.len() {
        match components[index] {
            Component::CurDir => index += 1,
            Component::Normal(name) => {
                let component = Path::new(name);
                let entry =
                    stat(&directory, component, FollowSymlinks::No).wrap_err_with(|| {
                        format!(
                            "stat media parent component {} in {}",
                            name.to_string_lossy(),
                            path.display()
                        )
                    })?;
                if entry.is_symlink() {
                    // Path-based read_link on purpose: cap-primitives'
                    // sandboxed read_link refuses absolute targets, but this
                    // is not a security decision — the resolved path restarts
                    // a fully pinned fd-relative walk below, so a racily
                    // swapped link target just yields a different path to pin.
                    let target =
                        std::fs::read_link(resolved_prefix.join(name)).wrap_err_with(|| {
                            format!(
                                "resolve media parent link {} in {}",
                                name.to_string_lossy(),
                                path.display()
                            )
                        })?;
                    let mut resolved = if target.is_absolute() {
                        target
                    } else {
                        resolved_prefix.join(target)
                    };
                    for rest in &components[index + 1..] {
                        resolved.push(rest.as_os_str());
                    }
                    return Ok(PinnedLeaf::Redirect(resolved));
                }
                // Pinned no-follow open: a swap after the stat fails closed
                // instead of redirecting the descent.
                directory = open(&directory, component, &workspace_dir_options(true))
                    .wrap_err_with(|| {
                        format!(
                            "open media parent component {} in {}",
                            name.to_string_lossy(),
                            path.display()
                        )
                    })?;
                resolved_prefix.push(name);
                index += 1;
            }
            Component::ParentDir => {
                // `..` is legal in raw upload paths and in relative symlink
                // targets (`alias -> ../real`). A directory fd cannot be
                // "popped", so normalize lexically against the prefix we
                // already pinned and restart the fd-relative walk from that
                // absolute path. At `/`, `pop()` is a no-op, matching the
                // kernel's rule that root has no parent. No symlink is
                // followed here and the leaf no-follow guard is unchanged.
                resolved_prefix.pop();
                let mut resolved = resolved_prefix.clone();
                for rest in &components[index + 1..] {
                    resolved.push(rest.as_os_str());
                }
                return Ok(PinnedLeaf::Redirect(resolved));
            }
            _ => {
                return Err(eyre::eyre!(
                    "media path contains an unsupported component: {}",
                    path.display()
                ));
            }
        }
    }

    let leaf = match components.last() {
        Some(Component::Normal(leaf)) => *leaf,
        _ => {
            return Err(eyre::eyre!(
                "media path has no file component: {}",
                path.display()
            ));
        }
    };
    let expected = stat(&directory, Path::new(leaf), FollowSymlinks::No).wrap_err_with(|| {
        format!(
            "stat media leaf {} relative to its pinned parent",
            path.display()
        )
    })?;
    if expected.is_symlink() || !expected.is_file() {
        eyre::bail!(
            "refusing to read media through a symlink: {}",
            path.display()
        );
    }
    if expected.len() > MAX_MEDIA_BYTES {
        eyre::bail!(
            "media file is over {MAX_MEDIA_BYTES} bytes: {}",
            path.display()
        );
    }

    if let Some(before_open) = before_open {
        before_open();
    }
    let mut options = OpenOptions::new();
    options.read(true);
    // O_NOFOLLOW alone is not enough for cap-primitives' manual resolver:
    // its final-component recovery follows links when `follow` stays Yes.
    options._cap_fs_ext_follow(FollowSymlinks::No);
    use cap_primitives::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
    let file = open(&directory, Path::new(leaf), &options).wrap_err_with(|| {
        format!(
            "open media leaf {} relative to its pinned parent",
            path.display()
        )
    })?;
    Ok(PinnedLeaf::Opened(file))
}

fn read_open_media(file: std::fs::File, path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let meta = file
        .metadata()
        .wrap_err_with(|| format!("read metadata for opened media: {}", path.display()))?;
    if !meta.is_file() {
        eyre::bail!("media leaf is not a regular file: {}", path.display());
    }
    if meta.len() > MAX_MEDIA_BYTES {
        eyre::bail!(
            "media file is over {MAX_MEDIA_BYTES} bytes: {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_MEDIA_BYTES + 1)
        .read_to_end(&mut bytes)
        .wrap_err_with(|| format!("failed to read media: {}", path.display()))?;
    if bytes.len() as u64 > MAX_MEDIA_BYTES {
        eyre::bail!(
            "media file is over {MAX_MEDIA_BYTES} bytes: {}",
            path.display()
        );
    }
    Ok(bytes)
}

/// Walk every ancestor of `resolved` (including `resolved` itself) and
/// refuse if any one is a symlink or Windows reparse point. Shared with the
/// tool-time read (`coding_tools::read_image_header_no_follow`). The
/// render-time workspace path enforces the same no-follow rule with
/// fd-relative opens instead of a racy second walk. Stops at
/// `workspace_root` (inclusive) so we
/// never recurse into system roots. Returns `Ok(())` when none of the
/// inspected entries are symlinks; returns `PermissionDenied` with a
/// descriptive message when any are.
///
/// Safety properties:
///
/// * Uses `symlink_metadata`, which does NOT follow the link, so a
///   symlinked ancestor is correctly classified.
/// * When `resolved` does not live under `workspace_root` the stop never
///   matches and the walk runs to `/`, inspecting system directories.
///   Non-canonical paths under a symlinked system prefix (macOS `/tmp`,
///   `/var`) would be refused, so callers only point this at paths that
///   genuinely live under the root.
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
/// reach a request. Bytes determine both the MIME label and whether the
/// payload may leave under an image label at all.
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
    verify_snapshot(path, &bytes)?;
    // The tool sniffed a recognised image header when it ran; the file may
    // have been swapped since. Re-sniff so arbitrary payload cannot leave
    // the machine labelled image/png.
    if !is_sniffed_image(&bytes) {
        eyre::bail!(
            "media file does not match a recognised image header (PNG / JPEG / GIF / WEBP): {path}"
        );
    }

    let mime = image_mime(&bytes).expect("sniffed image has a MIME type");

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((mime.to_string(), encoded))
}

/// MIME derived from bytes, never an untrusted filename or server label.
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

const SNAPSHOT_PREFIX: &str = "octos-image-snapshot-";

/// Freeze already-authorized media before the tool result enters history.
/// Private, read-only copies survive process restart; the original path is
/// never reread by a later request. Hosts may remove these with the transcript.
pub fn snapshot_image(path: &str, scope_root: Option<&Path>) -> Result<std::path::PathBuf> {
    let bytes = read_media_no_follow(path, scope_root)?;
    if is_snapshot(path) {
        verify_snapshot(path, &bytes)?;
        return Ok(path.into());
    }
    snapshot_image_bytes(&bytes)
}

pub fn snapshot_image_bytes(bytes: &[u8]) -> Result<std::path::PathBuf> {
    use std::io::Write;
    eyre::ensure!(
        bytes.len() as u64 <= MAX_MEDIA_BYTES,
        "image exceeds {MAX_MEDIA_BYTES} bytes"
    );
    let extension = match image_mime(bytes) {
        Some("image/png") => "png",
        Some("image/jpeg") => "jpg",
        Some("image/gif") => "gif",
        Some("image/webp") => "webp",
        _ => eyre::bail!("unsupported or invalid image bytes"),
    };
    // Keep these with persistent application data rather than OS temp files:
    // a resumed/rewound transcript still refers to the original bytes.
    let root = dirs::data_local_dir()
        .ok_or_else(|| eyre::eyre!("no local application-data directory for media snapshots"))?
        .join("octos")
        .join("model-media");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&root)?;
    eyre::ensure!(
        !std::fs::symlink_metadata(&root)?.file_type().is_symlink(),
        "media snapshot directory must not be a symlink"
    );
    let directory = tempfile::Builder::new()
        .prefix(SNAPSHOT_PREFIX)
        .tempdir_in(root)?;
    let path = directory
        .path()
        .join(format!("{:x}.{extension}", Sha256::digest(bytes)));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    let _ = directory.keep();
    Ok(path)
}

fn is_snapshot(path: &str) -> bool {
    Path::new(path)
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.to_string_lossy().starts_with(SNAPSHOT_PREFIX))
}

/// Snapshots have a content digest as their filename but private, random parent
/// directories. Use the digest for identity; serializers still verify bytes on
/// every read. Ordinary mutable paths must never deduplicate by basename.
pub fn image_identity(path: &str) -> String {
    if is_snapshot(path) {
        if let Some(name) = Path::new(path).file_name().and_then(|p| p.to_str()) {
            return format!("snapshot:{name}");
        }
    }
    format!("path:{path}")
}

fn verify_snapshot(path: &str, bytes: &[u8]) -> Result<()> {
    if is_snapshot(path) {
        let digest = format!("{:x}", Sha256::digest(bytes));
        eyre::ensure!(
            Path::new(path).file_stem().and_then(|s| s.to_str()) == Some(digest.as_str()),
            "image snapshot content changed; refusing to replace historical pixels"
        );
    }
    Ok(())
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
    fn should_reject_tampered_snapshot_instead_of_changing_historical_pixels() {
        let dir = tempfile::Builder::new()
            .prefix(SNAPSHOT_PREFIX)
            .tempdir()
            .unwrap();
        let original = tiny_png();
        let path = dir
            .path()
            .join(format!("{:x}.png", Sha256::digest(&original)));
        std::fs::write(&path, &original).unwrap();
        assert!(encode_image(path.to_str().unwrap(), None).is_ok());
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nreplacement").unwrap();
        let error = encode_image(path.to_str().unwrap(), None).unwrap_err();
        assert!(error.to_string().contains("snapshot content changed"));
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

    fn tiny_png() -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    #[cfg(unix)]
    fn replace_dir_with_link(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn replace_dir_with_link(target: &Path, link: &Path) -> std::io::Result<()> {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "mklink /J exited with {status}"
            )))
        }
    }

    #[test]
    fn should_never_read_redirected_bytes_when_parent_changes_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let path = ws.join("sub/img.png");
        let original = tiny_png();
        let mut redirected = tiny_png();
        redirected.extend_from_slice(b"redirected-parent");
        std::fs::write(&path, &original).unwrap();
        std::fs::write(outside.join("img.png"), &redirected).unwrap();
        let link_target = ws.join("sub");
        let renamed = ws.join("sub-original");
        let outside_in_hook = outside.clone();

        let result = read_media_no_follow_inner(&path, Some(&ws), move || {
            // cap-std may intentionally deny deletion of an already-open parent
            // on Windows. That is also a safe outcome; Unix permits the swap.
            if std::fs::rename(&link_target, renamed).is_ok() {
                replace_dir_with_link(&outside_in_hook, &link_target).unwrap();
            }
        });

        assert!(
            !matches!(&result, Ok(bytes) if bytes == &redirected),
            "the request must never read bytes reached through the replacement parent"
        );
        if let Ok(bytes) = &result {
            assert_eq!(bytes, &original);
        }
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_a_symlinked_out_of_root_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let upload = dir.path().join("upload");
        std::fs::create_dir_all(&upload).unwrap();
        let outside = dir.path().join("outside.png");
        std::fs::write(&outside, tiny_png()).unwrap();
        std::os::unix::fs::symlink(&outside, upload.join("img.png")).unwrap();
        assert!(
            read_media_no_follow_inner(&upload.join("img.png"), None, || {}).is_err(),
            "a symlinked leaf must be refused"
        );
    }

    // Apple filesystem APIs reject invalid UTF-8 at file-creation time; the
    // arbitrary-byte path contract is exercised on Unix platforms that allow it.

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
}
