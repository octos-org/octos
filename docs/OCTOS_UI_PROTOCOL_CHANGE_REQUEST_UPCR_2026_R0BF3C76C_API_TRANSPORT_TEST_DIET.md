# UPCR-2026-R0BF3C76C — Remove protocol-inventory test mirrors

Status: proposed (implemented for review)
Task: T-0444 / tracking #3069
Capability: none (no protocol change)

## Decision record

This test-only diet removes five exact protocol-surface inventory tests and
three helpers used only by them: the specification §6 catalog mirror, stdio
capability-list mirrors, advertised-method dispatch enumeration, and the
server-supported route inventory. They duplicate production method lists,
map keys, and per-method fixtures without exercising additional user chains.

Real OUP session/turn/approval flows, stdio auth-bound typed rejection,
NDJSON frame limits and recovery, reconnect/replay, profile/tenant scope, and
recovery/concurrency tests remain unchanged. No RPC method, notification,
payload, schema, capability, permission, lifecycle, or client-visible
behavior changes. Cite this record as `UPCR-2026-R0BF3C76C`.

Validation: the retained UI protocol transport suite and required CI gates
run on the PR; before/after test-line counts and keep/delete rationale are
recorded there.
