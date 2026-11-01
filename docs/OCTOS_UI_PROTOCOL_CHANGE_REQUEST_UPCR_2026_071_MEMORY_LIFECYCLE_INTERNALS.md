# UPCR-2026-071 — Memory lifecycle internals without protocol contract changes

Status: proposed (implemented for review)
Task: T-0110
Capability: none (no protocol change)

## Problem

Embedding and session-runtime lifecycle work touches the UI protocol transport implementation, so the protocol-change gate requires an explicit record. The changes are internal resource management: lazy/shared embedding state, prioritized blocking decode, runtime-cache invalidation on session deletion, side-metadata cleanup, and a heuristic cache byte budget.

## Contract

No RPC method, notification, event payload, schema, capability, permission, compatibility behavior, JSONL format, or persistence format is added, removed, or changed. `session/delete` keeps its existing authorization, response, and durable-delete behavior; it additionally invalidates the corresponding in-memory runtime and side metadata so a later open cannot reuse deleted state. Cache eviction and embedding scheduling remain server-internal and are not exposed through the protocol. No consumer migration is required.

## Validation

Exercise session deletion across profiles and side metadata, cache budget accounting, model-backed embedding lifecycle tests, and the existing UI protocol session suites. Verify the UPCR registry lint and full CI matrix.
