# UPCR-2026-070 — Internal cleanup without protocol contract changes

Status: proposed (implemented for review)
Task: T-0114 / A-14
Capability: none (no protocol change)

## Problem

Architecture cleanup removes unwired code and module-wide dead-code allows from files that also own UI protocol implementation. The protocol-change gate requires an explicit record even though this cleanup does not alter the wire contract.

## Contract

No RPC method, notification, event payload, schema, capability, permission, or compatibility behavior is added, removed, or changed. Deleted lifecycle methods were crate-internal and had no production dispatch. Test-only and platform-only gates do not alter live protocol paths. Turn completion and terminal payloads remain unchanged.

## Validation

Run CLI all-target checks and focused agent, bus, orchestrator, and UI tests. Review the diff for wire-compatible deletion only; no consumer migration is required.
