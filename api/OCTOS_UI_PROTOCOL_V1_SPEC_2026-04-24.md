# Octos UI Protocol v1 Spec — 2026-04-24

Status: draft spec for `M9.1`.

Sprint: `coding-green`

This is the first protocol document for the M9 control-plane layer. It is intentionally narrower than the eventual end-state. The goal is to define one client/runtime boundary that both `octos-tui` and future server work can target without baking unresolved M8 runtime defects into the contract.

Code sketch:

- draft Rust types live in [crates/octos-core/src/ui_protocol.rs](/Users/yuechen/home/octos/crates/octos-core/src/ui_protocol.rs:1)

Related planning:

- [OCTOS_M9_ISSUE_STACK_2026-04-24.md](../docs/OCTOS_M9_ISSUE_STACK_2026-04-24.md)
- [OCTOS_TUI_ARCHITECTURE_2026-04-24.md](../docs/OCTOS_TUI_ARCHITECTURE_2026-04-24.md)
- [OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24.md](../docs/OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24.md)

## 1. Goals

`UI Protocol v1` should give Octos clients a first-class interactive boundary for:

- opening or resuming a session
- starting and interrupting turns
- consuming live turn output
- receiving stable tool/task/progress state
- supporting approval, diff preview, and task-output drill-down
- reconnecting without heuristic merge logic

This protocol is not meant to replace every REST route immediately. It is meant to become the authoritative interactive layer while REST remains useful for snapshot hydrate and compatibility.

## 2. Non-Goals

`UI Protocol v1` does not try to:

- replace all existing REST endpoints on day one
- model every internal runtime detail
- freeze the final end-state of the session event ledger
- compensate for known-bad M8 runtime behavior

If an M8 runtime surface is still non-authoritative, the protocol should either:

- avoid exposing it yet, or
- mark it clearly as draft/non-authoritative

## 3. Transport

Recommended transport:

- JSON-RPC 2.0 over WebSocket
- JSON-RPC 2.0 over stdio for trusted local-process clients, governed by
  accepted
  [UPCR-2026-016](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_016_STDIO_TRANSPORT.md)

Why:

- request/response fits turn control and approval response
- notifications fit live streaming and task/progress updates
- one long-lived socket is a better fit than stitching together `/api/chat`, `/api/ws`, and SSE

REST remains useful for:

- initial session lists
- artifact/file hydrate
- compatibility during migration

Stdio transport rules:

- `octos serve --stdio` reads one newline-delimited JSON-RPC object per line
  from stdin and writes one newline-delimited JSON-RPC response or notification
  per line to stdout.
- stdout is protocol-only. Logs and diagnostics must go to stderr.
- Stdio is a local trusted transport. It does not carry HTTP headers,
  WebSocket Origin checks, or bearer-token headers.
- Stdio clients must send one complete UTF-8 JSON object per line. Servers and
  clients may reject frames larger than `MAX_TEXT_FRAME_BYTES` with
  `frame_too_large`. Servers must enforce the bound while reading the line,
  not after buffering an unbounded frame.
- A failed stdout write or closed pipe terminates the stdio AppUI connection
  and stops dispatching new requests for that connection.
- Stdio does not define an application heartbeat. Pipe EOF on stdin and write
  failure on stdout are the stdio liveness signals; after either signal the
  server must clean up connection-owned live forwarders and active turns.
- Stdio shares the WebSocket AppUI method surface. A method advertised in
  `supported_methods` must route to the same server handler and return the
  same result/error shape over both transports. Transport-only unsupported
  errors are allowed only for methods omitted from `supported_methods` or
  listed in the checked-in conformance allowlist.
- Stdio clients may send `client_hello` as their first request to negotiate
  the same feature-token set that WebSocket clients normally send through
  `X-Octos-Ui-Features` or the `ui_feature` query parameter.
- Because stdio has no `X-Profile-Id` header, profile-scoped methods resolve
  identity in this order: explicit `params.profile_id`, profile encoded in
  `params.session_id`, profile bound by the most recent successful `session/open`,
  then the server default profile. Clients should pass `profile_id` explicitly
  before `session/open`.

## 4. Versioning

Protocol identifier:

- `octos-ui/v1alpha1`

Rules:

- incompatible wire changes require a new protocol version
- additive fields are allowed inside one version
- clients should treat unknown fields as ignorable
- clients must not assume unknown enum variants are impossible forever

### 4.1 Change Control

`UI Protocol v1` is a client/runtime contract. No sprint worker, runtime
implementation, TUI implementation, or web implementation may change the wire
contract informally.

Protocol-governed surfaces include:

- protocol identifier and schema/capability version constants
- JSON-RPC method names
- notification names
- command params
- command result payloads
- notification payloads
- enum variants serialized on the wire
- cursor semantics
- approval, diff, task-output, and replay semantics
- capability negotiation and unsupported-capability behavior

Allowed without a change request:

- internal runtime/config types that do not serialize through AppUi/UI Protocol
- server implementation fixes that preserve the same wire contract
- client rendering changes that consume the same wire contract
- documentation clarifications that do not change behavior

Formal change request required:

- any new method or notification
- any new required field
- any new enum variant serialized over the wire
- any semantic change to an existing field
- any approval/diff/task/replay behavior change visible to clients
- any compatibility or capability-negotiation change

Process:

1. Create a change request from
   [OCTOS_UI_PROTOCOL_CHANGE_REQUEST_TEMPLATE.md](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_TEMPLATE.md).
2. Mark it `proposed` and link the related M issue.
3. Review compatibility, capability negotiation, tests, and rollout plan.
4. Mark it `accepted` before code changes land.
5. Update this spec, `octos-core` protocol types, server tests, TUI tests, and
   tmux/e2e tests in the same implementation change.

Executable contract gate:

- [crates/octos-core/src/ui_protocol.rs](/Users/yuechen/home/octos/crates/octos-core/src/ui_protocol.rs:1)
  contains literal golden tests for the v1 protocol identifier, schema
  versions, JSON-RPC version, command method set, notification method set, and
  representative wire payloads.
- Any change to those golden tests is a protocol contract change unless it only
  fixes a test typo that does not alter the expected wire contract.
- Workers must not update the golden contract tests to make code pass unless
  the related UPCR is already marked `accepted`.

Current M9 sandbox-parity decision:

- `M9.10`, `M9.12`, `M9.13`, and `M9.15` should not require protocol changes.
  They are internal config/runtime/sandbox enforcement work.
- `M9.14` additive approval payload fields are governed by accepted
  [UPCR-2026-001](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md).
  Any additional approval semantics, persistent policy mutation, or non-additive
  field change requires another accepted UPCR.
- `M9.17` workspace/artifact/git pane snapshot payloads are governed by
  accepted
  [UPCR-2026-002](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_002_PANE_SNAPSHOTS.md).
  That UPCR authorizes snapshot hydration only; live pane-update notifications
  require a future accepted UPCR.
- Per-session workspace cwd selection is governed by accepted
  [UPCR-2026-003](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_003_SESSION_WORKSPACE_CWD.md).
  That UPCR authorizes launch/open-time workspace binding only; in-session cwd
  mutation UX or persistent cwd approval policy requires a future accepted UPCR.
- The additive `cancelled` variant on `TaskRuntimeState` (used by the
  `task/updated` notification) is governed by accepted
  [UPCR-2026-004](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_004_TASK_RUNTIME_CANCELLED.md).
  That UPCR carries the `task_supervisor` cancellation lifecycle through to
  the wire so cancelled tasks no longer fall back to `Running` in the UI.
- The additive `task/list`, `task/cancel`, and `task/restart_from_node`
  command methods are governed by accepted
  [UPCR-2026-005](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_005_TASK_CONTROL_RPCS.md).
  That UPCR closes M9 harness audit gap #704 by giving clients first-class
  AppUi RPCs for the supervisor's `cancel` / `relaunch` / task-snapshot
  primitives, gated behind the `harness.task_control.v1` feature flag.
- The additive `is_snapshot_projection: bool` field on the
  `task/output/read` result is governed by accepted
  [UPCR-2026-006](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_006_TASK_OUTPUT_SNAPSHOT_PROJECTION.md).
  That UPCR closes M9 harness audit gap #707 by giving clients a single
  wire-level boolean for snapshot vs. live-tail semantics, independent of the
  open `source` enum and the free-form `limitations[]` registry.
- The additive `reason`, `terminal_state`, and `ack_timeout` optional fields
  on `TurnInterruptResult` are governed by accepted
  [UPCR-2026-008](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_008_TURN_INTERRUPT_TYPED_FIELDS.md).
  That UPCR closes M9 protocol-as-contract audit issue #721 by codifying the
  diagnostic fields the `turn/interrupt` handler has been emitting since the
  protocol shipped. The typed contract is now equivalent to the wire shape;
  the canonical minimal `{ "interrupted": <bool> }` response is preserved.
- The additive `capabilities` field on `SessionOpened` (carrying the
  negotiated `UiProtocolCapabilities` payload) is governed by accepted
  [UPCR-2026-007](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_007_SESSION_OPEN_CAPABILITIES.md).
  That UPCR closes M9 harness audit gap #720 by emitting the negotiated
  method/notification/feature surface in-band so clients no longer have
  to read the spec doc to know which `X-Octos-Ui-Features` tokens the
  server honours. The field is the in-band counterpart to the
  capability-negotiation rules in this section: `supported_features` is
  the intersection of the client's `X-Octos-Ui-Features` request with
  the server's known feature registry; absent header falls back to the
  first-server-slice default.
- The additive `session/hydrate` command (returning the authoritative
  chat-state projection: messages, threads, turns, pending approvals) is
  governed by accepted
  [UPCR-2026-009](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_009_SESSION_HYDRATE.md),
  gated behind the `state.session_hydrate.v1` feature flag.
- The additive `thread/graph/get` command (lifting the in-memory
  `Session::threads()` partition onto the wire so clients no longer
  reconstruct grouping from message-ordering heuristics) is governed by
  accepted
  [UPCR-2026-010](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_010_THREAD_GRAPH_GET.md),
  gated behind `state.thread_graph.v1`.
- The additive `turn/state/get` command (deterministic turn lifecycle
  introspection backed by the active-turn registry AND a durable ledger
  projection) is governed by accepted
  [UPCR-2026-011](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_011_TURN_STATE_GET.md),
  gated behind `state.turn_state_get.v1`. Returns `state: "unknown"`
  rather than an error for missing turns.
- The additive `message/persisted` notification (durable-commit
  confirmation per session row, fired AFTER `add_message_with_seq`'s
  fsync) is governed by accepted
  [UPCR-2026-012](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_012_MESSAGE_PERSISTED.md),
  gated behind `event.message_persisted.v1`. Strict-ordered per session.
- The additive M9-γ projection `Envelope` shape (canonical
  `(thread_id, seq, client_message_id?, payload)` tuple consumed by the
  deterministic web client projection) is governed by accepted
  [UPCR-2026-014](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_014_PROJECTION_ENVELOPE.md),
  gated behind `projection.envelope.v1`. The shape is documented in § 14
  "M9-γ Envelope" of this spec; legacy `message/delta`,
  `message/persisted`, `tool/*`, and `turn/completed` notifications
  continue to flow on connections that do not negotiate this feature
  until `M9-γ-3` deletes them.
- The additive stdio AppUI transport is governed by accepted
  [UPCR-2026-016](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_016_STDIO_TRANSPORT.md).
  It changes only framing and process launch. Method names, params, results,
  notifications, errors, and capability semantics remain shared with the
  WebSocket transport.
- The additive runtime/auth/LLM-profile inspection methods
  (`config/capabilities/list`, `session/status/read`, `auth/*`,
  `profile/llm/*`, `mcp/status/list`, `tool/status/list`) are governed by
  accepted
  [UPCR-2026-017](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_017_RUNTIME_PROFILE_INSPECTION.md).
  They let TUI and other non-web clients render dashboard-equivalent login,
  provider, model, MCP, tool, and runtime status from server truth.
- The additive local solo onboarding and permission-policy inspection methods
  (`profile/local/create`, `permission/profile/list`,
  `permission/profile/set`, and the extended `session/status/read` runtime
  policy stamp) are governed by accepted
  [UPCR-2026-018](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_018_LOCAL_SOLO_ONBOARDING_AND_POLICY.md).
  They let local clients create a no-OTP solo owner profile and render the
  server's effective sandbox/approval/filesystem/network policy.
- The additive backend-owned review workflow method (`review/start`) is
  governed by
  [UPCR-2026-019](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_019_AGENT_SUPERVISION.md),
  gated behind `review.start.v1`. It starts a product-level review workflow
  that the backend implements with native/CLI/MCP specialist agents. It is
  not a generic UI-side subagent scheduler.
- The additive coding tool contract inspection fields on `tool/status/list`
  and `session/status/read` are governed by proposed
  [UPCR-2026-020](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_020_CODEX_TOOL_PARITY.md).
  They let clients verify that the backend exposes Codex-compatible
  model-visible coding tools without letting clients invoke those tools
  directly.
- The additive backend context lifecycle surface (`context.lifecycle.v1`,
  `context` and `context_state` on `session/open`, `session/hydrate`,
  legacy REST-bridge `session/status.get`, AppUI `session/status/read`,
  `turn/state/get`, and context lifecycle notifications) is governed by the
  M16 context-manager workstream
  [OCTOS_CONTEXT_MANAGER_GAP_CONTRACT](../docs/OCTOS_CONTEXT_MANAGER_GAP_CONTRACT.md).
  It lets AppUI clients inspect the server-owned prompt context generation,
  transcript hash, checkpoint, compaction, and recovery state without
  reconstructing it from chat rows.
