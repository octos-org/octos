use crate::Context;
use anyhow::{Context as _, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

impl Context {
    pub fn path(&self, p: impl AsRef<Path>) -> Result<PathBuf> {
        let p = p.as_ref();
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        ensure!(
            joined.starts_with(&self.root),
            "path is outside the workspace"
        );
        if joined.exists() {
            let real = joined.canonicalize()?;
            ensure!(real.starts_with(&self.root), "path escapes workspace");
            return Ok(real);
        }
        let mut ancestor = joined.clone();
        while !ancestor.exists() {
            // Reject dangling symlinks too.
            ensure!(
                fs::symlink_metadata(&ancestor).is_err(),
                "unresolved path is a symlink"
            );
            ensure!(ancestor.pop(), "path has no existing ancestor");
        }
        let real = ancestor.canonicalize()?;
        ensure!(real.starts_with(&self.root), "symlink escapes workspace");
        let suffix = joined.strip_prefix(&ancestor)?;
        ensure!(
            !suffix
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
            "unresolved parent path"
        );
        Ok(real.join(suffix))
    }
    pub fn read(&self, p: impl AsRef<Path>) -> Result<Vec<u8>> {
        let p = self.path(p)?;
        ensure!(p.is_file(), "not a regular file: {}", p.display());
        ensure!(
            fs::metadata(&p)?.len() <= 256 * 1024 * 1024,
            "file exceeds 256 MiB"
        );
        Ok(fs::read(p)?)
    }
    pub fn json(&self, p: impl AsRef<Path>) -> Result<Value> {
        Ok(serde_json::from_slice(&self.read(p)?)?)
    }
    pub fn write_new(&self, p: impl AsRef<Path>, bytes: &[u8]) -> Result<PathBuf> {
        let p = self.path(p)?;
        let parent = p
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
        fs::create_dir_all(parent)?;
        self.path(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(&p).map_err(|e| e.error)?;
        Ok(p)
    }
    pub fn write_json(&self, p: impl AsRef<Path>, v: &Value) -> Result<PathBuf> {
        self.write_new(p, &serde_json::to_vec_pretty(v)?)
    }
    pub fn replace_json(&self, p: impl AsRef<Path>, v: &Value) -> Result<()> {
        let p = self.path(p)?;
        let parent = p
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
        fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(&serde_json::to_vec_pretty(v)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(p).map_err(|e| e.error)?;
        Ok(())
    }
    pub fn binding(&self, p: impl AsRef<Path>) -> Result<Value> {
        let p = self.path(p)?;
        Ok(
            serde_json::json!({"path":p.strip_prefix(&self.root)?.to_string_lossy(),"sha256":sha_file(&p)?}),
        )
    }
    pub fn verify(&self, b: &Value) -> Result<PathBuf> {
        let p = self.path(crate::string(b, "path")?)?;
        ensure!(
            sha_file(&p)? == crate::string(b, "sha256")?,
            "artifact hash changed: {}",
            p.display()
        );
        Ok(p)
    }
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn sha_file(p: &Path) -> Result<String> {
    let mut f =
        fs::File::open(p).with_context(|| format!("cannot open artifact {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("cannot read artifact {}", p.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
