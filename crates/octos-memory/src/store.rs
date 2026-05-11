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
///
/// # Degraded mode
///
/// `redb` is a single-writer-single-process embedded database — only
/// one process at a time may hold the OS file lock on
/// `episodes.redb`. In the production fleet `octos serve` and
/// `octos gateway` run as separate processes that bootstrap
/// `ProfileRuntime` independently per profile, and both call
/// `EpisodeStore::open` against the same path. The first opener (the
/// long-lived `octos serve` daemon) wins; the second opener (`octos
/// gateway` subprocesses) previously crashed with
/// `redb::DatabaseError::DatabaseAlreadyOpen` ("Database already
/// open. Cannot acquire lock."), launchd restarted it, and the cycle
/// repeated every ~2 seconds.
///
/// To prevent that crashloop without sacrificing serve-mode
/// persistence, [`EpisodeStore::open_or_degraded`] falls back to a
/// **degraded** in-memory store when the redb file is already locked:
/// `db` is `None`, all mutating operations silently no-op, and all
/// public read operations return empty. Callers that need to observe
/// the degradation (logging, metrics) can read [`Self::is_degraded`].
///
/// Two opener entry points formalize the role split:
/// - [`Self::open`] (strict) — fails when the lock is already held.
///   The right choice for the process that *must* own the canonical
///   store (`octos serve`, `octos chat`, the test suite). Surfaces
///   deployment misconfigurations as errors instead of silently
///   degrading the canonical writer.
/// - [`Self::open_or_degraded`] — falls back to the degraded handle
///   on lock contention. The right choice for `octos gateway`
///   subprocesses (and other companion processes) that should keep
///   going when the canonical store is owned elsewhere.
///
/// This split prevents the gateway-starts-first dev workflow from
/// flipping canonical ownership to gateway and degrading serve.
///
/// Episode reads on the gateway path are best-effort already
/// (memory-bank recall happens through the in-process `MemoryStore`,
/// not `EpisodeStore`), and episode writes from a sub-agent are
/// completion-summary persistence that serve will redo on
/// `/api/chat` if it cares. The fleet still benefits from gateway
/// channel polling staying alive.
pub struct EpisodeStore {
    /// `Some(db)` when the redb file was successfully opened (the
    /// owning process). `None` when this is a degraded in-memory
    /// fallback because the redb file lock was already held by
    /// another process (see the type-level docs).
    db: Option<Arc<Database>>,
    index: RwLock<HybridIndex>,
}

