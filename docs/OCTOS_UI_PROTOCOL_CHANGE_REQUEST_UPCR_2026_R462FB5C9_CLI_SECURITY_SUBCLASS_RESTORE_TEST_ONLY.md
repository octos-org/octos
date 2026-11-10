# UPCR-2026-R462FB5C9 — CLI security subclass test restoration (test-only)

Status: proposed
PR: #3185

## Decision record

T-0550 / #3069 restores 15 compact security representatives under `octos-cli`, including tests in the UI-protocol hydrate-response and durable-ledger source files. Those paths trigger the protocol-visible gate, but this PR changes **no production code, wire type, numeric error code, frame shape, capability, dispatcher behavior, or client contract**. All added Rust code in those files is under `cfg(test)`; integration changes are test targets only.

The retained contract is fail-closed: an unshrinkable hydrate response returns the existing `hydrate_references_unavailable` / `response_bytes_limit` error instead of a partial result, and credentials are redacted before durable persistence and replay. The PR adds regression coverage for those existing behaviors; it does not introduce a protocol revision or alter compatibility.

`docs/upcr/README.md` intentionally has zero diff under the decentralized-id rule. The bounded test-only growth is +873 test-lines and has human `test-growth-approved` from BH3GEI.
