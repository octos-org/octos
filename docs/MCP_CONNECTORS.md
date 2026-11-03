# MCP connector management

UPCR-2026-062 adds profile-scoped MCP connector CRUD and live status. Stdio is
privileged: a connector can execute a process on the core host, so the web/API
cannot supply arbitrary commands. Only an operator can maintain the deployment
allowlist.

## Stdio allowlist

Set `mcp_stdio_allowlist` in the deployment's `config.json`. It is not a
profile-editable field and no network-install/npx entry is built in.

```json
{
  "mcp_stdio_allowlist": [
    {
      "id": "local-fs",
      "command": "/opt/octos/bin/filesystem-mcp",
      "args": ["--root", "/srv/shared"],
      "env": { "MCP_MODE": "readonly" },
      "concurrency_class": "exclusive"
    }
  ]
}
```

- `command` must be an absolute executable path.
- `args` and the allowed environment are fixed by the operator; OUP accepts only
  the entry id. Existing `BLOCKED_ENV_VARS` stripping still applies.
- `concurrency_class` is `safe` or `exclusive`; web-created stdio connectors
  default to `exclusive` when omitted.
- An empty/missing allowlist means web stdio creation is refused.

HTTP endpoints must be http(s), must not contain URL userinfo, and are checked
against private/loopback targets and the existing SSRF resolver. OAuth requires
HTTPS. Secret headers are not accepted by the PR1 web contract.

## OAuth callback and token storage

`mcp/oauth/start` accepts an optional `redirect_uri`. The preferred value is the
current web Origin plus `/oauth/mcp/callback`; it must match the connection
Origin and be listed in `OCTOS_APPUI_ALLOWED_ORIGINS`. The same URI is used in
OAuth dynamic registration. With no usable Origin, the flow uses
`manual_callback_url`: the user pastes the complete callback URL from the browser
address bar. Flow state is one-time and expires after 10 minutes. After
`mcp/oauth/complete`, clients must remove code/state from browser history.

Tokens are namespaced by `(profile_id, normalized_url)`. OS keyring is preferred;
when it is unavailable, tokens are encrypted with XChaCha20-Poly1305 in the
profile data dir. The random key and token files are created with mode 0600 and
never stored in profile JSON or returned by OUP. `token_storage` reports
`keyring` or `encrypted_file`; there is no plaintext fallback.

## Limits and status

At most 32 connectors per profile. Explicit reconnects are limited to one every
5 seconds per connector. `mcp/status/list` reports disabled/connected/failed/
closed state and tool count from the live `McpClient`, not from tool names.
