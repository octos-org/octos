# UPCR-2026-062 — MCP Connector Management and OAuth

Status: proposed (PR1 and PR2 implemented for review)
Task: T-0045
Capabilities: `mcp.connector.manage.v1`, `mcp.oauth.v1`

## Scope and ownership

Connectors are profile-scoped, not session-owned. Every mutation resolves the
selected session through `raw_scoped_llm_profile_id` and `validate_session_scope`;
profile writes have the same authorization boundary as provider saves. Session
ingress and host-managed external clients are denied. Mutations persist through
`ProfileStore::save_with_merge`, then use the existing runtime transition so new
turns see the new config while active turns retain their old runtime Arc.

Every mutation records actor/profile/connector/result in the existing admin
audit. DTOs and audit summaries never include command args, env, HTTP headers,
access tokens, or refresh tokens.

## Limits

- 32 connectors per profile (`connector_limit`).
- Existing MCP initialize/tools-list timeout remains 30 seconds per server.
- Explicit reconnect is rate limited to once per connector per 5 seconds.
- OAuth flows expire after 10 minutes and are single-use.

## PR1 methods (`mcp.connector.manage.v1`)

- `mcp/connector/create {session_id,connector}` creates a server-generated id.
  `connector.transport` is `stdio` or `http`; name is required. Stdio accepts only
  `allowlist_entry_id`, never command/args/env. HTTP accepts url/scopes/
  concurrency_class but no static secret headers.
- `mcp/connector/update {session_id,connector_id,patch}` updates name, URL,
  allowlist entry, scopes, or concurrency class without changing transport/id.
- `mcp/connector/delete {session_id,connector_id,revoke_oauth?}` is idempotent.
  OAuth token revocation is implemented in PR2.
- `mcp/connector/set_enabled {session_id,connector_id,enabled}` persists disabled
  state and stops future connections.
- `mcp/connector/reconnect {session_id,connector_id}` rebuilds the live runtime.

`mcp/status/list` remains ungated and adds enabled/closed state. Status comes
from the profile's live `McpClient`, including disabled, connected, zero-tool,
failed, and closed states; it is not inferred from tool visibility.

## Stdio allowlist

Deployment-level `mcp_stdio_allowlist` entries have `{id,command,args,env,
concurrency_class}`. Command is an absolute executable path; argv and env schema
are operator-fixed. No npx/network-install entry is built in. Stdio defaults to
`exclusive` unless the operator explicitly chooses `safe`.

## PR2 methods (`mcp.oauth.v1`)

- `mcp/oauth/start {session_id,connector_id,scopes?,redirect_uri?}` returns
  `{flow_id,authorization_url,expires_at,completion,token_storage}`.
- `mcp/oauth/complete {session_id,flow_id,callback_url}` validates one-time
  state/iss, exchanges the code, persists credentials, and reconnects.
- `mcp/oauth/cancel {session_id,flow_id}` is idempotent.
- `mcp/oauth/revoke {session_id,connector_id}` deletes credentials and reconnects
  as unauthorized.

The preferred redirect is the current connection Origin plus the fixed path
`/oauth/mcp/callback`. That Origin must be in `OCTOS_APPUI_ALLOWED_ORIGINS` and
the same URI is used during dynamic registration. Without a usable Origin the
client uses `manual_callback_url` and pastes the full callback URL. State binds
flow/session/profile and is one-time/TTL-bound. Browsers clear code/state from
history after completion.

OAuth access and refresh tokens never enter OUP, logs, audit, or profile JSON.
Token storage is profile+URL namespaced. OS keyring is preferred; when unavailable
(e.g. headless Linux Secret Service), the fallback is a per-profile data-dir
encrypted file with mode 0600. The data-dir key is randomly generated on first
use, stored separately with mode 0600, and never serialized into profile config.
Status identifies `keyring` vs `encrypted_file` storage. No silent plaintext
fallback is allowed.

## Errors

`invalid_params`, `resource_not_found(profile|mcp_connector|oauth_flow)`,
`auth_scope_violation`, `forbidden`, `connector_limit`, `stdio_not_allowlisted`,
`connector_disabled`, `reconnect_rate_limited`, `connector_start_failed`,
`oauth_flow_expired`, `oauth_state_mismatch`, `oauth_exchange_failed`, and
`token_storage_unavailable`. Details are redacted and never include secrets.
