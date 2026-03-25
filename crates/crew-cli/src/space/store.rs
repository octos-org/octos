//! JSON file persistence for spaces.
//!
//! Each space is stored as `{data_dir}/spaces/{id}.json`.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

use super::types::Space;

/// Persistent store for spaces.
pub struct SpaceStore {
    spaces_dir: PathBuf,
}

impl SpaceStore {
    /// Open (or create) the space store at `data_dir/spaces/`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let spaces_dir = data_dir.join("spaces");
        std::fs::create_dir_all(&spaces_dir)
            .wrap_err_with(|| format!("failed to create spaces dir: {}", spaces_dir.display()))?;
        Ok(Self { spaces_dir })
    }

    /// List all spaces.
    pub fn list(&self) -> Result<Vec<Space>> {
        let mut spaces = Vec::new();
        let entries = match std::fs::read_dir(&self.spaces_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(spaces),
            Err(e) => return Err(e).wrap_err("failed to read spaces directory"),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Space>(&content) {
                        Ok(space) => spaces.push(space),
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "skipping invalid space");
                        }
                    },
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to read space");
                    }
                }
            }
        }
        spaces.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(spaces)
    }

    /// Get a single space by ID.
    pub fn get(&self, id: &str) -> Result<Option<Space>> {
        let path = self.space_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content =
            std::fs::read_to_string(&path).wrap_err_with(|| format!("failed to read space: {id}"))?;
        let space =
            serde_json::from_str(&content).wrap_err_with(|| format!("failed to parse space: {id}"))?;
        Ok(Some(space))
    }

    /// Save a space (create or update).
    pub fn save(&self, space: &Space) -> Result<()> {
        let path = self.space_path(&space.id);
        let content = serde_json::to_string_pretty(space).wrap_err("failed to serialize space")?;
        std::fs::write(&path, &content)
            .wrap_err_with(|| format!("failed to write space: {}", path.display()))?;
        Ok(())
    }

    /// Delete a space by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let path = self.space_path(id);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).wrap_err_with(|| format!("failed to delete space: {id}"))?;
        Ok(true)
    }

    fn space_path(&self, id: &str) -> PathBuf {
        self.spaces_dir.join(format!("{id}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_space(id: &str) -> Space {
        Space {
            id: id.to_string(),
            name: format!("Space {id}"),
            description: String::new(),
            owner_id: "test-user".to_string(),
            member_ids: vec![],
            notebook_ids: vec![],
            created_at: Utc::now(),
        }
    }

    #[test]
    fn should_save_and_load_space() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpaceStore::open(dir.path()).unwrap();
        let sp = make_space("sp-1");
        store.save(&sp).unwrap();
        let loaded = store.get("sp-1").unwrap().unwrap();
        assert_eq!(loaded.name, "Space sp-1");
    }

    #[test]
    fn should_list_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpaceStore::open(dir.path()).unwrap();
        store.save(&make_space("a")).unwrap();
        store.save(&make_space("b")).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn should_delete_space() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpaceStore::open(dir.path()).unwrap();
        store.save(&make_space("del")).unwrap();
        assert!(store.delete("del").unwrap());
        assert!(store.get("del").unwrap().is_none());
    }
}
