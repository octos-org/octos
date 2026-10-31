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

## Limits and status

At most 32 connectors per profile. Explicit reconnects are limited to one every
5 seconds per connector. `mcp/status/list` reports disabled/connected/failed/
closed state and tool count from the live `McpClient`, not from tool names.
