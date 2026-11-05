# UPCR-2026-R1601E7D1 — OUP write application services without protocol contract changes

Status: proposed (implemented for review)
Task: T-0275 / PR B
Capability: none (no protocol change)

## Problem

Cron auxiliary methods and smart-home control methods invoked Axum REST helpers directly, coupling mutations and device side effects to HTTP extractors and response bodies. Moving those paths behind typed application services touches protocol-owned transport code without changing its external contract.

## Contract

No RPC method, notification, event payload, schema, capability, permission, or compatibility behavior is added, removed, or changed. Cron list/create/delete/toggle retains profile/session authorization (including the session-scoped profile resolution from #2966), runtime ownership, mutation locking, gateway-running conflicts, idempotent delete, schedule/timezone validation, and persistence errors. Smart-home device command and camera start/stop retain profile-scoped bridge/token resolution, reply correlation, repeated-command behavior, and real side effects; invalid commands, offline/timeout, and bridge rejection remain failures rather than success. The two services do not share runtime ownership. No consumer migration is required.

## Validation

Run the consolidated cron panel, smart-home bridge/panel, and full UI protocol transport suites, including concurrent toggles, cross-profile origin refusal, session-scoped profile resolution, repeated command delivery, reply correlation, invalid/offline/timeout failures, and camera control. Verify strict Clippy, the quality ratchet, the UPCR gate, and the full CI matrix.