impl EpisodeStore {
    /// Open or create an episode store at the given path.
    ///
    /// **Strict mode** — fails if the redb file lock is already held
    /// by another process. This is the right entry point for the
    /// process that *must* own the canonical store (`octos serve`,
    /// `octos chat`, the agent test suite). If the lock is held, the
    /// caller likely has a deployment misconfiguration and should
    /// surface the error rather than silently degrading.
    ///
    /// For the `octos gateway` subprocess (and anywhere else that
    /// runs alongside an existing serve daemon and should keep going
    /// when the canonical store is owned elsewhere), use
    /// [`Self::open_or_degraded`].
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(data_dir.as_ref(), false).await
    }

    /// Open or create an episode store at the given path, falling
    /// back to a degraded in-memory store when the redb file lock is
    /// already held by another process.
    ///
    /// This is the right entry point for the `octos gateway`
    /// subprocess: `octos serve` always wins the lock in production
    /// (process_manager spawns gateway after `ProfileRuntime` has
    /// already bootstrapped every profile), so gateway always gets a
    /// degraded handle when serve is the parent daemon. Writes on a
    /// degraded handle silently no-op; reads return empty. See the
    /// type-level docs on [`EpisodeStore`].
    ///
    /// **Other failure modes still bubble up.** Only the typed
    /// `redb::DatabaseError::DatabaseAlreadyOpen` triggers the
    /// degraded fallback; corruption / I/O / permission errors are
    /// returned as `Err`.
    pub async fn open_or_degraded(data_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(data_dir.as_ref(), true).await
    }

    async fn open_inner(data_dir: &Path, allow_degraded: bool) -> Result<Self> {
        let data_dir = data_dir.to_path_buf();
        tokio::fs::create_dir_all(&data_dir)
            .await
            .wrap_err("failed to create data directory")?;

        let db_path = data_dir.join("episodes.redb");
        let db_path_for_log = db_path.clone();

        // redb is sync, so we spawn_blocking for the initial open + table init + index rebuild.
        //
        // We surface the `DatabaseAlreadyOpen` case as a typed sentinel
        // (`Ok(None)`) so the outer task can decide whether to install
        // the degraded fallback (gateway) or propagate the error
        // (serve). Every other error bubbles up verbatim.
        let result: Result<Option<(Database, HybridIndex)>> =
            tokio::task::spawn_blocking(move || {
                let db = match Database::create(&db_path) {
                    Ok(db) => db,
                    Err(redb::DatabaseError::DatabaseAlreadyOpen) => return Ok(None),
                    Err(e) => {
                        return Err(eyre::Report::new(e).wrap_err("failed to open redb database"));
                    }
                };

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
                Ok(Some((db, index)))
            })
            .await?;

        match result? {
            Some((db, index)) => Ok(Self {
                db: Some(Arc::new(db)),
                index: RwLock::new(index),
            }),
            None if allow_degraded => {
                warn!(
                    path = %db_path_for_log.display(),
                    "redb episode store already held by another process; \
                     installing degraded in-memory fallback. Writes will \
                     no-op and reads will return empty for this handle. \
                     This is expected for `octos gateway` subprocesses \
                     when `octos serve` already owns the lock."
                );
                Ok(Self {
                    db: None,
                    index: RwLock::new(HybridIndex::new(DEFAULT_DIMENSION)),
                })
            }
            None => Err(eyre::eyre!(
                "failed to open redb database at {}: \
                 Database already open. Cannot acquire lock. \
                 (Strict `EpisodeStore::open` was used — if this is the \
                 `octos gateway` subprocess, call `open_or_degraded` \
                 instead.)",
                db_path_for_log.display(),
            )),
        }
    }

    /// `true` when this store is operating in the degraded in-memory
    /// fallback mode described on the type-level docs (the redb file
    /// lock was already held when [`Self::open_or_degraded`] ran).
    /// Callers can use this for diagnostics, metrics, or to skip
    /// persistence-dependent codepaths.
    pub fn is_degraded(&self) -> bool {
        self.db.is_none()
    }

    /// Store an episode.
    ///
    /// In degraded mode ([`Self::is_degraded`]) the disk write is
    /// skipped and the call returns `Ok(())`. The in-memory hybrid
    /// index is updated with the summary so [`Self::find_relevant_hybrid`]
    /// (which is index-only when populated) can match it for ranking,
    /// but full episode bodies come from disk — so the public read
    /// methods ([`Self::find_relevant`], [`Self::find_relevant_hybrid`])
    /// still return empty on a degraded handle because there is no
    /// DB to fetch bodies from. The intent is "writes accepted, reads
    /// empty," not in-memory persistence.
    pub async fn store(&self, episode: Episode) -> Result<()> {
        let episode_id = episode.id.clone();
        let episode_id_for_index = episode_id.clone();
        let summary = episode.summary.clone();

        // Degraded fallback: skip the disk write, update the in-memory
        // index, return success. See type-level docs on `EpisodeStore`.
        let Some(db) = self.db.clone() else {
            match self.index.write() {
                Ok(mut idx) => idx.insert(&episode_id_for_index, &summary, None),
                Err(e) => warn!("index write lock poisoned, skipping update: {e}"),
            }
            return Ok(());
        };

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
    ///
    /// Delegates to `find_relevant_hybrid` (BM25-only, no embedding) and
    /// post-filters by CWD. Falls back to a direct DB scan if the hybrid
    /// index is empty.
    pub async fn find_relevant(
        &self,
        cwd: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        // Check if hybrid index has documents
        let index_populated = self
            .index
            .read()
            .map(|idx| !idx.is_empty())
            .unwrap_or(false);

        if index_populated {
            // Over-fetch to account for CWD filtering, then filter
            let candidates = self.find_relevant_hybrid(query, None, limit * 4).await?;
            let filtered: Vec<Episode> = candidates
                .into_iter()
                .filter(|ep| ep.working_dir == cwd)
                .take(limit)
                .collect();

            if !filtered.is_empty() {
                return Ok(filtered);
            }
            // Fall through to DB scan if hybrid returned no CWD matches
        }

        // Fallback: direct DB scan (for empty index or no CWD matches)
        self.find_relevant_db_scan(cwd, query, limit).await
    }

    /// Direct DB scan fallback for CWD-scoped relevance search.
    async fn find_relevant_db_scan(
        &self,
        cwd: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        // Degraded fallback: no DB to scan; return empty.
        let Some(db) = self.db.clone() else {
            return Ok(Vec::new());
        };
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

    /// Store an embedding for an episode.
    ///
    /// In degraded mode the disk write is skipped and the call
    /// returns `Ok(())`. The in-memory hybrid index is updated with
    /// the embedding, but the public read methods still return
    /// empty for the same reason as [`Self::store`] — they need
    /// disk-backed bodies. Follows the same "writes accepted, reads
    /// empty" contract.
    pub async fn store_embedding(&self, episode_id: &str, embedding: Vec<f32>) -> Result<()> {
        // Degraded fallback: skip the disk write, update the in-memory
        // embedding entry only, return success.
        let Some(db) = self.db.clone() else {
            match self.index.write() {
                Ok(mut idx) => {
                    let _ = idx.add_embedding(episode_id, &embedding);
                }
                Err(e) => warn!("index write lock poisoned, skipping embedding update: {e}"),
            }
            return Ok(());
        };
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

    /// Delete an episode by its ID. Removes from all DB tables and the in-memory index.
    ///
    /// Returns `true` if the episode existed and was deleted.
    ///
    /// In degraded mode the disk delete is skipped; this attempts to
    /// remove the entry from the in-memory index only and returns
    /// `false` (there is nothing the degraded handle can authoritatively
    /// claim was persisted).
    pub async fn delete_by_id(&self, episode_id: &str) -> Result<bool> {
        // Degraded fallback: no DB to delete from; clear the in-memory
        // entry (if any) and report `false`.
        let Some(db) = self.db.clone() else {
            match self.index.write() {
                Ok(mut idx) => {
                    let _ = idx.remove(episode_id);
                }
                Err(e) => warn!("index write lock poisoned, skipping removal: {e}"),
            }
            return Ok(false);
        };
        let ep_id = episode_id.to_string();

        let found = tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write()?;
            let existed = {
                // Remove from episodes table
                let mut episodes = write_txn.open_table(EPISODES_TABLE)?;
                let old = episodes.remove(ep_id.as_str())?;

                if let Some(old_json) = &old {
                    // Parse to get the cwd so we can update the cwd index
                    if let Ok(episode) = serde_json::from_str::<Episode>(old_json.value()) {
                        let cwd = episode.working_dir.to_string_lossy().to_string();
                        let mut cwd_index = write_txn.open_table(CWD_INDEX_TABLE)?;
                        // Read and drop the immutable borrow before mutating
                        let existing: Option<Vec<String>> =
                            cwd_index.get(cwd.as_str())?.map(|ids_json| {
                                serde_json::from_str(ids_json.value()).unwrap_or_default()
                            });
                        if let Some(mut ids) = existing {
                            ids.retain(|id| id != &ep_id);
                            if ids.is_empty() {
                                cwd_index.remove(cwd.as_str())?;
                            } else {
                                let new_json = serde_json::to_string(&ids)?;
                                cwd_index.insert(cwd.as_str(), new_json.as_str())?;
                            }
                        }
                    }
                }

                // Remove embedding
                let mut embeddings = write_txn.open_table(EMBEDDINGS_TABLE)?;
                embeddings.remove(ep_id.as_str())?;

                old.is_some()
            };
            write_txn.commit()?;
            Ok::<_, eyre::Report>(existed)
        })
        .await??;

        // Remove from in-memory hybrid index
        if found {
            match self.index.write() {
                Ok(mut idx) => {
                    idx.remove(episode_id);
                }
                Err(e) => warn!("index write lock poisoned, skipping removal: {e}"),
            }
        }

        Ok(found)
    }

    /// Delete multiple episodes by their IDs.
    ///
    /// Returns the number of episodes that were actually deleted.
    pub async fn delete_many(&self, episode_ids: &[String]) -> Result<usize> {
        let mut deleted = 0;
        for id in episode_ids {
            if self.delete_by_id(id).await? {
                deleted += 1;
            }
        }
        Ok(deleted)
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
        let ids: Vec<String> = matches.into_iter().map(|(id, _)| id).collect();
        // Degraded fallback: there is no on-disk store to read from.
        // The hybrid index only knows about episodes inserted in this
        // process's lifetime (which is empty at open for a degraded
        // handle) so returning an empty Vec is correct.
        let Some(db) = self.db.clone() else {
            return Ok(Vec::new());
        };

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
        // Verify empty store returns no results
        let results = store
            .find_relevant(Path::new("/nonexistent"), "anything", 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_store_and_find() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let ep = make_episode("Fixed parser bug", "/tmp/project");
        store.store(ep).await.unwrap();

        let results = store
            .find_relevant(Path::new("/tmp/project"), "parser", 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "Fixed parser bug");
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
    async fn should_delete_episode_when_id_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let ep = make_episode("Delete me", "/proj");
        let ep_id = ep.id.clone();
        store.store(ep).await.unwrap();

        // Verify it exists
        let results = store
            .find_relevant(Path::new("/proj"), "delete", 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        // Delete it
        let deleted = store.delete_by_id(&ep_id).await.unwrap();
        assert!(deleted);

        // Verify it's gone
        let results = store
            .find_relevant(Path::new("/proj"), "delete", 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn should_return_false_when_deleting_nonexistent_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let deleted = store.delete_by_id("nonexistent-id").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn should_delete_many_episodes_when_bulk_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        let ep1 = make_episode("First episode", "/proj");
        let ep2 = make_episode("Second episode", "/proj");
        let ep3 = make_episode("Third episode", "/proj");
        let id1 = ep1.id.clone();
        let id2 = ep2.id.clone();
        let id3 = ep3.id.clone();
        store.store(ep1).await.unwrap();
        store.store(ep2).await.unwrap();
        store.store(ep3).await.unwrap();

        // Delete two of three
        let count = store
            .delete_many(&[id1, id2, "nonexistent".to_string()])
            .await
            .unwrap();
        assert_eq!(count, 2);

        // Only ep3 should remain
        let results = store
            .find_relevant(Path::new("/proj"), "episode", 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id3);
    }

    #[tokio::test]
    async fn should_not_find_deleted_episode_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let ep_id;
        {
            let store = EpisodeStore::open(dir.path()).await.unwrap();
            let ep = make_episode("Ephemeral data", "/proj");
            ep_id = ep.id.clone();
            store.store(ep).await.unwrap();
            store.delete_by_id(&ep_id).await.unwrap();
        }
        // Reopen and verify deletion persisted
        let store = EpisodeStore::open(dir.path()).await.unwrap();
        let results = store
            .find_relevant(Path::new("/proj"), "ephemeral", 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_reopen_persists_data() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = EpisodeStore::open(dir.path()).await.unwrap();
            let ep = make_episode("persistent data", "/proj");
            store.store(ep).await.unwrap();
        }
        // Reopen and verify data persists via find_relevant
        let store = EpisodeStore::open(dir.path()).await.unwrap();
        let results = store
            .find_relevant(Path::new("/proj"), "persistent", 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "persistent data");
    }

    /// RED-first repro for the production crashloop tracked in #899
    /// (PR #888 / M11-F gateway consolidation).
    ///
    /// `octos serve` and `octos gateway` run as separate processes;
    /// both call `EpisodeStore::open*` against the same per-profile
    /// data dir. The second open used to crash with
    /// `redb::DatabaseError::DatabaseAlreadyOpen` because redb is a
    /// single-writer-single-process embedded database. This test
    /// pins the new contract: `open_or_degraded` returns a degraded
    /// in-memory fallback handle instead of an error.
    #[tokio::test]
    async fn should_return_degraded_store_when_redb_already_held_by_another_handle() {
        let dir = tempfile::tempdir().unwrap();
        // Owner: behaves like `octos serve` holding the lock. Strict
        // `open` is used so misconfigurations would still surface.
        let owner = EpisodeStore::open(dir.path()).await.unwrap();
        assert!(
            !owner.is_degraded(),
            "first opener should hold the canonical DB",
        );

        // Second opener: behaves like an `octos gateway` subprocess.
        // It opts into the degraded fallback via `open_or_degraded`.
        // Before the fix this returned `Err(... DatabaseAlreadyOpen ...)`;
        // now it must succeed with a degraded fallback.
        let degraded = EpisodeStore::open_or_degraded(dir.path()).await.unwrap();
        assert!(
            degraded.is_degraded(),
            "open_or_degraded must return a degraded in-memory fallback",
        );

        // The degraded handle accepts writes (silent no-op on disk)
        // and reports them as successful — gateway sub-agents that
        // record completion episodes don't crash.
        let ep = make_episode("recorded on degraded handle", "/proj");
        let ep_id = ep.id.clone();
        degraded
            .store(ep)
            .await
            .expect("store on degraded handle must succeed");
        degraded
            .store_embedding(&ep_id, vec![0.0_f32; 1536])
            .await
            .expect("store_embedding on degraded handle must succeed");

        // The owner still sees zero episodes — degraded writes are
        // not persisted to disk, so the owner's view is unchanged.
        let owner_view = owner
            .find_relevant(Path::new("/proj"), "recorded", 10)
            .await
            .unwrap();
        assert!(
            owner_view.is_empty(),
            "degraded writes must not surface to the owner; got {owner_view:?}",
        );

        // The degraded handle's own reads also return empty: the
        // hybrid index can match the summary for ranking, but
        // `find_relevant` / `find_relevant_hybrid` fetch full episode
        // bodies from disk, and the degraded handle has no disk
        // backing. This is the "writes accepted, reads empty"
        // contract — anything stricter would be incorrect because
        // the canonical store is owned elsewhere.
        let degraded_view = degraded
            .find_relevant(Path::new("/proj"), "recorded", 10)
            .await
            .unwrap();
        assert!(
            degraded_view.is_empty(),
            "degraded reads must return empty; got {degraded_view:?}",
        );
    }

    /// Strict `open` must fail (not silently degrade) when the redb
    /// file lock is already held. This locks down the contract that
    /// codex's round-1 review of #899 called out: a second
    /// `Serve`-role bootstrap should never quietly flip canonical
    /// ownership.
    #[tokio::test]
    async fn should_error_on_strict_open_when_redb_already_held() {
        let dir = tempfile::tempdir().unwrap();
        let _owner = EpisodeStore::open(dir.path()).await.unwrap();

        let err = EpisodeStore::open(dir.path())
            .await
            .err()
            .expect("strict open must error when lock is held");
        let msg = err.to_string() + " " + &format!("{err:?}");
        assert!(
            msg.contains("Database already open") || msg.contains("Cannot acquire lock"),
            "strict open error must surface the lock contention; got: {err:?}",
        );
    }
}
