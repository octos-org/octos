# UPCR-2026-085 — Launch dispatch transport coverage without protocol changes

Status: proposed (implemented for review)
Task: T-0178 (post-merge test coverage)
Capability: none (no protocol change)

## Problem

#2971's `launch/resolve` authentication scoping shipped with handler-wrapper tests only: the WebSocket `UiCommand::LaunchResolve` dispatch arm and the real `stdio_connection_with_io` loop were never exercised, so wiring regressions (dropped authenticated identity, stdio inheriting a prior `session/open` binding, stdio applying the WS admin-owned sticky rule) passed CI undetected.

## Contract

No production code, RPC method, notification, payload, schema, capability, or permission changes. This change only adds tests: one drives a real loopback WebSocket upgrade through `ui_protocol_connection` into the `UiCommand::LaunchResolve` dispatch arm; one drives `stdio_connection_with_io` through `session/open` followed by `launch/resolve`. Existing wrapper tests are preserved unchanged.

## Validation

Run the launch/dispatch focused tests and the full CLI library suite. Mutation evidence: dropping the WS authenticated identity fails with `cross_profile != activate`; making stdio inherit the prior binding fails with `zbeta != coding`; applying admin-owned-only sticky policy to stdio fails with `zbeta != coding`.
