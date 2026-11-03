//! Durable, per-profile addresses of project session stores. Never transcripts
//! or permission grants; readers must validate a saved path before using it.
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::Path;
#[cfg(any(feature = "api", test))]
use std::path::PathBuf;

pub(crate) fn remember(data_dir: &Path, workspace: &Path) -> io::Result<()> {
    let workspace = dunce::canonicalize(workspace)?;
    if !workspace.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace is not a directory",
        ));
    }
    let bytes = serde_json::to_vec(&workspace)?;
    // Independent atomic records avoid read/modify/write races between
    // separate terminal and server processes, without a long-lived lock.
    let dir = data_dir.join("session-workspaces");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:x}.json", Sha256::digest(&bytes)));
    if std::fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(any(feature = "api", test))]
pub(crate) fn load(data_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(data_dir.join("session-workspaces")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut roots = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        if entry.metadata()?.len() > 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace address is too large",
            ));
        }
        let root: PathBuf = serde_json::from_slice(&std::fs::read(entry.path())?)?;
        if root.is_absolute() {
            roots.push(root);
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_preserve_workspace_addresses_without_process_state() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("profile");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let canonical = dunce::canonicalize(&workspace).unwrap();
        remember(&data, &workspace).unwrap();
        remember(&data, &canonical).unwrap();
        assert_eq!(load(&data).unwrap(), vec![canonical]);
        assert!(load(&temp.path().join("other-profile")).unwrap().is_empty());
    }

    #[test]
    fn should_keep_simultaneous_registrations_of_different_projects() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("profile");
        let mut expected = Vec::new();
        std::thread::scope(|scope| {
            for i in 0..12 {
                let workspace = temp.path().join(format!("project-{i}"));
                std::fs::create_dir_all(&workspace).unwrap();
                expected.push(dunce::canonicalize(&workspace).unwrap());
                let data = &data;
                scope.spawn(move || remember(data, &workspace).unwrap());
            }
        });
        expected.sort();
        assert_eq!(load(&data).unwrap(), expected);
    }
}
