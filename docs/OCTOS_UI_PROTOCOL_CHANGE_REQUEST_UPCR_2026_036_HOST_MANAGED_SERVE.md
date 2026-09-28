# Octos UI Protocol Change Request: Host-Managed Serve

## Header

- Request id: `UPCR-2026-036`
- Date: 2026-09-28
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: transport authentication (a WebSocket bearer subprotocol) on every
  server; for `octos serve --host-managed` only, the external-client identity,
  its refusal to answer host-owned app peers, turn ownership, and the
  unavailability of `server/shutdown`. No method, event or field is added or
  removed. Revised after the post-merge review: turn ownership is the
  connection's (not the turn id's), a live turn id is unique across sessions,
  and `turn_in_progress` omits `data.turn_id` for external clients.
- Origin: OctoSense shells share one kernel between native apps and an
  external web or terminal client (see `docs/HOST_MANAGED_SERVE.md`).

## Problem

An app shell owns one `octos serve` for its native clients and wants the
person to attach a web client or terminal UI to the same runtime. The existing
serve modes do not fit:

1. On a phone every installed app reaches loopback, so solo login and the
   trusted-loopback `X-Profile-Id` path would hand the runtime to any app.
2. One admin token would give an external client everything the host has,
   including the admin API and the ability to answer a host-owned app peer's
   approvals, which UPCR-2026-034 reserves for the person in the app.
3. A browser cannot set `Authorization` on a WebSocket, so the token travels
   in the URL.
4. `server/shutdown` lets any client stop a process the host owns.

## Contract

### WebSocket bearer subprotocol (every server)

A client may authenticate the `/api/ui-protocol/ws` upgrade with

```text
Sec-WebSocket-Protocol: octos-ui, octos.bearer.<token>
```

The token is read from the first `octos.bearer.` entry. It is checked after
`Authorization: Bearer` and before `?token=`/`?_token=`, which remain
supported. When the client offers `octos-ui`, the server selects it in the
handshake response. The bearer entry is never echoed. A client that offers
the bearer entry must also offer `octos-ui`, because browsers fail a handshake
in which the server selects no offered protocol. Clients that offer no
subprotocol are unaffected.

### Host-managed identities

On a host-managed server exactly two tokens authenticate, both delivered on
the server's stdin: the host token (admin) and, when the host configured one,
the external token, which resolves to the user identity `_main` (role user).
The external identity may upgrade `/api/ui-protocol/ws` and use no other
route: 403 on REST routes, 401 on `/api/admin/*`.

### External clients call an allowlist

On the socket, a connection that is not authenticated with the host token
(including a session-ingress connection) may call only
`config/capabilities/list`, `session/status/read`, `system/status.get`,
`session/open`, `session/hydrate`, `session/messages_page`,
`session/status.get`, `turn/start`, `turn/interrupt`, `turn/steer`,
`turn/state/get`, `approval/respond`, `approval/scopes/list`,
`user_question/respond` and `diff/preview/get`. Every other method, typed or
raw, fails with `permission_denied`, `data.kind: "external_method_denied"`.

Moreover:

- a call whose parameters name a host-owned app-peer session (`peer-…` or
  `peerctx-…` topic) under any key containing `session`, at any depth, fails
  with
  `host_owned_peer_session_denied`; for `approval/respond` and
  `user_question/respond` the kind is `host_owned_peer_answer_denied`:

  ```json
  {"code": -32120, "message": "an external client cannot answer a host-owned app peer's approval; answer it in the app",
   "data": {"kind": "host_owned_peer_answer_denied"}}
  ```

  Nothing is decided and the prompt stays pending for the host;
- `approval/respond` and `user_question/respond` are accepted only for a
  session the same connection opened successfully
  (`external_session_not_opened`);
- a `sandbox` override, a `cwd`, any key containing `topic`, or turn `media`
  that are not upload handles fail with `external_parameter_denied`;
  `session.workspace_cwd.v1` is never negotiated for such a connection;
- `turn/interrupt`, `turn/steer`, `approval/respond` and
  `user_question/respond` are accepted only for turns this connection started
  (`external_turn_denied`). Turn ids are client-chosen, so ownership is never
  inferred from one: the server records the starting connection on the
  running turn and on each approval and question the turn raises, and checks
  that. An external approval never records an approval scope;
