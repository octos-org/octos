//! The Recall tier: one index over app records (Documents), mirrored
//! episodes and memory-bank pages (Knowledge), with small int8 vectors, a
//! persisted HNSW graph and heat-based aging.
//!
//! Design record: `docs/adr/personal-memory-tiers.md`. Budget for 10k mails
//! + 5k events: ~14 MB on disk, <15 MB resident.
//!
//! Storage: `<data_dir>/recall.redb` (tables `records`, `vectors`, `meta`)
//! plus `<data_dir>/recall-index/` holding the dumped graph and a manifest
//! that pins it to a (embedder, dimension, generation) triple. When the
//! manifest matches, open reloads the graph instead of rebuilding it; when
//! it does not (embedder changed, crash before a dump), the graph is rebuilt
//! from the stored int8 vectors and dumped again.
//!
//! The store is synchronous: every operation is a few milliseconds and the
//! callers (agent tools, UI protocol, FFI) already run on blocking-safe
//! threads or wrap it in `spawn_blocking`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::hybrid_search::HybridIndex;
use crate::quant::{QuantizedVector, mrl_truncate};
use crate::record::{Record, RecordKind, Trust};

const RECORDS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("records");
const VECTORS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("vectors");
const META_TABLE: TableDefinition<&str, &str> = TableDefinition::new("meta");

const INDEX_DIR: &str = "recall-index";
const GRAPH_BASENAME: &str = "recall";
const MANIFEST_FILE: &str = "manifest.json";

/// Default stored/resident vector width (Matryoshka-truncated).
pub const DEFAULT_RECALL_DIMENSION: usize = 256;

/// Tunables. Defaults follow the ADR budget.
#[derive(Debug, Clone)]
pub struct RecallConfig {
    /// Vector width kept at rest and in the graph (MRL truncation target).
    pub dimension: usize,
    /// Identifier of the embedder producing vectors (`provider/model`).
    /// Vectors and the persisted graph are only reused when it matches.
    pub embedder_id: String,
    /// Records newer than this keep their vector resident in the graph;
    /// older ones are BM25-only until touched.
    pub hot_days: i64,
    /// Heat recency half-life.
    pub half_life_days: f32,
    /// Upper bound on resident vectors (the HNSW graph capacity).
    pub max_resident_vectors: usize,
    /// Upper bound on records per source; the coldest beyond it are deleted
    /// by [`RecallStore::age`].
    pub max_records_per_source: usize,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            dimension: DEFAULT_RECALL_DIMENSION,
            embedder_id: String::new(),
            hot_days: 180,
            half_life_days: 120.0,
            max_resident_vectors: 10_000,
            max_records_per_source: 20_000,
        }
    }
}

/// Compact per-record facts kept in memory for filtering (~100 B each).
#[derive(Debug, Clone)]
struct Meta {
    kind: RecordKind,
    source: String,
    timestamp: DateTime<Utc>,
    /// A vector exists at rest.
    stored_vector: bool,
    /// The vector is in the graph.
    resident: bool,
    visits: u32,
    last_visit: Option<DateTime<Utc>>,
    promoted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    embedder_id: String,
    dimension: usize,
    generation: u64,
    layout: Vec<(String, bool)>,
}

/// Filters for [`RecallStore::search`].
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub kinds: Vec<RecordKind>,
    pub sources: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
}

