# Host-managed serve

`octos serve --host-managed` runs the HTTP/WebSocket server as a child of an
embedding host, such as an app shell on a phone or desktop. The host starts
the process, owns its lifecycle, and decides whether an external client (a web
client or a terminal UI the person runs) may attach to the same agent runtime.
It is the serve counterpart of [`octos acp --host-managed`](HOST_MANAGED_ACP.md):
opt-in, fail-closed, and without effect on ordinary `octos serve`.

The protocol-visible parts are specified in
[UPCR-2026-036](OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_036_HOST_MANAGED_SERVE.md).

## Threat model

Loopback is not an authentication boundary. On Android every installed app
can connect to `127.0.0.1`; on a shared computer so can every other local
user; and a web page can reach loopback through the person's browser (DNS
rebinding, cross-site WebSocket). The server therefore requires a bearer token
on every route except the unauthenticated public ones (`/health`,
`/api/version`, `/pair/*`), and it trusts nothing about a connection's origin
by itself.

## Credentials

| Credential | Source | Identity | May use |
| --- | --- | --- | --- |
| Host token | stdin, first line | admin | every route, as an admin token does today |
| External token | stdin, second line (empty: none) | user `_main`, role user | `GET /api/ui-protocol/ws`, and there only the allowlist below |

- Only these two tokens authenticate. There is no solo login (not even with
  `OCTOS_SOLO_LOGIN`), no trusted-proxy `X-Profile-Id`, no hashed admin-token
  store, no `OCTOS_TEST_TOKEN` and no OTP session.
- The host writes both tokens as the first two lines of the server's stdin,
  then keeps stdin open (the lifeline, below). Tokens never go in the
  environment: any process of the same user can read `/proc/<pid>/environ`,
  and on Android that includes this app's own tools. `OCTOS_AUTH_TOKEN` and
  `OCTOS_HOST_EXTERNAL_TOKEN` in the environment, `--auth-token` and the
  config file's `auth_token` are all refused. Tokens must be at least 32
  characters of RFC 7230 `tchar`, and the two must differ.
- Neither token is printed, logged or returned by a route. The only exception
  is a successful pairing claim, which returns the external token (see
  below).
- With an empty second line external clients are disabled. To revoke or
  rotate external access, the host restarts the server with a new external
  token; open external connections end with the process.

An external identity:

- gets 403 on every REST route and 401 on every `/api/admin/*` route
  (`stop-all`, `token/rotate` and the rest included);
- may call only these methods on the socket; everything else, typed or raw,
  fails with `data.kind: "external_method_denied"`:
  `config/capabilities/list`, `session/status/read`, `system/status.get`,
  `session/open`, `session/hydrate`, `session/messages_page`,
  `session/status.get`, `turn/start`, `turn/interrupt`, `turn/steer`,
  `turn/state/get`, `approval/respond`, `approval/scopes/list`,
  `user_question/respond` and `diff/preview/get`. So there is no profile, LLM
  or sub-provider configuration (a client could otherwise point a provider's
  `base_url` elsewhere and receive its stored key), no skill install or
  action, no snapshot restore, no session fork or delete, no peer method and
  no `server/shutdown`;
- cannot name a host-owned app-peer session (`peer-…` or `peerctx-…`
  topic, in any case) under any key containing `session` or `topic`, at any
  depth: `host_owned_peer_session_denied`, or
  `host_owned_peer_answer_denied` for an answer. Those sessions carry the
  apps' memory and workspaces, and their prompts are answered by the person
  in the app (UPCR-2026-034);
- answers approvals and questions only on sessions it opened successfully on
  the same connection (`external_session_not_opened` otherwise);
- may not set a sandbox override (`sandbox`: it could widen read access or
  turn the sandbox off), a workspace (`cwd`), or a separate `topic` (the
  handlers fold a topic into the session key, so it could name an app peer's
  session), and may attach turn media only as upload handles (`up/…`):
  `external_parameter_denied`. `session.workspace_cwd.v1` is never negotiated
  for it, so its sessions stay in the workspace octos bound them to (for a
  new session, `<data dir>/users/<session>/workspace`);
