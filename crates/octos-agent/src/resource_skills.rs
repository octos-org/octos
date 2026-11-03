//! Account-local installation of versioned instructions and portable helpers.
use std::{collections::BTreeMap, fs, path::Path};

use eyre::{Result, WrapErr, ensure};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/resource_skills.rs"));

pub(crate) const DIRECTORY: &str = "bundled-resource-skills";
const MARKER: &str = ".resource-bundle-sha256";
type Files<'a> = Vec<(&'a str, &'a [u8])>;

fn require_plain_path(root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root)?;
    let mut current = root.to_path_buf();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            current.push(component);
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "symlinked managed skill path: {}",
                current.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn install_one(base: &Path, backups: &Path, name: &str, files: &Files<'_>) -> Result<bool> {
    let target = base.join(name);
    require_plain_path(base, &target)?;
    let mut digest = Sha256::new();
    for &(path, bytes) in files {
        digest.update(path.as_bytes());
        digest.update([0]);
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
        require_plain_path(base, &target.join(path))?;
    }
    require_plain_path(base, &target.join(MARKER))?;
    let version = format!("{:x}", digest.finalize());
    if fs::read_to_string(target.join(MARKER)).is_ok_and(|v| v == version)
        && files
            .iter()
            .all(|(path, bytes)| fs::read(target.join(path)).is_ok_and(|v| v == *bytes))
    {
        return Ok(false);
    }

    let stage = tempfile::Builder::new()
        .prefix(&format!(".{name}-stage-"))
        .tempdir_in(base)?;
    for &(path, bytes) in files {
        let output = stage.path().join(path);
        let parent = output
            .parent()
            .ok_or_else(|| eyre::eyre!("resource path has no parent: {}", output.display()))?;
        fs::create_dir_all(parent)?;
        fs::write(output, bytes)?;
    }
    fs::write(stage.path().join(MARKER), version)?;
    let previous = if target.exists() {
        require_plain_path(backups, backups)?;
        fs::create_dir_all(backups)?;
        let holder = tempfile::Builder::new()
            .prefix(&format!("{name}-"))
            .tempdir_in(backups)?;
        let backup = holder.path().join("previous");
        fs::rename(&target, &backup)?;
        let _ = holder.keep(); // Preserve the whole previous version, including local edits.
        Some(backup)
    } else {
        None
    };
    if let Err(error) = fs::rename(stage.path(), &target) {
        if let Some(previous) = previous {
            fs::rename(previous, &target).wrap_err("restore previous resource skill")?;
        }
        return Err(error.into());
    }
    Ok(true)
}

pub(crate) fn bootstrap(data_dir: &Path) -> Result<usize> {
    let base = data_dir.join(DIRECTORY);
    require_plain_path(data_dir, &base)?;
    fs::create_dir_all(&base)?;
    let lock_path = base.join(".install.lock");
    require_plain_path(&base, &lock_path)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut packages: BTreeMap<&str, Files<'_>> = BTreeMap::new();
    for &(path, bytes) in RESOURCE_FILES {
        let (name, relative) = path
            .split_once('/')
            .expect("package-relative resource path");
        packages.entry(name).or_default().push((relative, bytes));
    }
    let mut installed = 0;
    for (name, files) in packages {
        ensure!(
            files.iter().any(|(p, _)| *p == "SKILL.md"),
            "missing skill entrypoint: {name}"
        );
        installed += usize::from(install_one(
            &base,
            &data_dir.join("skill-backups"),
            name,
            &files,
        )?);
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_reuse_repair_and_backup_resource_bundle() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(bootstrap(dir.path()).unwrap(), 2);
        assert_eq!(bootstrap(dir.path()).unwrap(), 0);
        let script = dir
            .path()
            .join(DIRECTORY)
            .join("pptx/scripts/native_deck.py");
        fs::remove_file(&script).unwrap();
        assert_eq!(bootstrap(dir.path()).unwrap(), 1);
        assert!(script.is_file());
        assert_eq!(
            fs::read_dir(dir.path().join("skill-backups"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn should_preserve_previous_files_on_resource_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("resources");
        let backup = dir.path().join("backups");
        fs::create_dir_all(&base).unwrap();
        let old = vec![("SKILL.md", &b"v1"[..]), ("obsolete.py", &b"old"[..])];
        let new = vec![("SKILL.md", &b"v2"[..]), ("new.py", &b"new"[..])];
        assert!(install_one(&base, &backup, "test", &old).unwrap());
        fs::write(base.join("test/local.txt"), "local change").unwrap();
        assert!(install_one(&base, &backup, "test", &new).unwrap());
        assert!(!base.join("test/obsolete.py").exists());
        let saved = fs::read_dir(backup)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("previous");
        assert_eq!(
            fs::read_to_string(saved.join("local.txt")).unwrap(),
            "local change"
        );
        assert_eq!(fs::read_to_string(saved.join("SKILL.md")).unwrap(), "v1");
    }

    #[test]
    fn should_serialize_concurrent_resource_installation() {
        let dir = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = dir.path().to_path_buf();
                std::thread::spawn(move || bootstrap(&path).unwrap())
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .sum::<usize>(),
            2
        );
        assert_eq!(bootstrap(dir.path()).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_symlinked_resource_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(DIRECTORY)).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(DIRECTORY).join("pptx"))
            .unwrap();
        assert!(bootstrap(dir.path()).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
