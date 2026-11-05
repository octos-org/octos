# UPCR-2026-RA5312057 — Remove the unused pipeline crate without protocol changes

Status: proposed (implemented for review)
Task: T-0396
PR: #3044

## Decision record

The unused standalone `crates/octos-pipeline` crate is removed by owner decision
(shu 2026-11-05, T-0396). Two comments in `ui_protocol_transport.rs` are updated
only so they no longer refer to deleted source. No RPC method, notification,
payload, schema, capability, permission, compatibility behavior, or runtime wire
mapping is added, removed, or changed. Existing supervised-task and progress
behavior remains unchanged. Cite this record as `UPCR-2026-RA5312057`.

## Validation

Run `cargo check --workspace --all-targets`, Rust formatting, the quality
ratchet, UPCR registry/coverage checks, document-reference lint, and the CI
matrix. The protocol diff is comment-only.
