//! Episode store: persistent storage for episodes using redb (pure Rust).

use std::path::Path;
use std::sync::{Arc, RwLock};

use eyre::{Result, WrapErr};
use redb::{Database, ReadableTable, TableDefinition};
use tracing::{debug, warn};

use crate::episode::Episode;
use crate::hybrid_search::HybridIndex;

/// Table for episodes: key = episode_id, value = JSON
const EPISODES_TABLE: TableDefinition<&str, &str> = TableDefinition::new("episodes");

/// Index table for episodes by working directory: key = cwd, value = list of episode IDs (JSON)
const CWD_INDEX_TABLE: TableDefinition<&str, &str> = TableDefinition::new("cwd_index");

/// Table for episode embeddings: key = episode_id, value = bincode-serialized Vec<f32>
const EMBEDDINGS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("embeddings");

/// Default embedding dimension (OpenAI text-embedding-3-small).
const DEFAULT_DIMENSION: usize = 1536;

/// Store for episodes using redb (pure Rust embedded database).
pub struct EpisodeStore {
    db: Arc<Database>,
    index: RwLock<HybridIndex>,
}

impl EpisodeStore {
    /// Open or create an episode store at the given path.
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&data_dir)
            .await
            .wrap_err("failed to create data directory")?;

        let db_path = data_dir.join("episodes.redb");

        // redb is sync, so we spawn_blocking for the initial open + table init + index rebuild
        let (db, index) = tokio::task::spawn_blocking(move || {
            let db = Database::create(&db_path).wrap_err("failed to open redb database")?;

            // Initialize tables
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(EPISODES_TABLE)?;
                let _ = write_txn.open_table(CWD_INDEX_TABLE)?;
                let _ = write_txn.open_table(EMBEDDINGS_TABLE)?;
            }
            write_txn.commit()?;

            // Rebuild in-memory hybrid index from stored data
            let mut index = HybridIndex::new(DEFAULT_DIMENSION);
            {
                let read_txn = db.begin_read()?;
                let episodes_table = read_txn.open_table(EPISODES_TABLE)?;
                let embeddings_table = read_txn.open_table(EMBEDDINGS_TABLE)?;

                for entry in episodes_table.iter()? {
                    let (key, value) = entry?;
                    let ep_id = key.value().to_string();
                    if let Ok(episode) = serde_json::from_str::<Episode>(value.value()) {
                        let embedding: Option<Vec<f32>> = embeddings_table
                            .get(ep_id.as_str())
                            .ok()
                            .flatten()
                            .and_then(|v| bincode::deserialize(v.value()).ok());
                        index.insert(&ep_id, &episode.summary, embedding.as_deref());
                    }
                }
            }

            debug!(path = %data_dir.display(), "opened episode store");
            Ok::<_, eyre::Report>((db, index))
        })
        .await??;

        Ok(Self {
            db: Arc::new(db),
            index: RwLock::new(index),
        })
    }

    /// Store an episode.
    pub async fn store(&self, episode: Episode) -> Result<()> {
        let db = self.db.clone();
        let episode_id = episode.id.clone();
        let episode_id_for_index = episode_id.clone();
        let summary = episode.summary.clone();
        let cwd = episode.working_dir.to_string_lossy().to_string();
        let episode_json =
            serde_json::to_string(&episode).wrap_err("failed to serialize episode")?;

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write()?;
            {
                // Store episode
                let mut table = write_txn.open_table(EPISODES_TABLE)?;
                table.insert(episode_id.as_str(), episode_json.as_str())?;

                // Update cwd index
                let mut index = write_txn.open_table(CWD_INDEX_TABLE)?;
                let existing: Vec<String> = index
                    .get(cwd.as_str())?
                    .map(|v| match serde_json::from_str(v.value()) {
                        Ok(val) => val,
                        Err(e) => {
                            // Don't replace with empty Vec — try to salvage by
                            // extracting any quoted strings that look like episode IDs.
                            let raw = v.value();
                            warn!("failed to parse cwd index JSON ({} bytes): {e}; attempting salvage", raw.len());
                            let salvaged: Vec<String> = raw
                                .split('"')
                                .enumerate()
                                .filter_map(|(i, s)| {
                                    // Odd indices are inside quotes in valid JSON arrays
                                    if i % 2 == 1 && !s.is_empty() && !s.contains(['[', ']', ',']) {
                                        Some(s.to_string())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            warn!("salvaged {} episode IDs from corrupted index", salvaged.len());
                            salvaged
                        }
                    })
                    .unwrap_or_default();

                let mut ids = existing;
                if !ids.contains(&episode_id) {
                    ids.push(episode_id);
                }
                let ids_json = serde_json::to_string(&ids)?;
                index.insert(cwd.as_str(), ids_json.as_str())?;
            }
            write_txn.commit()?;
            Ok::<_, eyre::Report>(())
        })
        .await??;

        // Update in-memory hybrid index (text only, no embedding yet)
        match self.index.write() {
            Ok(mut idx) => idx.insert(&episode_id_for_index, &summary, None),
            Err(e) => warn!("index write lock poisoned, skipping update: {e}"),
        }

        Ok(())
    }

    /// Find episodes relevant to a query in the given directory.
    pub async fn find_relevant(
        &self,
        cwd: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let db = self.db.clone();
        let cwd_str = cwd.to_string_lossy().to_string();
        let query = query.to_lowercase();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let episodes_table = read_txn.open_table(EPISODES_TABLE)?;
            let index_table = read_txn.open_table(CWD_INDEX_TABLE)?;

            // Get episode IDs for this cwd
            let episode_ids: Vec<String> = index_table
                .get(cwd_str.as_str())?
                .map(|v| match serde_json::from_str(v.value()) {
                    Ok(val) => val,
                    Err(e) => {
                        let raw = v.value();
                        warn!("failed to parse episode index JSON ({} bytes): {e}; attempting salvage", raw.len());
                        raw.split('"')
                            .enumerate()
                            .filter_map(|(i, s)| {
                                if i % 2 == 1 && !s.is_empty() && !s.contains(['[', ']', ',']) {
                                    Some(s.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect()
                    }
                })
                .unwrap_or_default();

            // Tokenize query consistently (#127): split on non-alphanumeric, filter short tokens
            let terms: Vec<String> = query
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 2)
                .map(|w| w.to_string())
                .collect();

            // Load and filter episodes
            let mut results: Vec<(Episode, usize)> = Vec::new();

            for id in episode_ids {
                if let Some(json) = episodes_table.get(id.as_str())? {
                    if let Ok(episode) = serde_json::from_str::<Episode>(json.value()) {
                        // Tokenize summary the same way for word-boundary matching (#130)
                        let summary_tokens: Vec<String> = episode
                            .summary
                            .to_lowercase()
                            .split(|c: char| !c.is_alphanumeric())
                            .filter(|w| w.len() >= 2)
                            .map(|w| w.to_string())
                            .collect();
                        let relevance = terms
                            .iter()
                            .filter(|term| summary_tokens.contains(term))
                            .count();

                        if relevance > 0 {
                            results.push((episode, relevance));
                        }
                    }
                }
            }

            // Sort by relevance (descending) then by date (descending)
            results.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| b.0.created_at.cmp(&a.0.created_at))
            });

            Ok(results.into_iter().take(limit).map(|(e, _)| e).collect())
        })
        .await?
    }

    /// Get the N most recent episodes for a directory.
    ///
    /// Retained for admin API use (e.g. dashboard episode browser).
    #[allow(dead_code)]
    pub async fn recent_for_cwd(&self, cwd: &Path, n: usize) -> Result<Vec<Episode>> {
        let db = self.db.clone();
        let cwd_str = cwd.to_string_lossy().to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let episodes_table = read_txn.open_table(EPISODES_TABLE)?;
            let index_table = read_txn.open_table(CWD_INDEX_TABLE)?;

            // Get episode IDs for this cwd
            let episode_ids: Vec<String> = index_table
                .get(cwd_str.as_str())?
                .map(|v| match serde_json::from_str(v.value()) {
                    Ok(val) => val,
                    Err(e) => {
                        warn!("failed to parse episode index JSON: {e}");
                        Vec::new()
                    }
                })
                .unwrap_or_default();

            // Load episodes
            let mut episodes: Vec<Episode> = Vec::new();

            for id in episode_ids {
                if let Some(json) = episodes_table.get(id.as_str())? {
                    if let Ok(episode) = serde_json::from_str::<Episode>(json.value()) {
                        episodes.push(episode);
                    }
                }
            }

            // Sort by created_at descending
            episodes.sort_by(|a, b| b.created_at.cmp(&a.created_at));

            Ok(episodes.into_iter().take(n).collect())
        })
        .await?
    }

    /// Get an episode by ID.
    ///
    /// Retained for admin API use (e.g. episode detail endpoint).
    #[allow(dead_code)]
    pub async fn get(&self, id: &str) -> Result<Option<Episode>> {
        let db = self.db.clone();
        let id = id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(EPISODES_TABLE)?;

            if let Some(json) = table.get(id.as_str())? {
                let episode: Episode = serde_json::from_str(json.value())?;
                Ok(Some(episode))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    /// Store an embedding for an episode.
    pub async fn store_embedding(&self, episode_id: &str, embedding: Vec<f32>) -> Result<()> {
        let db = self.db.clone();
        let ep_id = episode_id.to_string();
        let emb_bytes = bincode::serialize(&embedding).wrap_err("failed to serialize embedding")?;

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(EMBEDDINGS_TABLE)?;
                table.insert(ep_id.as_str(), emb_bytes.as_slice())?;
            }
            write_txn.commit()?;
            Ok::<_, eyre::Report>(())
        })
        .await??;

        // Attach embedding to the existing in-memory index entry.
        match self.index.write() {
            Ok(mut idx) => {
                idx.add_embedding(episode_id, &embedding);
            }
            Err(e) => warn!("index write lock poisoned, skipping embedding update: {e}"),
        }

        Ok(())
    }

    /// Hybrid search across all episodes (not cwd-scoped).
    pub async fn find_relevant_hybrid(
        &self,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        // Search the in-memory index
        let matches = {
            let idx = self
                .index
                .read()
                .map_err(|e| eyre::eyre!("index lock poisoned: {e}"))?;
            idx.search(query, query_embedding.as_deref(), limit)
        };

        // Fetch full episodes from DB
        let db = self.db.clone();
        let ids: Vec<String> = matches.into_iter().map(|(id, _)| id).collect();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(EPISODES_TABLE)?;

            let mut episodes = Vec::new();
            for id in &ids {
                if let Some(json) = table.get(id.as_str())? {
                    if let Ok(episode) = serde_json::from_str::<Episode>(json.value()) {
                        episodes.push(episode);
                    }
                }
            }

            // Preserve the ranking order from hybrid search
            let id_order: std::collections::HashMap<&str, usize> = ids
                .iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), i))
                .collect();
            episodes.sort_by_key(|e| id_order.get(e.id.as_str()).copied().unwrap_or(usize::MAX));

            Ok(episodes)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::episode::{Episode, EpisodeOutcome};
    use octos_core::{AgentId, TaskId};
    use std::path::PathBuf;

    fn make_episode(summary: &str, cwd: &str) -> Episode {
        Episode::new(
            TaskId::new(),
            AgentId::new("test-agent"),
            PathBuf::from(cwd),
            summary.into(),
            EpisodeOutcome::Success,
        )
    }

    #[tokio::test]
    async fn test_open_creates_db() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();
        // Verify we can get a nonexistent episode
        let result = store.get("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_store_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let ep = make_episode("Fixed parser bug", "/tmp/project");
        let ep_id = ep.id.clone();

        store.store(ep).await.unwrap();

        let retrieved = store.get(&ep_id).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, ep_id);
        assert_eq!(retrieved.summary, "Fixed parser bug");
    }

    #[tokio::test]
    async fn test_recent_for_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let ep1 = make_episode("first task", "/project");
        let ep2 = make_episode("second task", "/project");
        let ep3 = make_episode("other dir task", "/other");

        store.store(ep1).await.unwrap();
        store.store(ep2).await.unwrap();
        store.store(ep3).await.unwrap();

        let recent = store
            .recent_for_cwd(Path::new("/project"), 10)
            .await
            .unwrap();
        assert_eq!(recent.len(), 2);

        let other = store.recent_for_cwd(Path::new("/other"), 10).await.unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].summary, "other dir task");
    }

    #[tokio::test]
    async fn test_recent_for_cwd_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        for i in 0..5 {
            store
                .store(make_episode(&format!("task {i}"), "/proj"))
                .await
                .unwrap();
        }

        let recent = store.recent_for_cwd(Path::new("/proj"), 3).await.unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[tokio::test]
    async fn test_find_relevant() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        store
            .store(make_episode("Fixed parser bug in tokenizer", "/proj"))
            .await
            .unwrap();
        store
            .store(make_episode("Added new API endpoint", "/proj"))
            .await
            .unwrap();
        store
            .store(make_episode("Refactored parser module", "/proj"))
            .await
            .unwrap();

        let results = store
            .find_relevant(Path::new("/proj"), "parser", 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_find_relevant_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        store
            .store(make_episode("Fixed UI layout", "/proj"))
            .await
            .unwrap();

        let results = store
            .find_relevant(Path::new("/proj"), "database", 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_store_embedding_and_hybrid_search() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let ep = make_episode("Implemented vector search", "/proj");
        let ep_id = ep.id.clone();
        store.store(ep).await.unwrap();

        // Store a dummy embedding
        let embedding = vec![0.1f32; 1536];
        store
            .store_embedding(&ep_id, embedding.clone())
            .await
            .unwrap();

        // Hybrid search (text only, no query embedding)
        let results = store
            .find_relevant_hybrid("vector search", None, 10)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, ep_id);
    }

    #[tokio::test]
    async fn test_reopen_persists_data() {
        let dir = tempfile::tempdir().unwrap();
        let ep_id;
        {
            let store = EpisodeStore::open(dir.path()).await.unwrap();
            let ep = make_episode("persistent data", "/proj");
            ep_id = ep.id.clone();
            store.store(ep).await.unwrap();
        }
        // Reopen
        let store = EpisodeStore::open(dir.path()).await.unwrap();
        let retrieved = store.get(&ep_id).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().summary, "persistent data");
    }
}
