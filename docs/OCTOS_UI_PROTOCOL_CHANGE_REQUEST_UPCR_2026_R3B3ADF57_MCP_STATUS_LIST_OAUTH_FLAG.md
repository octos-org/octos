# UPCR-2026-R3B3ADF57 — mcp/status/list rows carry the connector oauth flag

Status: proposed
PR: #3145

## Decision record

`mcp/status/list` response `servers[]` rows gain an additive boolean field
`oauth`, sourced from the connector's `McpServerConfig.oauth`. The field is
always serialized (including `false`) so a fresh page can tell whether a
failed/expired connector uses OAuth and offer re-authorization, without
guessing from error text. Both construction paths (runtime
`client.statuses()` and the profile-config fallback) populate it, matching
the existing `connector_summary` `"oauth"` key. Old clients are unaffected
(additive field; they ignore it). No credentials beyond the existing scopes
are surfaced. Cite this record elsewhere as `UPCR-2026-R3B3ADF57`.
