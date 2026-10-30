use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use eyre::{Context, Result, bail};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

pub(crate) fn xml(value: impl AsRef<str>) -> String {
    quick_xml::escape::escape(value.as_ref()).into_owned()
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve an output under `root`, creating its parent directories only after
/// both lexical and existing-ancestor containment checks. Existing outputs are
/// refused: publication is complete-file, no-overwrite delivery.
pub(crate) fn output_path(root: &Path, output: &str, extension: &str) -> Result<PathBuf> {
    if output.trim().is_empty() {
        bail!("output must not be blank");
    }
    let root = root
        .canonicalize()
        .wrap_err("canonicalize workspace root")?;
    let requested = Path::new(output);
    let candidate = normalize_absolute(&if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    });
    if candidate.extension().and_then(|v| v.to_str()) != Some(extension) {
        bail!("output must end in .{extension}");
    }
    if !candidate.starts_with(&root) {
        bail!("output must stay inside the session workspace");
    }
    let parent = candidate
        .parent()
        .ok_or_else(|| eyre::eyre!("output has no parent directory"))?;
    // A not-yet-created path cannot be canonicalized. Check its nearest
    // existing ancestor first so an in-workspace symlink cannot redirect the
    // subsequent create_dir_all outside the workspace.
    let mut ancestor = parent;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| eyre::eyre!("output has no existing ancestor"))?;
    }
    let canonical_ancestor = ancestor
        .canonicalize()
        .wrap_err_with(|| format!("canonicalize {}", ancestor.display()))?;
    if !canonical_ancestor.starts_with(&root) {
        bail!("output must stay inside the session workspace");
    }
    std::fs::create_dir_all(parent).wrap_err_with(|| format!("create {}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .wrap_err_with(|| format!("canonicalize {}", parent.display()))?;
    if !parent.starts_with(&root) {
        bail!("output must stay inside the session workspace");
    }
    let name = candidate
        .file_name()
        .ok_or_else(|| eyre::eyre!("output must name a file"))?;
    let final_path = parent.join(name);
    if final_path.exists() {
        bail!(
            "output already exists; choose a new filename: {}",
            final_path.display()
        );
    }
    Ok(final_path)
}

pub(crate) fn write_package(
    final_path: &Path,
    write: impl FnOnce(&mut ZipWriter<File>) -> Result<()>,
) -> Result<()> {
    let parent = final_path
        .parent()
        .ok_or_else(|| eyre::eyre!("output has no parent directory"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .wrap_err("read system clock")?
        .as_nanos();
    let temp_path = parent.join(format!(
        ".{}.office-{}-{nonce}.tmp",
        final_path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("package"),
        std::process::id(),
    ));
    let file = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&temp_path)
        .wrap_err_with(|| format!("create temporary package {}", temp_path.display()))?;
    let mut zip = ZipWriter::new(file);
    let result = (|| {
        write(&mut zip)?;
        let file = zip.finish().wrap_err("finish OOXML ZIP package")?;
        file.sync_all().wrap_err("sync OOXML package")?;
        // Linking a completed same-filesystem temp file publishes atomically
        // and fails rather than overwriting an existing output.
        std::fs::hard_link(&temp_path, final_path).wrap_err_with(|| {
            format!(
                "publish package without overwrite at {}",
                final_path.display()
            )
        })?;
        Ok(())
    })();
    std::fs::remove_file(&temp_path).wrap_err("remove temporary package")?;
    result
}

pub(crate) fn start_file(zip: &mut ZipWriter<File>, name: &str) -> Result<()> {
    zip.start_file(
        name,
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
    )
    .wrap_err_with(|| format!("start ZIP member {name}"))
}

pub(crate) fn zip_text(zip: &mut ZipWriter<File>, name: &str, text: &str) -> Result<()> {
    start_file(zip, name)?;
    zip.write_all(text.as_bytes())
        .wrap_err_with(|| format!("write ZIP member {name}"))
}

pub(crate) fn zip_bytes(zip: &mut ZipWriter<File>, name: &str, bytes: &[u8]) -> Result<()> {
    start_file(zip, name)?;
    zip.write_all(bytes)
        .wrap_err_with(|| format!("write ZIP member {name}"))
}

pub(crate) fn read_workspace_file(root: &Path, requested: &str) -> Result<(PathBuf, Vec<u8>)> {
    let root = root
        .canonicalize()
        .wrap_err("canonicalize workspace root")?;
    let path = Path::new(requested);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let path = path
        .canonicalize()
        .wrap_err_with(|| format!("read input file {requested}"))?;
    if !path.starts_with(&root) || !path.is_file() {
        bail!("input file must stay inside the session workspace: {requested}");
    }
    let metadata = path.metadata().wrap_err("stat input file")?;
    if metadata.len() > 32 * 1024 * 1024 {
        bail!("input image exceeds the 32 MiB limit: {requested}");
    }
    let bytes = std::fs::read(&path).wrap_err_with(|| format!("read {}", path.display()))?;
    Ok((path, bytes))
}

pub(crate) fn validate_xml_text(text: &str, description: &str) -> Result<()> {
    if text.chars().any(|c| {
        !matches!(
            c,
            '\u{9}' | '\u{a}' | '\u{d}'
                | '\u{20}'..='\u{d7ff}'
                | '\u{e000}'..='\u{fffd}'
                | '\u{10000}'..='\u{10ffff}'
        )
    }) {
        bail!("{description} contains a character not allowed in XML");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn assert_well_formed_xml(xml_text: &str) {
    let mut reader = quick_xml::Reader::from_str(xml_text);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(error) => panic!("malformed XML: {error}\n{xml_text}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_not_create_parent_directories_outside_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().parent().unwrap().join(format!(
            "office-outside-{}",
            temp.path().file_name().unwrap().to_string_lossy()
        ));
        let output = format!(
            "../{}/nested/report.docx",
            outside.file_name().unwrap().to_string_lossy()
        );
        let error = output_path(temp.path(), &output, "docx").unwrap_err();
        assert!(error.to_string().contains("workspace"));
        assert!(!outside.exists());
    }
}
