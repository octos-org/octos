# ADR: Migrate the MCP client to the `rmcp` SDK (stdio + streamable-HTTP + OAuth 2.1)

Status: implemented (branch `feat/mcp-rmcp-oauth`) — all four increments landed:
1. ✅ stdio via rmcp (live-validated: real Gmail through better-email-mcp, clean child reap)
2. ✅ streamable-HTTP + static bearer (`Authorization` header → rmcp `auth_header`)
3. ✅ OAuth 2.1 transport + `octos mcp login`/`logout` + keyring token storage
4. ✅ tests (36 pass, 0 warnings in octos-agent) + live stdio validation

## Context

octos's MCP client (`crates/octos-agent/src/mcp.rs`) is hand-rolled and, per the
2026-07-09 deep review, has real protocol gaps: it never sends
`notifications/initialized`, hardcodes `protocolVersion` `2024-11-05`, reads
exactly one line per request (desyncs on any interleaved server notification),
has no OAuth, and can't drive the streamable-HTTP GET/SSE result channel. It
therefore cannot connect to OAuth-gated remote MCP servers the way codex/Claude
can — which is the capability we want ("do what codex does").

codex solved this by wrapping the official **`rmcp` Rust SDK** (v1.8) — not a
hosted third-party service — with `oauth2`, a `tiny_http` loopback catcher, and
the OS keyring for token storage. We adopt the same approach.

## Decision

Replace the hand-rolled client with an `rmcp`-backed one supporting three
transports, preserving the existing public surface (`McpServerConfig`,
`McpClient::start`, `register_tools`) so the 5 call sites are untouched.

