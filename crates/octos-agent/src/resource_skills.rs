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
