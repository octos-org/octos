# ADR-001 — Personal memory: three tiers by trust, one index

- Date: 2026-09-16 (proposal) / 2026-09-17 (implementation landed with the
  record; see "Implementation" below)
- Status: **Accepted — implemented through phase 3** on this branch. Phase 1
  (plugin skill, no kernel change) lives in Octoscript-AppCard
  (`apps/personal-data`, PR #98); phases 2–3 are in `octos-memory`,
  `octos-agent`, `octos-cli`, `octos-ffi`/`octos-uniffi` in the same PR as
  this record. Phase 4 (cross-device) stays optional and unimplemented.
- Scope: `octos-memory`, `octos-agent` tools, `octos-cli` runtime/profile,
  plugin skills; app-side ingestion in Octoscript-AppCard (Mail, Calendar) and
  the OctoSense phone shell.
- Branch: `design/personal-memory-tiers` → `main`.

## Context

The kernel keeps two memories that do not meet:

- **Episodic** (`crates/octos-memory/src/{store,hybrid_search}.rs`): `Episode.summary` (≤ 500 B) in `<data_dir>/episodes.redb` (tables `episodes`, `cwd_index`, `embeddings`), indexed by an in-process `hnsw_rs` HNSW (cosine, L2-normalised, M=16, ef=200/30) fused with BM25 (0.7/0.3). The graph is rebuilt from redb on every open; the cap is 10 000 vectors; deletes are tombstones; there is no time decay. Recall is automatic prompt injection at task/session start (`recall_relevant_episodes`, floor 0.55, 6 results) and is **disabled outright when no embedder is configured**. No agent tool searches it.
- **Memory bank** (`crates/octos-memory/src/memory_store.rs`): markdown under `<data_dir>/memory/` — `MEMORY.md`, daily notes, `bank/entities/*.md`, staging notes → LLM consolidation (`octos-cli/src/memory_consolidate`), a prompt-injection guard on every write (`guard.rs`), usage counters. Injected as a name + abstract list within `max_inject_tokens` (tail-truncated silently), then fetched by exact name (`recall_memory {name}`). Never embedded, never BM25-indexed.

Neither has an inbound path for app data. Mail keeps `mailbox-*.json` in its module's private storage and its `ServiceExecutor` refuses every tool call by design; Calendar keeps a reducer replica plus a sync server. The kernel exposes no memory-write API (HTTP/WS memory routes are viewer-only; FFI `octos_embed` returns a vector and stores nothing; the embedded/mobile runtime boots with `save_episodes: false`). MCP over loopback is refused (`reject_private_url_host`); the phone shell's module tools reach the pane's model, not the kernel agent.

MemoryOS (Kang, Ji, Zhao, Bai — arXiv 2506.06326) supports a tiered design: short/mid/long-term stores, eviction and promotion by *heat* (`visits + size + recency`), two-stage retrieval (segment → page), ~4 k recalled tokens and k≈10 pages as the quality/cost optimum; the mid-term tier is the ablation's largest contributor. It is dialogue-only and summary-heavy, carries no trust model, and says nothing about persistence or devices.

## Decision

Three tiers **by lifecycle and trust**, **one retrieval index**.

| Tier | Holds | Source of truth | How the model gets it |
| --- | --- | --- | --- |
| Working | the session's turns and tool results | session | it is the prompt (compaction unchanged) |
| Recall | episodes **and app records** (mail threads → messages, calendar series → events, contacts, notes) — untrusted, high volume | the apps (records) / redb (episodes) | on demand: `memory_search` → `memory_load`, two-stage, ≤ 4 k tokens; episodes also keep today's automatic injection |
| Knowledge | curated facts, entity pages, traits, the app manual — small, trusted, human-editable | markdown files | injected core + relevance-selected pages; also searchable |

Flows: ingestion → Recall; heat/usage nominates Recall items → staging → consolidation (guard, provenance) → Knowledge; aging evicts from Recall and demotes from injection in Knowledge. App data never enters Knowledge directly.

### Recall: disk and memory budget

The index stores per record `id, source, kind, parent, timestamp, title ≤ 120 B, abstract ≤ 300 B, trust, heat`, BM25 postings and **one vector**. No bodies; the apps stay the record of truth.

- Vectors are **record-level** (mail: subject + sender + a two-sentence summary; event: title + location + notes), **Matryoshka-truncated to 256 d** (`octos-embed-llama::mrl_truncate`; 384 d if measured quality demands), **int8 at rest** (256 B), dequantised into the graph.
- The HNSW graph is **persisted** (`hnsw_rs` `file_dump`/`HnswIo`) and only rebuilt on an embedder change (`octos memory reindex`).
- Per-source caps and **heat aging** (`heat = visits + size + e^{-Δt/μ}`, μ ≈ 4 months): cold records lose their vector first (they remain BM25-searchable), then their postings, then the record; real deletes with periodic compaction replace tombstones.
- **Residency**: only the hot window (e.g. the last six months of mail, all future events) is in the in-memory graph; older ranges are BM25-only until touched.
- BM25 is always on; vectors are an improvement when a local GGUF embedder is configured. Recall must never be *disabled* for lack of an embedder.

Budget for 10 000 mails + 5 000 events (~30 MB of text held by the apps): ≈ 4 MB vectors + ≈ 8 MB postings + ≈ 2 MB records ≈ **14 MB on disk**, **< 15 MB RAM** (hot-window vectors + graph). The naive alternative (1536-d f32, everything resident) is ~90 MB on disk and ~90 MB RAM.

### Retrieval

`memory_search {query, sources?, since?, limit?}` returns hits `{id, source, title, abstract, score, updated, trust}` across Recall and Knowledge from one `HybridIndex`, two-stage (segment → page, top-5 → top-10), then `memory_load {id, page?}` returns plain text. App content is labelled untrusted in the tool result. A standing prompt rule: search memory before answering about the user's people, dates, mail or past work.

### Knowledge

Bank pages are indexed on file change (mtime/hash). `recall_memory` gains `query`. Injection = fixed core (`MEMORY.md`, the app-cards manual) + the top-k pages relevant to the turn, within budget, instead of alphabetical tail truncation. Promotion carries provenance (`origin: mail:<id>`, confidence, timestamp), goes through staging + consolidation + guard, and is also available to the user explicitly (`octos memory remember`). Aging demotes from injection, never from the index.

### Privacy and trust

Never index credentials or app secrets. Mail bodies are indexed only on opt-in (default: subject, sender, date, summary). App content is untrusted on retrieval and guarded on every write into Knowledge. Raw app data never leaves its app; only derived records may be synced across devices (phase 4).

## Plan

1. **Search without kernel changes** — a `personal-data` plugin skill under `<data_dir>/skills/` with `mail_search`, `calendar_query`, `contacts_lookup`; Mail and Calendar maintain their own small index (FTS5 BM25, optional vectors) and answer over the skill protocol. Acceptance: on Mac and phone the agent answers "when is my dentist / what did Sam mail about the hike"; zero growth of octos memory.
2. **Recall tier in the kernel** — `Document` record kind beside `Episode`; `memory/ingest` UI-protocol method and FFI `octos_memory_upsert/search`; `memory_search`/`memory_load` tools; `save_episodes` on for the embedded runtime; persisted HNSW, int8 vectors, MRL truncation, heat aging, per-source caps. Acceptance: 10 k mails within the budget above, cold start < 1 s, p95 search < 50 ms on a OnePlus 6, BM25-only works.
3. **Knowledge indexed and fed** — bank indexing, relevance-selected injection, heat-driven promotion through consolidation with provenance. Acceptance: a 500-page bank selects the right pages ≥ 90 % on a small eval set; the guard blocks the injection corpus.
4. **Cross-device (optional)** — derived records carried by the calendar-style sync server; redb single-writer means the kernel owns the index and apps ingest through it.

## Implementation

What landed with this record (phase 1 in Octoscript-AppCard, phases 2–3 here):

- **Records and quantised vectors** — `octos_memory::{Record, RecordKind, Trust}`
  (`record.rs`: title ≤ 120 B, abstract ≤ 300 B, optional body ≤ 16 KiB,
  fingerprint, visits/last_visit, `heat()`); `quant.rs` (`mrl_truncate`,
  `QuantizedVector` int8 with per-vector scale, 6-byte header on disk).
- **One index, persisted** — `octos_memory::RecallStore` (`recall.rs`):
  `<data_dir>/recall.redb` (tables `records`, `vectors`, `meta`) plus
  `<data_dir>/recall-index/` with the dumped HNSW graph and a manifest pinned
  to (embedder, dimension, generation). Open reloads the graph when the
  manifest matches, else rebuilds from the int8 vectors and dumps again.
  `HybridIndex` gained `dump_hnsw`/`load_hnsw`/`attach_hnsw`/`layout`.
  Residency: only records inside `hot_days` (default 180) or ever visited
  keep a vector in the graph; the rest stay BM25-only until `touch`.
  `age()` evicts vectors beyond `max_resident_vectors` and deletes records
  beyond `max_records_per_source` by heat; `rebuild()` compacts. Vectors from
  a different embedder id or width are dropped on open and re-embedded by
  the backfill. Default width 256 (`memory.recall_dimension`), never wider
  than the configured embedder.
- **Tools** — `memory_search {query, kinds?, sources?, since?, until?, limit?}`
  and `memory_load {id}` (`octos-agent/src/tools/memory_{search,load}.rs`);
  `recall_memory` accepts `query` and answers with the best-matching bank page.
  Results label every record's trust; document bodies are not stored unless
  the producer sent them, so `memory_load` points back at the owning app.
- **Ingestion** — UI protocol `memory/search`, `memory/load`, `memory/ingest`
  (auth-bound, `auxiliary.rest_to_ws.v1`); FFI `octos_memory_upsert` /
  `octos_memory_search` / `octos_memory_load` / `octos_memory_stats` and the
  uniffi `Runtime::memory_*` twins; `octos memory ingest <file.json>`. Ingest
  refuses Knowledge records and forces Documents to untrusted.
- **Episodes** — every saved episode is mirrored as `episode:<id>` (vector
  stored after the fire-and-forget embed); the existing automatic injection
  path is unchanged.
- **Knowledge** — `memory_index::sync_bank` mirrors bank pages as
  `bank:<slug>` Knowledge records on content-hash change (and drops deleted
  pages); the memory prompt segment now ranks bank rows by relevance to the
  turn (`MemorySegmentProvider::with_recall`, top 12 rows, remainder disclosed)
  instead of listing every page alphabetically; `octos memory promote`
  nominates hot Documents (≥ N loads, never twice) into the staging area as
  host fact notes carrying provenance, where the existing consolidation and
  guard decide what reaches `MEMORY.md`.
- **Bundled embedder** — `embed-llama` is a default feature of `octos-cli`, `octos-ffi` and `octos-uniffi` (release builds include it; macOS adds Metal). With no `embedding` config the runtime uses EmbeddingGemma-300M Q8_0, fetched once into `<data_dir>/models/` from the public ggml-org release, SHA-256-pinned (`octos-cli/src/embed_model.rs`; `octos memory embedder [--fetch]`; `octos doctor` reports it; `embedding.auto_download=false` / `OCTOS_NO_MODEL_DOWNLOAD=1` opt out; licence in `docs/THIRD_PARTY_MODELS.md`). The Recall index records it as `llamacpp/embeddinggemma-300M-Q8_0`.
- **Upkeep** — profile bootstrap spawns bank sync, vector backfill and aging;
  `octos memory search|ingest|promote` operate on the same store.
- **Not done** — phase 4 (cross-device sync of derived records); the
  `personal-data` skill keeps its own app-side index until the apps push
  records through `memory/ingest` (next step on the app side).

## Consequences

- One tool and one index to reason about; the bank keeps its editable, auditable form.
- The embedder becomes optional rather than a precondition for recall.
- Costs: a persisted graph and quantised vectors add code in `octos-memory`; ingestion adds a write API the kernel deliberately lacked — it must inherit the guard and the viewer-only routes' authentication.
- Measured on the OnePlus 6 (Snapdragon 845, Android 15, EmbeddingGemma-300M Q8_0 via llama.cpp cross-built for arm64 without dotprod, 4 threads): model load ≈ 2 s; prompt processing ≈ 139 tok/s (`llama-bench pp128`), ≈ 115 tok/s on real batched records; 64 real mail/calendar records (≈ 6.8k tokens) embedded in 57 s ≈ 0.9 s per record; a batch of four short queries ≈ 0.5 s after load, ≈ 120 ms per query; peak RSS ≈ 636 MB (weights 312 MiB + context). Q4_0 was slower here (75 s for the same records, 581 MB): without dotprod the Q8 kernels win on this CPU. Consequence: query-time embedding is fine on the phone; bulk ingest must run in the background at ≈ 1 record/s, which the design already allows because BM25 answers immediately and vectors backfill in bounded batches. Retrieval quality at 256 d was checked on real data (Mac): semantic queries with no keyword overlap rank the right record first.
- Open questions: the pane-model vs kernel-agent tool split in the phone shell (skills reach the kernel, module tools do not); the embedder's memory footprint on the phone (≈ 600 MB resident while loaded) argues for loading it on demand and unloading after ingest.

## References

- `crates/octos-memory/src/{hybrid_search,store,memory_store,guard}.rs`; `crates/octos-agent/src/agent/memory.rs`; `crates/octos-cli/src/commands/memory.rs`
- Octoscript-AppCard `docs/LEDGER-ARCHITECTURE.md` §13 (multi-device: unresolved); `apps/mail/README.md` (isolation of mail and credentials)
- Kang, Ji, Zhao, Bai. *Memory OS of AI Agent*. arXiv:2506.06326, 2025.
