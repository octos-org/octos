# UPCR-2026-017: AppUI Runtime, Auth, And LLM Profile Inspection

Status: accepted

## Summary

Add AppUI JSON-RPC methods that let non-web clients render server-owned login,
runtime, provider, model, MCP, and tool state without reading local backend
configuration directly.

These methods are additive and share the existing AppUI JSON-RPC envelope. They
are transport-neutral: WebSocket and stdio clients receive the same method names
and compatible payloads.

## Methods

- `config/capabilities/list`
- `session/status/read`
- `auth/status`
- `auth/send_code`
- `auth/verify`
- `auth/me`
- `auth/logout`
- `profile/llm/catalog`
- `profile/llm/list`
- `profile/llm/upsert`
- `profile/llm/select`
- `profile/llm/delete`
- `profile/llm/test`
- `profile/llm/fetch_models`
- `mcp/status/list`
- `tool/status/list`

## Contract

- `config/capabilities/list` returns the server-advertised
  `UiProtocolCapabilities` payload outside the `session/open` flow.
- `session/status/read` returns session runtime status plus the runtime policy
  stamp visible to TUI, web, logs, and tests.
- `auth/*` exposes the email OTP login surface used by the dashboard. The
  current server may report an already-authenticated development account when a
  local token has been provisioned.
- `profile/llm/catalog` returns the dashboard provider catalog, including model
  family, model name, official routes, alternate provider routes, and custom
  OpenAI-compatible route support.
- `profile/llm/upsert` persists the selected provider into the same profile JSON
  shape as the dashboard: `config.llm.primary` plus `config.env_vars` keys.
  Secret values must be redacted from user-facing artifacts.
- `profile/llm/list`, `profile/llm/select`, `profile/llm/delete`,
  `profile/llm/test`, and `profile/llm/fetch_models` are the profile-management
  command surface used by slash-command onboarding.
- `mcp/status/list` and `tool/status/list` expose server truth. Clients must not
  infer MCP/tool runtime state from local config.

## Compatibility

Older servers reject these methods with `method_not_supported`. Clients must
consult `supported_methods` before enabling login/provider/model/MCP/tool slash
commands.

Unknown fields in catalog, status, or test-result payloads are ignorable.

## Tests

- AppUI capability advertisement includes these methods.
- TUI availability gates keep onboarding/model/MCP/tool commands disabled when
  these methods are absent.
- Profile upsert persists dashboard-compatible redacted JSON.
- Live tmux soak proves login status, catalog refresh, provider save, model
  turn, runtime policy artifact, and secret redaction.
