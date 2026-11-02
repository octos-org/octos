# UPCR-2026-077 — Immutable user-media snapshots before prompt persistence

Status: proposed (implemented for review)
Task: T-0099 / #2505 / O-018
Capability: none (no protocol change)

## Problem

User/upload image and video paths were persisted and later reread by providers. A source leaf or parent replacement after ingestion could therefore change the bytes associated with an existing user turn. Agent-only freezing would also miss the WS and session-actor rows persisted before Agent invocation.

## Contract

No RPC method, notification, payload schema, capability, permission boundary, or client file-handle shape changes. Image/video ingestion now writes a private read-only digest-named snapshot plus source provenance before the WS, session-actor, or direct-Agent user row is persisted. Durable `Message.media` references the snapshot rather than the mutable source; existing client-facing handles remain unchanged.

Rendering verifies snapshot digest and MIME and never follows provenance back to the source. Already-managed snapshots are idempotent, including pre-provenance digest-only snapshots after the same digest/MIME checks. Unmanaged raw paths cannot have their historical bytes proven retroactively and are refused instead of silently reread. Snapshot ingestion or verification failure never falls back to the source path. Tool model media and gateway video attachments follow the same freeze rule. No TTL, LRU, cleanup period, or new product default is introduced.

## Validation

The Agent ingestion regression was RED on the original path implementation (replacement bytes were read) and GREEN after snapshotting. Focused tests cover image and video provenance, source-parent replacement, concurrent readers, unmanaged-path refusal, and tamper detection. WS/session-actor persistence tests verify that pre-provider rows carry snapshot paths. Existing provider serializer tests use managed snapshots and retain their wire-shape assertions.