- The additive structured mid-turn user-question surface (the
  `user_question/respond` command, the `user_question/requested` notification,
  the `questions[]` / `answers[]` payloads, and the new `question_id`
  correlation id) is governed by proposed
  [UPCR-2026-023](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_023_ASK_USER_QUESTION.md),
  gated behind the `user_question.v1` feature flag. It is the codex/Claude
  `AskUserQuestion` shape implemented as "approval + choices + free-text": the
  agent tool blocks the turn at a deterministic tool boundary (mirroring
  `approval/requested`), the server emits `user_question/requested`, the client
  renders a single/multi-select picker plus a free-text "Other", and
  `user_question/respond` resumes the waiting tool. A turn interrupt cancels
  pending questions; a client lacking the capability receives the agent tool's
  structured-metadata/generic-text fallback instead of a blocking question. See
  [docs/design/ask-user-question-2026-06-03.md](../docs/design/ask-user-question-2026-06-03.md).
- The additive manifest-declared skill action surface
  (`skill/action/list`, `skill/action/invoke`, and the `skill.actions.v1`
  capability feature) is governed by accepted
  [UPCR-2026-026](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_026_SKILL_ACTIONS.md).
  It lets clients render and invoke skill-owned UI actions without gaining a
  generic AppUI tool-call primitive. Skill manifests own the action id, input
  schema, UI hints, bound backend tool, and any file-materialization mode.
- The additive skill action background-job surface
  (`skill/action/job/list`, `skill/action/job/read`, the
  `skill/action/job/updated` notification, and the `skill.action_jobs.v1`
  capability feature) is governed by accepted
  [UPCR-2026-027](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_027_SKILL_ACTION_JOBS.md).
  It lets clients observe manifest-declared background actions through generic
  persisted job snapshots. It does not introduce notebook-specific routes or a
  generic client-selected tool-call primitive.

## 5. Identity Model

These ids need to be stable and client-visible:

- `session_id`
  Uses Octos session identity. For now this can map to existing `SessionKey`.
  Profile-qualified local TUI/coding sessions use
  `{profile_id}:local:{client_id}#{topic}`; `local` is a recognized channel
  name for profile extraction, so stdio clients can recover profile scope from
  `session_id` after the initial `session/open`.
- `turn_id`
  One user-visible interaction turn. This is the primary correlation id for live output.
- `tool_call_id`
  One tool execution inside a turn.
- `approval_id`
  One approval request lifecycle.
- `preview_id`
  One diff preview lifecycle.
- `task_id`
  One background or delegated task.
- `output_cursor`
  A resumable cursor or offset into task output.
- `event_cursor`
  A resumable position in the ordered protocol event stream.

Current draft Rust types for `turn_id`, `approval_id`, `preview_id`, `output_cursor`, and `event_cursor` live in [ui_protocol.rs](/Users/yuechen/home/octos/crates/octos-core/src/ui_protocol.rs:1).

### 5.1 M9-γ projection identity (UPCR-2026-014)

Under the M9-γ deterministic projection model (§ 14), envelope identity
collapses to the per-thread `seq`. Specifically:

- The canonical projection key is `(thread_id, seq)` — see `Envelope`
  in § 14.
- `client_message_id` rides on user-message-rooted envelopes ONLY for
  the optimistic `<GhostBubble>` overlay's match-and-unmount logic;
  the projection itself MUST NOT consult it.
- The legacy per-row `message_id` (carried, for example, on
  `MessagePersistedEvent.message_id`) is **deprecated for projection
  identity** as of UPCR-2026-014. It survives in
  `Envelope.payload` (e.g. `assistant_persisted.meta.message_id`) for
  audit/render display, but the projection uses `seq` as the sole key.
  The field is retained — not deleted — so legacy
  `appendCompletionBubble` / `message/persisted` consumers continue to
  work until `M9-γ-3` removes them.

## 6. Envelope Model

Client commands are JSON-RPC requests.

Server notifications are JSON-RPC notifications.

The logical command/event names below mirror the current wire inventory:

- command source of truth:
  `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_COMMAND_METHODS`,
  `UI_PROTOCOL_FIRST_SERVER_METHODS`, and
  `crates/octos-cli/src/api/ui_protocol.rs::APPUI_EXTRA_METHODS`
- notification source of truth:
  `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_NOTIFICATION_METHODS`
- executable route inventory:
  `e2e/fixtures/appui-conformance/m18-route-inventory.json`
- human-readable wire inventory:
  `api/OCTOS_UI_PROTOCOL_WIRE_INVENTORY_2026-05-24.md`

Commands:

Session, turn, and approval core:

