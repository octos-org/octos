# Personal memory — three tiers by trust, one index

- Date: 2026-09-16 (proposal)
- Status: **Proposed**. Phase 1 (plugin skill, no kernel change) starts in
  Octoscript-AppCard alongside this ADR; phases 2–3 land in `octos-memory` /
  `octos-agent` behind follow-up PRs that cite this record.
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

## Consequences

- One tool and one index to reason about; the bank keeps its editable, auditable form.
- The embedder becomes optional rather than a precondition for recall.
- Costs: a persisted graph and quantised vectors add code in `octos-memory`; ingestion adds a write API the kernel deliberately lacked — it must inherit the guard and the viewer-only routes' authentication.
- Open questions: llama.cpp embedder build for Android arm64; retrieval quality at 256 d on real mail (measure before committing); the pane-model vs kernel-agent tool split in the phone shell (skills reach the kernel, module tools do not).

## References

- `crates/octos-memory/src/{hybrid_search,store,memory_store,guard}.rs`; `crates/octos-agent/src/agent/memory.rs`; `crates/octos-cli/src/commands/memory.rs`
- Octoscript-AppCard `docs/LEDGER-ARCHITECTURE.md` §13 (multi-device: unresolved); `apps/mail/README.md` (isolation of mail and credentials)
- Kang, Ji, Zhao, Bai. *Memory OS of AI Agent*. arXiv:2506.06326, 2025.
