# UPCR-2026-R79B943CA — oauth-path coverage + boundary restores (test-only)

Status: proposed
PR: #3164

## Decision record

This PR makes NO protocol-visible change. It edits only test files matched by
the UPCR gate's glob: a runtime `client.statuses()` test proving
`config.oauth` surfaces true/false on real rows (the `oauth` field itself was
covered by UPCR-2026-R3B3ADF57 / #3145), plus restoring two safety-boundary
tests #3144's keep-list deletion removed — `ws_origin_gate_allows_absent_or_
empty_origin` (#924 allow-side) and `should_normalize_safe_tool_context_at_
protocol_boundary` (`normalize_tool_context` rejection branches) — both listed
in TESTS-KEPT category 3. Cite this record elsewhere as `UPCR-2026-R79B943CA`.