Dependencies (vetted — compile cleanly in octos's tree, exit 0):
`rmcp = "1.8"` features `client, auth, macros, base64, transport-async-rw,
transport-child-process, transport-streamable-http-client-reqwest`; `oauth2 = "5"`;
reuse existing `keyring = "3"`. (rmcp pulls its own reqwest 0.13.2 alongside
octos's 0.12.28 — both coexist; at the transport boundary use rmcp's internal
client construction, not octos's reqwest type.)

## Behaviors to preserve (from the current mcp.rs)

- Fail-soft per-server `start()` (warn + skip a server that fails; never abort).
- Schema validation: `MAX_SCHEMA_DEPTH=10`, `MAX_SCHEMA_SIZE=64KB` → skip a tool
  whose schema exceeds limits.
- Per-server concurrency class (`resolved_concurrency_class`, default `Safe`,
  unknown → `Exclusive` fail-safe).
- `PROTECTED_NAMES` collision rejection (an MCP tool can't shadow a builtin).
- stdio env sanitization: `sanitize_command_env` + `EnvAllowlist::from_names` +
  `BLOCKED_ENV_VARS` before spawning the child.
- 60s per-tool-call timeout.
- `Tool` trait impl: `name/description/concurrency_class/input_schema/execute`;
  result mapping = join `content[].text`, `isError → success=false`.

## rmcp API blueprint (from codex `codex-rs/rmcp-client/`, rmcp 1.8)

- Handshake is automatic: `rmcp::service::serve_client(handler, transport)` sends
  `initialize` + `notifications/initialized` and returns
  `RunningService<RoleClient, S>`. `service.peer().peer_info()` = `InitializeResult`.
- Client handler: a struct impl'ing `rmcp::ClientHandler` whose `get_info()`
  returns `ClientInfo` (= `InitializeRequestParams`) with name/version.
- stdio transport: `TokioChildProcess::builder(tokio::process::Command)…spawn()`
  (build the `Command` with sanitized env first). `kill_on_drop(true)` +
  `process_group(0)` on unix for clean teardown (fixes the review's grandchild leak).
- streamable-HTTP: `StreamableHttpClientTransport` +
  `StreamableHttpClientTransportConfig::with_uri(url)`; `.auth_header(tok)` for a
  **static** bearer.
- streamable-HTTP + OAuth: `OAuthState::new(url)` → `set_credentials` →
  extract `AuthorizationManager` → `AuthClient::new(reqwest_client, manager)` →
  `StreamableHttpClientTransport::with_client(auth_client, cfg)`. `AuthClient`
  exposes `auth_manager: Arc<Mutex<AuthorizationManager>>` for refresh.
- tools: `service.list_tools(None) -> ListToolsResult{tools}`;
  `service.call_tool(CallToolRequestParam{name, arguments}) -> CallToolResult{content, is_error}`.
- OAuth login (interactive, `octos mcp login <server>`):
  `OAuthState::new(url)` → `start_authorization(&scopes, &redirect_uri, Some("octos"))`
  (does `.well-known` discovery + RFC 7591 dynamic client registration + PKCE) →
  present `get_authorization_url()` → `tiny_http` loopback on `/callback/{id}`
  (id = base64url(sha256(url))[..9]) → `handle_callback(code, state)` →
  `get_credentials()` → persist to keyring (service `"octos MCP Credentials"`,
  key `"{server}|{sha256(json)[..16]}"`) with wall-clock `expires_at`.
- Refresh: before each op, if `now+30s >= expires_at`, `manager.refresh_token()`,
  then re-persist if the token changed.

## Increment plan

1. **stdio via rmcp** — replace the hand-rolled stdio path; keep env sanitization,
   schema validation, concurrency class, protected names, 60s timeout. Re-validate
   against a real stdio server (better-email-mcp). Kills the desync/initialized/
   version P1s for stdio.
2. **streamable-HTTP (static bearer)** — replace the hand-rolled HTTP path.
3. **OAuth 2.1** — OAuth transport variant + `octos mcp login <server>` command +
   keyring token storage + refresh. Config: add `oauth: bool` / `scopes` to
   `McpServerConfig`. Validate against self-hosted `workspace-mcp
   --transport streamable-http` (no aggregator service).
4. **Tests** — metadata discovery, PKCE, token refresh, loopback redirect; port
   codex's `compute_expires_at_millis`/`token_needs_refresh`/`parse_oauth_callback`
   helpers.

## Security review resolution (codex, 5 rounds)

All rounds' findings are fixed. The OAuth-endpoint SSRF (the one deferred item)
is now closed: `SsrfOAuthHttpClient` (an `rmcp::transport::auth::OAuthHttpClient`)
SSRF-validates EVERY OAuth request URL — including literal-IP discovery /
registration / token endpoints the DNS resolver skips — before executing it
through an SSRF-filtered client, wired via `OAuthState::new_with_oauth_http_client`
in both connect + login. Combined with `reject_private_url_host` (config URL),
`SsrfDnsResolver` (hostname hosts), redirect-none, and the https-required rule,
the OAuth path is SSRF-guarded end to end.

### Resolved-by-behavior (verified, not code changes)

- **stdio child cleanup on shutdown.** Children are spawned with
  `kill_on_drop(true)`, which is preserved through rmcp's
  `TokioChildProcessBuilder::spawn`. rmcp's `ChildWithCleanup::drop` spawns an
  async `kill()` reaper (clean reap while the runtime is alive); if the runtime
  is already gone, dropping that closure drops the tokio `Child`, and
  `kill_on_drop` fires a synchronous SIGKILL — so **the child is never left
  running** (worst case: a brief zombie the OS reaps when octos exits).
  `RunningService::drop` also cancels the service via a drop-guard. An explicit
  `await`-cancel on graceful shutdown (via `RunningService::cancellation_token()`)
  is an optional clean-reap nicety; it would require threading a shutdown handle
  through all 8 call sites (chat/acp/serve/gateway/profile) and is not needed for
  safety.

### Known limitations

- **Refresh-token rotation across restarts.** octos persists tokens after the
  connect-time refresh, but rmcp's `AuthClient` may auto-refresh *mid-session*
  and that new token stays only in memory. A provider that rotates the refresh
  token on every use will leave the keyring with a stale refresh token, so a
  later octos run must `octos mcp login` again. Most providers don't rotate on
  every refresh; a full fix is codex's `OAuthPersistor` (persist after each op).
- **Unbounded stdio frame read.** rmcp's child-process transport uses
  `read_until` with no `MAX_LINE_BYTES` cap — accepted for operator-configured
  local stdio servers; a bounded codec would need a custom transport.
- **OAuth backend redirects are refused.** The OAuth HTTP client uses
  `redirect: none` (a followed hop would bypass the per-request SSRF/TLS check);
  a server whose `.well-known`/token endpoint 3xx-redirects would fail. None of
  the common providers require this.

## Non-goals / notes

- No third-party aggregator services (per direction). rmcp is a crate, self-hosted.
- Google upstream (Drive) still requires a Google OAuth client at the **server**
  (workspace-mcp); that's the server's concern. This ADR delivers the octos
  **client** OAuth capability, which is provider-agnostic.
