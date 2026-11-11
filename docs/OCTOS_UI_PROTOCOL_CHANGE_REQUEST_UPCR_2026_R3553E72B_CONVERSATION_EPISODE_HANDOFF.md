# UPCR-2026-R3553E72B — Conversation Episode Handoff

Status: proposed (implemented for review)
PR: T-0405
Issue: #1620

## Decision record

UPCR-2026-R3553E72B records an internal memory-handoff change with no UI
Protocol wire, capability, persistence-layout, or ledger-contract change.
After an installed ContextManager compaction, both the serve and gateway
pre-turn producers and prompt-context bridges hand the same authoritative
summary to the session's memory store as one `EpisodeSource::Conversation`
episode. A compaction whose snapshot persistence failed is not eligible.

The handoff uses a deterministic `(profile_id, session_key, workspace_root)`
episode id and an atomic insert-if-absent store transaction. Reconnects,
per-turn Agent reconstruction, concurrent bridge calls, and process restarts
therefore cannot duplicate the durable episode or its recall/embedding work.
Canonical and degraded handles share a per-episode OS lock and durable
completion marker beside the redb. The marker is published only after store,
recall, and embedding all succeed; process termination before completion is
retried. Mixed serve/gateway handoffs and gateway restarts therefore do not
duplicate recall/embedding work. A canonical handle may backfill the durable
body behind a completed degraded handoff without repeating that work. A failed write is not acknowledged and can be
retried by a later compaction; a turn never fails because the memory handoff
failed. Existing legacy Agent episode writes, workspace/profile namespaces,
and recall behavior remain unchanged.
