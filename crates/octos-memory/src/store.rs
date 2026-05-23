//! Episode store: persistent storage for episodes using redb (pure Rust).

use std::path::Path;
use std::sync::{Arc, RwLock};

use eyre::{Result, WrapErr};
use redb::{Database, ReadableTable, TableDefinition};
use tracing::{debug, warn};

use crate::episode::Episode;
use crate::hybrid_search::{HybridIndex, HybridScore};

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
        self.find_relevant_filtered(cwd, query, limit, None).await
    }

    /// NEW-06 defense-in-depth: CWD-scoped relevance search with an
    /// optional `min_best_modality` floor applied to the BM25 score
    /// (the only modality available on the no-embedder fallback path).
    ///
    /// The agent loop calls this when no embedder is configured so
    /// pipeline workers spawned without the parent embedder still
    /// drop sub-threshold matches BEFORE injection — even though the
    /// `find_relevant_hybrid` path inside this fallback only has BM25
    /// scores. A score of `1.0` on BM25 still passes; loose
    /// cross-domain "shared token" matches (the NEW-06 contamination
    /// pattern) do not.
    ///
    /// `min_best_modality == None` matches [`Self::find_relevant`]
    /// semantics exactly.
    pub async fn find_relevant_filtered(
        &self,
        cwd: &Path,
        query: &str,
        limit: usize,
        min_best_modality: Option<f32>,
    ) -> Result<Vec<Episode>> {
        // Check if hybrid index has documents
        let index_populated = self
            .index
            .read()
            .map(|idx| !idx.is_empty())
            .unwrap_or(false);

        if index_populated {
            // Inner-fetch sizing — codex P2 rounds 4 and 5 follow-up:
            //
            // The hybrid index is global (not cwd-scoped). After we
            // ask for the top-N candidates, we apply the cwd filter
            // locally. If we used the standard `limit * 4` pool, a
            // shared episode store with many foreign-cwd matches that
            // clear the same floor could truncate a current-cwd
            // match out BEFORE the cwd filter ever sees it — flipping
            // the "return empty when floor set" branch into a false-
            // negative for legitimately relevant local memories.
            //
            // Two regimes:
            // * No floor → keep the legacy `limit * 4` over-fetch.
            //   Without a floor there is no natural cap on the
            //   candidate set; growing the pool unboundedly would be
            //   wasted work and the legacy fall-through to
            //   `find_relevant_db_scan` already handles the no-cwd-
            //   match case.
            // * Floor supplied → size the inner fetch from the actual
            //   corpus (`HybridIndex::len`). Round 4 used
            //   `FLOOR_PREFILTER_POOL = HNSW_CAPACITY = 10_000`, but
            //   codex round 5 flagged that the BM25 inverted index
            //   keeps inserting documents AFTER HNSW saturates, so a
            //   corpus larger than 10K BM25-only docs could still
            //   truncate a local floor-clearing match. Reading the
            //   corpus size directly closes that gap without
            //   over-allocating for small stores.
            let corpus_size = self
                .index
                .read()
                .map(|idx| idx.len())
                .unwrap_or(crate::hybrid_search::FLOOR_PREFILTER_POOL);
            let inner_limit = if min_best_modality.is_some() {
                corpus_size
            } else {
                limit * 4
            };
            let candidates = self
                .find_relevant_hybrid_scored_filtered(query, None, inner_limit, min_best_modality)
                .await?;
            let filtered: Vec<Episode> = candidates
                .into_iter()
                .filter(|(ep, _)| ep.working_dir == cwd)
                .map(|(ep, _)| ep)
                .take(limit)
                .collect();

            if !filtered.is_empty() {
                return Ok(filtered);
            }
            // NEW-06 codex follow-up: when a caller passed a
            // `min_best_modality` floor and the scored+CWD-filtered set
            // came back empty (with an exhaustive `inner_limit`), the
            // correct answer is empty — nothing in the index cleared
            // the contamination floor for this cwd. Falling through to
            // the unscored `find_relevant_db_scan` would silently
            // bypass the floor (the scan is keyword-substring only
            // with no scoring infrastructure), which is exactly the
            // contamination pattern this filter exists to prevent.
            //
            // Only fall through to the unscored DB scan when no floor
            // was requested (legacy `find_relevant` semantics).
            if min_best_modality.is_some() {
                return Ok(Vec::new());
            }
            // Fall through to DB scan if hybrid returned no CWD matches
            // AND no contamination floor was requested.
        }

        // Fallback: direct DB scan (for empty index or no CWD matches).
        // The DB scan path is keyword-substring based with no scoring
        // infrastructure, so we can't apply `min_best_modality` here
        // without rewriting it. We only reach this path when the caller
        // did not request a floor (see early-return above), so legacy
        // unscored behaviour is preserved without bypassing the
        // contamination filter when one was asked for.
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
    ///
    /// Backward-compatible wrapper around [`Self::find_relevant_hybrid_scored`]
    /// that drops the similarity score. Callers that need to gate episode
    /// injection on a minimum similarity (e.g. the agent loop's "Relevant
    /// Past Experiences" system message) should call the `_scored` variant
    /// directly so cross-session contamination is filtered out (NEW-06).
    pub async fn find_relevant_hybrid(
        &self,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let scored = self
            .find_relevant_hybrid_scored(query, query_embedding, limit)
            .await?;
        Ok(scored.into_iter().map(|(ep, _)| ep).collect())
    }

    /// Hybrid search across all episodes (not cwd-scoped), returning each
    /// episode alongside a per-modality [`HybridScore`] breakdown.
    ///
    /// Results are sorted by descending `HybridScore::combined` (the
    /// same weighted-sum ranking as
    /// [`Self::find_relevant_hybrid`]). The breakdown lets callers
    /// apply a modality-aware minimum-similarity gate so a strong
    /// single-modality match (e.g. a keyword-perfect older episode
    /// without a stored embedding) isn't dropped just because the
    /// configured `bm25_weight` / `vector_weight` would down-weight the
    /// combined score below the gate. Without this, the agent loop's
    /// "Relevant Past Experiences" gate (NEW-06 fix) would strand
    /// legitimately relevant keyword-only matches.
    pub async fn find_relevant_hybrid_scored(
        &self,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        limit: usize,
    ) -> Result<Vec<(Episode, HybridScore)>> {
        self.find_relevant_hybrid_scored_filtered(query, query_embedding, limit, None)
            .await
    }

    /// Like [`Self::find_relevant_hybrid_scored`] but applies an
    /// optional `min_best_modality` floor on
    /// [`HybridScore::best_modality`] BEFORE the in-index
    /// combined-rank truncation to `limit`.
    ///
    /// This is the contamination-safe entry point for callers (such as
    /// the agent loop's "Relevant Past Experiences" injection) that
    /// would otherwise face the dead band where `limit` or more
    /// sub-threshold vector-only candidates crowd out a high-
    /// `best_modality` low-`combined` candidate (codex P2 round 2 on
    /// PR #1195). Pushing the floor down into the index ensures the
    /// guarantee holds regardless of memory-store size: if ANY
    /// candidate clears the floor, it reaches the returned set
    /// (subject to `limit`).
    ///
    /// `min_best_modality == None` matches
    /// [`Self::find_relevant_hybrid_scored`] semantics exactly.
    pub async fn find_relevant_hybrid_scored_filtered(
        &self,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        limit: usize,
        min_best_modality: Option<f32>,
    ) -> Result<Vec<(Episode, HybridScore)>> {
        // Search the in-memory index
        let matches = {
            let idx = self
                .index
                .read()
                .map_err(|e| eyre::eyre!("index lock poisoned: {e}"))?;
            idx.search_scored_filtered(query, query_embedding.as_deref(), limit, min_best_modality)
        };

        // Fetch full episodes from DB. Preserve (id, score) pairing so
        // callers can gate on a similarity threshold.
        let id_scores: Vec<(String, HybridScore)> = matches;
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

            // Build id -> score map and an id-order index so we can
            // attach the matching score to each fetched episode while
            // preserving the hybrid ranking order.
            let score_by_id: std::collections::HashMap<&str, HybridScore> =
                id_scores.iter().map(|(id, s)| (id.as_str(), *s)).collect();
            let id_order: std::collections::HashMap<&str, usize> = id_scores
                .iter()
                .enumerate()
                .map(|(i, (id, _))| (id.as_str(), i))
                .collect();

            let mut scored: Vec<(Episode, HybridScore)> = Vec::new();
            for (id, _) in &id_scores {
                if let Some(json) = table.get(id.as_str())? {
                    if let Ok(episode) = serde_json::from_str::<Episode>(json.value()) {
                        let score =
                            score_by_id
                                .get(episode.id.as_str())
                                .copied()
                                .unwrap_or(HybridScore {
                                    combined: 0.0,
                                    bm25: 0.0,
                                    vector: 0.0,
                                });
                        scored.push((episode, score));
                    }
                }
            }

            // Preserve the ranking order from hybrid search
            scored.sort_by_key(|(e, _)| id_order.get(e.id.as_str()).copied().unwrap_or(usize::MAX));

            Ok(scored)
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

    /// NEW-06 codex follow-up — when `min_best_modality` is supplied
    /// and the scored+cwd-filtered set comes back empty, the function
    /// MUST return empty instead of falling through to the unscored
    /// `find_relevant_db_scan` (which has no scoring infrastructure
    /// and would silently bypass the contamination floor).
    ///
    /// Reproduces the codex follow-up bug at lines 365-370. The
    /// scenario engineered below:
    /// * one episode at cwd `/proj` with a deliberately weak BM25
    ///   match for the query (so it does NOT clear the 0.99 floor);
    /// * one episode at a foreign cwd with a strong BM25 match (so
    ///   its score normalises high but it gets dropped by the cwd
    ///   filter).
    ///
    /// Pre-fix, `find_relevant_hybrid_scored_filtered` would return
    /// the foreign-cwd episode, the cwd filter would drop it, the
    /// `!filtered.is_empty()` short-circuit would fail, and execution
    /// would fall through to the DB scan — which IS cwd-scoped and
    /// matches by substring, so the weak `/proj` episode would be
    /// returned despite never having cleared the floor.
    ///
    /// Post-fix, when the caller supplies a floor, the fallthrough is
    /// gated off and the function returns empty.
    #[tokio::test]
    async fn find_relevant_filtered_returns_empty_when_floor_set_and_no_cwd_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        // Foreign-cwd episode with a strong BM25 match (verbatim query
        // tokens). Normalises to BM25=1.0 in the result set so it
        // clears any reasonable floor — but the cwd filter drops it.
        store
            .store(make_episode(
                "gravitational lensing observations of distant galaxies",
                "/foreign-cwd",
            ))
            .await
            .unwrap();
        // Target-cwd episode whose summary shares only the noise-token
        // "podcast" with the query. Its BM25 score is positive but
        // far below the foreign-cwd episode's score after normalisation,
        // so it does NOT clear the floor.
        store
            .store(make_episode("Apple CEO podcast", "/proj"))
            .await
            .unwrap();

        let results = store
            .find_relevant_filtered(
                Path::new("/proj"),
                "gravitational lensing observations",
                10,
                Some(0.5), // floor — foreign-cwd clears it, /proj does not
            )
            .await
            .unwrap();
        assert!(
            results.is_empty(),
            "find_relevant_filtered with a floor must NOT fall through \
             to unscored DB scan when the scored+cwd set is empty; \
             returned {} contaminated episodes: {results:?}",
            results.len()
        );
    }

    /// NEW-06 codex P2 rounds 4 + 5 — when a floor is supplied AND the
    /// hybrid index holds many foreign-cwd matches that all clear the
    /// floor with stronger scores, a current-cwd match that ALSO
    /// clears the floor must not be truncated out before the cwd
    /// filter sees it.
    ///
    /// Pre-fix-round-4: with `limit=1` and ~10 stronger foreign-cwd
    /// exact matches sharing the same query token, the inner fetch
    /// pool (`limit * 4 = 4`) returned only foreign episodes; the
    /// cwd filter dropped them all; the floor-set early-return then
    /// returned empty even though a local episode clearing the floor
    /// existed in the store.
    ///
    /// Round 4 raised the inner pool to `FLOOR_PREFILTER_POOL = 10_000`
    /// (the HNSW capacity), but codex round 5 flagged that the BM25
    /// inverted index keeps inserting beyond `HNSW_CAPACITY` (HNSW
    /// gracefully degrades to BM25-only), so a corpus with >10K
    /// foreign-cwd BM25 matches could still truncate out a local hit.
    ///
    /// Post-fix-round-5: the inner pool is sized from the actual
    /// corpus (`HybridIndex::len`), so no truncation happens before
    /// the cwd filter regardless of how large the BM25-only index
    /// grows.
    #[tokio::test]
    async fn find_relevant_filtered_returns_local_match_through_many_foreign_with_floor() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        // 20 foreign-cwd episodes that all match the query token —
        // these will all clear the floor and crowd the inner pool.
        // `limit * 4 = 4` (with the caller's `limit = 1`) is far
        // below 20, so without round-4 the local episode below would
        // be truncated out.
        for i in 0..20 {
            store
                .store(make_episode(
                    "deep_research gravitational lensing JWST",
                    &format!("/foreign-cwd-{i}"),
                ))
                .await
                .unwrap();
        }
        // Single local episode that also clears the floor.
        store
            .store(make_episode(
                "deep_research gravitational lensing JWST",
                "/proj",
            ))
            .await
            .unwrap();

        let results = store
            .find_relevant_filtered(
                Path::new("/proj"),
                "deep_research gravitational lensing JWST",
                1,
                Some(0.5),
            )
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "find_relevant_filtered must return the local floor-clearing \
             episode even when many foreign-cwd matches share the floor \
             pass (codex P2 round 4); returned {} episodes",
            results.len()
        );
        assert_eq!(
            results[0].working_dir,
            PathBuf::from("/proj"),
            "local match must be the one returned, not a foreign-cwd \
             leak: got {:?}",
            results[0].working_dir
        );
    }

    /// NEW-06 codex follow-up companion — when `min_best_modality` is
    /// `None`, the legacy fall-through to the unscored DB scan stays in
    /// place. Locks the "no behaviour change for legacy callers" half
    /// of the fix.
    #[tokio::test]
    async fn find_relevant_filtered_falls_through_to_db_scan_when_no_floor() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        store
            .store(make_episode("Fixed parser bug in tokenizer", "/proj"))
            .await
            .unwrap();

        // No floor → legacy behaviour: keyword-substring matching via
        // the index OR the DB scan returns the episode.
        let results = store
            .find_relevant_filtered(Path::new("/proj"), "parser", 10, None)
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "find_relevant_filtered with no floor must keep legacy \
             behaviour byte-for-byte — got {} episodes",
            results.len()
        );
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
    async fn find_relevant_hybrid_scored_returns_similarity_scores() {
        let dir = tempfile::tempdir().unwrap();
        let store = EpisodeStore::open(dir.path()).await.unwrap();

        store
            .store(make_episode("rust ownership borrow checker", "/proj"))
            .await
            .unwrap();
        store
            .store(make_episode("python web flask framework", "/proj"))
            .await
            .unwrap();

        let scored = store
            .find_relevant_hybrid_scored("rust ownership", None, 10)
            .await
            .unwrap();

        // At least one match returned with a populated HybridScore.
        assert!(!scored.is_empty(), "expected at least one match");
        let (top_ep, top_score) = &scored[0];
        assert!(
            top_ep.summary.contains("rust"),
            "top match should be the rust episode, got: {}",
            top_ep.summary
        );
        // BM25-only path (no query embedding): combined == bm25 score.
        assert!(
            top_score.combined > 0.0 && top_score.combined <= 1.0,
            "top combined score should be in (0, 1], got {}",
            top_score.combined
        );
        assert!(
            top_score.bm25 > 0.0,
            "BM25 score should be > 0 for the rust match (got {})",
            top_score.bm25
        );
        assert_eq!(
            top_score.vector, 0.0,
            "no embedding stored, vector score should be 0"
        );

        // Backward-compat: find_relevant_hybrid returns the same episodes
        // (without scores) in the same order.
        let plain = store
            .find_relevant_hybrid("rust ownership", None, 10)
            .await
            .unwrap();
        assert_eq!(plain.len(), scored.len());
        assert_eq!(plain[0].id, scored[0].0.id);
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
