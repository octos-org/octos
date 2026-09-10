//! Revisioned slide-edit documents. Rendering remains in the slides workflow;
//! this file is its durable input, independent of browser storage.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Mutex;

pub(crate) const FILENAME: &str = "manual-edits.json";
static SAVE_LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SlideEdit {
    pub index: usize,
    pub title: String,
    pub notes: String,
    pub layout: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditDocument {
    pub revision: String,
    pub saved_at: String,
    pub base_generated_at: Option<String>,
    pub slides: Vec<SlideEdit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveRequest {
    pub expected_revision: Option<String>,
    pub base_generated_at: Option<String>,
    pub slides: Vec<SlideEdit>,
}

fn read_from(mut file: std::fs::File) -> io::Result<Option<EditDocument>> {
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "slide edits too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

#[cfg(unix)]
fn read_at(parent: &std::os::fd::OwnedFd) -> io::Result<Option<EditDocument>> {
    use rustix::fs::{Mode, OFlags, openat};
    match openat(
        parent,
        FILENAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let file = std::fs::File::from(fd);
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "slide edits must be a regular file",
                ));
            }
            read_from(file)
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn read(root: &Path, relative: &Path) -> io::Result<Option<EditDocument>> {
    #[cfg(unix)]
    {
        read_at(&super::file_mutations::open_parent(root, relative)?)
    }
    #[cfg(not(unix))]
    {
        let path = super::file_mutations::checked_path(root, relative)?;
        match std::fs::File::open(path) {
            Ok(file) if file.metadata()?.is_file() => read_from(file),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "slide edits must be a regular file",
            )),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

pub(crate) fn save(root: &Path, relative: &Path, request: SaveRequest) -> io::Result<EditDocument> {
    if request.slides.is_empty()
        || request.slides.len() > 200
        || request.slides.iter().enumerate().any(|(index, slide)| {
            slide.index != index
                || slide.title.len() > 4096
                || slide.notes.len() > 65536
                || ![
                    "title",
                    "content",
                    "two-column",
                    "image-full",
                    "agenda",
                    "conclusion",
                ]
                .contains(&slide.layout.as_str())
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid slide edit document",
        ));
    }
    let _lock = SAVE_LOCK
        .lock()
        .map_err(|_| io::Error::other("slide edit lock failed"))?;
    #[cfg(unix)]
    let parent = super::file_mutations::open_parent(root, relative)?;
    #[cfg(unix)]
    let current = read_at(&parent)?;
    #[cfg(not(unix))]
    let current = read(root, relative)?;
    if current.as_ref().map(|doc| doc.revision.as_str()) != request.expected_revision.as_deref() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "another editor saved a newer revision; reload before saving",
        ));
    }
    let document = EditDocument {
        revision: uuid::Uuid::new_v4().to_string(),
        saved_at: chrono::Utc::now().to_rfc3339(),
        base_generated_at: request.base_generated_at,
        slides: request.slides,
    };
    let bytes = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "slide edits too large",
        ));
    }
    let temp_name = format!(".manual-edits-{}.tmp", document.revision);
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
        let fd = openat(
            &parent,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let result = (|| -> io::Result<()> {
            let mut file = std::fs::File::from(fd);
            file.write_all(&bytes)?;
            file.sync_all()?;
            renameat(&parent, temp_name.as_str(), &parent, FILENAME)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&parent, temp_name.as_str(), AtFlags::empty());
        }
        result?;
    }
    #[cfg(not(unix))]
    {
        let path = super::file_mutations::checked_path(root, relative)?;
        let temp = path.with_file_name(&temp_name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if let Err(err) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(temp);
            return Err(err);
        }
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(revision: Option<String>, title: &str) -> SaveRequest {
        SaveRequest {
            expected_revision: revision,
            base_generated_at: Some("baseline".into()),
            slides: vec![SlideEdit {
                index: 0,
                title: title.into(),
                notes: "private notes".into(),
                layout: "title".into(),
                thumbnail_url: None,
            }],
        }
    }
    #[test]
    fn persists_edits_across_reads_and_rejects_stale_writers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("deck")).unwrap();
        let path = Path::new("deck/manual-edits.json");
        assert!(read(&root, path).unwrap().is_none());
        let first = save(&root, path, request(None, "Edited title")).unwrap();
        assert_eq!(
            read(&root, path).unwrap().unwrap().slides[0].title,
            "Edited title"
        );
        assert!(save(&root, path, request(None, "stale overwrite")).is_err());
        let next = save(&root, path, request(Some(first.revision), "New title")).unwrap();
        assert_eq!(read(&root, path).unwrap().unwrap().revision, next.revision);
    }
    #[cfg(unix)]
    #[test]
    fn cannot_write_through_a_symlinked_project() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("deck")).unwrap();
        assert!(
            save(
                dir.path(),
                Path::new("deck/manual-edits.json"),
                request(None, "no")
            )
            .is_err()
        );
        assert!(!other.path().join(FILENAME).exists());
    }
}
