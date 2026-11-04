# UPCR-2026-R687F72FD — OUP query application services without protocol contract changes

Status: proposed (implemented for review)
Task: T-0275 / PR A
Capability: none (no protocol change)

## Problem

Session, content, memory, and smart-home read-only OUP methods invoked Axum REST helpers directly, coupling application behavior to HTTP extractors and response bodies. Consolidating those paths behind typed query services touches protocol-owned transport code and the SessionRef catalog without changing its external contract.

## Contract

No RPC method, notification, event payload, schema, capability, permission, or compatibility behavior is added, removed, or changed. Session list/messages/status/files/tasks/workspace/snapshot and title.set retain cursor/seq, history, workspace/profile authorization, and error behavior; the legacy session list and SessionRef catalog retain their external-client/feature gates, workspace fields, and active-turn semantics. Content/list retains catalog, path-handle, and preview behavior. Memory overview/entity and smart-home status/device-list retain profile scoping, missing/empty/offline behavior, redaction, and wire errors. The change only moves shared application logic behind typed services and keeps REST/OUP wire mapping separate. No consumer migration is required.

## Validation

Run the consolidated session handler, content catalog, memory panel, smart-home bridge/panel, and full UI protocol transport suites, including service/REST/OUP equivalence, cross-profile/workspace, missing/empty, limit/truncation, offline, timeout, and redaction cases. Verify strict Clippy, the quality ratchet, the UPCR gate, and the full CI matrix.
