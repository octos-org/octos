# UPCR-2026-R33917087 — Mechanical UI test-domain extraction without protocol changes

Status: proposed (implemented for review)
Task: T-0274 / T-0195 / T-0212 / T-0271
Capability: none (no protocol change)

## Decision record

Runtime-helper, workspace-browse, and session-CWD regression tests move
unchanged out of `ui_protocol_tests.rs` into three child modules. No RPC
method, notification, event payload, schema, capability, permission,
lifecycle, or client-visible behavior changes. The 63 source scenarios,
assertions, fixtures, platform guards, and ignored count are unchanged; only
source locations and fully qualified test module paths change. Shared test
support remains in the parent module. Cite this record as
`UPCR-2026-R33917087`.

Validation: the consolidated UI test inventory and the full transport group
run are recorded in the PR description; agent/CLI clippy, formatting, typos,
and diff checks pass.
