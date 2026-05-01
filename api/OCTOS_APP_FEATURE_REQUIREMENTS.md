# Octos App Feature Requirements

Status: proposed test contract.
Owner: octos-app.
Applies to: native Makepad desktop client, transport, store, renderer, coding
workspace, content/studio surfaces, and AppUI integration.

## Purpose

This document defines the feature requirements that `octos-app` must satisfy to
serve as the production native client for Octos. It is the desktop-client
counterpart to the server and TUI requirements documents in this directory.

The app must provide a richer visual and multi-pane experience than the TUI
while preserving the same AppUI contract. The app must not depend on private
server behavior, raw logs, or one-off REST shortcuts for interactive runtime
truth.

## Product Principles

- `octos-app` is an AppUI client first. Interactive runtime state comes from
  shared `octos-core` AppUI/UI Protocol types.
- REST is allowed for cold snapshots, file bytes, content libraries, and legacy
  compatibility. It must not replace AppUI for live turns, approvals, task
  lifecycle, diff previews, or task output.
- The store is the source of UI truth inside the app. Transport events fold into
  reducer state before widgets render.
- The app must degrade gracefully when capabilities are absent and must ignore
  unknown future capabilities and enum values where the shared protocol permits.
- UI decisions must support long-running coding sessions: many turns, many
  approvals, many tool calls, reconnects, and background tasks.
- Secrets and auth tokens must never appear in rendered UI, logs intended for
  users, crash summaries, or copied diagnostics.

## Requirement Matrix

