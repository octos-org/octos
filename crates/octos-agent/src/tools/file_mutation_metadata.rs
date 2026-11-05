//! Bounded process-local metadata used to render non-git file diffs.

use std::path::Path;

const FILE_MUTATION_PREVIEW_MAX_BYTES: usize = 256 * 1024;

/// Keep only bounded UTF-8 text for a non-git preview. Binary and oversized
/// files deliberately omit the previous-content side-channel.
pub fn file_mutation_previous_text(content: String) -> Option<String> {
    (content.len() <= FILE_MUTATION_PREVIEW_MAX_BYTES
        && !content.contains('\0')
        && !content.starts_with("%PDF-"))
    .then_some(content)
}

/// Capture an overwrite/delete baseline before mutation without following a
/// leaf symlink. Read raw bytes, never the PDF-to-text conversion in read_file.
/// This optional preview must not prevent an otherwise authorized mutation:
/// unreadable/non-text/oversized inputs omit the baseline, hence the diff.
/// Fenced writers must use their confined handle or skip this lexical read.
pub async fn read_file_mutation_previous_text(path: &Path) -> Option<String> {
    let read_path = path.to_owned();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<Option<String>> {
        use std::io::Read;

        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A swapped-in FIFO must not block the optional preview before
            // the descriptor's regular-file check below.
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(not(unix))]
        if read_path.symlink_metadata()?.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "symlink rejected",
            ));
        }
        let file = options.open(read_path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > FILE_MUTATION_PREVIEW_MAX_BYTES as u64 {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(FILE_MUTATION_PREVIEW_MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        match String::from_utf8(bytes) {
            Ok(content) => Ok(file_mutation_previous_text(content)),
            Err(_) => Ok(None),
        }
    })
    .await;
    match result {
        Ok(Ok(content)) => content,
        Ok(Err(error)) => {
            tracing::debug!(path = %path.display(), %error, "file diff baseline unavailable");
            None
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "file diff baseline reader failed");
            None
        }
    }
}
