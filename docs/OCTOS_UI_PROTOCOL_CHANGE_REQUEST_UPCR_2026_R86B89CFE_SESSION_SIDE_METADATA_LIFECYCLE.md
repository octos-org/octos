# UPCR-2026-R86B89CFE — Session side-metadata lifecycle without protocol contract changes

Status: proposed (implemented for review)
Task: T-0111
Capability: none (no protocol change)

## Problem

S7/S8 lifecycle work touches the UI protocol transport, ledger, workspace and session-scope implementations, so the protocol-change gate requires an explicit record. Reconstructible workspace/context/open-profile/compaction side metadata previously accumulated across sessions, persist-lock entries retained unused mutexes, and terminal approval/question entries had no bound.

## Contract

No RPC method, notification, event payload, schema, capability, permission, compatibility behavior, JSONL format, or persistence format is added, removed, or changed. LRU eviction applies only to reconstructible side metadata. Explicit permission selections are not evicted: a missing selection must never silently become a more permissive server default. Persist-lock reconstruction retains the committed sequence watermark; pending approvals/questions are never evicted, and recent terminal identifiers retain their existing typed late-response errors. Older terminal identifiers use the existing unknown-id behavior. No consumer migration is required.

## Validation

Exercise side-map and permission pressure, terminal approval/question bounds with pending reconnect hydration, ledger thread-sequence cleanup, Weak persist-lock/watermark preservation, session deletion and profile scope, the existing UI protocol suites, the UPCR registry lint, and the full CI matrix.
