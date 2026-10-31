# UPCR-2026-065 — Session-bound profile memory

- Status: proposed (implemented for review)

## Problem

First-run setup creates `coding` while a solo access token authenticates as
`admin`. The memory panel disabled access because the legacy memory RPCs would
read or write the authentication profile instead of the selected session.

## Contract

All seven memory methods accept optional `session_id` under negotiated
`memory.session_scope.v1` plus the existing `auxiliary.rest_to_ws.v1` gate.
Selection uses the session's embedded or recorded profile binding and the
UPCR-2026-064 session authorization rules. Unscoped admin may use named profiles;
users cannot cross profile boundaries. Routed admin is confined to its tenant.
Unknown raw ids fail closed, including after restart until session/open restores
the binding. Empty ids are invalid. Auth remains required.

Omitted/null session ids retain the legacy auth-profile behavior and all result
shapes remain unchanged. Clients check the new feature before sending scope so
an old kernel cannot silently ignore it. Explicit scope without negotiation is
rejected. The file read protections and memory admission/consolidation rules
remain unchanged. App-bound peers/contexts are refused rather than redirected
to profile memory; host-managed external and ingress access remain denied.

## Scope and verification

This is the ordinary profile-memory slice of the issue also discussed in
#2679; it does not adopt that PR's app namespace, skill, history or MCP changes.
Tests cover seven RPCs for admin/owner and foreign users, unknown sessions,
legacy calls, and feature negotiation. The paired browser change sends the
opened session id and preserves the old-kernel ownership guard with an upgrade
hint. Acceptance also exercises real first-run setup and memory read/write.