/// One search result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub id: String,
    pub kind: RecordKind,
    pub source: String,
    pub title: String,
    #[serde(rename = "abstract")]
    pub abstract_: String,
    pub score: f32,
    pub timestamp: DateTime<Utc>,
    pub trust: Trust,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpsertReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub vectors_stored: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgeReport {
    pub vectors_evicted: usize,
    pub records_deleted: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecallStats {
    pub records: usize,
    pub vectors_stored: usize,
    pub vectors_resident: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub by_source: BTreeMap<String, usize>,
    pub dimension: usize,
    pub embedder_id: String,
    pub graph_persisted: bool,
    pub disk_bytes: u64,
}

pub struct RecallStore {
    db: Arc<Database>,
    dir: PathBuf,
    config: RecallConfig,
    /// In-memory fallback (lock held elsewhere): never persists the graph.
    degraded: bool,
    index: RwLock<HybridIndex>,
    meta: RwLock<HashMap<String, Meta>>,
    /// Bumped on every vector-affecting change; the persisted graph is only
    /// reused when its manifest carries the same value.
    generation: RwLock<u64>,
    graph_persisted: RwLock<bool>,
}

impl RecallStore {
    /// Open or create the store under `data_dir`. Fails when another
    /// process holds the redb lock (the owner of the canonical store).
    pub fn open(data_dir: impl AsRef<Path>, config: RecallConfig) -> Result<Self> {
        Self::open_inner(data_dir.as_ref(), config, false)
    }

    /// Like [`Self::open`], but when another process already owns
    /// `recall.redb` (an `octos gateway` beside `octos serve`) fall back to
    /// an in-memory store: searches work over what this process ingests,
    /// nothing is persisted, and the graph is never dumped. Mirrors
    /// `EpisodeStore::open_or_degraded`.
    pub fn open_or_degraded(data_dir: impl AsRef<Path>, config: RecallConfig) -> Result<Self> {
        Self::open_inner(data_dir.as_ref(), config, true)
    }

    /// `true` when this handle runs on the in-memory fallback.
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    fn open_inner(dir: &Path, config: RecallConfig, allow_degraded: bool) -> Result<Self> {
        let dir = dir.to_path_buf();
        std::fs::create_dir_all(&dir).wrap_err("failed to create data directory")?;
        let (db, degraded) = match Database::create(dir.join("recall.redb")) {
            Ok(db) => (db, false),
            Err(redb::DatabaseError::DatabaseAlreadyOpen) if allow_degraded => {
                tracing::warn!(
                    path = %dir.join("recall.redb").display(),
                    "recall store already held by another process; using an in-memory \
                     fallback (nothing ingested by this process is persisted)"
                );
                let db = Database::builder()
                    .create_with_backend(redb::backends::InMemoryBackend::new())
                    .wrap_err("failed to open in-memory recall store")?;
                (db, true)
            }
            Err(e) => return Err(eyre::Report::new(e).wrap_err("failed to open recall.redb")),
        };
        {
            let txn = db.begin_write()?;
            {
                let _ = txn.open_table(RECORDS_TABLE)?;
                let _ = txn.open_table(VECTORS_TABLE)?;
                let _ = txn.open_table(META_TABLE)?;
            }
            txn.commit()?;
        }
        let mut store = Self {
            db: Arc::new(db),
            dir,
            config,
            degraded,
            index: RwLock::new(HybridIndex::new(0)),
            meta: RwLock::new(HashMap::new()),
            generation: RwLock::new(0),
            graph_persisted: RwLock::new(false),
        };
        store.load()?;
        Ok(store)
    }

    pub fn config(&self) -> &RecallConfig {
        &self.config
    }

    pub fn dimension(&self) -> usize {
        self.config.dimension
    }

    fn index_dir(&self) -> PathBuf {
        self.dir.join(INDEX_DIR)
    }

    // ------------------------------------------------------------ open

    fn load(&mut self) -> Result<()> {
        let now = Utc::now();
        let (records, vectors, stored_embedder, generation) = {
            let txn = self.db.begin_read()?;
            let rt = txn.open_table(RECORDS_TABLE)?;
            let vt = txn.open_table(VECTORS_TABLE)?;
            let mt = txn.open_table(META_TABLE)?;
            let mut records: Vec<Record> = Vec::new();
            for entry in rt.iter()? {
                let (_, v) = entry?;
                if let Ok(r) = serde_json::from_str::<Record>(v.value()) {
                    records.push(r);
                }
            }
            let mut vectors: HashMap<String, QuantizedVector> = HashMap::new();
            for entry in vt.iter()? {
                let (k, v) = entry?;
                if let Some(q) = QuantizedVector::from_bytes(v.value()) {
                    vectors.insert(k.value().to_string(), q);
                }
            }
            let embedder = mt
                .get("embedder")?
                .map(|v| v.value().to_string())
                .unwrap_or_default();
            let generation: u64 = mt
                .get("generation")?
                .and_then(|v| v.value().parse().ok())
                .unwrap_or(0);
            (records, vectors, embedder, generation)
        };

        // Stored vectors from another embedder or width are unusable.
        let vectors_valid = stored_embedder == self.config.embedder_id
            && vectors
                .values()
                .all(|q| q.dimension() == self.config.dimension);
        if !vectors_valid && !vectors.is_empty() {
            tracing::warn!(
                stored = %stored_embedder,
                configured = %self.config.embedder_id,
                "recall vectors were produced by another embedder — dropping them; \
                 re-embed with `octos memory reindex`"
            );
            let txn = self.db.begin_write()?;
            {
                let mut vt = txn.open_table(VECTORS_TABLE)?;
                let keys: Vec<String> = vectors.keys().cloned().collect();
                for k in keys {
                    vt.remove(k.as_str())?;
                }
            }
            txn.commit()?;
        }
        let vectors = if vectors_valid {
            vectors
        } else {
            HashMap::new()
        };

        let by_id: HashMap<String, &Record> = records.iter().map(|r| (r.id.clone(), r)).collect();
        let mut meta: HashMap<String, Meta> = HashMap::with_capacity(records.len());
        for r in &records {
            meta.insert(
                r.id.clone(),
                Meta {
                    kind: r.kind,
                    source: r.source.clone(),
                    timestamp: r.timestamp,
                    stored_vector: vectors.contains_key(&r.id),
                    resident: false,
                    visits: r.visits,
                    last_visit: r.last_visit,
                    promoted: r.promoted,
                },
            );
        }

        // Try the persisted graph first.
        let manifest_path = self.index_dir().join(MANIFEST_FILE);
        let manifest: Option<Manifest> = std::fs::read(&manifest_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let mut index = HybridIndex::new(self.config.dimension);
        let mut reloaded = false;
        if let Some(m) = manifest.filter(|m| {
            vectors_valid
                && m.embedder_id == self.config.embedder_id
                && m.dimension == self.config.dimension
                && m.generation == generation
                && m.layout
                    .iter()
                    .all(|(id, _)| id.is_empty() || by_id.contains_key(id))
        }) {
            match HybridIndex::load_hnsw(&self.index_dir(), GRAPH_BASENAME) {
                Ok(hnsw) => {
                    index
                        .attach_hnsw(hnsw, &m.layout, |id| by_id.get(id).map(|r| r.index_text()))?;
                    for (id, has_vector) in &m.layout {
                        if let Some(mm) = meta.get_mut(id) {
                            mm.resident = *has_vector;
                        }
                    }
                    // Records written after the dump (same generation ⇒ no
                    // vectors involved) are indexed BM25-only.
                    for r in &records {
                        if !m.layout.iter().any(|(id, _)| id == &r.id) {
                            index.insert(&r.id, &r.index_text(), None);
                        }
                    }
                    reloaded = true;
                }
                Err(e) => tracing::warn!("recall graph reload failed, rebuilding: {e}"),
            }
        }
        if !reloaded {
            index = HybridIndex::new(self.config.dimension);
            let mut resident = 0usize;
            for r in &records {
                let hot = self.is_hot(r.timestamp, r.visits, now);
                let vec = if hot && resident < self.config.max_resident_vectors {
                    vectors.get(&r.id).map(QuantizedVector::to_f32)
                } else {
                    None
                };
                if vec.is_some() {
                    resident += 1;
                }
                index.insert(&r.id, &r.index_text(), vec.as_deref());
                if let Some(mm) = meta.get_mut(&r.id) {
                    mm.resident = vec.is_some();
                }
            }
        }

        *self.index.write().unwrap() = index;
        *self.meta.write().unwrap() = meta;
        *self.generation.write().unwrap() = generation;
        *self.graph_persisted.write().unwrap() = reloaded;
        if !reloaded {
            self.persist_index()?;
        }
        tracing::debug!(
            records = records.len(),
            vectors = vectors.len(),
            reloaded,
            "opened recall store"
        );
        Ok(())
    }

    fn is_hot(&self, timestamp: DateTime<Utc>, visits: u32, now: DateTime<Utc>) -> bool {
        visits > 0 || (now - timestamp).num_days() <= self.config.hot_days
    }

    /// Dump the graph and write the manifest. Called after ingest batches
    /// and by `close`; cheap to call when nothing changed.
    pub fn persist_index(&self) -> Result<()> {
        if self.degraded {
            return Ok(());
        }
        let dir = self.index_dir();
        std::fs::create_dir_all(&dir)?;
        let generation = *self.generation.read().unwrap();
        let (dumped, layout) = {
            let index = self.index.read().unwrap();
            (index.dump_hnsw(&dir, GRAPH_BASENAME)?, index.layout())
        };
        if !dumped {
            let _ = std::fs::remove_file(dir.join(MANIFEST_FILE));
            *self.graph_persisted.write().unwrap() = false;
            return Ok(());
        }
        let manifest = Manifest {
            embedder_id: self.config.embedder_id.clone(),
            dimension: self.config.dimension,
            generation,
            layout,
        };
        let tmp = dir.join(format!("{MANIFEST_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec(&manifest)?)?;
        std::fs::rename(&tmp, dir.join(MANIFEST_FILE))?;
        // The generation and embedder also live in redb so a manifest from
        // a stale copy of the directory cannot pass.
        let txn = self.db.begin_write()?;
        {
            let mut mt = txn.open_table(META_TABLE)?;
            mt.insert("embedder", self.config.embedder_id.as_str())?;
            mt.insert("dimension", self.config.dimension.to_string().as_str())?;
            mt.insert("generation", generation.to_string().as_str())?;
        }
        txn.commit()?;
        *self.graph_persisted.write().unwrap() = true;
        Ok(())
    }

    fn bump_generation(&self) {
        *self.generation.write().unwrap() += 1;
        *self.graph_persisted.write().unwrap() = false;
    }

    // ------------------------------------------------------------ write

    /// Insert or update records. `vectors[i]` (any width ≥ `dimension`) is
    /// truncated and quantised; records whose `fingerprint` is unchanged
    /// keep their stored vector and are reported as unchanged.
    pub fn upsert(
        &self,
        mut records: Vec<Record>,
        vectors: Vec<Option<Vec<f32>>>,
    ) -> Result<UpsertReport> {
        let now = Utc::now();
        let mut report = UpsertReport::default();
        let existing: HashMap<String, Record> = {
            let txn = self.db.begin_read()?;
            let rt = txn.open_table(RECORDS_TABLE)?;
            let mut m = HashMap::new();
            for r in &records {
                if let Some(v) = rt.get(r.id.as_str())? {
                    if let Ok(old) = serde_json::from_str::<Record>(v.value()) {
                        m.insert(r.id.clone(), old);
                    }
                }
            }
            m
        };
        let txn = self.db.begin_write()?;
        let mut vector_changes = false;
        {
            let mut rt = txn.open_table(RECORDS_TABLE)?;
            let mut vt = txn.open_table(VECTORS_TABLE)?;
            let mut index = self.index.write().unwrap();
            let mut meta = self.meta.write().unwrap();
            for (i, r) in records.iter_mut().enumerate() {
                r.clamp();
                if r.id.trim().is_empty() {
                    continue;
                }
                let vector = vectors.get(i).and_then(|v| v.as_ref());
                let old = existing.get(&r.id);
                // Preserve usage counters across re-ingest.
                if let Some(old) = old {
                    r.visits = r.visits.max(old.visits);
                    r.last_visit = r.last_visit.or(old.last_visit);
                    r.promoted = r.promoted || old.promoted;
                }
                let unchanged = old.is_some_and(|old| {
                    !r.fingerprint.is_empty()
                        && old.fingerprint == r.fingerprint
                        && old.index_text() == r.index_text()
                });
                let had_vector = meta.get(&r.id).is_some_and(|m| m.stored_vector);
                if unchanged && (vector.is_none() || had_vector) {
                    report.unchanged += 1;
                    continue;
                }
                r.updated_at = now;
                rt.insert(r.id.as_str(), serde_json::to_string(&*r)?.as_str())?;
                let stored_q = match vector {
                    Some(v) if v.len() >= self.config.dimension => {
                        let q = QuantizedVector::from_f32(&mrl_truncate(v, self.config.dimension));
                        vt.insert(r.id.as_str(), q.to_bytes().as_slice())?;
                        report.vectors_stored += 1;
                        Some(q)
                    }
                    Some(v) => {
                        tracing::warn!(id = %r.id, got = v.len(), want = self.config.dimension, "vector too narrow for the recall index — stored BM25-only");
                        None
                    }
                    None => None,
                };
                // Re-index: tombstone the old entry (if any), insert fresh.
                let was_resident = meta.get(&r.id).is_some_and(|m| m.resident);
                index.remove(&r.id);
                let resident_capacity = index.hnsw_points() < self.config.max_resident_vectors;
                let hot = self.is_hot(r.timestamp, r.visits, now);
                let resident_vec: Option<Vec<f32>> = match (&stored_q, had_vector) {
                    (Some(q), _) if hot && resident_capacity => Some(q.to_f32()),
                    (None, true) if hot && resident_capacity => vt
                        .get(r.id.as_str())?
                        .and_then(|b| QuantizedVector::from_bytes(b.value()))
                        .map(|q| q.to_f32()),
                    _ => None,
                };
                index.insert(&r.id, &r.index_text(), resident_vec.as_deref());
                if stored_q.is_some() || was_resident || resident_vec.is_some() {
                    vector_changes = true;
                }
                meta.insert(
                    r.id.clone(),
                    Meta {
                        kind: r.kind,
                        source: r.source.clone(),
                        timestamp: r.timestamp,
                        stored_vector: stored_q.is_some() || had_vector,
                        resident: resident_vec.is_some(),
                        visits: r.visits,
                        last_visit: r.last_visit,
                        promoted: r.promoted,
                    },
                );
                if old.is_some() {
                    report.updated += 1;
                } else {
                    report.inserted += 1;
                }
            }
        }
        txn.commit()?;
        if vector_changes {
            self.bump_generation();
        }
        Ok(report)
    }

    /// Remove records (and their vectors). Returns how many existed.
    pub fn delete(&self, ids: &[String]) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut rt = txn.open_table(RECORDS_TABLE)?;
            let mut vt = txn.open_table(VECTORS_TABLE)?;
            let mut index = self.index.write().unwrap();
            let mut meta = self.meta.write().unwrap();
            for id in ids {
                if rt.remove(id.as_str())?.is_some() {
                    removed += 1;
                }
                vt.remove(id.as_str())?;
                index.remove(id);
                meta.remove(id);
            }
        }
        txn.commit()?;
        if removed > 0 {
            self.bump_generation();
        }
        Ok(removed)
    }

    /// Delete every record of a source (e.g. an account that was removed).
    pub fn delete_source(&self, source: &str) -> Result<usize> {
        let ids: Vec<String> = self
            .meta
            .read()
            .unwrap()
            .iter()
            .filter(|(_, m)| m.source == source)
            .map(|(id, _)| id.clone())
            .collect();
        self.delete(&ids)
    }

    /// Count a load: visits + 1, last_visit = now, and make the vector
    /// resident again if it had aged out of the graph.
    pub fn touch(&self, id: &str) -> Result<bool> {
        let Some(mut record) = self.get(id)? else {
            return Ok(false);
        };
        record.visits = record.visits.saturating_add(1);
        record.last_visit = Some(Utc::now());
        let txn = self.db.begin_write()?;
        let mut revived = false;
        {
            let mut rt = txn.open_table(RECORDS_TABLE)?;
            rt.insert(id, serde_json::to_string(&record)?.as_str())?;
            let vt = txn.open_table(VECTORS_TABLE)?;
            let mut meta = self.meta.write().unwrap();
            if let Some(m) = meta.get_mut(id) {
                m.visits = record.visits;
                m.last_visit = record.last_visit;
                if m.stored_vector && !m.resident {
                    if let Some(q) = vt
                        .get(id)?
                        .and_then(|b| QuantizedVector::from_bytes(b.value()))
                    {
                        let mut index = self.index.write().unwrap();
                        if index.hnsw_points() < self.config.max_resident_vectors
                            && index.add_embedding(id, &q.to_f32())
                        {
                            m.resident = true;
                            revived = true;
                        }
                    }
                }
            }
        }
        txn.commit()?;
        if revived {
            self.bump_generation();
        }
        Ok(true)
    }

    /// Apply the heat policy: per source, evict vectors from the graph for
    /// the coldest records above the resident budget, and delete the coldest
    /// records above the per-source cap. Real deletes; no tombstone build-up
    /// beyond what the graph needs until the next rebuild.
    pub fn age(&self, now: DateTime<Utc>) -> Result<AgeReport> {
        let mut report = AgeReport::default();
        let half_life = self.config.half_life_days;
        // Snapshot heats.
        let snapshot: Vec<(String, String, f32, bool)> = {
            let meta = self.meta.read().unwrap();
            let records =
                self.records_by_ids(meta.keys().cloned().collect::<Vec<_>>().as_slice())?;
            records
                .into_iter()
                .map(|r| {
                    let resident = meta.get(&r.id).is_some_and(|m| m.resident);
                    (
                        r.id.clone(),
                        r.source.clone(),
                        r.heat(now, half_life),
                        resident,
                    )
                })
                .collect()
        };
        // Per-source record cap.
        let mut by_source: HashMap<String, Vec<(String, f32, bool)>> = HashMap::new();
        for (id, source, heat, resident) in snapshot {
            by_source
                .entry(source)
                .or_default()
                .push((id, heat, resident));
        }
        let mut to_delete = Vec::new();
        for list in by_source.values_mut() {
            list.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            if list.len() > self.config.max_records_per_source {
                let excess = list.len() - self.config.max_records_per_source;
                to_delete.extend(list.drain(..excess).map(|(id, _, _)| id));
            }
        }
        report.records_deleted = self.delete(&to_delete)?;
        // Global resident-vector budget: evict the coldest resident vectors.
        let mut resident: Vec<(String, f32)> = by_source
            .values()
            .flatten()
            .filter(|(_, _, r)| *r)
            .map(|(id, heat, _)| (id.clone(), *heat))
            .collect();
        if resident.len() > self.config.max_resident_vectors {
            resident.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let excess = resident.len() - self.config.max_resident_vectors;
            let evict: Vec<String> = resident.drain(..excess).map(|(id, _)| id).collect();
            let records = self.records_by_ids(&evict)?;
            let mut index = self.index.write().unwrap();
            let mut meta = self.meta.write().unwrap();
            for r in records {
                index.remove(&r.id);
                index.insert(&r.id, &r.index_text(), None);
                if let Some(m) = meta.get_mut(&r.id) {
                    m.resident = false;
                }
                report.vectors_evicted += 1;
            }
        }
        if report.vectors_evicted > 0 {
            self.bump_generation();
        }
        Ok(report)
    }

    /// Rebuild the graph from stored vectors (after aging, an embedder
    /// change or a crash) and persist it.
    pub fn rebuild(&self) -> Result<()> {
        let now = Utc::now();
        let records = self.all_records()?;
        let vectors: HashMap<String, QuantizedVector> = {
            let txn = self.db.begin_read()?;
            let vt = txn.open_table(VECTORS_TABLE)?;
            let mut m = HashMap::new();
            for entry in vt.iter()? {
                let (k, v) = entry?;
                if let Some(q) = QuantizedVector::from_bytes(v.value()) {
                    m.insert(k.value().to_string(), q);
                }
            }
            m
        };
        let mut index = HybridIndex::new(self.config.dimension);
        let mut meta = self.meta.write().unwrap();
        let mut resident = 0usize;
        for r in &records {
            let hot = self.is_hot(r.timestamp, r.visits, now);
            let vec = if hot && resident < self.config.max_resident_vectors {
                vectors.get(&r.id).map(QuantizedVector::to_f32)
            } else {
                None
            };
            if vec.is_some() {
                resident += 1;
            }
            index.insert(&r.id, &r.index_text(), vec.as_deref());
            if let Some(m) = meta.get_mut(&r.id) {
                m.resident = vec.is_some();
                m.stored_vector = vectors.contains_key(&r.id);
            }
        }
        drop(meta);
        *self.index.write().unwrap() = index;
        self.bump_generation();
        self.persist_index()
    }

    // ------------------------------------------------------------ read

    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        let txn = self.db.begin_read()?;
        let rt = txn.open_table(RECORDS_TABLE)?;
        Ok(rt
            .get(id)?
            .and_then(|v| serde_json::from_str::<Record>(v.value()).ok()))
    }

    fn records_by_ids(&self, ids: &[String]) -> Result<Vec<Record>> {
        let txn = self.db.begin_read()?;
        let rt = txn.open_table(RECORDS_TABLE)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(v) = rt.get(id.as_str())? {
                if let Ok(r) = serde_json::from_str::<Record>(v.value()) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    pub fn all_records(&self) -> Result<Vec<Record>> {
        let txn = self.db.begin_read()?;
        let rt = txn.open_table(RECORDS_TABLE)?;
        let mut out = Vec::new();
        for entry in rt.iter()? {
            let (_, v) = entry?;
            if let Ok(r) = serde_json::from_str::<Record>(v.value()) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Records with no stored vector, as `(id, index_text)` ready to embed
    /// (input to `octos memory reindex` and the ingest backfill).
    pub fn records_needing_vectors(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let ids: Vec<String> = {
            let meta = self.meta.read().unwrap();
            let mut ids: Vec<(&String, &Meta)> =
                meta.iter().filter(|(_, m)| !m.stored_vector).collect();
            ids.sort_by_key(|(_, m)| std::cmp::Reverse(m.timestamp));
            ids.into_iter()
                .take(limit)
                .map(|(id, _)| id.clone())
                .collect()
        };
        Ok(self
            .records_by_ids(&ids)?
            .into_iter()
            .map(|r| (r.id.clone(), r.index_text()))
            .collect())
    }

    /// Store a vector for an existing record (backfill path).
    pub fn store_vector(&self, id: &str, vector: &[f32]) -> Result<bool> {
        let Some(record) = self.get(id)? else {
            return Ok(false);
        };
        if vector.len() < self.config.dimension {
            eyre::bail!(
                "vector too narrow: {} < {}",
                vector.len(),
                self.config.dimension
            );
        }
        let q = QuantizedVector::from_f32(&mrl_truncate(vector, self.config.dimension));
        let txn = self.db.begin_write()?;
        {
            let mut vt = txn.open_table(VECTORS_TABLE)?;
            vt.insert(id, q.to_bytes().as_slice())?;
        }
        txn.commit()?;
        let now = Utc::now();
        let mut meta = self.meta.write().unwrap();
        let mut index = self.index.write().unwrap();
        let hot = self.is_hot(record.timestamp, record.visits, now);
        let resident = hot
            && index.hnsw_points() < self.config.max_resident_vectors
            && index.add_embedding(id, &q.to_f32());
        if let Some(m) = meta.get_mut(id) {
            m.stored_vector = true;
            m.resident = resident;
        }
        drop(index);
        drop(meta);
        self.bump_generation();
        Ok(true)
    }

    /// Two-stage retrieval, stage one: rank records by hybrid score, apply
    /// the filter, return the best `limit` with their abstracts. Stage two
    /// is [`Self::get`] / the caller's `memory_load`.
    pub fn search(
        &self,
        query: &str,
        query_vector: Option<&[f32]>,
        filter: &SearchFilter,
    ) -> Result<Vec<Hit>> {
        let limit = filter.limit.clamp(1, 200);
        let truncated = query_vector.map(|v| mrl_truncate(v, self.config.dimension));
        let candidates = {
            let index = self.index.read().unwrap();
            // Over-fetch so filters have something to bite on.
            index.search_scored(query, truncated.as_deref(), limit * 6)
        };
        let filtered: Vec<(String, f32)> = {
            let meta = self.meta.read().unwrap();
            candidates
                .into_iter()
                .filter(|(id, _)| {
                    let Some(m) = meta.get(id) else { return false };
                    (filter.kinds.is_empty() || filter.kinds.contains(&m.kind))
                        && (filter.sources.is_empty()
                            || filter.sources.iter().any(|s| s == &m.source))
                        && filter.since.is_none_or(|t| m.timestamp >= t)
                        && filter.until.is_none_or(|t| m.timestamp <= t)
                })
                .map(|(id, s)| (id, s.combined))
                .take(limit)
                .collect()
        };
        let ids: Vec<String> = filtered.iter().map(|(id, _)| id.clone()).collect();
        let records = self.records_by_ids(&ids)?;
        let scores: HashMap<&str, f32> = filtered.iter().map(|(id, s)| (id.as_str(), *s)).collect();
        let mut hits: Vec<Hit> = records
            .into_iter()
            .map(|r| Hit {
                score: scores.get(r.id.as_str()).copied().unwrap_or(0.0),
                id: r.id,
                kind: r.kind,
                source: r.source,
                title: r.title,
                abstract_: r.abstract_,
                timestamp: r.timestamp,
                trust: r.trust,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.timestamp.cmp(&a.timestamp))
        });
        Ok(hits)
    }

    /// Documents hot enough to be worth distilling into Knowledge: not yet
    /// promoted, visited at least `min_visits` times, hottest first.
    pub fn nominate(&self, min_visits: u32, limit: usize) -> Result<Vec<Record>> {
        let now = Utc::now();
        let ids: Vec<String> = {
            let meta = self.meta.read().unwrap();
            meta.iter()
                .filter(|(_, m)| {
                    m.kind == RecordKind::Document && !m.promoted && m.visits >= min_visits.max(1)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut records = self.records_by_ids(&ids)?;
        records.sort_by(|a, b| {
            b.heat(now, self.config.half_life_days)
                .partial_cmp(&a.heat(now, self.config.half_life_days))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        records.truncate(limit);
        Ok(records)
    }

    pub fn mark_promoted(&self, ids: &[String]) -> Result<usize> {
        let mut records = self.records_by_ids(ids)?;
        let txn = self.db.begin_write()?;
        let mut n = 0;
        {
            let mut rt = txn.open_table(RECORDS_TABLE)?;
            let mut meta = self.meta.write().unwrap();
            for r in records.iter_mut() {
                r.promoted = true;
                rt.insert(r.id.as_str(), serde_json::to_string(&*r)?.as_str())?;
                if let Some(m) = meta.get_mut(&r.id) {
                    m.promoted = true;
                }
                n += 1;
            }
        }
        txn.commit()?;
        Ok(n)
    }

    pub fn stats(&self) -> RecallStats {
        let meta = self.meta.read().unwrap();
        let mut s = RecallStats {
            records: meta.len(),
            dimension: self.config.dimension,
            embedder_id: self.config.embedder_id.clone(),
            graph_persisted: *self.graph_persisted.read().unwrap(),
            ..Default::default()
        };
        for m in meta.values() {
            if m.stored_vector {
                s.vectors_stored += 1;
            }
            if m.resident {
                s.vectors_resident += 1;
            }
            *s.by_kind.entry(m.kind.as_str().to_string()).or_default() += 1;
            *s.by_source.entry(m.source.clone()).or_default() += 1;
        }
        s.disk_bytes = std::fs::metadata(self.dir.join("recall.redb"))
            .map(|m| m.len())
            .unwrap_or(0)
            + std::fs::read_dir(self.index_dir())
                .map(|d| {
                    d.flatten()
                        .filter_map(|e| e.metadata().ok())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
        s
    }

    /// Kinds/sources present, for tool descriptions and status output.
    pub fn sources(&self) -> Vec<String> {
        let meta = self.meta.read().unwrap();
        let mut v: Vec<String> = meta.values().map(|m| m.source.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Mirror an episode as a Recall record (`episode:<id>`).
pub fn record_from_episode(ep: &crate::Episode) -> Record {
    let summary = ep.summary.trim();
    let title = summary.lines().next().unwrap_or("").trim().to_string();
    let mut r = Record::new(
        format!("episode:{}", ep.id),
        RecordKind::Episode,
        "episodes",
        ep.created_at,
        if title.is_empty() {
            "(episode)".to_string()
        } else {
            title
        },
        summary,
    );
    r.parent = Some(ep.working_dir.to_string_lossy().to_string());
    r.fingerprint = format!("{:x}", md5_like(summary));
    r
}

/// Mirror a memory-bank page as a Knowledge record (`bank:<slug>`).
pub fn record_from_bank_page(
    slug: &str,
    abstract_: &str,
    modified: DateTime<Utc>,
    fingerprint: &str,
) -> Record {
    let mut r = Record::new(
        format!("bank:{slug}"),
        RecordKind::Knowledge,
        "bank",
        modified,
        slug.replace(['-', '_'], " "),
        abstract_,
    );
    r.fingerprint = fingerprint.to_string();
    r
}

/// Cheap stable content hash (FNV-1a 64) for fingerprints.
pub fn md5_like(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn cfg(dim: usize) -> RecallConfig {
        RecallConfig {
            dimension: dim,
            embedder_id: "test/e".into(),
            ..Default::default()
        }
    }

    fn doc(id: &str, title: &str, abs: &str, days_ago: i64) -> Record {
        let mut r = Record::new(
            format!("doc:mail:{id}"),
            RecordKind::Document,
            "mail",
            Utc::now() - Duration::days(days_ago),
            title,
            abs,
        );
        r.fingerprint = format!("fp-{id}");
        r
    }

    #[test]
    fn should_fall_back_to_memory_when_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let owner = RecallStore::open(dir.path(), cfg(4)).unwrap();
        assert!(
            RecallStore::open(dir.path(), cfg(4)).is_err(),
            "strict open refuses a held lock"
        );
        let degraded = RecallStore::open_or_degraded(dir.path(), cfg(4)).unwrap();
        assert!(degraded.is_degraded() && !owner.is_degraded());
        degraded
            .upsert(vec![doc("x", "shadow note", "only here", 1)], vec![None])
            .unwrap();
        assert_eq!(
            degraded
                .search(
                    "shadow",
                    None,
                    &SearchFilter {
                        limit: 5,
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
            1
        );
        degraded.persist_index().unwrap();
        assert!(!degraded.stats().graph_persisted);
        assert!(
            owner.get("doc:mail:x").unwrap().is_none(),
            "nothing leaks into the owner's store"
        );
    }

    #[test]
    fn should_search_bm25_only_without_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecallStore::open(dir.path(), cfg(4)).unwrap();
        let report = store
            .upsert(
                vec![
                    doc("1", "Dentist appointment", "Sunrise Dental on the 24th", 1),
                    doc("2", "Car rental deals", "same car less money", 2),
                ],
                vec![None, None],
            )
            .unwrap();
        assert_eq!(report.inserted, 2);
        let hits = store
            .search(
                "dentist",
                None,
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "doc:mail:1");
        assert_eq!(hits[0].trust, Trust::Untrusted);
        // Unchanged fingerprint ⇒ no rewrite.
        let again = store
            .upsert(
                vec![doc(
                    "1",
                    "Dentist appointment",
                    "Sunrise Dental on the 24th",
                    1,
                )],
                vec![None],
            )
            .unwrap();
        assert_eq!(again.unchanged, 1);
    }

    #[test]
    fn should_rank_by_vector_and_persist_graph_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![1.0, 0.0, 0.0, 0.0, 9.0, 9.0]; // wider than dim: MRL truncates to 4
        let b = vec![0.0, 1.0, 0.0, 0.0, 9.0, 9.0];
        {
            let store = RecallStore::open(dir.path(), cfg(4)).unwrap();
            let r = store
                .upsert(
                    vec![
                        doc("a", "alpha", "first thing", 1),
                        doc("b", "beta", "second thing", 1),
                    ],
                    vec![Some(a.clone()), Some(b.clone())],
                )
                .unwrap();
            assert_eq!(r.vectors_stored, 2);
            let hits = store
                .search(
                    "thing",
                    Some(&a),
                    &SearchFilter {
                        limit: 2,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(hits[0].id, "doc:mail:a");
            store.persist_index().unwrap();
            assert!(store.stats().graph_persisted);
            assert!(dir.path().join(INDEX_DIR).join(MANIFEST_FILE).exists());
        }
        {
            let store = RecallStore::open(dir.path(), cfg(4)).unwrap();
            let stats = store.stats();
            assert_eq!(stats.records, 2);
            assert_eq!(
                stats.vectors_resident, 2,
                "reloaded graph keeps both vectors resident"
            );
            assert!(stats.graph_persisted, "graph was reused, not rebuilt");
            let hits = store
                .search(
                    "thing",
                    Some(&b),
                    &SearchFilter {
                        limit: 2,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(hits[0].id, "doc:mail:b");
            // Inserting after a reload must still work.
            let c = vec![0.0, 0.0, 1.0, 0.0];
            store
                .upsert(
                    vec![doc("c", "gamma", "third thing", 1)],
                    vec![Some(c.clone())],
                )
                .unwrap();
            let hits = store
                .search(
                    "thing",
                    Some(&c),
                    &SearchFilter {
                        limit: 3,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(hits[0].id, "doc:mail:c");
            assert_eq!(hits.len(), 3);
        }
    }

    #[test]
    fn should_drop_vectors_from_another_embedder_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = RecallStore::open(dir.path(), cfg(4)).unwrap();
            store
                .upsert(
                    vec![doc("a", "alpha", "x", 1)],
                    vec![Some(vec![1.0, 0.0, 0.0, 0.0])],
                )
                .unwrap();
            store.persist_index().unwrap();
        }
        let mut other = cfg(4);
        other.embedder_id = "other/model".into();
        let store = RecallStore::open(dir.path(), other).unwrap();
        let s = store.stats();
        assert_eq!((s.records, s.vectors_stored, s.vectors_resident), (1, 0, 0));
        assert_eq!(store.records_needing_vectors(10).unwrap().len(), 1);
        assert!(
            store
                .store_vector("doc:mail:a", &[0.0, 1.0, 0.0, 0.0])
                .unwrap()
        );
        assert_eq!(store.stats().vectors_resident, 1);
    }

    #[test]
    fn should_keep_cold_records_bm25_only_and_revive_on_touch() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(4);
        c.hot_days = 30;
        let store = RecallStore::open(dir.path(), c).unwrap();
        store
            .upsert(
                vec![
                    doc("old", "ancient note", "long ago", 400),
                    doc("new", "fresh note", "yesterday", 1),
                ],
                vec![
                    Some(vec![1.0, 0.0, 0.0, 0.0]),
                    Some(vec![0.0, 1.0, 0.0, 0.0]),
                ],
            )
            .unwrap();
        let s = store.stats();
        assert_eq!((s.vectors_stored, s.vectors_resident), (2, 1));
        // BM25 still finds the cold one.
        let hits = store
            .search(
                "ancient",
                None,
                &SearchFilter {
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits[0].id, "doc:mail:old");
        assert!(store.touch("doc:mail:old").unwrap());
        assert_eq!(
            store.stats().vectors_resident,
            2,
            "touch makes the vector resident"
        );
        assert_eq!(store.get("doc:mail:old").unwrap().unwrap().visits, 1);
    }

    #[test]
    fn should_evict_and_delete_by_heat_when_aging() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(4);
        c.max_resident_vectors = 2;
        c.max_records_per_source = 3;
        let store = RecallStore::open(dir.path(), c).unwrap();
        let recs = (0..4)
            .map(|i| doc(&i.to_string(), &format!("note {i}"), "body", i))
            .collect();
        let vecs = (0..4)
            .map(|i| Some(vec![i as f32 + 1.0, 1.0, 0.0, 0.0]))
            .collect();
        store.upsert(recs, vecs).unwrap();
        assert_eq!(
            store.stats().vectors_resident,
            2,
            "graph capacity respected on insert"
        );
        let report = store.age(Utc::now()).unwrap();
        assert_eq!(
            report.records_deleted, 1,
            "coldest record beyond the per-source cap is deleted"
        );
        assert!(store.get("doc:mail:3").unwrap().is_none());
        assert_eq!(store.stats().records, 3);
    }

    #[test]
    fn should_filter_by_kind_source_and_time_and_nominate_hot_documents() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecallStore::open(dir.path(), cfg(4)).unwrap();
        let page = record_from_bank_page(
            "sam-lee",
            "Sam Lee: hiking friend, prefers weekends",
            Utc::now(),
            "h1",
        );
        let mut event = Record::new(
            "doc:calendar:hike",
            RecordKind::Document,
            "calendar",
            Utc::now() + Duration::days(3),
            "Weekend hike with Sam",
            "West Hill trail",
        );
        event.fingerprint = "e1".into();
        store
            .upsert(
                vec![page, event, doc("m", "Hike photos from Sam", "attached", 5)],
                vec![None, None, None],
            )
            .unwrap();
        let only_bank = store
            .search(
                "sam",
                None,
                &SearchFilter {
                    kinds: vec![RecordKind::Knowledge],
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(only_bank.len(), 1);
        assert_eq!(only_bank[0].trust, Trust::Trusted);
        let future = store
            .search(
                "sam",
                None,
                &SearchFilter {
                    since: Some(Utc::now()),
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            future.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["doc:calendar:hike"]
        );
        let mail_only = store
            .search(
                "sam",
                None,
                &SearchFilter {
                    sources: vec!["mail".into()],
                    limit: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(mail_only[0].id, "doc:mail:m");
        assert!(store.nominate(1, 5).unwrap().is_empty());
        store.touch("doc:mail:m").unwrap();
        store.touch("doc:mail:m").unwrap();
        let nominated = store.nominate(2, 5).unwrap();
        assert_eq!(nominated.len(), 1);
        store.mark_promoted(&["doc:mail:m".to_string()]).unwrap();
        assert!(store.nominate(1, 5).unwrap().is_empty());
        assert_eq!(store.sources(), vec!["bank", "calendar", "mail"]);
    }
}
