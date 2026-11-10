# UPCR-2026-R25D45254 — mcp/status fallback oauth coverage (test-only)

Status: proposed
PR: T-0538 follow-up

## Decision record

This PR makes NO protocol-visible change. It edits only
`crates/octos-cli/src/api/ui_protocol_tests.rs` (a test-only file matched by
the UPCR gate's glob), adding a test that pins the NO-runtime fallback path of
`mcp_status_list_result`: with no session runtime, the oauth flag is projected
from `profile.config.mcp_servers` — true for an OAuth connector and
always-present false for a plain one — so a fresh page can offer
re-authorization. The `oauth` field itself and its always-present
serialization are governed by UPCR-2026-R3B3ADF57 (#3145); this only proves
the fallback path keeps the same contract. Cite this record elsewhere as
`UPCR-2026-R25D45254`.
