//! JSON file persistence for notebooks.
//!
//! Each notebook is stored as a single JSON file: `{data_dir}/notebooks/{id}.json`
//! containing the notebook metadata, sources (with chunks), and notes.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

use super::types::Notebook;

/// Persistent store for notebooks.
pub struct NotebookStore {
    notebooks_dir: PathBuf,
}

impl NotebookStore {
    /// Open (or create) the notebook store at `data_dir/notebooks/`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let notebooks_dir = data_dir.join("notebooks");
        std::fs::create_dir_all(&notebooks_dir).wrap_err_with(|| {
            format!(
                "failed to create notebooks dir: {}",
                notebooks_dir.display()
            )
        })?;
        Ok(Self { notebooks_dir })
    }

    /// List all notebooks (without sources/notes detail for the list view).
    pub fn list(&self) -> Result<Vec<Notebook>> {
        let mut notebooks = Vec::new();
        let entries = match std::fs::read_dir(&self.notebooks_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(notebooks),
            Err(e) => return Err(e).wrap_err("failed to read notebooks directory"),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Notebook>(&content) {
                        Ok(nb) => notebooks.push(nb),
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "skipping invalid notebook");
                        }
                    },
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to read notebook");
                    }
                }
            }
        }
        notebooks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(notebooks)
    }

    /// Get a single notebook by ID.
    pub fn get(&self, id: &str) -> Result<Option<Notebook>> {
        let path = self.notebook_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read notebook: {id}"))?;
        let nb =
            serde_json::from_str(&content).wrap_err_with(|| format!("failed to parse notebook: {id}"))?;
        Ok(Some(nb))
    }

    /// Save a notebook (create or update).
    pub fn save(&self, notebook: &Notebook) -> Result<()> {
        let path = self.notebook_path(&notebook.id);
        let content =
            serde_json::to_string_pretty(notebook).wrap_err("failed to serialize notebook")?;
        std::fs::write(&path, &content)
            .wrap_err_with(|| format!("failed to write notebook: {}", path.display()))?;
        Ok(())
    }

    /// Delete a notebook by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let path = self.notebook_path(id);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path)
            .wrap_err_with(|| format!("failed to delete notebook: {id}"))?;
        Ok(true)
    }

    /// Return the file path for a notebook ID.
    fn notebook_path(&self, id: &str) -> PathBuf {
        self.notebooks_dir.join(format!("{id}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::types::*;
    use chrono::Utc;

    fn make_notebook(id: &str) -> Notebook {
        Notebook {
            id: id.to_string(),
            title: format!("Test {id}"),
            description: String::new(),
            cover_image: None,
            source_count: 0,
            note_count: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            owner_id: "test-user".to_string(),
            sources: vec![],
            notes: vec![],
            shared_with: vec![],
            book_meta: None,
            copyright_protected: false,
        }
    }

    #[test]
    fn should_save_and_load_notebook() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotebookStore::open(dir.path()).unwrap();
        let nb = make_notebook("nb-1");
        store.save(&nb).unwrap();
        let loaded = store.get("nb-1").unwrap().unwrap();
        assert_eq!(loaded.title, "Test nb-1");
    }

    #[test]
    fn should_list_notebooks() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotebookStore::open(dir.path()).unwrap();
        store.save(&make_notebook("a")).unwrap();
        store.save(&make_notebook("b")).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn should_delete_notebook() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotebookStore::open(dir.path()).unwrap();
        store.save(&make_notebook("del")).unwrap();
        assert!(store.delete("del").unwrap());
        assert!(store.get("del").unwrap().is_none());
    }

    #[test]
    fn should_return_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotebookStore::open(dir.path()).unwrap();
        assert!(store.get("nonexistent").unwrap().is_none());
    }
}
