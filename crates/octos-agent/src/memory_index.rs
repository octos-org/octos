//! Keeping the Recall/Knowledge index in step with its sources (ADR
//! "personal memory — three tiers by trust, one index", phases 2–3):
//!
//! - [`sync_bank`] mirrors memory-bank pages as Knowledge records (indexed
//!   on change, by content hash), so `memory_search` / `recall_memory
//!   {query}` can find a page by meaning and the prompt injection can be
//!   relevance-selected.
//! - [`backfill_vectors`] embeds records that have no vector yet (records
//!   ingested without an embedder, or after an embedder change).
//! - [`rank_bank_pages`] orders bank slugs by relevance to a query.
//! - [`nominate_for_promotion`] moves hot app records into the staging
//!   area with provenance, where the consolidation pass (guarded) decides
//!   whether they become long-term memory.

use std::sync::Arc;

use eyre::{Result, WrapErr};
use octos_llm::{EmbedQueueFull, EmbeddingPriority, EmbeddingProvider};
use octos_memory::{
    MemoryStore, NoteKind, NoteOrigin, RecallStore, Record, RecordKind, SearchFilter, StagingNote,
    UpsertReport, record_from_bank_page,
};

/// Records embedded per provider call.
const EMBED_BATCH: usize = 16;

/// FNV-1a over the page text: cheap, stable, no crypto needed.
fn content_hash(s: &str) -> String {
    format!("{:016x}", octos_memory_hash(s))
}

fn octos_memory_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

async fn embed_texts(
    embedder: &dyn EmbeddingProvider,
    texts: &[String],
) -> Result<Vec<Option<Vec<f32>>>> {
    let mut out = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(EMBED_BATCH) {
        let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
        match embedder
            .embed_with_priority(&refs, EmbeddingPriority::Background)
            .await
        {
            Ok(vecs) if vecs.len() == chunk.len() => out.extend(vecs.into_iter().map(Some)),
            Ok(vecs) => {
                tracing::warn!(
                    want = chunk.len(),
                    got = vecs.len(),
                    "embedder returned a short batch"
                );
                out.extend(std::iter::repeat_n(None, chunk.len()));
            }
            Err(e) => {
                // T-0317: a full bounded background queue is BACKPRESSURE, not
                // an embedding failure. Propagate it as a typed error so the
                // caller retries instead of recording a fake-successful
                // keyword-only batch (`backfill_vectors` used to fold this
                // into `Ok(0)` and stop draining until restart).
                if e.downcast_ref::<EmbedQueueFull>().is_some() {
                    return Err(e.wrap_err("embedding background queue full during index upkeep"));
                }
                tracing::warn!(error = %e, "embedding batch failed; records indexed keyword-only");
                out.extend(std::iter::repeat_n(None, chunk.len()));
            }
        }
    }
    Ok(out)
}

/// Mirror every bank page into the index. Pages whose content hash matches
/// the stored record are skipped (no re-embedding); deleted pages are
/// removed. Returns the upsert report.
pub async fn sync_bank(
    bank: &MemoryStore,
    recall: &Arc<RecallStore>,
    embedder: Option<&dyn EmbeddingProvider>,
) -> Result<UpsertReport> {
    let entities = bank.list_entities().await.wrap_err("list bank entities")?;
    let dir = bank.bank_entities_dir();
    let mut records = Vec::new();
    let mut texts = Vec::new();
    let mut live_ids = std::collections::HashSet::new();
    for (slug, abstract_) in entities {
        let id = format!("bank:{slug}");
        live_ids.insert(id.clone());
        let Some(content) = bank.read_entity(&slug).await.ok().flatten() else {
            continue;
        };
        let fingerprint = content_hash(&content);
        let existing = {
            let recall = recall.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || recall.get(&id)).await??
        };
        if existing
            .as_ref()
            .is_some_and(|r| r.fingerprint == fingerprint)
        {
            continue;
        }
        let modified = tokio::fs::metadata(dir.join(format!("{slug}.md")))
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Utc>::from)
            .unwrap_or_else(chrono::Utc::now);
        let record = record_from_bank_page(&slug, &abstract_, modified, &fingerprint);
        texts.push(record.index_text());
        records.push(record);
    }
    // Pages removed from disk leave the index too.
    let stale: Vec<String> = {
        let recall = recall.clone();
        tokio::task::spawn_blocking(move || recall.all_records())
            .await??
            .into_iter()
            .filter(|r| r.kind == RecordKind::Knowledge && !live_ids.contains(&r.id))
            .map(|r| r.id)
            .collect()
    };
    if !stale.is_empty() {
        let recall = recall.clone();
        tokio::task::spawn_blocking(move || recall.delete(&stale)).await??;
    }
    if records.is_empty() {
        return Ok(UpsertReport::default());
    }
    let vectors = match embedder {
        Some(e) => embed_texts(e, &texts).await?,
        None => vec![None; records.len()],
    };
    let recall = recall.clone();
    let report = tokio::task::spawn_blocking(move || {
        let report = recall.upsert(records, vectors)?;
        recall.persist_index()?;
        Ok::<_, eyre::Report>(report)
    })
    .await??;
    tracing::info!(
        inserted = report.inserted,
        updated = report.updated,
        "memory bank pages indexed"
    );
    Ok(report)
}