| ID | Requirement | Priority | Acceptance Criteria | Verification |
|---|---|---:|---|---|
| APP-001 | Native app shell | P0 | App launches to a usable shell with chat, navigation, connection state, profile/session context, and no placeholder-only landing page. | app smoke test |
| APP-002 | AppUI transport | P0 | WebSocket JSON-RPC transport sends and receives shared AppUI commands/results/notifications using `octos-core` types. | transport contract tests |
| APP-003 | Capability negotiation | P0 | App requests and parses supported AppUI features including typed approvals, pane snapshots, workspace cwd, task output, and future task-control features when available. Unknown features are ignored safely. | transport unit tests |
| APP-004 | Session open | P0 | App opens sessions with session id, profile id, auth token, optional cursor, and workspace cwd when configured. Invalid open errors surface clearly. | live/protocol smoke |
| APP-005 | Workspace cwd | P0 | App passes server-side workspace cwd for coding sessions and displays effective workspace root or typed rejection. | store/integration test |
| APP-006 | Reconnect and replay | P0 | App persists cursor per session, reconnects with `session/open { after }`, applies replay in order, handles cursor invalid/lossy replay by rehydrating snapshot state. | transport fault tests |
| APP-007 | Store reducer authority | P0 | All protocol notifications fold through `octos-app-store`; widgets do not mutate protocol truth directly. | reducer tests and code review |
| APP-008 | Chat stream | P0 | `message/delta` renders live assistant text; terminal turn events commit/clear live state without duplicating content after reconnect. | store tests |
| APP-009 | User prompt ordering | P0 | User prompts appear in chat before assistant/tool output they triggered. Queued prompts remain visually distinct from sent turns. | reducer/UI snapshot |
| APP-010 | Turn control | P0 | App can send `turn/start` and `turn/interrupt`; interruption status distinguishes no-active-turn, requested, interrupted, and failed. | transport/store tests |
| APP-011 | Approval cards | P0 | Typed approval requests render as command, diff, filesystem, network, sandbox escalation, or generic fallback cards with risk and clear decision controls. | UI snapshot tests |
| APP-012 | Approval response | P0 | Approve once, approve scoped/session, and deny send `approval/respond` with correct id/session/scope. Retry/stale errors render as decided/expired, not as pending. | contract tests |
| APP-013 | Approval lifecycle | P0 | `approval/decided`, `approval/auto_resolved`, and `approval/cancelled` update queue/history and remove actionable pending approvals. | store tests |
| APP-014 | Diff preview | P0 | Diff approvals and file mutation previews can fetch and render `diff/preview/get` results with file list, hunks, additions/removals, renames, truncation/limitations when present. | UI/render tests |
| APP-015 | Coding workspace | P0 | Coding screen shows approval queue/history, task dock, preview pane, command/network/filesystem/diff/output detail, and connection/task status. | app snapshot |
| APP-016 | Task dock | P0 | Task list renders task id/title/state/progress/output availability. Cancelled/completed/failed/running states are visually distinct. | store/render tests |
| APP-017 | Task output read | P0 | App can request `task/output/read`, preserve output cursor per task, append `task/output/delta`, cap buffers, and avoid duplicate output after reconnect. | store + transport tests |
| APP-018 | Task control readiness | P1 | When server advertises task-control support, app exposes list/cancel/restart affordances through AppUI. When absent, controls are hidden or disabled with explanation. | capability tests |
| APP-019 | Tool cards | P0 | Tool lifecycle renders command/tool cards with name, arguments or command, status, duration, output preview, error/success state, and expandable details. | UI snapshot |
| APP-020 | Progress and status | P0 | Progress events update visible working state, file mutation status, retry/cost summaries, and warnings without flooding chat with low-value event rows. | reducer tests |
| APP-021 | Pane snapshots | P1 | App hydrates workspace, artifacts, and git panes from `session/open.panes` when supported and falls back to REST/snapshot sources when absent. | protocol fixture |
| APP-022 | Content viewer | P1 | App opens generated content and artifacts using appropriate viewers for image album, markdown, audio, video, generic files, and external OS open fallback. | UI tests |
| APP-023 | Studio surfaces | P2 | Slides/sites/research producer screens use the same transport/store patterns and declare their server dependencies explicitly. | workstream tests |
| APP-024 | Auth and profile | P0 | App stores auth safely, sends bearer/profile headers, supports profile switching, handles 401/403 by entering auth-required state, and redacts secrets. | auth tests |
| APP-025 | Connection state | P0 | UI shows connected, connecting, reconnecting, offline, auth failed, and protocol error states. Connection state is folded into store and visible globally. | store/UI tests |
| APP-026 | Error handling | P0 | Typed AppUI errors map to user-actionable messages. Unknown errors preserve code/kind without panics. | error fixture tests |
| APP-027 | Replay lossy | P0 | `protocol/replay_lossy` creates a visible rehydrate warning and triggers or offers snapshot rehydrate. | reducer test |
| APP-028 | Forward compatibility | P0 | Unknown capabilities, unknown approval kinds, future task states, and additive payload fields do not crash decode or rendering. | fuzz/serde tests |
| APP-029 | Markdown rendering | P0 | Chat and content markdown render headings, lists, code, tables, links, images where supported, and CJK text without unsafe HTML/script execution. | render tests |
| APP-030 | Layout responsiveness | P0 | Main shell, chat, coding workspace, approval cards, task dock, and preview panes adapt to desktop window sizes without overlapping controls. | pixel/widget snapshots |
| APP-031 | Accessibility basics | P1 | Keyboard navigation covers core actions, focus state is visible, clickable controls have text labels/tooltips, and color is not the only state indicator. | manual + widget tests |
| APP-032 | Performance | P1 | 50 approvals, 500 messages, long tool output, and large task lists remain responsive with virtualized or bounded rendering. | stress tests |
| APP-033 | Persistence | P1 | Local app state persists sessions, cursor, selected profile, UI preferences, and safe drafts without persisting secrets in plaintext. | persistence tests |
| APP-034 | Build and packaging | P0 | App builds on supported platforms, has reproducible release profile, and CI runs store/transport/app checks. | CI |
| APP-035 | Observability | P1 | App records sanitized telemetry for connection failures, protocol errors, approval failures, task output failures, and panics. | telemetry tests |
| APP-036 | Client parity | P0 | App behavior remains protocol-compatible with `octos-tui` for core flows: session, turns, approvals, diffs, tasks, replay, cwd, and errors. | shared fixture tests |

## Major Interaction Flows

### 1. App Launch And Session Open

Precondition: user starts the native app with a configured server endpoint.

Expected flow:

1. App loads local preferences and auth/profile configuration.
2. App connects to the AppUI WebSocket and sends `session/open`.
3. App requests supported capabilities and workspace cwd when configured.
4. Store folds `session/open` result, pane snapshots, and connection state.
5. Shell renders chat/coding/content navigation with visible connection status.

### 2. Coding Chat Turn

Precondition: session is open and connected.

Expected flow:

1. User submits prompt from chat or coding screen.
2. Store records the user message before assistant/tool output.
3. App sends `turn/start`.
4. Live assistant text streams through `message/delta`.
5. Tool lifecycle, progress, task updates, approvals, and diff previews update
   structured UI surfaces.
6. Terminal turn event commits or fails the turn and clears active working
   state.

### 3. Approval Review

Precondition: server emits `approval/requested`.

Expected flow:

