# Octos UI Protocol Change Request: Semantic Context/Cache Diagnostics

## Header

- Request id: `UPCR-2026-029`
- Title: Negotiate display-only OUP semantic context and cache diagnostics
- Date: 2026-09-03
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Depends on: `context.lifecycle.v1`

## Summary

Adds the optional capability `context.semantic_cache.v1`. When advertised
alongside `context.lifecycle.v1`, the server may populate these additive fields
on `UiContextState`:

- `cache_epoch_id`
- `last_cache_invalidation_reason`
- `semantic_head_id`
- `semantic_head_kind`

The fields expose OUP-owned state for diagnostics. They do not let a client
choose compaction boundaries, rotate epochs, or alter provider-cache policy.

## Motivation

OUP is the single context authority for OctosCode and future clients. A client
needs enough information to explain a cache miss or compaction without parsing
prompt bodies or recreating ContextManager policy. Reusing the existing
context lifecycle records keeps one ordered lifecycle while the separate
capability lets clients distinguish older lifecycle servers from servers that
understand semantic epochs.

## Wire shape

The existing `context_state` object remains valid when every new field is
absent. A capable server may return:

```json
{
  "session_id": "dev:local:tui",
  "generation": 42,
  "transcript_hash": "sha256:...",
  "item_count": 17,
  "token_estimate": 3890,
  "recovery_state": "exact",
  "cache_epoch_id": "sha256:...",
  "last_cache_invalidation_reason": "compaction_installed",
  "semantic_head_id": "semblk_ctxitem_000117",
  "semantic_head_kind": "tool_interaction"
}
```

All identifiers are opaque. None is a prompt body, hidden-reasoning excerpt,
credential, tool output, or provider request payload.

## Negotiation and compatibility

- Feature name: `context.semantic_cache.v1`.
- Dependency: clients request it with `context.lifecycle.v1`; servers advertise
  only known requested features.
- A client must not infer semantic-cache support merely because an unknown
  optional field survived deserialization. It enables the diagnostic UI only
  when the feature is advertised.
- Old clients ignore the optional fields under normal JSON compatibility rules.
- Old servers omit both the feature and fields; the client keeps the existing
  generation/checkpoint/compaction view.
- Stdio clients request the feature during their ordinary capabilities probe,
  exactly like other context lifecycle capabilities.

## Replay semantics

The fields are snapshots of the ContextManager state at the lifecycle event's
generation. They inherit the durability and ordering of the containing
notification. A reconnect must not combine a replayed epoch identifier with a
newer generation or a different workspace-scoped session affinity.

## Client rules

- Display the values only as diagnostics.
- Never write them into the conversation transcript or model prompt.
- Never expose prompt content to make an identifier more descriptive.
- Unknown invalidation reasons and semantic block kinds render as opaque text.
- Absence is “server did not report”, not evidence of a cache hit or miss.

## Server rules

- ContextManager remains the source of truth.
- Epoch changes carry an explicit reason at the generation where they occur.
- Diagnostics contain hashes, counts, kinds, and reason codes only.
- Provider cache failure never changes correctness or session durability.

## Frontend convergence

OctosCode consumes this capability through OUP. Future `octos chat` and ACP
frontends must use the same OUP session/context lifecycle rather than install a
second semantic ledger or a client-owned compactor. Their adoption is a routing
change, not permission to copy ContextManager.

## Implementation anchors

- Feature and fields: `crates/octos-core/src/ui_protocol.rs`
- OUP lifecycle population and capabilities: `crates/octos-cli/src/api/ui_protocol_transport.rs`
- OctosCode decoding/rendering: `src/client_event.rs`, `src/model.rs`, `src/store.rs`
- Design and acceptance evidence: `docs/adr/oup-semantic-boundary-context-cache.md`
