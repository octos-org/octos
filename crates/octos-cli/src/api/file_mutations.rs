//! Persist file-browser mutations inside the already-authorized file root.

use std::io;
use std::path::{Component, Path, PathBuf};

pub(crate) fn validate_filename(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0', '\r', '\n'])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid filename",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn open_parent(root: &Path, relative: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = open(root, flags, Mode::empty())?;
    for part in relative.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(name) = part else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid file path",
            ));
        };
        parent = openat(&parent, name, flags, Mode::empty())?;
    }
    Ok(parent)
}

#[cfg(not(unix))]
pub(crate) fn checked_path(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    let mut path = root.canonicalize()?;
    for part in relative.components() {
        let Component::Normal(name) = part else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid file path",
            ));
        };
        path.push(name);
        match path.symlink_metadata() {
            Ok(metadata) => {
                #[cfg(windows)]
                let is_link = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0 // all reparse points, including junctions
                };
                #[cfg(not(windows))]
                let is_link = metadata.is_symlink();
                if is_link {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "symlink rejected",
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound && path == root.join(relative) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(path)
}

/// Rename never overwrites another file; delete never follows a link.
pub(crate) fn mutate(
    root: &Path,
    path: &Path,
    new_name: Option<&str>,
) -> io::Result<Option<PathBuf>> {
    if let Some(name) = new_name {
        validate_filename(name)?;
    }
    let root = root.canonicalize()?;
    // Keep the lexical relative path for the descriptor walk: canonicalizing
    // away a symlink here would lose the no-follow guarantee.
    let relative = path.strip_prefix(&root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file outside authorized root",
        )
    })?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid file path",
        ));
    }
    let leaf = relative
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing filename"))?;
    let target = new_name.map(|name| path.with_file_name(name));
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, FileType, statat, unlinkat};
        let parent = open_parent(&root, relative)?;
        let stat = statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only regular files can be changed",
            ));
        }
        if let Some(name) = new_name {
            if leaf == name {
                return Ok(target);
            }
            #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
            rustix::fs::renameat_with(
                &parent,
                leaf,
                &parent,
                name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
            {
                rustix::fs::linkat(&parent, leaf, &parent, name, AtFlags::empty())?;
                if let Err(err) = unlinkat(&parent, leaf, AtFlags::empty()) {
                    let _ = unlinkat(&parent, name, AtFlags::empty());
                    return Err(err.into());
                }
            }
        } else {
            unlinkat(&parent, leaf, AtFlags::empty())?;
        }
    }
    #[cfg(not(unix))]
    {
        // Match the platform fallback of the existing preview/file readers.
        checked_path(&root, relative)?;
        if !path.symlink_metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only regular files can be changed",
            ));
        }
        if let Some(target) = &target {
            if target == path {
                return Ok(Some(target.clone()));
            }
            std::fs::hard_link(path, target)?;
            if let Err(err) = std::fs::remove_file(path) {
                let _ = std::fs::remove_file(target);
                return Err(err);
            }
        } else {
            std::fs::remove_file(path)?;
        }
    }
    Ok(target)
}
