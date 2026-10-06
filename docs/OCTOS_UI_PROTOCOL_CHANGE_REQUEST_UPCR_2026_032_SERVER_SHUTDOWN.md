# Octos UI Protocol Change Request: Server Shutdown

## Header

- Request id: `UPCR-2026-032`
- Date: 2026-09-18
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: one additive raw AppUI method, `server/shutdown`

## Problem

A web client connected to a local `octos serve --solo` has no way to stop it;
the only way is Ctrl+C in the terminal that launched it.

## Contract

`server/shutdown` (params `{}`) returns `{ "stopping": true }` and stops the
serving process through the same `watch` switch SIGINT/SIGTERM use: drain,
stop gateways, exit. It is idempotent.

- **Advertised** in `config/capabilities/list` `supported_methods`, and
  **callable**, only when `local_solo_danger_allowed` holds (solo login opted
  in and a local deployment) **and** the process is an HTTP serve holding a
  stop switch (`AppState.serve_shutdown`; absent for `--stdio`).
- Otherwise it returns `invalid_request` with `data.kind =
  "server_shutdown_unavailable"` and stops nothing.
- Session-scoped (session-ingress) connections can never call it: it is on the
  raw-surface deny list, enforced at dispatch before the raw handler.
- The switch flips 250 ms after the request is handled; the WS loop writes the
  reply after the handler returns, so under outbound backpressure a client may
  not read the ack before the drain closes the socket. The stop still happens.

## Risk

- **One call stops the whole server.** On a tokenless loopback solo serve, any
  local process — or any page served from an origin on the dev allowlist
  (`OCTOS_APPUI_ALLOWED_ORIGINS`) — can stop it with a single RPC. This is the
  existing local-solo trust model (#2388), not a new exposure: the same
  principals can already drive turns and edit profiles. A DNS-rebinding page is
  refused at the WS Origin gate (403). Tightening the local trust model is
  tracked in #2319 / #2147.
- No non-WS path reaches the stop switch.

## Tests

- `should_advertise_server_shutdown_only_on_a_local_solo_http_serve`
- `should_flip_the_serve_stop_switch_when_server_shutdown_is_called`
- `should_refuse_server_shutdown_and_stop_nothing_when_unavailable`
- `should_bar_session_scoped_connections_from_server_shutdown`
- `raw_method_is_dispatched_covers_full_raw_surface` (now pins the method)
- `spec_section6_catalog_lists_every_advertised_method`
