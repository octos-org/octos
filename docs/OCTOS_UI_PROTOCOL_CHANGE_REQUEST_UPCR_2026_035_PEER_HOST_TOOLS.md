# Octos UI Protocol Change Request: Host-Registered Tools per App Peer

## Header

- Request id: `UPCR-2026-035`
- Date: 2026-09-27
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: three additive raw AppUI methods, `peer/tools/register`,
  `peer/tool/result` and `peer/input/reject` (#2618); three additive server
  notifications, `peer/tool/call`,
  `peer/tool/cancel` and `peer/input`; per-turn enforcement of a host-owned
  app peer's app tools and tool risk levels; the system agent's input to a
  host-owned peer delivered to its host
- Builds on: UPCR-2026-034 (host-owned app peers, host token, request
  contexts)
- Origin: OctoSense ADR 0002 "Event-driven app agents", sections 4 ("Apps
  expose their own tools") and 12 ("What an autonomous app agent may do");
  OctoSense issue #61 (News M2)

## Problem

A host-owned app peer (UPCR-2026-034) runs with the profile's ordinary tool
roster: shell, files, web, spawn, and so on. ADR 0002 wants each app's agent
to work through **its app's own tools** — typed operations implemented where
the capability lives (the app's host service), with a risk level each — plus
the few generic tools the app names, and nothing else. The kernel had no way
to learn an app's tools, to route a call back to the host, or to hold an
outward or destructive call for the person.

## Contract

Discovery: a server that lists `peer/tools/register` in
`config/capabilities/list` `supported_methods` implements this whole UPCR;
`peer/tool/call`, `peer/tool/cancel` and `peer/input` are in
`supported_notifications`.
`peer/input/reject` is in `supported_methods` wherever `peer/input` is sent.
All three methods are raw-surface methods (a session-ingress connection cannot
call them), profile scoped, and authorized like UPCR-2026-034 control calls:
the caller names the peer's originator `session_id` and presents the peer's
`host_token`.

### Host obligation (normative)

**Hosts MUST call `peer/tools/register` (possibly with an empty set) on the
connection that drives the peer's turns, after every successful
`peer/prepare` and again after every reconnect, before any `turn/start`.
Unregistered host-owned peers get no app memory or app context, and the
system agent's input cannot reach them. Hosts MUST handle `peer/input` by
starting the peer's turn on that connection.**

Registration only ADDS: a registered peer's own (host-driven) turns keep
the kernel tools the peer has without a registration (under the existing
peer restrictions), plus the host's app tools, plus the usual question and
approval flow. An empty set takes nothing away. A host that sets
`generic_tools` chooses the peer's kernel tools EXACTLY (see below).

### Migration for hosts

A host that uses only UPCR-2026-034 today (`peer/prepare`,
`peer/context/open`, `turn/start`) keeps its peers' tools, but their turns
get no app memory or app context, and the system agent's `peer_send_input`
to them fails, until it registers and handles `peer/input`. OctoSense's
`crates/app-peers` broker currently calls only `peer/prepare`,
`peer/context/open` and `turn/start`; it will register (the app's pinned
`tools.json`, or an empty set) in OctoSense's M3 pin-bump PR. Other hosts,
such as octoscode-web, must do the same: register on the connection that
drives the turns, after every `peer/prepare` and every reconnect.

### `peer/tools/register`

```
{session_id, peer?, host_token, profile_id?,
 tools?: [ToolDecl], generic_tools?: [string], if_version?: u64,
 call_timeout_ms?, approval_ttl_secs?, max_result_bytes?}
→ {slug, profile_id, version, previous_version,
   tools: [{name, model_name, risk, background, outward, confirm}],
   generic_tools, call_timeout_ms, approval_ttl_secs, max_result_bytes,
   applies: "next_turn"}
```

**Host session target.** Without `peer`, the set is registered on the host
session `session_id` itself: a session that is not an app peer, typically
the system agent's conversation, so the system agent can call the app tools
the host granted it. The credential is the host token of an app peer that
`session_id` prepared (only the host that prepared that session's app peers
holds one); a `peer-`/`peerctx-` session is refused (`peer_tools_invalid`),
as is an external connection of `serve --host-managed`. The result carries
`session_id` instead of `slug`. Such a set:

- lives in memory as long as the registering connection (the host registers
  again after every reconnect, as for a peer; `if_version` works the same);
- is ADDED to the turns the registering connection drives on that session,
  and only to those: every other turn on the session (an external web client,
  a kernel continuation) gets no app tool, and the session's other clients
  are not locked out of it;
- when `generic_tools` is given, is a host-only kernel tool list for that
  session: it narrows EVERY turn on the session while the set is registered
  (the host's, other clients', kernel wake-ups; never widening the profile
  policy), and no other session of the profile. A list that survives the
  host connection (durable, set on `session/open`) is a follow-up (#2605);
- routes its calls to that connection with `caller.kind: "system"` and
  `peer: null`; they are answered with `peer/tool/result` without `peer`
  (same credential); its approvals are host-routed like a peer's; its audit
  goes to `host_session_tool_audit.jsonl` in the profile data dir (the same
  row shape, `peer: null`).

`ToolDecl` is one entry of the app bundle's `tools.json`: `{name,
description, input_schema, output_schema?, risk, background?, outward?,
confirm?, shareable?}`. Unknown fields are refused, so a misspelled flag
never silently weakens a tool:

| Field | Meaning |
| --- | --- |
| `name` | `<app>.<tool>`: 2–4 `.`-separated segments of `[a-z][a-z0-9_]{0,31}`. The model sees it with `.` replaced by `_` (`news.list` → `news_list`; providers refuse `.` in tool names). A registration whose model names collide with each other or with any peer-safe generic tool (allowed or not, e.g. `read.file` → `read_file`) is refused. |
| `input_schema` | JSON Schema object (`"type": "object"`). ≤ 16 KiB. Checked structurally against the meta-schema's shapes: `type` names JSON Schema types, `properties` is an object of schemas, `required` is an array of strings, `items`, `additionalProperties`, `enum` and `anyOf`/`oneOf`/`allOf` are well formed (boolean subschemas allowed), nesting ≤ 10. |
| `output_schema` | Optional, same rules. Stored and echoed; not enforced by the kernel. |
| `risk` | `read`, `act` or `destructive`. |
| `background` | May run in a turn with no interactive client. Default `false`. |
| `outward` | Reaches past the app (send, post, share, buy). Gated like `destructive`. |
| `confirm` | `host` (default) or `app`: who confirms a gated call with the person. Independent of `risk`. |
| `app` | Optional. The app that OWNS the tool, `[a-z][a-z0-9_.-]{0,63}`; defaults to the name's first segment. A cross-app tool (another app's tool the host granted to this peer) names its owner here. Echoed in the result, in every `peer/tool/call`, in the approval details and in the audit. |
| `shareable` | App Hub metadata; accepted, not acted on by the kernel (the host decides cross-app grants). |

`generic_tools` is optional. Omitted (or `null`), the peer's host-driven
turns keep the kernel tools the peer has without a registration. Given (an
array, even empty), it is the peer's kernel tool set EXACTLY: the host
allows what the app's manifest declares and the person granted at install,
narrowing or widening the usual set as it sees fit. The kernel bakes in no
exclusions (research, command execution and anything else the peer's
session has may be listed), but a list never adds a tool the peer's session
does not have (for example the `peer_*` tools, which a peer session never
has), and names are only checked to be kernel tool names (no `.`; app tools
are dotted). A set stored before this field became optional (a plain list)
reads as that exact list.

Limits: 64 app tools, 256 generic tools, 2 KiB per description. Options are
clamped, and a host may raise the defaults up to the maximum:
`call_timeout_ms` 1–300 000 (default 30 000), `approval_ttl_secs` 1–604 800
(default 3 600), `max_result_bytes` 1–1 048 576 (default 262 144).

The set **replaces** the previous one whole; `version` increments on every
registration (the first is 1). With `if_version`, a registration whose
expected version is not the current one is refused
(`peer_tools_version_conflict`, `data.current_version`). Registrations of one
peer are serialized; different peers never wait on each other. The set is
durable (`peers/<slug>/host_tools.json`) and applies from the next turn
start. The registering connection becomes the peer's **tool host**: every
later `peer/tool/call` goes to it (a later registration from another
connection moves the route; a route whose connection is found closed is
dropped).

Typed `data.kind`: `peer_host_token_mismatch`, `peer_originator_mismatch`,
`peer_not_found`, `peer_not_host_bound`, `peer_closed`, `peer_tools_invalid`,
`peer_tools_version_conflict`.

### `peer/tool/call` (server → host notification)

```
{peer, session_id, context_id, turn_id, call_id, tool_call_id, args_digest,
 name, app, caller: {kind, peer, session_id, context_id, turn_id}, args,
 risk, confirm_required, timeout_ms, tools_version}
```

`app` is the tool's owning app and `caller` the calling side: `kind` is
`"app_peer"` (a system agent that runs as a host-owned app peer is its peer
too), `peer` is the app peer whose session makes the call (the peer the set
is registered on), `session_id` / `context_id` / `turn_id` the calling
session and turn. The host authorizes, audits and routes every call to the
owning app's executor from these, and the owning app's confirmation sheet
shows the caller. With a cross-app tool the
two differ, and the host authorizes the call against its grants from them.
The top-level `peer`, `session_id` and `context_id` repeat the caller.
`session_id` / `context_id` identify the calling session (the peer's own, or
one of its request contexts): the caller's identity for the host service.
`name` is the declared name (`news.list`). `confirm_required` is true when
the app must confirm the call with the person itself (`confirm: app`, person
present); it is false when the kernel already holds the person's approval,
so the person is never asked twice. `timeout_ms` is how long the kernel will
wait. The notification is ephemeral: if the host connection is gone the call
fails with `host_unavailable`; it is never replayed.

**Host obligations (MUST).** A host:

- MUST execute a call at most once per `(session_id, turn_id, tool_call_id,
  args_digest)`, answering a repeat with the first result;
- MUST NOT execute a call after it received `peer/tool/cancel` for its
  `call_id`, nor after `timeout_ms` has passed without an
  `awaiting_confirmation` acknowledgement;
- MUST send `status: "awaiting_confirmation"` before showing its own
  confirmation sheet for a gated call whose `confirm_required` is false
  (for `confirm_required: true` the kernel already waits the approval TTL).

### `peer/tool/result`

```
{session_id, peer?, host_token, profile_id?, call_id,
 ok?, data?, error?: {kind?, message} | string,
 status?: "awaiting_confirmation"}
→ {call_id, accepted: true, result_too_large?: {bytes, max},
   awaiting_confirmation?: true}
```

`status: "awaiting_confirmation"` is not a result: for a gated call it
extends the kernel's wait to the approval TTL (counted from the call), and
the host answers again later. `data` above `max_result_bytes` (serialized) is
not given to the model: the call ends with `result_too_large`. An error
`kind` must match `[a-z0-9_]{1,32}` and reaches the model as `host:<kind>`
(anything else becomes `host:error`), so a host can never pose as a kernel
outcome; the message is capped at 4 KiB. A result is accepted only from the
connection the call was sent to (the peer's tool host at that moment);
another connection holding the token is refused
(`peer_tool_result_wrong_connection`). A result for a call that finished,
timed out or was cancelled is refused (`peer_tool_call_not_found`) and, when
the kernel still remembers the call (one hour, 1 024 calls), audited as
`late_result`.

### `peer/input` (server → host notification)

```
{peer, session_id, input_id, turn_id, text}
```

The system agent's `peer_send_input` to a host-owned peer. The kernel never
runs it as a kernel-internal turn (which would have no tools and no app
routing): it sends `peer/input` to the peer's host connection, the one that
registered the peer's tools, and the host starts the turn itself:

```
turn/start {session_id: <session_id>, turn_id: <turn_id>,
            input: [{kind: "text", text: <text>}]}
```

on that same connection, so the turn is host-driven: it gets the peer's
tools, its approvals and questions are raised on the peer's session (the
app's conversation) and answered by the person in the app, and the system
agent follows it through the existing peer paths (question parks wake the
system agent, which answers with `peer_respond`; `peer_gather` and the
transcript carry the reply).

- `session_id` is the peer's own session, `<originator base>#peer-<slug>`.
- `input_id` identifies this input (the system agent's session, turn and
  tool call) and is the reply handle; the kernel sends one input once, so a
  re-dispatched tool call is not sent again. Hosts SHOULD also drop a
  repeated `input_id`.
- `turn_id` is a fresh id minted by the kernel; hosts SHOULD start the turn
  with it (a turn already running on the peer, or a retry, then fails with
  the usual `turn/start` errors instead of starting twice).
- Ephemeral: only the host connection receives it. If no host connection
  holds the peer's route (never registered, or disconnected),
  `peer_send_input` fails with an error that says the app is not connected,
  and nothing is queued or run. External clients of `serve --host-managed`
  never receive it (they can never register).
- `peer_send_input` waits up to 5 s for the host's answer: a `turn/start`
  with the input's `turn_id` ends the wait at once, and `peer/input/reject`
  (below) fails the call with the host's reason. A host that does neither
  within the wait leaves the call reporting `queued`, which means the
  notification was written to the host connection's socket, not that the
  host started (or will start) the turn; the system agent follows the turn
  through the peer paths.
- A turn the host starts with that `turn_id` on the peer's session is the
  system agent's request made for the person, so it counts as *attended*
  (below): the app's foreground tools run in it, as in a request context.
- That turn is labelled `system_agent` by the kernel (UPCR-2026-034, "The
  shared peer conversation"): the prompt starts with `[from the system
  agent]` and its `result.md` says `origin: system_agent`. A `turn/start`
  with that `turn_id` and any other `origin` is refused
  (`turn_origin_mismatch`) without answering the input.
- The peer's session is shared with the person's turns. While one runs,
  starting the input's turn is refused with `turn_in_progress`; the host
  queues the input (keeping its `turn_id`) and starts it after the running
  turn ends, or refuses it with `peer/input/reject` reason `busy` when its
  queue is full.
  Since 2026-09-29 hosts run the person's chat in parallel, in a request
  context opened with `share_history` (UPCR-2026-034, "Parallel person
  context with shared history"), so this queue holds only `peer/input`s.

Peers that are not host-owned keep today's behaviour (the gateway inbox or
the serve continuation queue).

### `peer/input/reject` (#2618)

```
{session_id, peer, host_token, profile_id?, input_id,
 reason: "signed_out" | "no_consent" | "busy" | "other",
 message?}
→ {input_id, rejected: true, reported_to: "call" | "system_session"}
```

The host refuses a `peer/input` it cannot act on, so the system agent learns
why the peer did not act instead of the refusal being only logged. OctoSense's
reasons: the account is signed out or suspended (`signed_out`), the person has
not granted the app consent (`no_consent`), the peer is busy past the host's
queue limit (`busy`). `other` needs a `message` (one line, 1–256 bytes, no
control characters, no app data); the other reasons take none. Unknown fields
and unknown reasons are refused (`invalid_params`; a bad `reason` or `message`
has `data.kind` `peer_input_reject_invalid`).

Accepted only:

- with the peer's originator `session_id` and `host_token`, like every host
  call (`peer_originator_mismatch`, `peer_host_token_mismatch`);
- from the connection the `peer/input` was sent to
  (`peer_input_wrong_connection` otherwise);
- for an input the kernel sent to that peer and still remembers (24 hours,
  4 096 inputs; `peer_input_not_found` otherwise);
- once per `input_id` (`peer_input_already_rejected`), and only while the
  input is unanswered: no `turn/start` with its `turn_id` yet
  (`peer_input_already_started`).

Effect:

- If the system agent's `peer_send_input` is still waiting (above), the call
  fails with `peer_input_rejected: <reason>` (and, for `other`, the message
  in parentheses); `reported_to: "call"`.
- If the call already returned, the refusal is recorded on the peer's
  blackboard (`peers/<slug>/input_rejections.jsonl`) and reported to the
  system session (the peer's originator) at the start of its next turn, as
  a `peer_results_ready` context event like the peer-results note, each
  refusal once; `reported_to: "system_session"`.
- The `turn_id` is released: a turn with it no longer counts as the system
  agent's request (not attended), and a later `turn/start` with it on the
  peer's session is refused (`peer_input_rejected`).
- Audited in `tool_audit.jsonl` as `decision: "peer_input_rejected"` with
  `outcome` (the reason), `message`, `input_id`, `turn_id` and
  `reported_to`; each sent input is audited as `peer_input_sent`.

Wire compatibility is additive: a host that never calls it behaves as
before (its `turn/start` answers the input; the system agent's call returns
as soon as it arrives, or after the wait).

### `peer/tool/cancel` (server → host notification)

`{call_id, reason}`, `reason` = `timeout` (no result within the wait) or
`cancelled` (the turn was interrupted while the call was in flight). After
it the host MUST NOT execute the call.

## Enforcement

For a session whose topic is `peer-<slug>` of a host-owned peer with a
registered set, or any `peerctx-<slug>.<context>` of it, every turn start:

- **Caller identity.** Neither the topic nor the base key is a credential:
  any client of the profile can open `<base>#peerctx-<slug>.<id>`, and the
  host's base key can be listed. A turn gets the peer's set only when BOTH
  hold:
  - the session is on the base key of the peer's recorded originator, and
  - the turn is driven by the connection that registered the set (the one
    that presented the host token and is the peer's tool host).

  Any other turn on a peer or context topic, and any turn with a malformed
  context topic, gets **no tools at all**. When the host's connection closes
  its routes are dropped, and the peer's turns get no tools until the host
  registers again.
  **Hosts MUST drive the peer's turns (`turn/start` on the peer and its
  request contexts) on the connection that registered the set.**
- **App context only for the host's turns.** The app's private context —
  its memory namespace (the injected memory snapshot, and every earlier
  `memory_update` event replayed from the session's context history), its
  workspace hint, the session prompt and the agent's instructions — reaches
  the model only on turns driven by the peer's host connection (the one
  holding its route). A turn on `peer-<slug>` or `peerctx-…` from any other
  connection, and every kernel-internal continuation, gets a fixed minimal
  system prompt and no memory at all: neither the app's nor the profile's
  (it never falls back to the profile). A host-owned peer that never
  registered a set has no host connection, so none of its turns get app
  context: **hosts register their set (it may be empty) on the connection
  that drives the peer's turns.** The transcript itself is not filtered:
  reading or extending a bound session's history from a foreign connection
  is #2556-1 / #2571.
- **Kernel-internal continuations get no tools.** A turn the kernel starts
  itself on a peer session (a background result, a goal continuation) is
  nobody's turn: it gets no tools, whichever connection it happens to run
  on. Runs in the person's absence are the host's: it starts them with
  `turn/start` on its own connection. The system agent's `peer_send_input`
  to a host-owned peer is never such a turn: it is delivered to the host as
  `peer/input` (above).
- **Approvals stay with the host.** An approval raised by a host-routed
  call is tagged with the peer. Its `approval/requested` (and the matching
  `approval/decided`, `approval/cancelled`) is written to the session's
  ledger as usual but is filtered out of every other connection's live
  forwarding, `session/open` replay, pending-approval list and
  `session/hydrate`; `approval/respond` for it from any other connection is
  refused (`peer_host_connection_only`). The tag is held in memory; after a
  kernel restart the pending approvals are gone anyway (their waiters were),
  and only the historical events remain in the ledger. Those historical
  `approval/requested` events carry the exact arguments
  (`approval_kind: "host_tool"`), so visibility is decided from the event
  too: a `host_tool` approval whose host the kernel no longer knows (after a
  restart or an eviction of the in-memory tag) is shown to no connection on
  replay, in pending lists or in `session/hydrate`.
- **Turns and turn controls stay with the host (#2571).** On the session of
  a peer with a registered set (the `peer-<slug>` session and its request
  contexts, on the originator's base key), `turn/start`, `turn/steer`,
  `turn/interrupt` (including a voice turn's `supersedes_turn_id`),
  `session/rollback`, `session/goal/set`, `session/goal/clear`,
  `session/goal/operator_transition`, `loop/create`, `monitor/create`,
  `monitor/resume` (judged by the monitor's own session) and `session/delete`
  are accepted only from the peer's host connection, on the WebSocket and the
  stdio/embedded transports alike (`peer_host_connection_only`): anything
  else written into such a session would be text in front of a turn that
  has the app's act tools. The rule is derived from the tool set on disk,
  so it holds from the first call after a kernel restart (then nobody
  drives the session until the host registers again).
  - **Interrupting a turn ends its host calls.** App tool calls run in their
    own tool tasks; `turn/interrupt` (and a voice supersede) ends every call
    of the turn still waiting on the host: the host gets
    `peer/tool/cancel {reason: "cancelled"}`, and a non-`read` call is an
    unknown outcome that is not resent. The turn is remembered as
    interrupted, so a call of it that had not reached the host yet (its tool
    task still in a hook, or its approval answered just before) is refused
    (`cancelled`) and never sent.
  - **A host connection that closes ends its calls.** Every call in flight
    to it ends at once (a `read` call as `host_unavailable`, any other as
    `outcome_unknown`) instead of waiting out its timeout; so does a failed
    send to it (the socket gone before its close was seen).
- **No host filesystem access.** A host-bound app session never runs with
  `Host` filesystem permissions (`danger_full_access`, e.g. a Solo profile
  with `--danger-full-access` or `permission/profile/set`): the kernel
  clamps them to workspace access (keeping the approval policy), and such a
  session always carries its workspace scope; if the scope cannot be built,
  the session does not start.
- **No stale runtimes.** A session runtime records the app binding it was
  built for. The runtime cache re-checks it on every lookup and rebuilds a
  runtime whose binding changed — e.g. one cached for `<base>#peer-<slug>`
  before `peer/prepare` bound the topic, which would otherwise keep the
  profile's memory and workspace and unclamped permissions. `peer/prepare`
  and `peer/context/open` also drop every runtime cached for the bound topic
  at once.
- **No copies of an app session.** `session/fork` of a host-owned app
  peer's session (`peer-<slug>` of a host-bound peer) or of any request
  context (`peerctx-…`) is refused for every caller, the host included
  (`app_peer_fork_refused`): a fork's child key drops the topic, so the copy
  would lose the binding — workspace, memory namespace and tool set — while
  keeping the app's history. There is no binding-preserving fork; a host
  that wants a fresh line of conversation opens a new request context. The
  other ways a session could be copied were audited: the serve surface has
  no export/import, move, clone or branch-from-message method
  (`session/title.set` renames the title only, `session/rollback` truncates
  in place, `session/delete` removes); the gateway's `/new`
  (`fork_from_parent_if_missing`) runs only in the gateway process, which
  never hosts app peers.
- **Budget (#2500).** A request context's turns are charged to the OWNING
  peer's token budget and gated by it at `turn/start`, like the peer's own
  turns (no blackboard result is written for a context). An app tool call is
  refused (`budget_exhausted`, or `budget_unavailable` when the accounting
  cannot be read) once the peer's budget is spent, from any of its sessions;
  the tokens a tool result costs are part of the turn's spend.
- **Visibility (additive).** A host-driven turn keeps the peer's usual
  kernel tools (exactly `generic_tools` of them when the host sets that
  list) and gets one routed tool per declared app tool, recorded with the
  tool origin `HostRouted`. An app tool whose model name a kernel tool
  already has is not offered (the kernel tool wins), and a name equal to a
  reserved built-in tool name (`RESERVED_BUILTIN_TOOL_NAMES`) is refused at
  registration. The set is applied after the profile `tool_policy` and
  tool envelope: `generic_tools` narrows what they left, and the app tools
  are added after them, so the profile policy does NOT filter app tools
  (the host, which authorizes every call, is their authority). A set file
  that exists but cannot be read fails closed: no tools at all.
- **Unchanged without a registration.** A host-owned peer that never
  registered keeps today's roster, so existing hosts keep their tools.
- **Arguments.** Must be an object carrying every `required` property of the
  input schema and at most 64 KiB serialized; the host validates the rest.
- **Person present or absent.** A call is *attended* when its turn carries
  an approval bridge (every AppUI `turn/start` does) and it comes from an
  open request context of the peer (one of the app's interactive clients,
  the app's own conversation, UPCR-2026-034), from a host turn started from
  a kernel `peer/input` (the system agent's request made for the person),
  from the person's own turn on the peer's session (`origin: person`,
  UPCR-2026-034), or from a host session set (the host's own conversation). A call from any
  other turn of the peer's own session (the app agent's background runs) or
  from a turn with no bridge is *unattended*.
- **Risk.** A tool is *gated* when it is `destructive` or marked `outward`.
  - `read` and `act` run. A tool not marked `background` runs only attended
    (`not_background`).
  - Gated, `confirm: host`: an explicit approval first, attended or not,
    through the turn's existing approval bridge: the same
    `approval/requested` → `approval/respond` path every tool uses, raised
    on the calling session (the peer's or the request context's — the owning
    app's conversation, never the system agent's), carrying the exact
    arguments. Declined → error result (`denied`); no answer within
    `approval_ttl_secs` → error (`expired`) and the parked approval is
    cancelled. The host is not called in either case. UPCR-2026-034's rule
    holds: the system agent cannot answer these approvals through
    `peer_respond`; the person answers them in the app.
  - Gated, `confirm: app`, from any caller (the owning app's agent, another
    app's agent, the system agent's input, attended or not): the call goes
    to the host at once with `confirm_required: true` and no kernel
    approval. The host hands the confirmation to the OWNING app, whose own
    sheet shows who is calling (`caller`) and is the only prompt (e.g. a
    messaging app's `send_message`). If the owning app is not running or
    nobody is there, the host keeps the call waiting (up to the approval
    TTL, after an `awaiting_confirmation` acknowledgement) or refuses it
    visibly with an error result.
  - A turn with no approval bridge never runs a gated call that needs an
    approval: error (`approval_unavailable`), host not called.
  - Kernel approvals are **once-only**: they cover exactly this call and
    its arguments. A remembered approval scope (`approve_for_tool`,
    `approve_for_session`, `approve_for_turn`) never answers one, and
    `approval/respond` records no scope from one (the decision applies to
    this call only). Otherwise a single "always" would approve every later
    call of the tool on the session, whatever its arguments and whoever
    started the run. The other approval bridges honour the flag as well: the
    `octos chat` requester neither auto-resolves a once-only request from a
    session "always" nor records one from it, and the approved-tool replay
    path refuses it.
  - The person-absent approvals of `confirm: app` and `confirm: host` tools
    are raised on the peer's own session (`peer-<slug>`); the host surfaces
    them in the app's conversation (for native modules, OctoSense's
    `crates/app-peers` broker does).
- **One execution per occurrence.** Every non-`read` call is claimed before
  any approval or host call under
  `<session>/<turn>/<tool_call_id>/<argument digest>` (the `peer_send_input`
  occurrence shape plus a SHA-256 of the arguments, whose object keys are
  sorted, so key order does not matter). A re-dispatch of the same call is
  refused (`duplicate`): it neither asks again nor reaches the host again. A
  provider that reuses tool-call ids (`call_1`) with other arguments is a
  different occurrence. `read` calls may repeat. Claims are kept 24 h and
  only expired claims are evicted: when one tool set (the peer, or the host
  session a set is registered on) holds 4 096 unexpired claims, its next
  non-`read` call is refused (`busy`, host_busy) rather than forgetting a
  claim. The bound is per tool set, so a busy app never refuses another
  app's calls; the same per-peer bound applies to the `peer/input`
  deliveries remembered for de-duplication.
- **No retry after an unknown outcome.** A non-`read` call whose outcome is
  unknown — it timed out, or its turn was interrupted while the host was
  working on it — marks `(tool set, tool, argument digest)` for 24 h, where
  the tool set is the peer (or, for a set registered on a host session, that
  session). The same call from any session of that peer (its own session or
  any request context), in any later turn and under any tool-call id, is not
  sent unless the person approves it through an approval whose text says the
  earlier outcome is unknown (`approved_after_unknown`, which clears the
  mark); without an approval channel it is refused (`outcome_unknown_before`).
- **Routing and waiting.** At most 16 calls in flight per peer
  (`host_busy`). A call waits `call_timeout_ms`; a `confirm_required` call
  waits the approval TTL instead (the app's sheet may take as long as an
  approval would), and a gated call's wait extends to the approval TTL on an
  `awaiting_confirmation` acknowledgement (an acknowledgement of a call that
  is not gated is refused, `peer_tool_ack_not_gated`). A call waiting on the
  person holds one of the peer's 16 slots for as long as `approval_ttl_secs`
  (up to 7 days); hosts that confirm slowly should keep the TTL short.
  `peer/tool/result` takes the call out of the pending set and hands its
  result to the waiter under one lock, and the waiter at its deadline takes
  the call out under the same lock before looking for a result: either the
  host's answer wins and is delivered (never reported unknown while the
  host was told `accepted`), or the deadline wins and the answer is refused
  and audited as late. When the deadline wins the kernel sends
  `peer/tool/cancel`, and:
  - a `read` call ends with `timeout`;
  - any other call ends with `outcome_unknown`: the model is told the app may
    or may not have acted and must not retry, but check with a read tool or
    ask the person. The audit outcome is `unknown`.
- **Audit.** Every call, including refused ones, appends one JSON line to
  `peers/<slug>/tool_audit.jsonl`: `ts, peer, context_id, session_id,
  turn_id, tools_version, tool, app, tool_call_id, risk, decision, outcome,
  duration_ms, args_bytes, result_bytes` (a `late_result` row carries `app`
  and `call_id` too). `decision` is one of `allowed`,
  `approved`, `approved_after_unknown`, `app_confirms`, `denied`, `expired`,
  `approval_unavailable`, `duplicate`, `busy`, `outcome_unknown_before`,
  `not_background`, `invalid_args`, or `late_result` (a host
  answer after the kernel stopped waiting, with its `call_id`); `outcome` is
  `ok`, `error:<kind>`, `unknown` or `not_called`. Arguments and results
  themselves are not logged. The file is capped at 16 MiB: at the cap one
  `audit_full` marker is written and later rows are dropped; the host owns
  rotation.

### Host-managed serve (UPCR-2026-036)

Under `octos serve --host-managed`, an external client (the external token)
is never a peer's host:

- `peer/tools/register` and `peer/tool/result` are refused to it
  (`permission_denied`, `data.kind: "external_method_denied"`), both by the
  external method allowlist and again in the handlers, even with a valid
  peer `host_token`. Only a host-token connection can register, so only it
  becomes a tool host.
- It cannot name a `peer-`/`peerctx-` session at all
  (`host_owned_peer_session_denied`), and a turn it drives never counts as
  the host's: it gets no host tools and no app context.
- Every host-routed tool is registered with the tool origin
  `ToolOrigin::HostRouted` (`octos_agent::ToolOrigin`, recorded by
  `ToolRegistry` from `Tool::origin`). An external turn keeps only
  `ToolOrigin::Builtin` tools on its allowlist, so a host-routed tool is
  excluded by construction whatever its name.

Answering approvals: every runtime approval records the connection whose
turn raised it, and a host-routed call's approval also records the peer's
route. `approval/respond` for a host-routed call is accepted only from the
connection that raised it or the peer's current host connection
(`peer_host_connection_only`); this is read from the approval itself, so it
never fails open. An external client answers only approvals raised on its
own connection by its own turns: a turn id alone is not enough, since turn
ids are client-chosen. The reverse also holds: an approval raised by an
external client's turn goes only to that client and is answered only by it,
never by the host (`external_approval_owner_only`; UPCR-2026-036).

## One declaration source: `tools.json`

The app bundle's `tools.json` (checked and pinned by App Hub at admission) is
the ONE declaration of an app's tools, for native modules and script apps
alike; its entries are the `tools` of `peer/tools/register` as they are.
Whoever owns the host-owned app peer registers them: for a native module
that is OctoSense's `crates/app-peers` broker, which creates the peer with
`peer/prepare`, holds its host token, registers the pinned `tools.json`
when it opens (or resumes) the peer and after every App Hub update, and
executes each `peer/tool/call` with the calling session's identity. A module
never declares its tools a second way.

## Cross-app tools and approval rendering

- **Cross-app tools.** An app agent may call another app's tools when the
  calling app's manifest asks for them and the host granted them. The host
  registers such a tool on the calling app's peer like any other, with
  `app` naming its owner. The kernel routes it to the host and enforces its
  risk; the host authorizes every call against its grants (from `app` and
  `caller` on `peer/tool/call`). There is no second, agent-level consent.
  The system agent gets app tools the same way when it runs as a host-owned
  app peer, or on its own (non-peer) session: `peer/tools/register` without
  `peer` registers a set on the host's session (see "Host session
  target" under `peer/tools/register`), added only to the turns the registering connection drives there.
- **The host renders every approval.** A host-routed call's approval is sent
  only to the host connection, with `approval_kind: "host_tool"` and
  `typed_details.host_tool` = `{app, tool, args, risk, outward,
  calling_peer?, calling_session_id, context_id?, tool_call_id?,
  outcome_unknown_before}` (always, whatever the connection negotiated). The
  host draws the sheet (in the app's conversation or batched in the system
  chat) and answers with `approval/respond`. An agent's own text is never an
  approval surface. `confirm: host` is the normal path; `confirm: app` means
  the app's own sheet, also host UI.
- **Standing rules are the host's.** Outward and destructive tools always
  need the person: a live approval in the host UI, or a standing rule the
  host keeps, keyed to (owning app, tool) and showing the calling app. The
  kernel never remembers a decision for a host-routed call (once-only); a
  host with a standing rule answers the approval itself.

## Non-goals and follow-ups

- **Read access to the peer's folder for request contexts** (a
  `read_parent` flag on `peer/context/open`) and **`peer/purge`** (erase a
  host-owned peer and free its binding) are follow-ups: #2603, #2604.
- **Transcript reads by foreign clients (#2556-1).** A foreign connection
  of the profile can still `session/open`, `session/hydrate` and page the
  messages of a host peer's session (the write side is closed above). Under
  `serve --host-managed` external clients cannot name peer sessions at all,
  so only non-host-managed servers are affected, where every client of the
  profile holds the profile's own credential; binding the read side to the
  host is #2556-1.
- **Presence beyond request contexts.** Attended means "an open request
  context with an approval bridge". A host that wants a finer signal (the
  app window focused, the screen on) would need a per-turn flag; not in this
  change.
- **Full JSON Schema validation** of arguments and of results against
  `output_schema` stays with the host; the kernel checks shape, `required`
  and size.
- **Durable host routing across restarts.** The route is in memory; after a
  kernel or host restart the host re-registers (the set itself is durable, so
  enforcement never lapses in between — the peer's turns get no tools).
- **A durable approval park for unattended runs.** Background runs woken by
  events (News M3) run through the host's AppUI connection and get the
  normal approval bridge. A runner with no client at all refuses gated tools
  today; parking those for a later client is a follow-up.
- **Audit rotation.** `tool_audit.jsonl` stops at 16 MiB; rotating it is the
  host's, as for transcripts.
- **Paths that do not build the turn registry here** are not filtered by the
  set: `skill/action/invoke` on a peer session (client-driven),
  `review/start` (its own review agent), `octos chat`, and the gateway's
  in-process peer inbox (a `peer_send_input` delivered to a gateway actor).
  Host-owned peers are serve peers and are not driven through the gateway;
  refusing host-owned peers on that path explicitly is a follow-up.
- **Host-chosen bindings (UPCR-2026-034).** The peer's `cwd` is validated
  like a session open but not restricted further (a host could bind `$HOME`
  or the profile dir), and the memory namespace is only syntax-checked, not
  tied to an app id. Restricting both is a follow-up.
- **Schema enforcement.** The kernel checks only an object shape and
  `required`; `additionalProperties`, types and formats are the host's to
  enforce.
- **Reconnects.** A host that reconnects and registers again with the same
  token receives later calls; a call sent to its old connection ends when
  that connection closes (a non-`read` call as `outcome_unknown`).
- **Risk labels are the host's declaration.** The kernel enforces exactly
  what the host declares (risk, `outward`, `confirm`, `background`); it does
  not judge whether `news.delete` is honestly labelled. The check on the
  declaration is App Hub's admission gate on the app bundle's `tools.json`,
  which pins it. The strength of the whole mechanism is the secrecy of the
  host token, which today is minted once and never rotated or revoked:
  rotation and recovery are tracked in #2562 (item 2).

## Tests

- `peer_host_tool` unit tests (octos-agent, 13; also
  `should_describe_the_owning_app_the_tool_and_the_caller_when_asking_for_approval`
  and `should_mark_a_host_routed_tool_by_origin_whatever_its_name`):
  `should_run_read_and_act_tools_without_approval`,
  `should_run_destructive_only_after_an_explicit_approve_with_the_exact_arguments`
  (the request is once-only),
  `should_never_call_the_host_when_approval_is_declined_expired_or_unavailable`,
  `should_gate_an_outward_act_tool_like_destructive`,
  `should_raise_one_approval_per_occurrence` (and a reused id with other
  arguments is a new call),
  `should_digest_arguments_independently_of_key_order`,
  `should_send_an_act_call_once_but_let_reads_repeat`,
  `should_report_an_unanswered_act_call_as_unknown_not_failed` (no resend
  without an approval stating the unknown outcome; approved → resent),
  `should_let_the_app_confirm_when_the_person_is_present`,
  `should_hand_an_app_confirmed_call_to_the_owning_app_whoever_calls`,
  `should_refuse_foreground_tools_unattended_and_bad_arguments`
- `agent::execution` (octos-agent, 1): `should_never_auto_approve_a_once_only_request`
- `peers::host_tools` unit tests (5):
  `should_validate_names_schemas_and_collisions`,
  `should_accept_a_tools_json_entry_and_refuse_unknown_fields`,
  `should_evict_only_expired_claims_and_refuse_when_full`,
  `should_deliver_a_result_that_lands_at_the_deadline` (paused time),
  `should_deliver_a_result_taken_before_the_deadline_but_delivered_after_it`
  (an injected pause between taking the call and delivering it; fails on
  the previous ordering)
- `peer_host_tools_tests` (octos-cli, real profile runtime and sessions, 27):
  `should_advertise_and_dispatch_the_peer_tool_methods`,
  `should_refuse_a_registration_without_the_host_token`,
  `should_offer_exactly_the_registered_tools_and_refuse_an_unlisted_one`,
  `should_route_an_app_tool_call_to_the_host_and_back`,
  `should_time_out_and_cancel_a_call_the_host_never_answers` (late result
  audited),
  `should_run_a_destructive_tool_only_after_the_persons_approval` (includes
  the system agent's `peer_respond` being refused),
  `should_not_run_a_declined_or_expired_destructive_call_nor_ask_twice`,
  `should_replace_the_tool_set_atomically_and_refuse_a_stale_version`,
  `should_hand_a_confirm_app_call_to_the_owning_app_whoever_calls`,
  `should_report_an_unanswered_act_call_as_unknown_and_never_resend_it`
  (also in a later turn),
  `should_treat_a_call_interrupted_while_the_host_worked_as_unknown`,
  `should_wait_for_the_apps_confirmation_sheet_instead_of_timing_out`,
  `should_never_answer_or_remember_a_host_tool_approval_by_scope`,
  `should_give_no_tools_to_a_session_on_a_foreign_base_key`,
  `should_give_no_tools_to_another_connection_on_the_hosts_base_key`,
  `should_give_no_tools_to_a_kernel_internal_continuation`,
  `should_keep_a_host_tool_approval_and_turn_controls_on_the_host_connection`
  (live forwarding filtered, `approval/respond` and `turn/interrupt` /
  `turn/steer` refused from another connection, the host answers),
  `should_clamp_host_filesystem_access_for_a_bound_app_session`,
  `should_rebuild_a_session_runtime_cached_before_the_peer_was_bound`
  (fails without the cache re-check),
  `should_refuse_to_fork_an_app_peer_session`,
  `should_accept_a_tool_result_only_from_the_connection_the_call_was_sent_to`,
  `should_charge_a_request_contexts_turns_and_tools_to_its_peers_budget`,
  `should_route_a_real_turns_app_tool_call_to_the_host_end_to_end` and
  `should_ask_the_person_before_a_real_turns_destructive_call_end_to_end`,
  `should_give_app_memory_only_to_the_host_connections_turns` (a scripted
  model's requests: the host's turns on the peer and on a context carry the
  app's memory and never the profile's; a foreign connection's turns on both
  and a kernel continuation carry neither, including memory replayed from
  the host's earlier turns)
  (a real `turn/start` through `run_standalone_turn` with a scripted model:
  roster, host routing, the tool result in the model's context, the approval
  on the host connection),
  `should_offer_exactly_the_generic_tools_the_host_sets` (exact sets with no
  kernel-side exclusions, never adding a tool the session lacks, schema
  shapes),
  `should_refuse_an_awaiting_confirmation_ack_for_a_call_that_is_not_gated`
- Additive registration and `peer/input` (octos-cli `peer_host_tools_tests`):
  `should_keep_the_peers_kernel_tools_when_it_registers_an_empty_set`,
  `should_never_let_an_app_tool_shadow_a_kernel_tool`,
  `should_deliver_the_system_agents_input_to_the_host_connection_when_the_peer_is_host_owned`
  (no kernel turn queued; one input sent once; a later turn's reused
  tool-call id is a new input; only the host connection receives it),
  `should_fail_visibly_and_run_nothing_when_the_app_is_not_connected`,
  `should_run_the_system_agents_input_as_a_host_driven_turn_with_tools_end_to_end`
  (the host starts the turn from `peer/input`; the model gets the usual tools
  plus the app tool; the destructive call's approval goes to the app),
  `should_answer_a_host_peer_sessions_questions_only_on_the_owning_or_host_connection`,
  `should_carry_the_owning_app_and_the_caller_when_a_cross_app_tool_is_called`
- Host-managed serve (UPCR-2026-036): `should_refuse_host_tool_registration_and_results_when_the_connection_is_external`,
  `should_never_give_an_external_turn_a_host_routed_tool_when_one_is_registered`,
  `should_answer_a_host_tool_approval_only_on_its_connection_when_turn_ids_collide`,
  `should_refuse_a_host_tool_whose_model_name_is_a_kernel_tools`, and in
  octos-agent `should_mark_a_host_routed_tool_by_origin_whatever_its_name`
- `peer/input/reject` (#2618, octos-cli `peer_host_tools_tests`):
  `should_fail_the_waiting_peer_send_input_with_the_hosts_reason`,
  `should_end_the_wait_without_an_error_when_the_host_starts_the_turn`,
  `should_report_a_rejection_after_the_call_returned_on_the_system_session`,
  `should_refuse_a_rejection_from_a_foreign_connection_with_a_bad_token_twice_or_after_the_turn_started`,
  `should_refuse_an_unknown_reason_an_unknown_field_or_a_bad_message`,
  `should_refuse_a_turn_start_with_a_rejected_inputs_turn_id` (also: the
  released turn id no longer runs a foreground app tool); the method is in
  the advertise/dispatch and external-refusal tests
- Round 11 (octos-cli `peer_host_tools_tests`):
  `should_cancel_an_in_flight_host_call_when_the_turn_is_interrupted`
  (a real `turn/start` + `turn/interrupt`; fails without the fix),
  `should_end_in_flight_calls_at_once_when_the_host_connection_drops`,
  `should_run_a_foreground_tool_in_a_host_turn_started_from_peer_input`,
  `should_hide_a_host_tool_approval_whose_host_is_unknown`,
  `should_refuse_foreign_writes_to_a_host_peer_session_when_its_set_is_on_disk`,
  `should_refuse_foreign_turn_controls_on_a_host_peer_session_when_its_set_is_on_disk`
  (no turn has run on the session; a corrupt `host_tools.json` is confined
  too),
  `should_give_the_system_agent_the_app_tools_the_host_registers_on_its_session`
- `spec_section6_catalog_lists_every_advertised_method`