/// Embed up to `limit` records that have no stored vector. Returns how
/// many vectors were written.
///
/// T-0317 error contract: a shed [`EmbedQueueFull`] (bounded background
/// queue backpressure) propagates as `Err`, so a non-empty pending set can
/// never collapse into a fake-successful `Ok(0)` that stops the caller's
/// drain until restart. Real embedding failures keep the keyword-only
/// fallback (records stay indexed, the batch is logged, the drain can move
/// on) — the two classes are never conflated.
pub async fn backfill_vectors(
    recall: &Arc<RecallStore>,
    embedder: &dyn EmbeddingProvider,
    limit: usize,
) -> Result<usize> {
    let pending = {
        let recall = recall.clone();
        tokio::task::spawn_blocking(move || recall.records_needing_vectors(limit)).await??
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = pending.iter().map(|(_, t)| t.clone()).collect();
    let vectors = embed_texts(embedder, &texts).await?;
    let recall = recall.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let mut n = 0;
        for ((id, _), v) in pending.into_iter().zip(vectors) {
            if let Some(v) = v {
                if recall.store_vector(&id, &v).unwrap_or(false) {
                    n += 1;
                }
            }
        }
        if n > 0 {
            recall.persist_index()?;
        }
        Ok::<_, eyre::Report>(n)
    })
    .await??;
    Ok(stored)
}

/// Bank page slugs ordered by relevance to `query` (best first), at most
/// `limit`. Empty when the index holds no pages or the query is blank.
pub async fn rank_bank_pages(
    recall: &Arc<RecallStore>,
    embedder: Option<&dyn EmbeddingProvider>,
    query: &str,
    limit: usize,
) -> Vec<String> {
    let query = query.trim();
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    let vector = match embedder {
        Some(e) => e
            .embed(&[query])
            .await
            .ok()
            .and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0))),
        None => None,
    };
    let recall = recall.clone();
    let q = query.to_string();
    tokio::task::spawn_blocking(move || {
        recall.search(
            &q,
            vector.as_deref(),
            &SearchFilter {
                kinds: vec![RecordKind::Knowledge],
                limit,
                ..Default::default()
            },
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default()
    .into_iter()
    .filter_map(|h| h.id.strip_prefix("bank:").map(str::to_string))
    .collect()
}

/// Promotion, step one: nominate hot app records (visited at least
/// `min_visits` times, not yet promoted) into the staging area as host
/// fact notes carrying provenance. The existing consolidation pass then
/// runs them through the guard before anything reaches MEMORY.md. Returns
/// the nominated records.
pub async fn nominate_for_promotion(
    bank: &MemoryStore,
    recall: &Arc<RecallStore>,
    min_visits: u32,
    limit: usize,
    session_key: Option<String>,
) -> Result<Vec<Record>> {
    nominate_for_promotion_from_sources(bank, recall, min_visits, limit, session_key, &[]).await
}

/// App-scoped selection; the source labels narrow data, never confer authority.
/// Existing callers retain the unfiltered behavior through the wrapper above.
pub async fn nominate_for_promotion_from_sources(
    bank: &MemoryStore,
    recall: &Arc<RecallStore>,
    min_visits: u32,
    limit: usize,
    session_key: Option<String>,
    sources: &[String],
) -> Result<Vec<Record>> {
    let _promotion = recall.lock_promotion().await;
    let candidates = {
        let recall = recall.clone();
        let sources = sources.to_vec();
        tokio::task::spawn_blocking(move || {
            recall.nominate_from_sources(min_visits, limit, &sources)
        })
        .await??
    };
    let mut promoted = Vec::new();
    for r in candidates {
        let content = format!(
            "{}: {}\n\nprovenance: {} ({}, {}; loaded {} time(s))",
            r.title,
            r.abstract_,
            r.id,
            r.source,
            r.timestamp.format("%Y-%m-%d"),
            r.visits
        );
        let note = StagingNote {
            origin: NoteOrigin::Host,
            kind: NoteKind::Fact,
            content,
            session_key: session_key.clone(),
            sensitive: false,
            replaces_id: None,
        };
        match bank.write_staging_note(&note).await {
            Ok(_) => promoted.push(r),
            Err(e) => tracing::warn!(id = %r.id, error = %e, "promotion note rejected"),
        }
    }
    if !promoted.is_empty() {
        let ids: Vec<String> = promoted.iter().map(|r| r.id.clone()).collect();
        let recall = recall.clone();
        tokio::task::spawn_blocking(move || recall.mark_promoted(&ids)).await??;
    }
    Ok(promoted)
}
