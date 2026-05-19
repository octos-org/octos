# UPCR-2026-016: AppUI Stdio Transport

Status: accepted

## Summary

Add a local process transport for AppUI JSON-RPC over stdio. This is a transport
addition only: method names, params, results, notifications, errors, and
capability semantics stay identical to the WebSocket transport.

## Contract

- `octos serve --stdio` reads newline-delimited JSON-RPC 2.0 requests from
  stdin.
- It writes newline-delimited JSON-RPC 2.0 responses and notifications to
  stdout.
- stdout is reserved for protocol frames. Logs and diagnostics must go to
  stderr.
- Each line contains one complete UTF-8 JSON object.
- Implementations may enforce the same maximum text-frame size used by the
  WebSocket transport and reject oversized lines with `frame_too_large`.
  Servers must enforce the bound while reading the line, before accepting an
  unbounded allocation.
- A failed stdout write or closed pipe terminates the stdio AppUI connection
  and stops dispatching new requests for that connection.
- Stdio clients may send `client_hello` as their first request. Its
  `supported_features` field is equivalent to the WebSocket
  `X-Octos-Ui-Features` / `ui_feature` negotiation inputs, and the server
  replies with `server_hello` plus negotiated capabilities.
- Request IDs are echoed exactly in responses.
- Notifications have no `id`, matching the existing AppUI notification
  envelope.
- Method names, capability semantics, result shapes, and error shapes are
  shared with WebSocket. If stdio advertises a method in `supported_methods`,
  it must route to the same backend handler as WebSocket.
- A stdio session is local-process trusted. It does not use HTTP headers,
  WebSocket Origin checks, or bearer-token headers.
- Because stdio has no `X-Profile-Id` header, profile-scoped methods resolve
  identity in this order: explicit `params.profile_id`, profile encoded in
  `params.session_id`, profile bound by the most recent successful `session/open`,
  then the server default profile. Clients should pass `profile_id` explicitly
  before `session/open`.

## Compatibility

Existing WebSocket clients are unchanged. Stdio clients consume the same AppUI
wire contract without opening a network listener.

## Tests

- CLI parse/build check for `octos serve --stdio`.
- Stdio request/response smoke for `config/capabilities/list`.
- Stdio tmux soak with `octos-tui --stdio-command "octos serve --stdio ..."`
  covering onboarding provider save, `session/open`, `turn/start`, model output,
  and terminal capture artifacts.