- a `turn/start` refused because the session already runs a turn carries
  `data: {"kind": "turn_in_progress"}` without the running turn's
  `turn_id` (the host's connection still receives `turn_id`);
- an external connection drains no background continuations;
- a call naming a profile other than `_main` (a `profile_id` at any depth,
  or another profile's session key) fails with `external_profile_denied`;
- a turn started by such a connection gets a fixed allowlist of built-in
  workspace, web, question and memory tools, applied to the finished
  per-turn registry (default-deny: no command or code execution, git,
  delegation, administration, peers, `send_file`, task, MCP or plugin tools),
  so the model cannot drive the host-owned peers or read the host's
  processes through it. The filter checks each tool's recorded origin as
  well as its name: only compiled-in tools survive, so a plugin or MCP tool
  under an allowlisted name (say `memory_search`) is dropped.

### Unique live turn ids (every connection)

On a host-managed server, a `turn/start` or `review/start` whose `turn_id`
names a turn still running in another session fails with
`invalid_request`, `data: {"kind": "turn_id_in_use"}`; the client retries
with a fresh id. A finished turn frees its id. Other servers are unchanged.

This extends UPCR-2026-034's "Approvals belong to the person" from the owning
system agent to external clients, and keeps the apps' memory and workspaces
out of their reach.

### `server/shutdown`

A host-managed server never advertises `server/shutdown` in
`config/capabilities/list`, and answers `server_shutdown_unavailable` to every
caller. It already required solo login (UPCR-2026-032), which host-managed
never enables. The host stops the server by closing its stdin.

## Risk

- The external token drives the person's own sessions of profile `_main`
  (for example the shared system conversation), which is what attaching a
  client to the person's assistant means. It cannot configure the profile,
  install skills, restore snapshots, reach app peers, or run code through a
  turn. Before this allowlist, `profile/llm/upsert` would have let it point a
  provider's `base_url` at itself and receive the stored key. The combination
  of the #2556 residuals (a self-reported originator on `peer/prepare` and a
  namespace nesting with a legitimate app's) is also closed here. #2556
  remains the general fix for other deployments.
- A peer topic prefix is the refusal criterion, so ordinary agent-staged
  peers' sessions are also host-only for external clients. Their originator
  answers them through `peer_respond`, which is unchanged.

## Tests

- `should_accept_a_bearer_subprotocol_without_echoing_it`
- `should_resolve_only_the_two_configured_tokens`
- `should_confine_the_external_token_to_the_ui_protocol_socket` (includes
  `supports_server_shutdown` staying false)
- `should_reach_admin_routes_only_with_the_host_token`
- `should_refuse_an_external_answer_to_a_host_owned_peer_approval`
- `should_refuse_an_external_answer_to_a_host_owned_peer_question`
- `should_let_the_host_answer_a_host_owned_peer_approval`
- `should_let_an_external_client_answer_its_own_session_approval`
- `should_admit_only_configured_origins_on_a_host_managed_ws_upgrade`
- `stdio_default_feature_list_matches_the_stdio_defaults`
- `should_allow_external_clients_only_the_allowlisted_methods` (walks the
  dispatch table)
- `should_refuse_external_calls_on_host_owned_peer_sessions`
- `should_gate_external_clients_over_the_real_socket`
- `should_give_external_turns_no_code_admin_or_peer_tools` (includes MCP and
  plugin names)
- `should_confine_external_calls_to_the_main_profile_at_any_depth`
- `should_keep_pairing_off_when_the_enable_cannot_be_audited`
- `should_catch_peer_topics_sandbox_overrides_and_local_media`
- `should_refuse_an_external_session_open_in_a_foreign_workspace`
- `should_let_an_external_client_answer_only_its_own_turns_approvals_once`
  (the same turn id on the host's approval: still refused)
- `should_let_an_external_client_answer_only_its_own_turns_questions`
- `should_let_an_external_client_steer_and_interrupt_only_turns_it_owns`
- `should_refuse_a_turn_id_live_in_another_session_on_a_host_managed_server`
  (also the `turn_in_progress` payload with and without `turn_id`)
- `should_drop_plugin_and_mcp_tools_with_allowlisted_names_from_an_external_turn`
- `tests/serve_host_managed.rs`
  `serve_host_managed_refuses_an_external_turn_reusing_a_host_turn_id`: over
  the real socket, an external client that reuses the running host turn's id
  in another session is refused (`turn_id_in_use`), and cannot steer or
  interrupt the host's turn
- octos-agent: `should_record_where_each_tool_came_from`,
  `should_keep_only_builtin_tools_whatever_their_names`,
  `should_refuse_a_plugin_tool_named_like_a_builtin`,
  `protected_names_cover_every_reserved_builtin`,
  `should_deny_process_secrets_behind_globs_and_relative_paths`
- `tests/serve_host_managed.rs`
  `serve_host_managed_gives_an_external_turn_only_the_allowlisted_tools`:
  a real WS turn on `#system` with a scripted model; the model receives
  exactly the allowlisted tools, while the host's own turn keeps its full
  set
- octos-agent: `should_refuse_process_environments_in_every_scope`,
  `should_refuse_a_workspace_symlink_into_a_process_view`
- `a_rate_limited_code_refills_its_budget_instead_of_burning`
- `should_audit_the_pairing_ceremony_without_the_code`
- `tests/serve_host_managed.rs`: tokens on stdin (never in
  `/proc/<pid>/environ`), refused in the environment, stdin EOF, host SIGKILL,
  inherited listener (serial CI step)