- `session/open`
- `session/hydrate` (gate `state.session_hydrate.v1`, accepted `UPCR-2026-009`)
- `session/rollback` (conversation-only rewind; drops the last N user turns and
  re-projects the trimmed thread exactly like `session/hydrate`; #1516)
- `session/fork` (branch a session into a new one with copied history; #1613)
- `session/btw` (quick aside question answered while the current turn runs; #1609)
- `turn/start`
- `turn/interrupt`
- `turn/state/get` (gate `state.turn_state_get.v1`, accepted `UPCR-2026-011`)
- `thread/graph/get` (gate `state.thread_graph.v1`, accepted `UPCR-2026-010`)
- `approval/respond`
- `approval/scopes/list` (approval-scope discovery; first-server slice)
- `user_question/respond` (gate `user_question.v1`, proposed `UPCR-2026-023`)
- `permission/profile/list`, `permission/profile/set`
  (accepted `UPCR-2026-018`)
- `diff/preview/get`

Task and harness control:

- `task/output/read`
- `task/list` (capability-gated `harness.task_control.v1`, accepted `UPCR-2026-005`)
- `task/cancel` (capability-gated `harness.task_control.v1`, accepted `UPCR-2026-005`)
- `task/restart_from_node` (capability-gated `harness.task_control.v1`, accepted `UPCR-2026-005`)
- `task/artifact/list`, `task/artifact/read`
  (capability-gated, canonical aliases of `agent/artifact/*`; #965 / accepted `UPCR-2026-019`)

Supervised review and M15 agent/goal/loop autonomy (capability-gated, accepted
`UPCR-2026-019` / `UPCR-2026-021`):

- `review/start`
- `agent/list`, `agent/status/read`, `agent/output/read`,
  `agent/artifact/list`, `agent/artifact/read`, `agent/interrupt`, `agent/close`
- `session/goal/get`, `session/goal/set`, `session/goal/clear`
- `loop/create`, `loop/list`, `loop/delete`, `loop/pause`, `loop/resume`,
  `loop/fire_now`

Router (Wave4-A):

- `router/set_mode`, `router/get_metrics`

M12 Phase-D auxiliary REST→WS surface (all gated `auxiliary.rest_to_ws.v1`):

- `session/list`, `session/snapshot`, `session/messages_page`,
  `session/status.get`, `session/files.list`, `session/tasks.list`,
  `session/workspace.get`, `session/title.set`, `session/delete`
- `system/status.get`
- `content/list`, `content/delete`, `content/bulk_delete`
- `memory/overview`, `memory/entity`, `cron/list`, `cron/toggle`

Runtime, auth, profile, and onboarding inspection (server-handled
`APPUI_EXTRA_METHODS`):

- `config/capabilities/list` (accepted `UPCR-2026-017`)
- `client_hello` (accepted `UPCR-2026-016`)
- `profile/local/create` (accepted `UPCR-2026-018`)
- `session/status/read` (accepted `UPCR-2026-017`)
- `auth/status`, `auth/send_code`, `auth/verify`, `auth/me`, `auth/logout`
  (accepted `UPCR-2026-017`; `auth/me` and `auth/logout` are omitted from the
  stdio capability set and return typed `auth_unavailable` per § stdio policy)
- `profile/llm/catalog`, `profile/llm/list`, `profile/llm/upsert`,
  `profile/llm/select`, `profile/llm/delete`, `profile/llm/test`,
  `profile/llm/fetch_models` (accepted `UPCR-2026-017`)
- `profile/skills/list`, `profile/skills/registry/search`,
  `profile/skills/install`, `profile/skills/remove` (server-handled skills
  management)
- `skill/action/list`, `skill/action/invoke` (gate `skill.actions.v1`,
  accepted `UPCR-2026-026`; manifest-declared skill actions only)
- `skill/action/job/list`, `skill/action/job/read` (gate
  `skill.action_jobs.v1`, accepted `UPCR-2026-027`; persisted background skill
  action jobs only)
- `mcp/status/list`, `tool/status/list` (accepted `UPCR-2026-017`)
- `onboarding/workspace_probe` (gate `onboarding.workspace_probe.v1`,
  local-solo only; #1057)

Notifications:

Session (server-pushed open-state echo for reconnect/replay):

- `session/open` (same method name as the §7 command; emitted as
  `UiNotification::SessionOpened` and replayed from the durable ledger)

Turn, message, and tool lifecycle:

- `turn/started`, `turn/completed`, `turn/error`
- `message/delta`
- `message/reasoning_delta` (live LLM reasoning/thinking stream, sibling of
  `message/delta`; #1502)
- `message/persisted` (accepted `UPCR-2026-012`)
- `turn/spawn_complete` (gate `event.spawn_complete.v1`; M10 background-tool completion envelope)
- `tool/started`, `tool/progress`, `tool/completed`

Approval lifecycle:

- `approval/requested`, `approval/auto_resolved`, `approval/decided`,
  `approval/cancelled`

Structured user-question lifecycle (gate `user_question.v1`, proposed
`UPCR-2026-023`):

- `user_question/requested`

Skill action jobs (gate `skill.action_jobs.v1`, accepted `UPCR-2026-027`):

- `skill/action/job/updated`

Task and progress:

- `task/updated`
- `plan/updated` (gate `plan.todos.v1`; model-authored plan/todo checklist
  snapshot that replaces any prior plan wholesale; #1622)
- `task/output/delta`
- `progress/updated`
- `warning`
- `protocol/replay_lossy`

Projection and session bridging (accepted `UPCR-2026-014`):

- `projection/envelope` — wire `params` carries the bare `Envelope`
  fields FLATTENED with the routing keys `session_id` (bare base key) +
  optional `topic`, so a multi-session client can route each envelope
  (`feat(envelope-wire-routing)`); see § 14.1.
- `file/attached`
- `session/event`

Voice rich-output visual lifecycle (#1477, ungated; accepted
`UPCR-2026-024`):

- `visual/generating`, `visual/succeeded`, `visual/failed` — typed
  lifecycle for a background visual artifact (illustrated HTML / image /
  infographic) produced by a voice turn. Emitted on the same
  ledger-backed live path as `file/attached`, but kept distinct from it:
  `file/attached` stays a pure artifact-delivery signal while these carry
  the placeholder lifecycle, so the split survives a future
  `projection.envelope.v1` cutover. See § 8.

Voice reply-audio streaming (gate `event.voice_audio.v1`; #1504):

- `voice/audio_chunk` — streamed reply-audio frames (base64) for a voice turn.
  Delivery is gated by the `event.voice_audio.v1` capability: a client that did
  not negotiate it is filtered off the chunk stream and instead receives the
  whole-file audio as a `file/attached` envelope, which is itself gated by
  `event.file_attached.v1` (the reply audio has no other carrier — a client
  that negotiated neither capability receives no playable reply audio).

Voice exit intent (ungated; accepted `UPCR-2026-025`):

- `voice/exit` — the voice turn detected an end / goodbye / mute intent
  (the model appended an in-band `[[EXIT]]` control marker, which the
  backend strips from every model-/client-facing surface). The client
  leaves the `/voice` screen and returns home — after the turn's farewell
  audio finishes playing (navigation is gated client-side on the reply
  audio draining). Emitted on the same ledger-backed live path as
  `file/attached`. See § 8.

Router and queue (Wave4-A):

- `router/status`, `router/failover`, `queue/state`

M15 agent/goal/loop autonomy (accepted `UPCR-2026-021`):

- `agent/updated`, `agent/output/delta`, `agent/artifact/updated`
- `session/goal/updated`, `session/goal/cleared`
- `loop/updated`, `loop/fired`, `loop/completed`

M16 context lifecycle (gate `context.lifecycle.v1`):

- `context/compaction_completed`, `context/compaction_started`, `context/normalization_reported`

## 7. Command Semantics

### `session/open`

Purpose:

- open a session for interactive control
- declare the client’s current `after` cursor for resume/replay

Minimum params:

- `session_id`
- optional `profile_id`
- optional `cwd`
  Capability-gated per-session workspace request from accepted
  `UPCR-2026-003`. Clients may send it only when requesting
  `session.workspace_cwd.v1`. The server must canonicalize and approve it
  against runtime filesystem roots before binding cwd-scoped tools.
- optional `after`

Expected result:

- active session metadata
- accepted cursor baseline if relevant
- optional `workspace_root` when the server has accepted or already knows the
  session workspace

Optional result fields from accepted `UPCR-2026-002`:

- `panes`
  Capability-gated workspace, artifact, and git pane snapshot payload. Servers
  may include it only when `pane.snapshots.v1` is negotiated. Clients must keep
  fallback pane rendering when it is absent.

Optional result fields from accepted `UPCR-2026-003`:

- `workspace_root`
  Canonical server-approved workspace root for the session. Clients should use
  it for display/status and must not infer approval from the requested `cwd`
  alone.

Required result fields from accepted `UPCR-2026-007`:

- `capabilities`
  Negotiated `UiProtocolCapabilities` payload. Always present. Carries the
  protocol version, capability schema version, server-advertised method and
  notification sets, and the `supported_features` subset honoured for this
  session. When the client did not send `X-Octos-Ui-Features`, the field
  echoes the server's first-server-slice default so a discovery-aware client
  can still learn the surface in-band. When the client sent feature tokens,
  `supported_features` is the intersection of the request with the server's
  known feature registry — the server never advertises a flag the client did
  not request. Capability-gated methods (`task/list`, `task/cancel`,
  `task/restart_from_node` behind `harness.task_control.v1`) appear in
  `supported_methods` only when their gating feature is in the negotiated
  `supported_features`, so the advertised method set always agrees with the
  callable surface.

Optional result fields from the M16 `context.lifecycle.v1` contract:

- `context`
  Server-owned lifecycle envelope for the opened session. Present when
  `context.lifecycle.v1` is available for the connection. It contains
  `schema = "octos.context.lifecycle.v1"`, the same `context_state` under
  `state`, and compaction metadata including count and the latest compaction
  record.
- `context_state`
  Server-owned model-visible context state for the opened session. Present
  when `context.lifecycle.v1` is available for the connection. It uses the
  `UiContextState` shape documented under `session/status/read` and is sourced
  from the same canonical profile/session store used by `turn/start` and
  `session/hydrate`.

### `session/hydrate`

Purpose:

- return the authoritative chat-state projection for a session
- hydrate messages, threads, turns, pending approvals, and replay envelopes
  according to the request's `include` filter

Gate:

- `state.session_hydrate.v1`

Minimum params:

- `session_id`
- optional `include`
- optional `after`

Optional result fields from the M16 `context.lifecycle.v1` contract:

- `context`
  Full lifecycle envelope for the hydrated session, using the same shape as
  `session/open`.
- `context_state`
  Typed model-visible context state for the hydrated session. This state must
  be read from the same canonical profile/session store used by `turn/start`,
  not reconstructed by the client from hydrated chat rows.

### `turn/state/get`

Purpose:

- return deterministic lifecycle state for one turn using the active-turn
  registry plus the durable ledger projection
- return `state = "unknown"` rather than an error for a missing turn

Gate:

- `state.turn_state_get.v1`

Minimum params:

- `session_id`
- `turn_id`

Optional result fields from the M16 `context.lifecycle.v1` contract:

- `context`
  Full lifecycle envelope for the requested session at the time of the state
  read. During an active turn this must prefer any live prompt-time compacted
  context generation over a rebuild from durable user-facing rows.
- `context_state`
  Typed model-visible context state corresponding to `context.state`.

### `turn/start`

Purpose:

- start one user-visible turn on a session

Minimum params:

- `session_id`
- `turn_id`
- `input`

Behavior:

- server emits `turn/started`
- server may emit zero or more `message/delta`, `tool/*`, `task/updated`, `warning`
- server finishes with `turn/completed` or `turn/error`

### `review/start`

Purpose:

- start the server-owned product code-review workflow for a session
- let the backend choose and supervise native/CLI/MCP specialist agents
- expose progress through the existing `turn/*`, `task/*`, and `agent/*`
  notification surfaces

Gate:

- `review.start.v1`

Minimum params:

- `session_id`
- optional `turn_id`; if omitted, the server assigns one
- optional `profile_id`, scoped by the same profile/session rules as
  `turn/start`
- optional `target`; accepted shapes include
  `{ "type": "uncommitted_changes" }`, `{ "type": "base_branch",
  "base_branch": "main" }`, `{ "type": "commit", "commit": "..." }`, and
  `{ "type": "custom", "path": "..." }`
- optional `prompt` or `instructions`
- optional `delivery`; current implementation supports inline chat delivery

Result:

```json
{
  "accepted": true,
  "session_id": "local:demo",
  "turn_id": "019e...",
  "workflow": "code_review",
  "backend": "native",
  "agent_count": 4
}
```

Behavior:

- server emits `turn/started`
- server emits `task/updated` and `task/output/delta` for the review swarm
- server resolves native specialists from server configuration, not from a
  hard-coded AppUI client contract. Resolution order is:
  `OCTOS_REVIEW_NATIVE_SPECIALISTS_JSON`, profile
  `review.native_specialists`, built-in default template. Optional CLI/MCP
  specialists are added when their backend configuration is available, so
  `agent_count` is dynamic.
- server emits `agent/updated`, `agent/output/delta`, and
  `agent/artifact/updated` for specialist lifecycle, output, and artifacts
- server mirrors supervised background tasks launched by the legacy
  `TaskSupervisor` path, including `spawn_only`, `run_pipeline`, and child
  session tasks, into the same `agent/updated` surface. Clients should treat
  `agent/list`, `agent/status/read`, `agent/output/read`, and
  `agent/artifact/*` as the unified supervision surface instead of special
  casing review specialists.
- server may emit intermediate `message/delta` when one specialist finishes
- server emits a final joined assistant answer, then `turn/completed`
- `turn/interrupt` against the returned `turn_id` cancels the workflow and
  terminally reports `turn/error` with `code = "interrupted"`

### `turn/interrupt`

Purpose:

- stop a running turn deterministically

Minimum params:

- `session_id`
- `turn_id`

Behavior:

- if the turn is still running, server stops it and emits terminal state
- if already completed, behavior should be idempotent and explicit

Minimum result fields:

- `interrupted` (`bool`)
  `true` iff the server stopped the turn (or the turn had already been
  interrupted). `false` iff the interrupt was declined or the turn was
  already in a non-`interrupted` terminal state.

Optional result fields from accepted `UPCR-2026-008`:

- `reason` (`string`)
  Non-terminal diagnostic explanation when `interrupted` is `false`. String
  registry; initial value: `turn_id_mismatch`. Future values must be
  registered via UPCR.
- `terminal_state` (`string`)
  Set when interrupt was sent against a turn that had already reached a
  terminal state. String registry; values: `completed`, `errored`,
  `interrupted`. Future values must be registered via UPCR.
- `ack_timeout` (`bool`)
  Set to `true` only when the server captured the interrupt and emitted the
  wire-side terminal event but could not confirm client receipt within the
  ack window. The interrupt itself is captured (`interrupted` is `true`);
  only client-side receipt is uncertain. Omitted otherwise.

The canonical minimal wire shape is preserved: when no diagnostic fields
apply, the result is `{ "interrupted": <bool> }`.

### `approval/respond`

Purpose:

- answer an `approval/requested` event

Minimum params:

- `session_id`
- `approval_id`
- `decision`

Optional params from accepted `UPCR-2026-001`:

- `approval_scope`
  String registry with initial values `request`, `turn`, and `session`.
  Scope is advisory in v1alpha1 and must not silently create persistent allow
  rules.
- `client_note`
  Human-readable client note for audit/display. Servers must not require it.

### `user_question/respond`

Purpose:

- answer a `user_question/requested` event

Minimum params:

- `session_id`
- `question_id`
- `answers`
  Per-question answer list, one entry per question in the originating
  `user_question/requested` event, in question order. Each entry carries:
  - `selected_labels` — selected option label(s). Empty when the user supplied
    only free text. For a single-select question this is 0 or 1 entries; for a
    `multi_select` question it is 0..N. Labels must match the option labels from
    the request.
  - `free_text` — optional string from the free-text "Other" escape hatch.

Optional params (governed by accepted `UPCR-2026-023`):

- `client_note`
  Human-readable client note for audit/display. Servers must not require it.

### `diff/preview/get`

Purpose:

- fetch the canonical diff preview for one pending proposal

Minimum params:

- `session_id`
- `preview_id`

### `task/output/read`

Purpose:

- fetch recent task output or resume from a cursor/offset

Minimum params:

- `session_id`
- `task_id`
- optional `cursor`
- optional `limit_bytes`

Result fields (subset relevant to this spec; see `TaskOutputReadResult` for
the full struct):

- `source` — open snake_case enum identifying the read source. Today's
  runtime always emits `runtime_projection`; future sources (e.g. a
  disk-routed stdout/stderr stream) will introduce additional variants.
  Clients MUST NOT switch on this enum to decide whether the cursor is a
  stable byte-stream offset or an advisory snapshot offset; use
  `is_snapshot_projection` for that.
- `cursor` / `next_cursor` — byte offsets into the returned text window.
  When `is_snapshot_projection` is `true` the offsets are interpreted within
  the snapshot served by this response; when it is `false` the offsets are
  stable positions in the live byte stream the source exposes (see
  `is_snapshot_projection` below).
- `live_tail_supported: bool` — whether the read *source* has a live-tail
  mode (i.e. whether `task/output/delta` notifications can be expected for
  the same task). Today's `runtime_projection` source always reports
  `false`.
- `is_snapshot_projection: bool` — required, governed by accepted
  [UPCR-2026-006](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_006_TASK_OUTPUT_SNAPSHOT_PROJECTION.md).
  When `true`, the response was projected from a point-in-time snapshot of
  the task ledger; `cursor` / `next_cursor` are advisory across reads
  because a fresh `task/output/read` may project a different snapshot.
  When `false`, the response was sourced from a live byte-monotonic stream
  and `next_cursor` is a stable resume offset. Today's runtime always emits
  `is_snapshot_projection: true`.
- `limitations` — free-form list of `{ code, message }` entries describing
  source-specific caveats (e.g. `live_tail_unavailable`,
  `disk_output_unavailable`). Clients MUST NOT rely on specific `code`
  values as a contract for snapshot vs. live-tail semantics; that contract
  is carried by `is_snapshot_projection`.

### `task/list`

Capability-gated by accepted `UPCR-2026-005`. Servers expose it only when
`harness.task_control.v1` is advertised in `UiProtocolCapabilities`.

Purpose:

- enumerate tasks the runtime tracks for one session, with one entry per task
  including lifecycle/runtime state, optional child-session linkage, and output
  cursors. Primary consumer is the `/ps`-style task panel.

Minimum params:

- `session_id`
- optional `topic` — sub-topic suffix appended as `<session>#<topic>` for
  grouping; the server falls back to the bare session if omitted or empty

Result fields:

- `session_id` and optional `topic` echoed from the request
- `tasks` — array of task snapshots; each entry's `state` is the canonical
  `TaskRuntimeState` (the same enum as `task/updated`), so cancelled tasks
  surface as `cancelled` per accepted `UPCR-2026-004`

Errors follow the v1 taxonomy (see § 10):

- `runtime_unavailable` with `data.kind = "runtime_unavailable"` when the
  server has no task supervisor wired

A `task/list` request for an inactive or unknown session returns an empty
`tasks` array rather than `unknown_session`, matching how the
`SessionTaskQueryStore` snapshot already handles missing supervisors.

### `task/cancel`

Capability-gated by accepted `UPCR-2026-005`. Maps to
`TaskSupervisor::cancel(task_id)` (via `SessionTaskQueryStore::cancel_task`,
which dispatches to the owning supervisor) and preserves the cancel-race
guard from PR #709: once a task transitions to `cancelled`, later runtime
state transitions cannot overwrite it. Re-entrant cancel of an
already-terminal task surfaces as the `task_already_terminal` error rather
than a second success — the supervisor *state* is the idempotent invariant,
not the wire response.

Purpose:

- cancel a single tracked task and return its final wire state

Minimum params:

- `task_id`
- `session_id` — wire-optional but validated as required at handler time;
  omitting it returns `invalid_params` so clients cannot cross-cancel tasks
  across sessions
- optional `profile_id` — forwarded to the connection-profile validator

Result fields:

- `task_id` echoed from the request
- `status` — canonical `TaskRuntimeState` value; cancelled tasks surface as
  `cancelled` per accepted `UPCR-2026-004`

Errors follow the v1 taxonomy (see § 10):

- `unknown_task` when the supervisor has no task with that id, or the task is
  scoped to a different session than the request
- `invalid_params` with `data.kind = "task_already_terminal"` when applied to
  a task already in a terminal state (including a task that was already
  cancelled)
- `invalid_params` (with the existing `expected_profile_id` /
  `actual_profile_id` data fields) when the connection profile does not match
  the requested `session_id` or `profile_id`. The taxonomy reuses
  `validate_session_scope`, which the rest of the AppUi command surface
  already returns as `invalid_params` for profile mismatches

### `task/restart_from_node`

Capability-gated by accepted `UPCR-2026-005`. Maps to
`TaskSupervisor::relaunch(task_id, opts)` for operator-triggered relaunch of a
previously failed or terminal task, optionally beginning from a specific
pipeline node.

Purpose:

- relaunch a tracked task from a chosen node and return the supervisor-assigned
  successor task id

Minimum params:

- `task_id`
- optional `node_id` — pipeline node id to resume from; forwarded to
  `RelaunchOpts.from_node`
- `session_id` — wire-optional but validated as required at handler time,
  same rule as `task/cancel`
- optional `profile_id` — forwarded to the connection-profile validator

Result fields:

- `original_task_id` echoed from the request
- `new_task_id` — supervisor-assigned id of the relaunched successor
- optional `from_node` — echoed when the supervisor accepted the requested
  node

Errors follow the v1 taxonomy (see § 10):

- `unknown_task` when the supervisor has no task with that id, or the task is
  scoped to a different session than the request
- `invalid_params` with `data.kind = "task_still_active"` when applied to a
  non-terminal task
- `invalid_params` (with the same `expected_profile_id` / `actual_profile_id`
  data fields documented for `task/cancel`) when the connection profile does
  not match the requested `session_id` or `profile_id`

### Runtime, Auth, And LLM Profile Inspection

Accepted `UPCR-2026-017` adds the dashboard-equivalent inspection and
onboarding command surface below. These commands are additive and appear in
`UiProtocolCapabilities.supported_methods` only when implemented by the server.
Clients must use that method list to enable or disable slash commands.

`client_hello`:

- optional first request on any transport
- required for stdio clients that need feature-token negotiation equivalent to
  WebSocket `X-Octos-Ui-Features` / `ui_feature`
- params:

  ```json
  {
    "transport": "stdio",
    "client": { "name": "octos-tui" },
    "supported_features": [
      "approval.typed.v1",
      "session.workspace_cwd.v1",
      "context.lifecycle.v1"
    ]
  }
  ```

- result:

  ```json
  {
    "type": "server_hello",
    "transport": "stdio",
    "client_transport": "stdio",
    "client": { "name": "octos-tui" },
    "capabilities": {
      "version": {
        "protocol": "octos-ui/v1alpha1",
        "schema_version": 1,
        "jsonrpc": "2.0"
      },
      "capabilities_schema_version": 2,
      "supported_features": ["approval.typed.v1"],
      "supported_methods": ["session/open"],
      "supported_notifications": ["turn/started"]
    }
  }
  ```

- if `supported_features` is omitted or empty, the server preserves the
  connection's existing feature negotiation state
- if `supported_features` is present, the server rebuilds negotiated
  capabilities from those tokens and the current transport

`config/capabilities/list`:

- returns the same `UiProtocolCapabilities` schema advertised by
  `session/open`, but without requiring a session to be opened first
- servers that support local solo onboarding advertise
  `profile/local/create` in `supported_methods` and
  `profile.local_create.v1` in `supported_features`
- servers that support server-owned permission inspection advertise
  `permission.profile.v1`; servers that expose the extended runtime policy
  stamp advertise `runtime.policy_stamp.v1`
- unauthenticated stdio servers must omit `auth/me`, `content/list`, and
  `content/delete` from `supported_methods` and list them under
  `unsupported` with a reason; direct calls to those methods still return the
  typed `auth_unavailable` error with code `-32120`

`profile/local/create`:

- local-only no-OTP solo onboarding command
- request:

  ```json
  {
    "name": "Ada Lovelace",
    "username": "ada",
    "email": "ada@example.com"
  }
  ```

- result:

  ```json
  {
    "profile_id": "ada",
    "user_id": "ada",
    "name": "Ada Lovelace",
    "username": "ada",
    "email": "ada@example.com",
    "created": true,
    "runtime_mode": "solo"
  }
  ```

- the server creates or returns one local owner `User` plus matching
  `UserProfile`; `profile_id` is derived from the normalized username
- email is metadata only; this command MUST NOT call `auth/send_code`,
  `auth/verify`, SMTP, or any `AuthManager` OTP flow
- idempotent for the same normalized username, name, and email
- rejects username collisions with different local owner metadata using
  `invalid_params` and `data.kind = "profile_local_collision"`
- rejects invalid name, username, or email using `invalid_params` and
  `data.kind` values `profile_local_invalid_name`,
  `profile_local_invalid_username`, or `profile_local_invalid_email`
- rejects non-local/non-solo runtimes using `permission_denied` and
  `data.kind = "profile_local_unsupported"`

`session/status/read`:

- returns runtime status for the selected profile/session plus a runtime policy
  stamp containing provider/model/profile/tool/sandbox-visible state
- when `context.lifecycle.v1` is advertised, also returns compact context
  inspection fields:
  - `context_state`: active model-visible context generation, transcript hash,
    checkpoint/compaction IDs, token estimate, item count, and recovery state
  - `context`: the compact lifecycle status envelope containing the active
    `context_state` plus compaction count and the most recent compaction
    record
- `context.lifecycle.v1` is advertised by `config/capabilities/list` when the
  backend can expose backend-owned context state for AppUI turns. Clients should
  render this state from `session/status/read` and must not infer it from chat
  rows or local transcript heuristics.
- `session/open`, `session/hydrate`, legacy REST-bridge
  `session/status.get`, and `turn/state/get` also include `context` and
  `context_state` when `context.lifecycle.v1` is available.
  `session/status.get` returns the same `context_state` both at top level and
  under `status.context_state` so legacy status-object renderers can still read
  the value from the status body. AppUI JSON-RPC clients should use
  `session/status/read`; `session/status.get` is not an alias for that method.
- A connection with no feature header follows the first-server-slice discovery
  behavior from `UPCR-2026-007`: context snapshots and lifecycle notifications
  are available. Once a client sends any feature header, `context.lifecycle.v1`
  is opt-in and the server must not send context snapshots or lifecycle events
  unless that feature was negotiated.
- Context inspection must use the canonical profile/session store. A profiled
  coding session must not read the top-level daemon session store if its
  turns persist into a `ProfileRuntime` session manager.
- `runtime_policy_stamp` contains the server-effective values:

  ```json
  {
    "runtime_mode": "solo",
    "profile_id": "ada",
    "workspace_root": "/Users/ada/project",
    "approval_policy": "never",
    "sandbox_mode": "danger-full-access",
    "permission_profile": "danger_full_access",
    "filesystem_scope": "host",
    "network": "allowed",
    "tool_policy_id": "profile",
    "mcp_servers": [],
    "memory_scope": "profile-session"
  }
  ```

  Example `context` payload:

  ```json
  {
    "schema": "octos.context.lifecycle.v1",
    "state": {
      "session_id": "ada:local:tui#coding",
      "thread_id": null,
      "generation": 8,
      "transcript_hash": "sha256:...",
      "last_checkpoint_id": "ctxchk_000008",
      "last_compaction_id": "ctxcmp_000001",
      "token_estimate": 4231,
      "item_count": 17,
      "recovery_state": "exact"
    },
    "compaction": {
      "count": 1,
      "last": {
        "compaction_id": "ctxcmp_000001",
        "checkpoint_id": "ctxchk_000008",
        "status": "installed",
        "policy_id": "compact-context-v1",
        "trigger": "pre_turn",
        "input_generation": 7,
        "output_generation": 9,
        "input_transcript_hash": "sha256:...",
        "replacement_transcript_hash": "sha256:...",
        "installed_transcript_hash": "sha256:...",
        "input_item_count": 42,
        "retained_count": 16,
        "dropped_count": 26,
        "summary_item_id": "ctxitem_000043",
        "token_estimate_before": 8012,
        "token_estimate_after": 4231,
        "error": null
      }
    }
  }
  ```

`permission/profile/list`:

- request includes `session_id`
- returns `current` plus server-supported permission profiles
- local solo servers MAY include `danger_full_access`; tenant/cloud servers
  must omit it or reject attempts to select it

`permission/profile/set`:

- request includes `session_id` and partial `update`
- accepted `mode` values are `read_only`, `workspace_write`, and
  `danger_full_access`
- accepted `update.approval_policy` values are `on-request`, `on_request`,
  `ask`, and `never`; clients use `on-request` to clear a previous `never`
  selection and return to approval-gated behavior
- `danger_full_access` means `approval_policy=never`,
  `sandbox_mode=danger-full-access`, `filesystem_scope=host`, and
  `network=allowed`
- dangerous full-host access is rejected outside local solo mode using
  `permission_denied` and `data.kind = "permission_profile_disallowed"`

`auth/status`, `auth/send_code`, `auth/verify`, `auth/me`, `auth/logout`:

- expose the email OTP login flow used by the dashboard
- use structured errors for invalid OTP, expired OTP, and unauthenticated state
- unauthenticated stdio does not advertise the auth-bound `auth/me` method;
  callers that invoke it anyway receive `-32120` with
  `data.kind = "auth_unavailable"`

`profile/llm/catalog`:

- returns the dashboard provider catalog, including model family, model name,
  official provider routes, alternate provider routes such as AutoDL or
  WiseModel, and custom OpenAI-compatible route support

`profile/llm/upsert`:

- persists the selected family/model/route into dashboard-compatible profile
  JSON under `config.llm.primary`
- stores secret material only through `config.env_vars` keys; user-facing
  artifacts and captures must redact secret values
- when `set_primary: false` and the profile already has a primary model, the
  server appends or replaces the selection under `config.llm.fallbacks[]`.
  Replacements match by family, model, route id, and base URL. If the profile
  has no primary model yet, the server promotes the first upsert to primary so
  coding sessions always have an effective default model.

`profile/llm/list`, `profile/llm/select`, `profile/llm/delete`,
`profile/llm/test`, `profile/llm/fetch_models`:

- provide the model/provider management surface used by TUI onboarding and
  slash-command flows
- `profile/llm/test` must execute a minimal provider API probe using either
  the supplied raw `api_key` or the saved `route.api_key_env` value from the
  profile. It returns the same mutation-shaped provider state as
  `profile/llm/upsert`, but `applied` means “connection verified”, not
  “profile saved”. Failed probes return `applied: false` plus optional
  `message` and `error` fields; clients must clear in-flight test state and
  keep the provider editable/retryable.

`skill/action/list`:

- requires `session_id`; accepts optional `profile_id`, `surface`, and `tags[]`
- bootstraps the session's profile runtime and returns manifest actions loaded
  for that runtime
- returns `{ profile_id, session_id, count, actions }`
- each action includes `id`, `skill_id`, `label`, `execution`, optional
  `description`, `tags[]`, `surfaces[]`, `input_schema`,
  `ui_schema`, and `available`
- `skill_dir` is server-only and is never returned to AppUI clients
- when `surface` is present, actions with an empty `surfaces[]` remain
  eligible; actions with non-empty `surfaces[]` must include the requested
  surface
- every requested tag must be present in the action's `tags[]`
- actions bound to tools that are unavailable in the session runtime are not
  listed; clients must treat the list as server truth for the current session
- when a negotiated connection lacks `skill.action_jobs.v1`, actions declared
  with `execution: "background"` are omitted; synchronous actions remain
  available through `skill.actions.v1`

`skill/action/invoke`:

- requires `session_id` and `action_id`; accepts optional `profile_id` and
  `arguments`
- `action_id` may be either the manifest action id (`source.import`) or the
  skill-qualified id (`mofa-notebook-source/source.import`)
- the server resolves the session runtime and invokes only its loaded manifest
  binding; clients cannot override the
  backend tool name, input mode, or file-materialization mode at call time
- `single` input mode forwards the JSON object in `arguments` to the bound tool
- `file_each` input mode requires `arguments.paths[]` and invokes the bound
  tool once per materialized path, inserting each path into `file_argument`
  (default `path`)
- `file_materialization` is manifest-owned and defaults to `raw`:
  `raw` forwards each string unchanged; `workspace_relative` copies owned upload
  references into `<workspace>/uploads/` and passes workspace-relative paths
  including images; `turn_media` uses the existing chat-turn media behavior
  where non-images become workspace paths and images use the vision-readable
  upload path
- result shape is `{ action_id, ok, results }`; `file_each` also includes
  `materialized_paths`
- each `results[]` entry contains `success`, `output`, `file_modified`,
  `artifacts[]`, and `structured_metadata`; `file_modified` is an opaque
  session-workspace handle or `null`, and each artifact contains `handle`,
  `display_name`, `media_type`, and `size`
- `ws/...` artifact handles require the owning `session_id` when fetched;
  clients must not interpret the handle payload or accept raw absolute paths
- files missing from or outside the session workspace are omitted from
  `artifacts[]`
- when the manifest action declares `execution: "background"`, response shape
  is `{ action_id, ok, batch_id, jobs }`; the server appends persisted job
  snapshots and emits `skill/action/job/updated` for subsequent state changes
- background actions require `skill.action_jobs.v1`; direct invocation without
  that negotiated capability fails with `method_not_supported`

`skill/action/job/list`:

- requires `session_id`; accepts optional `profile_id`, `batch_id`, and
  `action_id`
- returns `{ profile_id, session_id, count, jobs }`
- each job is the latest persisted snapshot for that `job_id`
- job status is one of `queued`, `running`, `succeeded`, `failed`, or
  `abandoned`
- queued/running jobs from a previous server process are surfaced as
  `abandoned` after startup recovery; clients must not assume automatic resume
- job list/read, replay, and live notification delivery are scoped by both
  `profile_id` and `session_id`; equal bare session IDs in different profiles
  must not expose each other's jobs

`skill/action/job/read`:

- requires `session_id` and `job_id`; accepts optional `profile_id`
- returns `{ job }` with the latest persisted snapshot
- missing jobs return a typed AppUI error rather than an empty success payload

`skill/action/job/updated`:

- server notification emitted after every persisted background job snapshot
- payload is `{ profile_id, session_id, job }`
- the `job` object uses the same wire shape returned by
  `skill/action/job/list` and `skill/action/job/read`

`mcp/status/list` and `tool/status/list`:

- return server-owned MCP and tool state so clients do not inspect backend
  config, provider config, MCP config, tool registry, memory, or sandbox state
  directly

### Coding Tool Contract Inspection

Proposed `UPCR-2026-020` extends the existing runtime inspection methods for
Codex-compatible coding sessions. The tools described here are model-visible
backend tools, not AppUI client commands. TUI and web clients render the
contract and warnings; they do not invoke these tools directly.

Capability feature:

- `coding.tool_contract.v1`

Optional capability feature flags:

- `coding.patch_tool.v1`
- `coding.exec_session.v1`
- `coding.plan_tool.v1`
- `coding.user_input_tool.v1`
- `coding.subagent_aliases.v1`
- `coding.image_view.v1`
- `coding.dynamic_tool_search.v1`
- `coding.image_generation.v1`

`session/status/read`:

- when `coding.tool_contract.v1` is negotiated,
  `runtime_policy_stamp` includes the effective server-owned coding tool
  contract fields:

  ```json
  {
    "tool_contract_id": "codex-compatible-coding-v1",
    "tool_contract_version": "1",
    "model_toolset": "coding",
    "dynamic_tool_discovery": "enabled"
  }
  ```

`tool/status/list`:

- when `coding.tool_contract.v1` is negotiated, the result includes
  `coding_tool_contract`
- `coding_tool_contract.required_tools[]` entries describe the effective
  model-visible tool name, category, status, backend implementation or alias,
  capability flag, and policy state
- `coding_tool_contract.missing_required_tools[]` lists any required
  Codex-parity tools that the backend cannot expose for the effective profile

Initial Codex-parity tool names:

- P0: `apply_patch`, `exec_command`, `write_stdin`, `update_plan`,
  `request_user_input`, `spawn_agent`, `send_input`, `resume_agent`,
  `wait_agent`, and `close_agent`
- P1: `view_image`, `tool_search`, and `tool_suggest`
- P2: generic `image_generation`

Tool status values:

- `available`
- `aliased`
- `disabled_by_policy`
- `missing`
- `unimplemented`

Required security rules:

- tool contract resolution happens only inside the server-owned session runtime
  factory
- aliases are policy-equivalent to their backend tools
- disabled tools are not advertised to the model
- client UIs must not infer coding tool availability from local files
- WebSocket and stdio return the same tool contract payload

Errors use the existing AppUI taxonomy with these structured `data.kind`
values when applicable:

- `tool_contract_unavailable`
- `coding_tool_denied`
- `coding_tool_missing`
- `exec_session_unknown`

### M12 Phase D: Auxiliary RPC

M12 Phase D-1 added thirteen auxiliary JSON-RPC methods that migrated the
non-chat data plane from REST onto the same `/api/ui-protocol/ws` JSON-RPC
connection that already carries chat. See
`docs/adr/m12-phase-d-auxiliary-rest-to-ws.md` for migration rationale,
endpoint inventory, and Phase D-1 → D-5 plan.

Capability feature:

- `auxiliary.rest_to_ws.v1` (`UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1` in
  `crates/octos-core/src/ui_protocol.rs`)

Negotiation is **strict opt-in**: a client that does not negotiate the feature
receives `method_not_supported` (`-32004`) on every method below, even when no
feature header is sent. This is what makes Phase D-1 truly additive — clients
that have not been updated cannot trip into the new methods without explicit
negotiation. Phase D-5 retired the corresponding REST routes; clients that
have not migrated now receive `404` from the legacy URLs.

Common error envelope (all methods):

- `unknown_session` (`-32100`) with `data.session_id` — session-scoped methods
  when the addressed session is not in the server's session table
- `resource_not_found` (`-32170`) with `data.resource_type = "content"` and
  `data.identifier` — non-session methods (content/*) when the addressed row
  is missing
- `invalid_params` (`-32602`) — schema validation failure, including
  per-method caps and validation rules (see individual entries below)
- `runtime_not_ready` — REST 503 (gateway-proxied method on a standalone
  server)
- `auth_unavailable` (`-32120`) — content methods called without a usable
  identity (the dispatcher additionally closes the WS connection with
  `1008 auth_expired` so the client's auth-expired flow can clear the token)
- `method_not_supported` (`-32004`) — capability not negotiated
- `internal_error` (`-32603`) — REST 5xx other than 503; non-JSON REST body

The dispatcher also surfaces `data.rest_status` (REST status code) and an
optional `data.detail` field (REST handler's human-readable error text,
capped at 2 KiB) so panels can render REST-source error messages without a
second round trip.

Request/response Rust types live in `crates/octos-core/src/ui_protocol.rs`
(`SessionListParams`/`SessionListResult`, …, `ContentBulkDeleteParams`/
`ContentBulkDeleteResult`).

#### `session/list`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/sessions`
- Params type: `SessionListParams` (empty object).
- Result type: `SessionListResult` — `{ sessions: SessionInfo[] }`. The
  `sessions` field forwards the JSON body of the legacy REST handler
  verbatim (one `SessionInfo` per entry).
- Errors: collection endpoint; an unexpected 404 surfaces as
  `resource_not_found` with `data.resource_type = "session"` rather than
  `unknown_session`.

#### `session/snapshot`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: combined `GET /api/sessions/{id}/status` + `/files` + `/tasks`
  (single bootstrap round trip).
- Params type: `SessionSnapshotParams` — `{ session_id: string, topic?: string }`.
- Result type: `SessionSnapshotResult` — `{ status, files, tasks }`. Each
  field carries the JSON body of the corresponding legacy REST endpoint
  verbatim. The dispatcher fans the three calls out concurrently and
  surfaces the first error.
- Errors: `unknown_session` with `data.session_id` on REST 404.

#### `session/messages_page`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/sessions/{id}/messages`
- Params type: `SessionMessagesPageParams` —
  `{ session_id: string, limit?: number, offset?: number,
  since_seq?: number, topic?: string }`. `limit` defaults to
  `SESSION_MESSAGES_PAGE_DEFAULT_LIMIT` (100) and is clamped to
  `SESSION_MESSAGES_PAGE_MAX_LIMIT` (500); `offset` is clamped to
  `SESSION_MESSAGES_PAGE_MAX_OFFSET` (10 000). These clamps match the
  legacy REST handler.
- Result type: `SessionMessagesPageResult` —
  `{ messages, has_more: bool, next_offset: number }`. `messages` forwards
  the REST handler's `MessageInfo[]`. `has_more` is `true` when the
  returned page is exactly `limit` entries; `next_offset` is `offset + len`.
- Errors: `unknown_session` on REST 404; `runtime_not_ready` on REST 503
  (gateway-proxied method on standalone server). NOTE: the original REST
  contract returned an empty page silently for some 404 cases. The WS
  dispatcher mirrors the REST handler at `handlers.rs:767` / `handlers.rs:783`
  precisely so 404 → `unknown_session` and 503 → `runtime_not_ready`.

#### `session/status.get`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/sessions/{id}/status` (status-pill poller; separate
  from `session/snapshot` so periodic polling does not pay for files/tasks).
- Params type: `SessionStatusGetParams` — `{ session_id: string, topic?: string }`.
- Result type: `SessionStatusGetResult` —
  `{ status: { active, has_deferred_files, has_bg_tasks, ... },
  context_state?: UiContextState }`. The `context_state` field is folded in
  when `context.lifecycle.v1` is also negotiated for the connection (same
  shape as `session/status/read`).
- Errors: `unknown_session` on REST 404.

#### `session/files.list`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/sessions/{id}/files`
- Params type: `SessionFilesListParams` — `{ session_id: string }`.
- Result type: `SessionFilesListResult` — `{ files: SessionFileInfo[] }`.
- Errors: `unknown_session` on REST 404.

#### `session/tasks.list`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/sessions/{id}/tasks`
- Params type: `SessionTasksListParams` — `{ session_id: string, topic?: string }`.
- Result type: `SessionTasksListResult` — `{ tasks: BackgroundTaskInfo[] }`.
  Proxied from the gateway in multi-session deployments; empty in
  standalone mode.
- Errors: `unknown_session` on REST 404.

#### `session/workspace.get`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/sessions/{id}/workspace-contract`
- Params type: `SessionWorkspaceGetParams` — `{ session_id: string }`.
- Result type: `SessionWorkspaceGetResult` —
  `{ contracts: WorkspaceContractStatus[] }`.
- Errors: `unknown_session` on REST 404.

#### `session/title.set`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `PATCH /api/sessions/{id}/title`
- Params type: `SessionTitleSetParams` — `{ session_id: string, title: string }`.
  `title` is trimmed; an empty or whitespace-only title returns
  `invalid_params`. Length is capped at `SESSION_TITLE_SET_MAX_CHARS`
  (200 characters); over-length returns `invalid_params` (matches the
  legacy REST handler at `handlers.rs:1162`).
- Result type: `SessionTitleSetResult` — `{ session_id, title }`. The REST
  endpoint returned `204 No Content`; the WS shape lifts the resolved title
  into the response body so the SPA can roundtrip the rename without a
  follow-up read.
- Errors: `unknown_session` on REST 404; `invalid_params` on title
  validation failure.

#### `session/delete`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `DELETE /api/sessions/{id}`
- Params type: `SessionDeleteParams` — `{ session_id: string }`.
- Result type: `SessionDeleteResult` (empty object; the REST endpoint
  returned `204 No Content`).
- Errors: `unknown_session` on REST 404.

#### `system/status.get`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/status` (the agent/server status, distinct from
  `/api/auth/status` which stays REST).
- Params type: `SystemStatusGetParams` (empty object).
- Result type: `SystemStatusGetResult` — `{ status: StatusResponse }`.
  `status` carries the JSON body of the legacy REST handler
  (`handlers.rs:2592`).
- Errors: `internal_error` on JSON serialization failure (no addressable
  resource).

#### `content/list`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/my/content`
- Params type: `ContentListParams` — `{ filters?: object }`. `filters`
  mirrors the REST `ContentQuery` shape (`category`, `search`, `from`,
  `to`, `sort`, `limit`, `offset`, `session_id`). Empty / null filters
  fall back to the REST default (no filtering). Invalid filter JSON
  returns `invalid_params`.
- Result type: `ContentListResult` — `{ entries: ContentEntry[], total: number }`.
- Errors: `auth_unavailable` (`-32120`) with WS close code `1008 auth_expired`
  if the connection has no usable identity; `invalid_params` on filter
  parse failure; `resource_not_found` with `data.resource_type = "content"`
  on REST 404 (collection endpoint — id is empty).

#### `content/delete`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `DELETE /api/my/content/{id}`
- Params type: `ContentDeleteParams` — `{ id: string }`.
- Result type: `ContentDeleteResult` — `{ deleted: bool }`. `deleted` is
  `true` when the row was removed and `false` when the id was not in the
  catalog (the REST handler returned the same boolean inside
  `ActionResponse.ok`).
- Errors: `auth_unavailable` with WS close code `1008 auth_expired`;
  `resource_not_found` with `data.resource_type = "content"` and
  `data.identifier = <id>` on REST 404 (previously misclassified as
  `unknown_session`; corrected in codex review 2026-05-12).

#### `content/bulk_delete`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `POST /api/my/content/bulk-delete`
- Params type: `ContentBulkDeleteParams` — `{ ids: string[] }`. `ids` is
  capped at `CONTENT_BULK_DELETE_MAX_IDS` (256); over-cap requests are
  rejected with `invalid_params` before any catalog write-lock is taken
  (`data.max_ids`, `data.requested_ids`). This prevents a single
  oversized request from monopolizing the catalog write-lock and is a
  finer check than the coarser 1 MiB WS frame limit.
- Result type: `ContentBulkDeleteResult` — `{ deleted: number }`. `deleted`
  is the row count parsed back out of the REST handler's
  `ActionResponse.message` ("N item(s) deleted.").
- Errors: `auth_unavailable` with WS close code `1008 auth_expired`;
  `invalid_params` on the over-cap guard; `resource_not_found` with
  `data.resource_type = "content"` on REST 404 (collection endpoint).

#### `memory/overview`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/my/memory`
- Params type: `MemoryOverviewParams` — `{}` (accepts `params: {}` or
  `params: null`; the `params` member itself must be present — the
  shared frame parser rejects a request without one, codex #1621 r5).
- Result type: `MemoryOverviewResult` — `{ overview: MemoryOverviewResponse }`.
  `overview` carries the REST panel body whole (`memory_panel.rs`), plus
  RPC-layer truncation metadata: each document field is capped to a
  per-field JSON-ESCAPED byte budget (`long_term` 96 KiB, `today`
  48 KiB, each `recent[]` note 24 KiB) so the result fits one WS text
  frame; capped fields are clean UTF-8 prefixes DECLARED via
  `<field>_truncated` + `<field>_total_bytes` beside them (always
  present) — never spliced with an in-band marker.
- Errors: `auth_unavailable` (`-32120`) with WS close code
  `1008 auth_expired` if the connection has no usable identity;
  `resource_not_found` with `data.resource_type = "memory"` on REST 404
  (collection-style endpoint — id is empty).

#### `memory/entity`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/my/memory/entities/{name}`
- Params type: `MemoryEntityParams` — `{ name: string }` (the entity
  page stem, as returned in each overview entity summary).
- Result type: `MemoryEntityResult` — `{ name: string, content: string,
  content_truncated: bool, content_total_bytes: number }`. `content` is
  capped at a 384 KiB JSON-ESCAPED budget; when capped it is a clean
  UTF-8 prefix with the truth declared in the two metadata fields.
- Errors: `auth_unavailable` with WS close code `1008 auth_expired`;
  `resource_not_found` with `data.resource_type = "memory_entity"` and
  `data.identifier = <name>` on REST 404.

#### `cron/list`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `GET /api/my/cron`
- Params type: `CronListParams` — `{}` (accepts `params: {}` or
  `params: null`; the `params` member itself must be present, as above).
- Result type: `CronListResult` — `{ jobs: CronJobRow[], count: number,
  gateway_running: bool }`. Mirrors the REST body minus the redundant
  `ok` flag; `gateway_running` reports whether a spawned gateway child
  owns `cron.json` (toggles are refused while it does).
- Errors: `auth_unavailable` with WS close code `1008 auth_expired`;
  `resource_not_found` with `data.resource_type = "cron"` on REST 404
  (collection-style endpoint — id is empty).

#### `cron/toggle`

- Gate: `auxiliary.rest_to_ws.v1`
- Replaces: `PUT /api/my/cron/{job_id}/enabled`
- Params type: `CronToggleParams` — `{ job_id: string, enabled: bool }`.
- Result type: `CronToggleResult` — `{ job: CronJobRow }`, rendered
  exactly as a `cron/list` entry.
- Errors: `auth_unavailable` with WS close code `1008 auth_expired`.
  Refusals forward the REST error body's `reason` as `data.detail` so
  clients branch on typed fields, not message strings:
  `data.detail = "gateway_running"` with `data.rest_status = 409` when a
  spawned gateway owns the store, `resource_not_found` with
  `data.resource_type = "cron_job"`, `data.identifier = <job_id>`, and
  `data.detail = "job_not_found"` on a stale row.

## 8. Event Semantics

### `turn/started`

Marks the start of one client-visible turn. This creates the turn lifecycle boundary for the UI.

### `session/open`

Carries the opened-session notification and optional cursor baseline. The
notification payload shares the `SessionOpened` shape used by
`SessionOpenResult.opened`, including the required `capabilities` field
from accepted `UPCR-2026-007` (see § 7).

When `context.lifecycle.v1` is available for the connection, the notification
payload may also include `context` and `context_state` with the same semantics
as the `session/open` result.

Optional pane fields from accepted `UPCR-2026-002`:

- `panes`
  Contains optional `workspace`, `artifacts`, and `git` snapshots plus
  non-fatal limitations. Initial workspace entry kinds are string values:
  `directory`, `file`, `symlink`, and `other`.

Capability feature:

- `pane.snapshots.v1`
  Advertised through optional `supported_features` in
  `UiProtocolCapabilities`. Clients request it through `X-Octos-Ui-Features`
  using comma or space-separated feature tokens.

Optional workspace fields from accepted `UPCR-2026-003`:

- `workspace_root`
  The canonical server-approved root used to bind cwd-scoped coding tools for
  the session. It may be present even when `panes` is absent.

Capability feature:

- `session.workspace_cwd.v1`
  Advertised through optional `supported_features` in
  `UiProtocolCapabilities`. Clients request it through `X-Octos-Ui-Features`
  using comma or space-separated feature tokens. A `cwd` param sent without
  this feature must be rejected with `invalid_params` and `kind:
  feature_required`.

### `message/delta`

Carries incremental assistant output for the active turn. This is ephemeral until later committed history/event-ledger work makes the durable mapping explicit.

### `tool/started`, `tool/progress`, `tool/completed`

Carry live tool execution state, correlated by `tool_call_id`.

### `approval/requested`

Carries a blocking user-decision point. While this is unresolved, the turn remains paused at a deterministic boundary.

Required fallback fields:

- `session_id`
- `approval_id`
- `turn_id`
- `tool_name`
- `title`
- `body`

Optional typed fields from accepted `UPCR-2026-001`:

- `approval_kind`
  String registry with initial values `command`, `diff`, `filesystem`,
  `network`, and `sandbox_escalation`.
- `risk`
  Display/audit risk label.
- `typed_details`
  Tagged object whose `kind` should match `approval_kind` when both are present.
  Known detail groups are `command`, `sandbox`, `diff`, `filesystem`,
  `network`, and `sandbox_escalation`.
- `render_hints`
  Optional display hints such as labels, default decision, danger state, and
  monospace fields.

Compatibility rules:

- Generic `title` and `body` remain mandatory fallback text for v1alpha1.
- Unknown `approval_kind` or `typed_details.kind` values must fall back to
  generic rendering and remain actionable.
- Diff approvals reference existing `diff/preview/get` through
  `typed_details.diff.preview_id`; full diffs are not embedded in
  `approval/requested`.

Capability feature:

- `approval.typed.v1`
  Advertised through optional `supported_features` in `UiProtocolCapabilities`.
  The capability payload schema version is `2`.

### `user_question/requested`

Carries a structured multiple-choice question the agent is asking the user
mid-turn. While this is unresolved, the turn remains paused at a deterministic
boundary (the same blocking-tool boundary as `approval/requested`).

Required fallback fields:

- `session_id`
- `question_id`
- `turn_id`
- `title`
  Mandatory generic fallback text.
- `body`
  Mandatory generic fallback text.

Structured field (governed by accepted `UPCR-2026-023`):

- `questions`
  An array of 1–4 questions. Each question carries:
  - `header` — short label, ≤ 12 characters. An over-long header is
    **truncated** to the limit (char-boundary safe, ellipsis-marked) server-side
    rather than rejected, so a model that sends a descriptive header
    ("Favorite Color") still gets a rendered picker (live-soak hardening
    2026-06-04).
  - `question` — the question text.
  - `options` — an array of 2–4 options, each with `label` and `description`.
  - `multi_select` — `bool`; when `true` the user may select more than one
    option.
  - `allow_free_text` — `bool`; the server forces this `true` so a free-text
    "Other" escape hatch is always offered alongside the options.

Compatibility rules:

- Generic `title` and `body` remain mandatory fallback text for v1alpha1.
- Unknown fields must fall back to generic rendering and remain actionable: a
  client that does not understand `questions` renders `title`/`body` and the
  user can still answer (for example via free text).
- Clients that do not advertise the `user_question.v1` capability receive the
  agent tool's structured-metadata / generic-text fallback instead of a blocking
  question, so the turn never hard-blocks on a non-supporting client (the agent
  tool degrades exactly like the existing `request_user_input` codex tool).

Capability feature:

- `user_question.v1`
  Advertised through optional `supported_features` in `UiProtocolCapabilities`.
  Clients request it through `X-Octos-Ui-Features` using comma or
  space-separated feature tokens.

### `task/updated`

Carries task lifecycle and summary updates that are useful to clients even before the full unified ledger exists.

### `task/output/delta`

Carries live chunks of task output for a task/output viewer.

### `warning`

Carries non-terminal operator-visible warnings without collapsing them into generic errors.

### `turn/completed`

Marks the normal terminal event for a turn.

Optional fields from accepted `UPCR-2026-014` (M9-α-9):

- `tokens_in` / `tokens_out`
  Aggregated input / output token counts for the completed turn.
  Absent when the runtime did not surface usage to the wire.
- `session_result`
  Object carrying the final assistant row's durable identity:
  `{ "committed_seq": u64, "message_id": "<session>:<seq>:<ts_ns>",
  "client_message_id"?: string }`. Mirrors the SSE-only
  `session_result` frame so a WS client can stamp authoritative seq
  onto an optimistic bubble without an extra REST roundtrip. Absent
  when the turn ended without a final assistant row.

### `turn/started`

Optional fields from accepted `UPCR-2026-014` (M9-α-9):

- `topic`
  Sub-topic suffix that scopes the turn within a session (mirrors the
  `<session>#<topic>` shape carried on REST/SSE chat). Absent when the
  turn is not topic-scoped.

### `file/attached`

Per-turn file attachment event introduced by `UPCR-2026-014` (M9-α-9).
Mirrors the SSE `file:` frame the agent loop emits for tools that
declare `files_to_send`. Payload fields:

- `session_id`, `turn_id` — turn-scoping fields (required).
- `path` — filesystem path or URL the tool produced.
- `tool_call_id` — originating tool call (optional; omitted on
  background-result paths that don't run inside a tool execution).
- `mime` — MIME-type hint (optional; clients fall back to extension
  sniffing when absent).

### `visual/generating`, `visual/succeeded`, `visual/failed`

Typed visual-artifact lifecycle introduced by `UPCR-2026-024` (#1477,
voice rich output). A voice turn may append an in-band `[[VISUAL:...]]`
control marker; the backend strips it from every model-/client-facing
surface and instead drives the client off these three structured events,
so the client never scrapes the marker out of the assistant text. Ungated
and emitted on the same ledger-backed live path as `file/attached` (durable
append → replayed on reconnect).

The lifecycle is `generating → (succeeded | failed)` and is deliberately
decoupled from `file/attached`, which stays a pure artifact-delivery
signal: the client raises and clears the "generating" placeholder off
these events, NOT off `file/attached`. Payload fields:

- `visual/generating` — `session_id`, `turn_id` (required); `kind`
  (`html` | `illustrated` | `image` | `infographic`); optional `topic`.
- `visual/succeeded` — same fields as `generating`, plus `files`: the
  workspace-relative filenames of the delivered artifact(s) (the same paths
  carried on the accompanying `file/attached` event(s); omitted when empty).
  Emitted alongside `file/attached` on the success branch.
- `visual/failed` — `session_id`, `turn_id` (required); optional `topic`
  and `reason` (failure/timeout/cancel detail).

### `voice/exit`

Typed voice-exit signal introduced by `UPCR-2026-025`. A voice turn may
append an in-band `[[EXIT]]` control marker after a short spoken farewell
when the user expresses an end / goodbye / mute intent; the backend strips
it from every model-/client-facing surface (live `message/delta`, persisted
`response.content`, assistant carriers) and instead emits this structured
event, so the client never scrapes the marker out of the assistant text.
Ungated and emitted on the same ledger-backed live path as `file/attached`
(durable append → replayed on reconnect).

The client uses it to leave the `/voice` screen and return home, but gates
the actual navigation on its OWN reply-audio queue draining — so the spoken
farewell is heard before the screen changes. The event is the trigger; the
client owns the timing. Payload fields:

- `voice/exit` — `session_id`, `turn_id` (required); optional `topic`.

### `session/event`

Wrapper envelope introduced by `UPCR-2026-014` (M9-α-9) that bridges
legacy `/api/sessions/:id/events/stream` SSE frames onto the unified
WS surface during the α coexistence period. The legacy stream is
free-form; this wrapper preserves the original `type` (as `kind`) plus
the full frame body (as `payload`) so WS-only clients keep observing
every signal SSE consumers see while each event kind gradually lifts
onto a typed v1 envelope. Optional `topic` echoes the legacy frame's
topic for client-side scoping.

### `turn/error`

Marks the abnormal terminal event for a turn.

### `turn/spawn_complete`

Completion-as-new-envelope event for `spawn_only` background tool results.
Carries the late assistant `content` plus optional `media` attachments and
the originating user prompt's `client_message_id` under
`response_to_client_message_id`, so the client can render a NEW assistant
bubble under the correct user prompt without splice-merging into the existing
spawn-acknowledgement bubble.

Capability gate: `event.spawn_complete.v1`. When the capability is not
negotiated, the same durable row appears as `message/persisted` instead — the
ledger commit is unchanged, only the wire kind flips.

Required fields: `session_id`, `task_id`, `seq`, `message_id`, `source`,
`cursor`, `persisted_at`, `content`. Optional fields: `topic`, `turn_id`,
`thread_id`, `tool_call_id`, `response_to_client_message_id`, `media`.

Full field set and semantics are documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### `approval/auto_resolved`

Notification emitted when an incoming approval request was auto-resolved by
a previously recorded scope policy entry, instead of surfacing a fresh
`approval/requested` to the client.

Required fields: `session_id`, `approval_id`, `turn_id`, `tool_name`,
`scope`, `scope_match`, `decision`. Full field set documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### `approval/decided`

Durable record of an approval decision (manual or auto-resolved). Replayed
on reconnect so a client that connected after the decision renders the
approval card as Decided rather than as still pending. Carries identifiers
and decision metadata only; payload bodies (command strings, diffs) are
intentionally omitted for compliance / PII reasons.

Required fields: `session_id`, `approval_id`, `turn_id`, `decision`,
`decided_at`, `decided_by`, `auto_resolved`. Optional fields: `scope`,
`policy_id`, `client_note`. Full field set documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### `approval/cancelled`

Durable notification announcing that a previously pending approval was
cancelled by the server before any client could respond. The reason registry
is open: clients should treat unknown reasons as opaque strings and may add
new entries as future drains land (e.g. `session_closed`). Initial values:
`turn_interrupted`.

Required fields: `session_id`, `approval_id`, `turn_id`, `reason`. Full
field set documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### `progress/updated`

Standalone rich progress notification payload for kinds that do not fit
the first-wave `turn/*`, `tool/*`, or `task/*` envelopes — status pills,
retry-with-backoff banners, file-mutation notices, and token / cost
heartbeats.

Required fields: `session_id`, `metadata`. Optional field: `turn_id`. The
`metadata.kind` field is an open registry; initial values include `status`,
`retry_backoff`, `file_mutation`, and `token_cost_update`. Full field set,
typed sub-objects, and forward-compat `extra` map documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

`progress/updated` is a durable ledger event. Each frame is committed to
the per-session append-only ledger before the wire frame is enqueued, so
`session/open` replay returns the entries that ledger retention still
covers. Under per-connection backpressure a live-socket drop is reported
via `protocol/replay_lossy` (see § 9); the dropped `progress/updated`
frames are still recoverable from the ledger. Clients SHOULD treat the
latest received `progress/updated` of a given `metadata.kind` as
authoritative for UI rendering.

### `context/compaction_completed`

Notification that a server-owned context-manager compaction pass committed.
Carries the post-compaction `context_state` and a typed `compaction` record
with counts (`input_item_count`, `retained_count`, `dropped_count`), token
estimates before/after, and hash anchors for the input and replacement
transcripts.

Capability gate: `context.lifecycle.v1`.

Required fields: `session_id`, `context_state`, `compaction`. Full field
set, `UiContextState` shape, and `UiContextCompactionRecord` shape
documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### `context/compaction_started`

Notification that a server-owned context-manager compaction pass is about
to run. Emitted immediately before the pass with the PRE-compaction
`context_state` (its `token_estimate` is the "before" size), the `trigger`
label that the eventual `context/compaction_completed` record repeats, and
`threshold_tokens` (the context-window-derived limit that tripped the
pass) so clients can render an honest fullness percentage and an
in-progress state (spinner/progress bar).

Always followed by `context/compaction_completed` for the same pass.
Today's serve compaction is synchronous, so both notifications may arrive
in one delivery batch; clients MUST tolerate a zero-duration window.

Capability gate: `context.lifecycle.v1`.

Required fields: `session_id`, `context_state`, `trigger`,
`threshold_tokens`. Documented by
[UPCR-2026-026](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_026_COMPACTION_STARTED.md).

### `context/normalization_reported`

Notification that a prompt-normalization pass ran ahead of an LLM call.
Carries counts of repaired / dropped / synthetic / truncated items so AppUI
can render context-hygiene status without re-running normalization locally.

Capability gate: `context.lifecycle.v1`.

Required fields: `session_id`, `context_state`, `normalization`. Full field
set and the `UiContextNormalizationReport` shape documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### `protocol/replay_lossy`

Wire signal that one or more durable notifications were dropped due to
per-connection backpressure. The client should diverge from its cursor and
rehydrate via REST snapshot or `session/open` replay. Carries the last
durable cursor so the client can resume cleanly.

`protocol/replay_lossy` is itself a durable ledger event. The "lossy" name
describes the condition it reports (other durable notifications were
dropped from the live socket under per-connection backpressure), not its
own durability — the reference server appends the signal to the per-session
ledger via the same write-ahead path as every other durable notification
before attempting the wire send. Reconnecting and issuing a fresh
`session/open` replays both the `protocol/replay_lossy` signal and the
durable events that the per-connection ring dropped. See § 9 for reconnect
rules.

Required fields: `session_id`, `dropped_count`. Optional field:
`last_durable_cursor`. Full field set documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

## 9. Reconnect and Cursor Rules

The protocol needs explicit reconnect semantics. `UI Protocol v1` should treat these as part of the contract, not implementation detail.

Rules:

- client reconnects with the last durable `event_cursor` it has applied
- server replays ordered notifications after that cursor before switching the socket to live mode
- client must treat replay as authoritative over its previous ephemeral state
- message deltas that were never durably committed may be discarded during reconnect

The durable/ephemeral split should be explicit:

- durable: ordered replayable protocol events
- ephemeral: in-flight deltas not yet attached to a durable cursor boundary

When a connected client falls behind its per-connection backpressure ring,
the server emits `protocol/replay_lossy` (see § 8) carrying the last durable
cursor it is confident the client observed. The client must diverge from its
local cursor and rehydrate via `session/open` replay or REST snapshot. The
`protocol/replay_lossy` signal is itself committed to the durable ledger via
the same write-ahead path as every other durable notification — its name
reports a *backlog* condition (durable frames dropped from the live socket)
rather than its own durability, so reconnecting clients observe the signal
on replay alongside the surrounding durable events. The full wire contract
for the signal is documented by
[UPCR-2026-022](../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md).

### 9.1 Ledger Durability Contract (M9-FIX-05 / #643)

The reference server implementation (`octos-cli`) backs the cursor contract with a per-session **append-only on-disk ledger** in addition to the in-memory ring. Concretely:

- **Write-ahead.** Every durable notification is committed to disk before the wire frame is emitted. A server crash between disk-commit and wire-emit leaves the event recoverable; the client observes it on the next `session/open` replay.
- **Recovery on startup.** The ledger scans `<data_dir>/ui-protocol/<session_id>/ledger-*.log`, streams all retained log files in order, and hydrates the latest `retained_per_session` entries (default 4096) into RAM. Cursors persisted by clients across daemon restarts continue to resolve when the retained on-disk log range covers them.
- **Eviction.** Per-session ring buffer (default 4096 events), active-session cap (default 1024 sessions), idle TTL (default 1 hour). Evicted sessions remain durable on disk; only RAM is reclaimed.
- **Cursor validity across restart.** A pre-restart cursor resolves if the retained log range covers it; otherwise the server returns `CURSOR_OUT_OF_RANGE` and the client re-hydrates via REST snapshot.
- **Capability advertisement.** Servers MAY advertise `ledger.durable.v1: true|false` in `session/open` if they choose a Path B (RAM-only) configuration. Clients that receive `false` MUST treat any post-restart cursor as invalid.

See `docs/M9-LEDGER-DURABILITY-ADR.md` for the full decision record.

## 10. Error Model

The protocol needs a stable error taxonomy.

Minimum categories:

- `invalid_request`
- `unknown_session`
- `unknown_turn`
- `unknown_approval`
- `unknown_preview`
- `unknown_task`
- `cursor_out_of_range`
- `profile_unresolved`
- `runtime_unavailable`
- `permission_denied`
- `internal_error`

Rules:

- transport errors and runtime errors should not be conflated
- errors should include machine-readable `code` and human-readable `message`
- idempotent commands should say so explicitly in their success/error behavior
- a request that names a profile which is not present in server profile storage
  must fail with JSON-RPC `INVALID_PARAMS` and
  `data.kind = "profile_unresolved"`; it must not fabricate a runtime policy
  stamp for that profile or silently fall back to a default profile

## 11. Relationship to REST

The original migration-era split below has been **superseded by M12 Phase D**
(`docs/adr/m12-phase-d-auxiliary-rest-to-ws.md`, Accepted). The AppUI **data
plane** is now the WS UI Protocol v1 (`/api/ui-protocol/ws`); the 13 auxiliary
endpoints (`GET /api/sessions`, `/api/sessions/{id}/*`, `/api/status`,
`/api/my/content*`) were migrated to the `auxiliary.rest_to_ws.v1` methods
(§6 / §7) and retired from the REST router.

After M12, REST survives only for four planes the WS protocol cannot or should
not serve:

- **AUTH / bootstrap** — `/api/auth/*`, `/api/register`, `/api/my/profile`.
  The bearer token and `selected_profile` must be established over HTTP
  *before* a WS handshake can authenticate (the WS bridge reads its credential
  from the same store these calls populate). Keeping auth on a tiny, well-known
  prefix is also what lets the 401-reaper scope to `/api/auth/*` only.
- **BLOB / binary I/O** — `/api/upload`, `/api/site-files/upload`,
  `/api/files`, `/api/files/{path}`, `/api/files/list`,
  `/api/my/content/{id}/thumbnail`, `/api/my/content/{id}/body`,
  `/api/site-preview/*`, `/api/preview/{profile}/{session}/{slug}/*`,
  `/api/my/preview/sign`. Bodies exceed `MAX_TEXT_FRAME_BYTES` (1 MiB) and want
  HTTP range/streaming/`<img src>`/browser-native caching.
- **INFRA / OPS** — `/health`, `/metrics`, `/api/version`,
  `/api/internal/frps-auth`, `/api/events/harness`. Non-AppUI consumers (load
  balancers, Prometheus, the reverse proxy) require plain HTTP.
- **ADMIN control plane** — `/api/admin/*` and the operator/config endpoints
  consumed by the **admin dashboard** SPA (`dashboard/src/api.ts`), e.g.
  `/api/my/test-provider`, `/api/my/provider-models`, `/api/my/test-search`,
  `/api/my/model-limits`, `/api/my/soul`. These functionally overlap
  `profile/llm/*` but serve the REST-based admin SPA, which is intentionally
  outside the AppUI migration scope.

Note: `POST /api/tasks/{task_id}/cancel` is **not** an AppUI duplicate of the
WS `task/cancel` method — it backs the `octos-bus` API channel
(`crates/octos-bus/src/api_channel.rs`) and is the channel/CLI task-cancel path.

Original migration-era split (historical):

- REST: session lists, artifact/file lists, compatibility hydrate
- protocol: turn lifecycle, approvals, diff preview, task output, live
  progress, resumable event flow

## 12. M8 Gate

This spec should not freeze over known M8 runtime defects.

Before productionizing protocol features that depend on runtime truth, the following M8 areas need to be repaired:

- `ToolContext` propagation
- resume sanitizer correctness
- hard refusal for worktree-missing resume
- real M8.7 output/summary wiring
- profile/manifest authority
- concurrency classification for mutating/task-spawning tools

See [OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24.md](../docs/OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24.md).

## 13. Immediate Next Steps

1. Keep the shared Rust types in `octos-core` aligned with this doc.
2. Build the mock `octos-tui` scaffold against these draft types.
3. When M8 fixes land, start server-side `M9.1` transport wiring against the same shapes.

## 14. M9-γ Envelope

Status: **additive**, governed by accepted `UPCR-2026-014`. Capability-gated
behind `projection.envelope.v1`. Legacy `message/delta`, `message/persisted`,
`tool/*`, and `turn/completed` notifications continue to flow on connections
that do not negotiate this feature, until `M9-γ-3` deletes them.

ADR: [`docs/M9-GAMMA-SERVER-PROJECTION-ADR.md`](../docs/M9-GAMMA-SERVER-PROJECTION-ADR.md).

This section defines the canonical envelope shape that the M9-γ
deterministic projection consumes. The web client maintains an
append-only `Vec<Envelope>` indexed by `(thread_id, seq)` and the
projection function `(committed_log) → ChatViewModel` is pure,
deterministic, and side-effect free. Identity collapses to `seq`;
`client_message_id` lives ONLY on `user_message` envelopes (see
§ 14.2) for the optimistic `<GhostBubble>` overlay's match-and-unmount
path (the projection MUST NOT consult it).

**Turn shape** (locked by § 14.2): every chat turn begins with exactly
one `user_message` envelope (server-mirrored from the client's send),
followed by zero or more `assistant_delta` / `tool_*` / `file_attached`
/ `assistant_persisted` envelopes, terminated by exactly one
`turn_completed` envelope. A refresh-only projection reconstructs the
`UserView` for the chat exclusively from `user_message` envelopes —
`assistant_delta` and `assistant_persisted` alone are insufficient.

### 14.1 Envelope

Wire shape (JSON):

```json
{
  "thread_id": "thread-1",
  "seq": 18,
  "client_message_id": "01900000-0000-7000-8000-000000000001",
  "payload": { "type": "...", "data": { ... } },
  "session_id": "local:demo",
  "topic": "planning"
}
```

The `projection/envelope` notification's JSON-RPC `params` is the bare
`Envelope` fields (`thread_id`, `seq`, `client_message_id?`, `payload`)
FLATTENED with the routing keys `session_id` and optional `topic`. The
routing keys let a multi-session client (e.g. the TUI, which holds
several sessions on one connection) route each envelope to the correct
session and topic-scoped pane. The bare `Envelope` keys remain at the
top level, so a client that reads `thread_id`/`seq`/`payload` top-level
and ignores unknown keys decodes the frame unchanged — the routing
addition is backward-compatible. A decoder that receives an OLD frame
lacking `session_id`/`topic` defaults `session_id` to the empty key and
`topic` to absent, and falls back to its ambient connection context for
routing.

> History: an earlier revision (UPCR-2026-014 + codex #1336 round-2
> BLOCKER 4) stripped `session_id`/`topic` from the wire and kept them
> only on the durable ledger's on-disk record. That left a multi-session
> consumer with an unroutable empty `session_id`. The wire is now
> un-stripped (`feat(envelope-wire-routing)`); the **disk** record shape
> — a NESTED `{ session_id, topic, envelope }` object via the
> `EnvelopeNotification` derive — is UNCHANGED, so post-restart
> topic-scoped replay still routes (BLOCKER 4's actual invariant holds).

Field contract:

- `thread_id` (`string`, required) — Multi-turn cluster identity. All
  envelopes for one logical conversation share a `thread_id`.
- `seq` (`u64`, required) — Server-assigned strict total order WITHIN
  this `thread_id`. Strictly monotonic; gaps are an error and trigger
  rehydration. Identity for the projection.
- `client_message_id` (`string`, optional) — Populated ONLY on
  `user_message` envelopes (the optimistic `<GhostBubble>` overlay
  matches its server reflection here). Absent on every other variant
  (`assistant_delta`, `assistant_persisted`, `tool_*`, `file_attached`,
  `turn_completed`). The projection MUST NOT consult this field. A
  server emitting `client_message_id` on a non-`user_message` envelope
  is a wire contract violation.
- `payload` (object, required) — Sealed tagged union; see § 14.2.
- `session_id` (`string`, optional on the wire for backward-compat,
  always emitted by current servers) — The bare base session key for
  client-side routing. A multi-session client routes the envelope to
  this session; the projection itself does not consult it.
- `topic` (`string`, optional) — Topic suffix for topic-scoped routing.
  Omitted when the envelope is not topic-scoped.

Rust source: [`Envelope`](/Users/yuechen/home/octos/crates/octos-core/src/ui_protocol.rs:1)
in `octos-core::ui_protocol`. TS source: `Envelope` in
[`crates/octos-web/src/runtime/ui-protocol-types.ts`](/Users/yuechen/home/octos/crates/octos-web/src/runtime/ui-protocol-types.ts:1).

### 14.2 Payload (sealed tagged union)

Wire form: JSON with `"type"` discriminator and content under `"data"`
(matches Rust `serde(tag = "type", content = "data", rename_all = "snake_case")`).
Variants:

#### `user_message`
User-message turn root — server-mirrored from the client's send. Every
chat turn begins with exactly one `user_message` envelope. The
projection's `UserView` is reconstructed from these envelopes alone —
a refresh-only projection cannot recover user bubbles from
`assistant_delta` / `assistant_persisted`. The carrying envelope's
`client_message_id` is populated here (and ONLY here) so the
optimistic `<GhostBubble>` overlay can match its server reflection.

```json
{ "type": "user_message",
  "data": {
    "text": "<user prompt>",
    "files": [
      { "path": "/tmp/upload.png", "mime": "image/png", "size_bytes": 2048 }
    ]
  } }
```

`files` is an array of [`FileRef`](#145-fileref) entries; omitted on
the wire when empty.

#### `assistant_delta`
One streamed assistant text fragment. Multiple `assistant_delta`
envelopes for the same `thread_id` accumulate (concatenate by `seq`
order) into the live assistant bubble.

**Reconciliation rule** — `assistant_delta.text` events APPEND
(concatenate by ascending `seq`). When an `assistant_persisted`
envelope arrives for the same `thread_id`, its `text` field REPLACES
the accumulated streamed text (the persisted form is canonical). This
avoids double-rendering the final body when both delta and persisted
events project into the same view.

```json
{ "type": "assistant_delta", "data": { "text": "<fragment>" } }
```

#### `assistant_persisted`
Final assistant text persisted to the ledger after streaming completes.
Carries durable [`MessageMeta`](#143-messagemeta) so the projection can
finalize the bubble's identity and surface attachments. Per the
`assistant_delta` reconciliation rule above, `text` REPLACES the
concatenated streamed deltas for the same thread (canonical final
form).

```json
{ "type": "assistant_persisted",
  "data": {
    "text": "<full text>",
    "meta": {
      "message_id": "01900000-0000-7000-8000-000000000018",
      "persisted_at": "2026-05-09T18:30:01Z",
      "media": ["report.md"]
    }
  } }
```

#### `tool_start`
Tool invocation begun. The projection opens a tool-call card keyed on
`tool_call_id`. `arguments_preview` (optional) is a compact
`key: value` echo of the call arguments, server-bounded to 700 chars
(UTF-8-safe) — display fidelity for the card, not a replayable
argument record. Omitted for argument-less calls and for envelopes
persisted before the field existed.

```json
{ "type": "tool_start",
  "data": { "tool_call_id": "tc-1", "name": "shell",
            "arguments_preview": "command: \"cargo test\"" } }
```

#### `tool_progress`
Tool emitted a progress message. Idempotent per `(tool_call_id, seq)`;
the projection appends in `seq` order.

```json
{ "type": "tool_progress",
  "data": { "tool_call_id": "tc-1", "message": "running…" } }
```

#### `tool_end`
Tool invocation finished. `error` is set iff `status === "error"`;
omitted on the wire when null. `reason` is an optional human-readable
detail field, primarily populated for `skipped` and `aborted` outcomes
(see below); omitted on the wire when null. `output_preview` (optional)
carries the first lines of the tool result, server-bounded to
2048 chars (UTF-8-safe) — the result excerpt under the tool card; the
`error` field is bounded the same way. `duration_ms` (optional) is the
call's wall-clock duration. Both are omitted for envelopes persisted
before the fields existed.

```json
{ "type": "tool_end",
  "data": { "tool_call_id": "tc-1", "status": "complete",
            "output_preview": "test result: ok. 815 passed",
            "duration_ms": 4321 } }
```

```json
{ "type": "tool_end",
  "data": { "tool_call_id": "tc-2", "status": "error", "error": "…" } }
```

```json
{ "type": "tool_end",
  "data": { "tool_call_id": "tc-3", "status": "skipped",
            "reason": "deadline elapsed before tool started" } }
```

```json
{ "type": "tool_end",
  "data": { "tool_call_id": "tc-4", "status": "aborted",
            "reason": "user issued turn/interrupt" } }
```

`status` is a closed snake_case enum:

- `complete` — tool ran to natural completion.
- `error` — tool surfaced a failure (`error` carries the message).
- `skipped` — tool was intentionally not run (deadline-skip,
  pre-condition unmet). `reason` explains why.
- `aborted` — tool execution was interrupted by an external signal
  (user `turn/interrupt`, system cancellation). `reason` carries
  detail.

Future values require a follow-up UPCR.

#### `file_attached`
File attached to the current thread (e.g. `.md` report from
`deep_search` or `.mp3` from `fm_tts`). The projection adds the
attachment to the most-recent assistant bubble in `thread_id`.

```json
{ "type": "file_attached",
  "data": { "path": "/tmp/report.md",
            "mime": "text/markdown",
            "size_bytes": 4096 } }
```

#### `turn_completed`
**Hard barrier** — terminal payload for a turn within `thread_id`. Per
the M9-γ ADR and § 14.6 below, any envelope arriving on the same
`thread_id` AFTER this one is DROPPED by the projection (and counted
in `octos_projection_post_completion_drop_total`). Threads are NOT
reused — a new turn must use a NEW `thread_id`. Carries
[`EnvelopeTokenUsage`](#144-envelopetokenusage); zero-valued fields are
omitted on the wire.

```json
{ "type": "turn_completed",
  "data": { "token_usage": { "input_tokens": 100, "output_tokens": 250 } } }
```

### 14.3 `MessageMeta`

```json
{
  "message_id": "01900000-0000-7000-8000-000000000018",
  "persisted_at": "2026-05-09T18:30:01Z",
  "media": ["report.md"]
}
```

- `message_id` (`string`, required) — Server-assigned UUID of the
  durable row. Stable across replays. Mirrors
  `MessagePersistedEvent.message_id`. **Note**: `message_id` is retained
  here for audit/render display only; the projection uses `seq` as the
  sole identity key (see § 5.1).
- `persisted_at` (RFC 3339, required) — Wall-clock commit time.
- `media` (`string[]`, optional) — File attachments persisted with the
  message. Empty for assistant rows that carry only text. Omitted on
  the wire when empty.

### 14.4 `EnvelopeTokenUsage`

```json
{ "input_tokens": 100, "output_tokens": 250 }
```

Open object — all five fields default to zero and are omitted on the
wire when zero (Rust `serde(skip_serializing_if = "is_zero_u64")`):

- `input_tokens` (`u64`)
- `output_tokens` (`u64`)
- `reasoning_tokens` (`u64`)
- `cache_read_tokens` (`u64`)
- `cache_write_tokens` (`u64`)

Future fields require a follow-up UPCR.

### 14.5 `FileRef`

```json
{ "path": "/tmp/upload.png", "mime": "image/png", "size_bytes": 2048 }
```

Wire-form file reference carried on `user_message` envelopes (and
reused as the canonical attachment shape elsewhere — `file_attached`
embeds the same triple inline). All three fields are required:

- `path` (`string`) — Absolute path the server resolved for the file.
- `mime` (`string`) — IANA media type (e.g. `image/png`,
  `text/markdown`).
- `size_bytes` (`u64`) — Byte size at upload/persist time.

### 14.6 Hard barrier semantics

Per the M9-γ ADR and the `Envelope` Rust doc-comment, the server MUST
emit at most one `turn_completed` envelope per `(thread_id, turn)`.
After that envelope, the projection enforces the barrier with a single
deterministic rule:

> After `turn_completed` for `thread_id` T, any subsequent envelope
> with the same `thread_id` is **DROPPED** by the projection. The
> projection records the drop in the
> `octos_projection_post_completion_drop_total` metric. Threads are
> **NOT reused** — a new turn MUST use a NEW `thread_id`.

This is the canonical wire-level enforcement of the "phantom bubble"
elimination that motivated M9-γ. The drop is silent at the projection
layer (the metric is the operational signal); clients do NOT
rehydrate, restart, or treat the situation as a desync. The same
behaviour is implemented by the M9-γ-2 projection
([`octos-web` PR #93](https://github.com/octos-org/octos-web/pull/93)).

A server that needs to emit a follow-up assistant or tool event
belonging to a logically separate turn MUST mint a new `thread_id` for
that turn — the projection treats the new `thread_id` as a brand-new
chat thread and projects it independently.

### 14.7 Capability negotiation

Clients request `projection.envelope.v1` via the `X-Octos-Ui-Features`
header at `session/open` time. Servers advertise it through
`UiProtocolCapabilities.supported_features` (UPCR-2026-007) when they
emit canonical envelopes; pre-existing connections (TUI, octos-app
legacy) continue to receive only the legacy notification surface they
negotiated.

The capability schema version remains `2`; this is an additive feature
flag and does not bump the schema version.

## 15. Wave4-A — Adaptive Router + Queue Surface

The router/queue notifications and commands ship without a feature
flag — they are additive on the existing capabilities envelope. Clients
that don't recognize the methods drop them at the JSON-RPC parser. The
schema version remains `2`.

### 15.1 `router/status` (notification)

Adaptive routing snapshot pushed adjacent to `turn/started` and
`turn/completed`. No-op on connections whose session profile has no
`AdaptiveRouter` attached (single-provider config or
`adaptive_routing.enabled = false`).

```json
{
  "jsonrpc": "2.0",
  "method": "router/status",
  "params": {
    "kind": "router_status",
    "session_id": "local:demo",
    "provider_name": "zai/glm-5-turbo",
    "mode": "lane",
    "qos_ranking": true,
    "lane_scores": { "ollama/llama3.2": 0.62, "zai/glm-5-turbo": 0.21 },
    "circuit_breakers": { "ollama/llama3.2": "closed", "zai/glm-5-turbo": "closed" }
  }
}
```

`lane_scores` keys are deterministic (`BTreeMap` lex-sorted) so a client
that diffs successive snapshots gets stable key order. `mode` is the
lowercase string rendering of `AdaptiveMode` (`off` | `hedge` | `lane`).
`circuit_breakers` values are `"closed"` / `"open"` / `"half_open"` (the
last is reserved for a future tri-state breaker).

### 15.2 `router/failover` (notification)

Adaptive router crossed lanes. Emitted as durable so a reconnecting
client can catch up.

```json
{
  "jsonrpc": "2.0",
  "method": "router/failover",
  "params": {
    "kind": "router_failover",
    "session_id": "local:demo",
    "from_provider": "zai/glm-5-turbo",
    "to_provider": "ollama/llama3.2",
    "reason": "chat_error: 429 rate limited",
    "elapsed_ms": 12345
  }
}
```

`reason` is free-text from `AdaptiveRouter`. `elapsed_ms` is the wall
time from initial provider attempt to failover decision.

### 15.3 `queue/state` (notification — client-emitted today)

Pending-queue snapshot. The queue is client-side (`octos-web`
`runtime/ui-protocol-send.ts`); the server never emits this variant.
The wire shape is defined here so a future server-side queue (or a TUI
client) can publish into the same DOM event channel:

```json
{
  "jsonrpc": "2.0",
  "method": "queue/state",
  "params": {
    "kind": "queue_state",
    "session_id": "local:demo",
    "pending_count": 3,
    "head_client_message_id": "cmid-12345"
  }
}
```

`head_client_message_id` is omitted when the queue is empty (the
in-flight turn has landed).

### 15.4 `router/set_mode` (RPC request)

Runtime mode toggle. Mode change is session-scoped — it persists for
the lifetime of the `AdaptiveRouter` (process lifetime today), not
across restarts.

```json
{
  "jsonrpc": "2.0",
  "id": "req-set-mode",
  "method": "router/set_mode",
  "params": {
    "session_id": "local:demo",
    "mode": "hedge"
  }
}
```

Response (success):

```json
{ "jsonrpc": "2.0", "id": "req-set-mode", "result": { "mode": "hedge" } }
```

Errors:

- `INVALID_PARAMS` with no `data` — unknown mode string. The valid set
  is `off` / `hedge` / `lane`.
- `INVALID_PARAMS` with `data: { "kind": "runtime_unavailable" }` —
  this session's profile has no `AdaptiveRouter` attached.

### 15.5 `router/get_metrics` (RPC request)

On-demand snapshot mirroring `router/status` (same payload shape minus
the `session_id` echo). Lets a client poll without subscribing to the
push channel.

```json
{
  "jsonrpc": "2.0",
  "id": "req-get-metrics",
  "method": "router/get_metrics",
  "params": { "session_id": "local:demo" }
}
```

Response:

```json
{
  "jsonrpc": "2.0",
  "id": "req-get-metrics",
  "result": {
    "provider_name": "zai/glm-5-turbo",
    "mode": "lane",
    "qos_ranking": true,
    "lane_scores": { "zai/glm-5-turbo": 0.21 },
    "circuit_breakers": { "zai/glm-5-turbo": "closed" }
  }
}
```

Error shape identical to `router/set_mode` (`runtime_unavailable` data
tag when no router is attached).

### 15.6 Behavioral guarantees

- `router/status` emitted at `turn/started` and `turn/completed`. Never
  in the middle of a turn (use `router/get_metrics` to poll).
- `router/failover` published per-attempt — emitting BEFORE the retry,
  so a transition is observable even when the retry itself fails.
- The router's failover broadcast channel is **non-blocking**: slow
  subscribers observe `RecvError::Lagged` and skip; the router NEVER
  stalls on a stuck client.
- `adaptive_routing.enabled = false` (or absence of the block) means
  no `AdaptiveRouter` is built — `router/*` methods return
  `runtime_unavailable`. This was a config-correctness fix in Wave4-A
  (the previous behavior was silent default-ON).

## 16. M15 Agent, Goal, And Loop Autonomy Notifications

These notifications are capability-related to `coding.autonomy.v1` and
the optional `coding.agent_control.v1`, `coding.goal_runtime.v1`, and
`coding.loop_runtime.v1` groups. They are typed in
`crates/octos-core/src/ui_protocol.rs` and preserve compatibility with
the raw M15 AppUI fixture payloads.

Agent notifications:

- `agent/updated`: params are `{ "session_id": SessionKey, "agent": Agent }`.
  The backend sends this for native review specialists, CLI/MCP specialists,
  and mirrored `TaskSupervisor` background work. Mirrored task agents use a
  stable `agent_id` derived from the child session when available and expose
  `backend_kind` as either `spawn_child_session` or `task_supervisor:<tool>`.
- `agent/output/delta`: params are `{ "session_id": SessionKey,
  "agent_id": string, "cursor": { "offset": number }, "text": string }`.
- `agent/artifact/updated`: params are `{ "session_id": SessionKey,
  "agent_id": string, "artifacts": AgentArtifact[] }`.

Whenever an `agent/updated` transition enters a terminal state
(`completed`, `failed`, or `interrupted`), the backend queues a master
continuation through the same scatter-join scheduler. Repeating the same
terminal state must not queue duplicate continuations.

Goal notifications:

- `session/goal/updated`: params are `{ "session_id": SessionKey,
  "profile_id"?: string, "goal": Goal, "transition_actor": string }`.
- `session/goal/cleared`: params are `{ "session_id": SessionKey,
  "profile_id"?: string, "cleared": boolean, "goal": null,
  "transition_actor": string }`.

Loop notifications:

- `loop/updated`: params are `{ "session_id": SessionKey,
  "profile_id"?: string, "loop_id"?: string, "loop": Loop,
  "ok"?: boolean, "status"?: string, "deleted"?: boolean }`.
- `loop/fired`: params are `{ "session_id": SessionKey,
  "profile_id"?: string, "loop_id": string, "loop"?: Loop,
  "fire"?: LoopFire, "ok"?: boolean, "status"?: string }`.
- `loop/completed`: params are `{ "session_id": SessionKey,
  "profile_id"?: string, "loop_id": string, "loop"?: Loop,
  "status"?: string, "completed_at_ms"?: number, "result"?: object,
  "error"?: string }`.

`Agent`, `Goal`, and `Loop` shapes match UPCR-2026-021. String status
fields are open registries; clients must preserve unknown values. The
`LoopFire` object mirrors the `loop/fire_now` result object (`queued`,
optional `duplicate`, `continuation_id`, `dedupe_key`, `reason`,
`priority`, and `message`).