- steers, interrupts, and answers approvals and questions only for turns its
  own connection started: `external_turn_denied` for any other turn, such as
  the host's on a shared conversation. Ownership is the connection's, never
  the turn id's (turn ids are client-chosen): the server records the starting
  connection on the running turn and on every approval and question that turn
  raises, and checks it on `turn/steer`, `turn/interrupt`, `approval/respond`
  and `user_question/respond`. Its approval answers are once-only; it never
  records an approval scope;
- keeps its own turns' approvals and questions to itself: an approval or
  `ask_user_question` question an external turn raises goes only to that
  connection (live, replay, `session/open` pending lists,
  `session/hydrate`), never to the host or another client, and an
  `approval/respond` or `user_question/respond` for it from any other
  connection, the host included, fails with `external_approval_owner_only`
  or `external_question_owner_only`. A remembered approve scope never
  answers it. So the host's automation (developer mode, standing rules)
  never decides for an external client (OctoSense ADR 0004, gap G1);
- learns no other turn's id from a refusal: a `turn/start` on a session that
  is already running a turn fails with `data.kind: "turn_in_progress"` and no
  `data.turn_id` (the host's connection still gets it);
- never runs background continuations (the system agent's wakes, loops and
  goals); the host's connection or the global drain runs them with the
  full tool set;
- names no profile but `_main`: any `profile_id` (at any depth) or session
  key of another profile is refused (`external_profile_denied`);
- starts turns with a fixed set of compiled-in tools only (default-deny):
  `read_file`, `write_file`, `edit_file`, `diff_edit`, `apply_patch`, `glob`,
  `grep`, `list_dir`, `code_structure`, `check_workspace_contract`, `web_search`, `web_fetch`,
  `ask_user_question`, `recall`, `recall_memory`, `memory_search`,
  `memory_load`, `view_image`, `view_video` and `tool_search`. The filter is
  applied to the finished per-turn registry, after every tool the turn
  builder adds (`spawn`, `peer_*`, `send_file`, task tools, MCP, plugins),
  and it checks each tool's origin, not only its name: the registry records
  whether a tool is compiled in, a plugin's or an MCP server's, and only a
  compiled-in tool survives. A plugin or MCP tool named `memory_search` (or
  any other allowlisted name) is dropped. Plugins may not register a
  compiled-in tool's name at all, and MCP tools may not shadow one. No
  command or code execution (the `workspace_*` git tools included),
  delegation, administration, peers, MCP server or plugin tool, whatever its
  name. The memory tools read the system agent's own memory, not the apps'.
  The model cannot reach the apps' assistants or the host's processes
  through such a turn;

On a host-managed server a live turn id is also unique across sessions, for
every connection: a `turn/start` or `review/start` reusing the id of a turn
still running in another session fails with `data.kind: "turn_id_in_use"`
(retry with a fresh id).

For every session, no file tool opens a process's private view, however it
is spelled: `/proc/self`, `/proc/thread-self` or `/proc/<pid>` and anything
under them (environment, command line, `fd/`, `root/`, …), or `/dev/fd` and
`/dev/std*`. The raw path, its normalization and its canonical target are all
judged, so `..` and workspace symlinks cannot reach them. The shell policy
also refuses commands that name a process's `/proc/<pid>` view, reading their
words as paths (quotes dropped, `..` folded, globs such as `/pro[c]/1/env*`
and `cd /proc && cat 1/environ` followed). That check is best-effort defense
in depth: a shell can always build a path it cannot see. The control for
external clients is that their turns have no shell or code execution at all.
`web_fetch` and the search tools go through the shared SSRF check
(`octos_research::net`): loopback (including this server's own port),
private, link-local and metadata addresses are refused, each redirect hop is
re-checked, and DNS answers are pinned.

## Network guards

- **Bind.** `--host 127.0.0.1` only, and a Local deployment.
- **Host header.** Every request must name the listener:
  `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`; anything else is
  answered 421 before routing. This blocks DNS rebinding. A tunnel into the
  device must preserve the port number (for example `adb forward tcp:P tcp:P`).
- **Origin.** CORS and the WebSocket upgrade trust only the configured origins
  (`appui.allowed_origins` or `OCTOS_APPUI_ALLOWED_ORIGINS`), without the
  built-in development, ominix or per-tenant origins, and without the
  listener's own loopback origins. A WebSocket upgrade that carries the
  browser-only `Sec-Fetch-*` headers but no `Origin` is refused. A client that
  is not a browser sends neither and proceeds to the token check.
  Origin only protects a browser that holds a token from other web pages.
  It does not authenticate: any local process can send any `Origin`. The
  token is the control.

## Sending the token

In order of preference:

1. `Authorization: Bearer <token>` (native clients).
2. `Sec-WebSocket-Protocol: octos-ui, octos.bearer.<token>` (browsers, which
   cannot set `Authorization` on a WebSocket). The server selects `octos-ui`,
   so the bearer entry is never echoed. Offer both entries: a browser fails a
   handshake in which the server selects none of the offered protocols.
3. `?token=<token>`, kept for compatibility. It is never logged (request
   spans record route templates), but URLs can end up in browser history, so
   prefer 2.

The subprotocol path works on every `octos serve`, not only host-managed.

## Pairing

A host-managed server mints no pairing code at startup and prints none.
While the host shows its pairing UI, it calls:

- `POST /api/admin/host/pairing` (host token): mints an 8-character code
  (Crockford base32) valid for five minutes and for one successful claim, and
  returns `{code, server_origin, expires_in_secs}`. A new call replaces the
  previous code. With external access disabled it answers 409
  `external_access_disabled`.
- `DELETE /api/admin/host/pairing` (host token): turns pairing off.

`/pair/info` and `/pair/claim` keep their contract (loopback only, 404 when
pairing is off). A claim returns the external token, never the host token.
Both host calls and every claim are recorded in the admin audit log
(`host.pairing.enable`, `host.pairing.disable`, `host.pairing.claim` with its
outcome), never with the code; if the enable cannot be audited, pairing stays
off and the call fails. Failed claims are rate-limited (10 per minute) rather
than burning the code, so another local process cannot lock the person out by
guessing. A claim that carries an `Origin` must come from a configured origin.

## Lifecycle

- **Stdin.** The host keeps the child's stdin open as a lifeline. Bytes are
  ignored; EOF (the host closed it, exited or crashed) stops the server
  through the normal drain path, the same one SIGTERM takes.
- **Orphans, per platform.** Stdin EOF covers every way the host can end on
  every platform: exit, crash, SIGKILL (macOS, Linux, Android) or
  TerminateProcess (Windows) all close the host's end of the pipe, because the
  OS closes a dead process's descriptors. `tests/serve_host_managed.rs` kills
  the pipe's holder with SIGKILL. EOF cannot see a host whose pipe end
  outlives it, because another process inherited it. On Linux and Android the
  server therefore also asks for SIGTERM when its parent dies
  (`PR_SET_PDEATHSIG`), and refuses to start if the parent is already gone.
  The signal follows the parent thread that spawned the child, so a host
  spawns it from a long-lived thread. On macOS and Windows the host must not
  leak the write end: spawn children close-on-exec, as Rust's
  `std::process` does.
- **Profiles run in process.** As with `--solo`, profiles run inside the
  server; no gateway children are started.
- **Inherited listener (Unix).** With `--listen-fd <FD>` the server serves the
  listening TCP socket the host passed as descriptor `FD` (stream, bound to
  `127.0.0.1`, not stdio) instead of binding `--port`. The host keeps its copy
  across restarts, so no other process can take the port while no server
  runs, and clients that connect in between wait in the backlog. The server
  makes its copy close-on-exec. Without it, a host that restarts the server
  on a fixed port can lose that port to another local process; the host
  should then bind a new port and rotate the external token.

```sh
printf '%s\n%s\n' "$HOST_TOKEN" "$EXTERNAL_TOKEN" | NO_COLOR=1 \
  octos serve --host-managed --host 127.0.0.1 --port 0 --data-dir <dir>
# (a real host keeps the pipe open: its end of stdin is the lifeline)
```

The server prints `Listening: http://127.0.0.1:<port>` when it accepts
connections.
