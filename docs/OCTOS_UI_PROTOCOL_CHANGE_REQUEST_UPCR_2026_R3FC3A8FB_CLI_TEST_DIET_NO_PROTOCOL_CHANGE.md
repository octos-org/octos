# UPCR-2026-R3FC3A8FB — CLI retained-test deletion, no protocol change

Status: proposed
PR: TBD

## Decision record

T-0505 / #3069 deletes only `octos-cli` tests outside the approved and merged
`crates/octos-cli/TESTS-KEPT.md` inventory (`39db145d45d8cd27a5e031d310f7840cf7058ca6`),
plus fixtures and `#[cfg(test)]` helpers that become unused. The 66 retained tests,
their assertions, and their synchronization semantics are unchanged.

This record covers edits to files with `ui_protocol_*` names solely because they
contain or support deleted tests. No RPC method, envelope, event, error mapping,
permission, capability advertisement, persistence format, or client/server wire
behavior changes. Production protocol code is not being used as a substitute for
test deletion; prerequisite production cleanup landed separately in #3125.

Compatibility and durability contracts are therefore unchanged. Validation is
actual test discovery parity (66 expected, 66 discovered, no missing or extra),
execution of all retained tests, strict all-target Clippy, and the normal CI gates.
Cite this record as `UPCR-2026-R3FC3A8FB`.