1. Pending approval appears in queue and, when relevant, in coding preview pane.
2. Typed details render the right subtype view.
3. User can approve once, approve scoped/session, or deny.
4. App sends `approval/respond`.
5. Response and lifecycle notifications update approval history.
6. Duplicate/stale responses never leave a pending card behind.

### 4. Diff And File Mutation Review

Precondition: approval or progress event includes a diff preview id.

Expected flow:

1. App fetches `diff/preview/get`.
2. Diff renders by file and hunk, with clear additions/removals.
3. Large or unavailable diff content shows limitation state.
4. User can inspect diff before approving a diff/file mutation.

### 5. Task Output Drill-Down

Precondition: task is selected in task dock.

Expected flow:

1. User opens task output.
2. App sends `task/output/read` with last cursor and byte limit.
3. Store updates rolling output buffer and cursor.
4. Future `task/output/delta` appends without duplicates.
5. Reconnect rehydrates visible task state and output cursor safely.

### 6. Reconnect And Recovery

Precondition: WebSocket drops or app resumes from sleep.

Expected flow:

1. App enters reconnecting state without discarding current visible state.
2. App reconnects and sends last cursor in `session/open`.
3. Valid replay applies in order.
4. Cursor invalid or lossy replay triggers snapshot rehydrate and visible user
   warning.
5. Pending approvals/tasks are reconciled to server truth.

## Required Test Coverage

### Store Tests

- Session open, cursor update, reconnect, replay lossy, and cursor reset.
- Turn start, live delta, completed, error, and interrupt.
- Approval requested, responded, decided, auto-resolved, cancelled, stale.
- Task updated for pending, running, completed, failed, cancelled.
- Task output read result, output delta, cursor preservation, duplicate
  prevention, and buffer cap.
- Unknown/future wire values.
- Connection and auth state transitions.

### Transport Tests

- JSON-RPC request/response correlation.
- WebSocket reconnect and replay.
- Auth/profile headers.
- Capability request and parsing.
- `session/open` with workspace cwd.
- `approval/respond`, `diff/preview/get`, `task/output/read`.
- Future task-control methods once server API lands.
- Fault injection for malformed frames, unknown methods, timeout, closed socket,
  and typed AppUI errors.

### UI/Render Tests

- Chat stream with markdown, CJK, code, table, and long message.
- Coding screen empty, active approval, typed diff, command approval, selected
  task output, and failed task.
- Task dock with many tasks and all states.
- Connection status banner/dot across all global states.
- Window-size snapshots for small, normal, and large desktop sizes.
- Secret redaction in copied diagnostics and logs.

### Live Tests

- Connect to real `octos serve`.
- Run a live coding turn with approval, diff preview, task output, and reconnect.
- Compare core protocol behavior with `octos-tui` on the same fixture.
- Verify app remains responsive through a long-running coding session.

## Current Implementation Notes

The current local `octos-app` workspace already contains implementation pieces
for this contract:

- `crates/octos-app-transport` handles AppUI transport/capabilities.
- `crates/octos-app-store` folds protocol notifications into app state.
- `app/src/backend/octos_ui.rs` owns WebSocket/backend integration.
- `app/src/main.rs` wires backend state, task output handle, and app actions.
- `app/src/app/coding.rs` provides the coding screen surface.
- `crates/octos-app-store/src/approvals.rs` owns approval queue/history.
- `crates/octos-app-store/src/state.rs` handles task state, approval lifecycle,
  replay-lossy, and reducers.

Known operational issue:

- `/Users/yuechen/home/octos-app` is currently not a git repository in this
  environment. Before release work, restore or reclone it as a real checkout so
  diffs, branches, commits, and PRs are auditable.

Known gaps relative to this requirements document:

- AppUI task-control methods depend on the server task-control merge path.
- Diff hunk rendering and large-diff behavior need full UI snapshot coverage.
- Some coding screen implementation is still monolithic and should be split
  only when doing so lowers test and maintenance risk.
- True task stdout/stderr live-tail depends on the server feature; current app
  support must respect the server's snapshot-projection limitation.
- Cross-client parity with `octos-tui` should be added as shared fixtures, not
  hand-checked only.

## Release Gate

No `octos-app` branch should be considered production-ready unless:

- all P0 requirements have store, transport, UI, or live coverage
- the app builds from a real git checkout
- AppUI protocol changes compile against current `octos-core`
- unknown/future protocol values do not crash decode/render paths
- auth/profile/secret hygiene is verified
- live coding session with approvals, diff, task output, and reconnect passes
