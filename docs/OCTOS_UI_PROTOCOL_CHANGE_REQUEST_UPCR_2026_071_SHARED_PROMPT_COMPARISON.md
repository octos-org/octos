# UPCR-2026-071 — Shared prompt comparison without protocol contract changes

Status: proposed (implemented for review)
Task: T-0114 / A-07
Capability: none (no protocol change)

## Problem

SessionActor and AppUI duplicated prompt coverage helpers, and SessionActor drifted by omitting media and reasoning from comparison. Unifying the helpers touches protocol-owned transport code without changing its RPC contract.

## Contract

No RPC method, notification, payload, schema, capability, or permission changes. The canonical internal comparator retains AppUI's existing role, content, media, reasoning, tool-call-id, and tool-call semantics. SessionActor stops treating different provider-visible media or reasoning as covered; AppUI behavior is unchanged. Turn and terminal wire behavior is unchanged.

## Validation

Add a SessionActor regression that fails before unification, retain the AppUI media/reasoning regression, and run the SessionActor and UI protocol transport suites plus all-target checks.
