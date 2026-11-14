# UPCR-2026-R3553E72B — Conversation Episode Handoff

Status: proposed (implemented for review)
PR: #3228
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
Canonical and degraded handles share a per-episode OS lock, a claim file and
a durable completion marker beside the redb. The first claim fixes the
episode's body: a retry from a later compaction, or a canonical handle after a
degraded one, finishes that episode rather than indexing its own text, so the
stored body, recall and the vector always describe one text. The marker is
published only after store, recall, and embedding all succeed, and records the
text and its vector; process termination before completion is retried. Mixed
serve/gateway handoffs and gateway restarts therefore do not duplicate the
embedding. A canonical handle that finds a handoff completed by a degraded one,
whose recall and vector were never durable, stores the claimed body, mirrors it
into its recall and stores the recorded vector, without embedding again. When
the marker holds no usable vector for that text (an earlier build's bare
marker, or a corrupt one), it embeds again; it never skips the backfill. A
corrupt claim is replaced, and the canonical redb row wins over a claim that
disagrees with it. Unreadable handoff files fail the handoff without a permit.
Without an embedder there is no handoff, so every completion has a vector. A
failed write is not acknowledged and can be retried by a later compaction; a turn never fails because the memory handoff
failed. Existing legacy Agent episode writes, workspace/profile namespaces,
and recall behavior remain unchanged.
