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

Why:

- request/response fits turn control and approval response
- notifications fit live streaming and task/progress updates
- one long-lived socket is a better fit than stitching together `/api/chat`, `/api/ws`, and SSE

REST remains useful for:

- initial session lists
- artifact/file hydrate
- compatibility during migration

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

## 5. Identity Model

These ids need to be stable and client-visible:

- `session_id`
  Uses Octos session identity. For now this can map to existing `SessionKey`.
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

The logical command/event names are:

Commands:

- `session/open`
- `turn/start`
- `turn/interrupt`
- `approval/respond`
- `diff/preview/get`
- `task/output/read`
- `task/list` (capability-gated, accepted `UPCR-2026-005`)
- `task/cancel` (capability-gated, accepted `UPCR-2026-005`)
- `task/restart_from_node` (capability-gated, accepted `UPCR-2026-005`)

Notifications:

- `turn/started`
- `turn/completed`
- `turn/error`
- `message/delta`
- `tool/started`
- `tool/progress`
- `tool/completed`
- `approval/requested`
- `task/updated`
- `task/output/delta`
- `warning`

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

## 8. Event Semantics

### `turn/started`

Marks the start of one client-visible turn. This creates the turn lifecycle boundary for the UI.

### `session/open`

Carries the opened-session notification and optional cursor baseline. The
notification payload shares the `SessionOpened` shape used by
`SessionOpenResult.opened`, including the required `capabilities` field
from accepted `UPCR-2026-007` (see § 7).

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

### `task/updated`

Carries task lifecycle and summary updates that are useful to clients even before the full unified ledger exists.

### `task/output/delta`

Carries live chunks of task output for a task/output viewer.

### `warning`

Carries non-terminal operator-visible warnings without collapsing them into generic errors.

### `turn/completed`

Marks the normal terminal event for a turn.

### `turn/error`

Marks the abnormal terminal event for a turn.

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
- `runtime_unavailable`
- `permission_denied`
- `internal_error`

Rules:

- transport errors and runtime errors should not be conflated
- errors should include machine-readable `code` and human-readable `message`
- idempotent commands should say so explicitly in their success/error behavior

## 11. Relationship to REST

During migration:

- REST remains valid for snapshot hydrate
- the protocol becomes the interactive source of truth

Suggested split:

- REST:
  - session lists
  - artifact/file lists
  - compatibility hydrate
- protocol:
  - turn lifecycle
  - approvals
  - diff preview
  - task output
  - live progress
  - resumable event flow

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
  "payload": { "type": "...", "data": { ... } }
}
```

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
`tool_call_id`.

```json
{ "type": "tool_start",
  "data": { "tool_call_id": "tc-1", "name": "shell" } }
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
(see below); omitted on the wire when null.

```json
{ "type": "tool_end",
  "data": { "tool_call_id": "tc-1", "status": "complete" } }
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
