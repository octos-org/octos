# UPCR-2026-RB4AE009A — Delivered-artifact retention and explicit cleaned downloads

Status: proposed (implemented for review)
Task: T-0099 / #2543 / O-019
Capability: none (no RPC schema change)

## Problem

Browser-downloadable copies of externally delivered files lived forever and an evicted/missing copy could not be distinguished from an unauthorized or absent download. A per-profile bound must not become a cross-profile cleanup-state oracle or change ordinary workspace/profile downloads.

## Contract

Each profile independently limits its external delivered copies with `artifact_retention`: 50 GiB by default, an optional entry limit, and no TTL. A versioned manifest persists deterministic sequence-based LRU and `present|evicted` state across restart. Eviction persists a tombstone before unlinking; sibling `.artifacts`, workspace files, and snapshots are out of scope. Re-delivery of the same reference creates a new present copy. Persisted profile policy remains effective without an active LLM runtime.

No RPC method, notification, session schema, capability, permission boundary, or client file-handle shape changes. After existing profile/session authorization, `/api/files` returns HTTP **410** with JSON `error.code = "artifact_cleaned"` for an evicted delivered copy. Missing/unauthorized requests retain their existing response. Ordinary profile/workspace downloads do not consult this manifest. Downloads open and touch under the profile lock and then read only the opened descriptor, so eviction/replacement cannot produce mixed bytes. Manifest keys are confined to the delivered-copy shape; all delivered file operations use nofollow parent descriptors. Corrupt or unexpectedly missing initialized manifests fail closed without deletion/re-adoption, and reconciliation repairs size/recency after a replacement crash.

## Validation

Focused tests cover default and nested config, byte and entry eviction, download recency, restart persistence, profile isolation, re-delivery, corrupt/missing-manifest fail-closed behavior, malicious manifest keys, symlink ancestors/cycles, replacement-crash reconciliation, legacy adoption without sibling deletion, and open-fd replacement/eviction. A real `serve_file_by_query` test without an active runtime verifies authorized 410 and identical foreign-evicted/foreign-missing 403 responses. No timer, cleanup period, implicit entry bound, or TTL is introduced.
