# Octos TUI Feature Requirements

Status: proposed test contract.
Owner: octos-tui.
Applies to: mock mode, protocol mode, live coding UX parity runs.

## Purpose

This document defines the user-facing feature requirements that `octos-tui`
must satisfy before it can be treated as a production coding client for the
Octos AppUI protocol. It is intended to be used by unit tests, snapshot tests,
tmux harnesses, and human review.

The goal is not visual imitation for its own sake. The goal is a terminal UX
where a coding user can always answer four questions:

- What is the model doing now?
- What is waiting on me?
- What changed in the workspace?
- What can I safely do next?

## Non-Negotiable Product Principles

- The TUI is an AppUI client. It must consume shared `octos-core` AppUI/UI
  Protocol types and must not invent private wire extensions.
- The first screen is the working coding interface, not a landing page or
  diagnostic page.
- The composer, system status, and blocking approvals must remain stable and
  visible while chat content scrolls.
- Chat output must be readable as a transcript. Raw protocol events, tracing
  logs, API keys, timestamps, and internal debug noise must not appear in the
  normal chat flow.
- Terminal colors must respect the selected theme and terminal constraints.
  Important states can use accent colors, but the UI must not force bright
  green or high-saturation blocks when the terminal theme does not call for it.
- Long-running coding sessions are the primary UX case. The TUI must remain
  understandable after many turns, many tool calls, queued user messages, and
  background tasks.

## Requirement Matrix

| ID | Requirement | Priority | Acceptance Criteria | Verification |
|---|---|---:|---|---|
| TUI-001 | Chat-first default layout | P0 | Default view shows transcript, composer, and status without requiring inspector mode. Composer never scrolls with transcript. | tmux screenshot at idle/running/done |
| TUI-002 | Stable composer cursor | P0 | Exactly one visible input cursor exists and it is inside the composer input line, never above the input line. | tmux visual assertion and cursor-position test |
| TUI-003 | Sticky status row | P0 | A status row is visually attached to the composer area and shows idle/working/blocked/error/done state. It is not rendered as chat text or composer input. | snapshot test and tmux capture |
| TUI-004 | Non-animated idle state | P0 | When no turn, task, approval, or background job is active, the status row shows a static `Idle` state with no spinner. | unit state test and idle tmux capture |
| TUI-005 | Live working state | P0 | During an active turn, the status row shows a changing working label, elapsed time, interrupt affordance, and background task count when available. | live tmux harness |
| TUI-006 | User message transcript rendering | P0 | Every submitted user prompt is inserted into chat history before the assistant/tool output caused by that prompt. User messages use subtle shading or a distinct prefix. | store unit test and transcript ordering capture |
| TUI-007 | Queued user messages | P0 | If the user submits while a turn is active, the message is shown as queued/staged near the composer, not as current composer text. The queued message must not be placed above older chat bubbles. | event-loop unit test and tmux capture |
| TUI-008 | Approval blocks execution | P0 | When approval is required, the TUI shows a blocking approval card and the model/tool stream must not appear to continue until a decision is sent or auto-resolved. | protocol e2e approval fixture |
| TUI-009 | Approval action clarity | P0 | Approval card shows one action per line: `y = approve this command once`, `s = approve this command/scope for the session`, `n = deny it`; diff approvals also show `d = view diff`. | render snapshot |
| TUI-010 | Markdown rendering | P0 | Assistant output renders headings, bullets, numbered lists, checkboxes, fenced code, inline code, bold emphasis, and markdown tables without showing raw markdown artifacts such as table pipes as plain wrapped prose. | renderer unit tests with golden text |
| TUI-011 | Paragraph-aware wrapping | P0 | A new markdown paragraph, table, code block, or section heading is treated as a separate render block. Collapse thresholds and spacing must not merge it into the previous paragraph. | renderer unit tests |
| TUI-012 | Strict left alignment | P0 | Assistant text is left aligned within its transcript block. The renderer must not introduce unexplained leading spaces in normal prose. | render snapshot |
| TUI-013 | Plan visibility | P0 | The live plan is sticky above or adjacent to the composer/status region, not only embedded in scrolling chat. | tmux screenshot |
| TUI-014 | Plan completeness | P0 | The plan pane shows all current steps or a clear collapsed-count indicator. It must not silently show only the first few items. | model/render unit test |
| TUI-015 | Plan status updates | P0 | Completed plan items are checked as `[x]` or equivalent as soon as protocol/model output indicates completion. Stale unchecked completed steps are a failure. | long-session harness |
| TUI-016 | Tool cards | P0 | Tool activity renders as recognizable cards with action label, tool name, command or target, cwd when known, status, elapsed time, and output preview. | renderer golden tests |
| TUI-017 | Tool action labels | P0 | Common actions use human-readable labels such as `Ran`, `Running`, `Waited`, `Watching`, `Coding`, `Reading`, `Edited`, `Searched`, `Built`, `Tested`, and `Installed` rather than raw protocol event names. | activity label unit tests |
| TUI-018 | Expandable tool output | P0 | Long tool output and command output are collapsed by default with a one-line summary and expandable with `Ctrl+O`. Expanded state can be toggled back. | keyboard and render tests |
| TUI-019 | Diff output collapse | P0 | Diff previews and file creation previews collapse aggressively enough that generated files do not flood the transcript. The preview must show file path, status, first relevant lines, and hidden-line count. | diff fixture render test |
| TUI-020 | Inline diff preview | P0 | Diff approvals and file mutation progress open inline diff preview when `preview_id` is available. Hunks can be selected with `[` and `]`. | store unit test and tmux capture |
| TUI-021 | Diff context staging | P0 | Pressing `c` on a selected diff hunk stages that hunk as next-turn context. If a turn is active, it queues context for the next turn. | existing context harness plus snapshot |
| TUI-022 | Slash commands | P1 | If slash commands are shown in status/help, `/ps`, `/stop`, and other advertised commands must work. If unsupported, they must not be advertised. | event-loop tests |
| TUI-023 | Background task registry | P1 | `/ps` or equivalent task view lists background tasks with id, title/tool, state, elapsed time, and cancel/stop affordance. | protocol task fixture |
| TUI-024 | Task cancellation | P1 | `/stop` or equivalent can cancel a running background task through AppUI task control when the server advertises support. Cancelled state is rendered distinctly. | protocol e2e |
| TUI-025 | Task output read | P1 | Selecting a task and requesting output reads from `task/output/read`, preserves cursor, appends deltas, and avoids duplicate output after reconnect. | reducer tests |
| TUI-026 | Activity noise folding | P0 | Low-value progress events such as token/cost updates, raw thinking markers, stream-end events, and transport bookkeeping are folded into status rows or hidden unless inspector/debug mode is active. | fixture render test |
| TUI-027 | Finished-turn recap | P1 | After a long turn completes, the TUI shows a subtle recap block in transcript or status history with elapsed time, background task count, files changed, validation, and next step when available. It must not appear inside the composer input. | long-session harness |
| TUI-028 | Error and replay visibility | P0 | Protocol errors, turn errors, replay-lossy warnings, approval-cancelled, and cancelled task states are surfaced in status/activity with actionable language. | reducer tests |
| TUI-029 | Theme discipline | P0 | Themes use subtle contrast and terminal-appropriate colors. `--theme terminal` must use terminal default foreground/background and avoid forcing green highlight. Existing themes must not use large saturated blocks for normal chat. | theme snapshot across themes |
| TUI-030 | Read-only protocol mode | P1 | `--readonly` opens and renders sessions but prevents `turn/start`; attempts to submit show clear read-only status and clear the draft. | event-loop unit test |
| TUI-031 | Workspace cwd display | P1 | Status or inspector shows effective cwd/workspace root from `session/open`, and wrong-cwd errors are visible as typed protocol errors. | protocol bootstrap test |
| TUI-032 | Inspector mode | P1 | Tab cycles to inspector panes for sessions, tasks, artifacts, workspace, and git. Inspector must not break composer stability or transcript scroll. | keyboard snapshot test |
| TUI-033 | Scrolling behavior | P0 | Transcript, workspace, git, task output, and diff views have deterministic scroll behavior. New output follows tail unless the user has intentionally scrolled up. | render/model tests |
| TUI-034 | Interrupt behavior | P0 | `Ctrl+C` interrupts active turn. `Esc` with queued messages interrupts active turn and sends queued work after the turn stops. Status text must distinguish interrupt requested vs no active turn. | event-loop tests |
| TUI-035 | Secret hygiene | P0 | Captures and normal UI must not show auth tokens, provider keys, raw bearer headers, or secret environment values. | redaction/harness checks |
| TUI-036 | Long-session stability | P0 | A 30+ minute real coding session remains responsive, does not truncate all history to only visible rows, and keeps enough transcript history for review. | live mini host parity test |

## Major Interaction Flows

### 1. Normal Coding Turn

Precondition: protocol session is open and idle.

Expected flow:

1. User types a prompt in the composer.
2. Pressing Enter creates a user chat bubble immediately.
3. Status changes from `Idle` to working with elapsed time.
4. Assistant text streams into a live assistant block.
5. Tool calls render as cards using action labels such as `Running` and `Ran`.
6. Long output is collapsed with an expand hint.
7. Turn completion commits live assistant text to history, clears active spinner, updates plan checks, and shows done/recap state.

### 2. Approval-Gated Command

Precondition: active turn requests command, sandbox, network, filesystem, or diff approval.

Expected flow:

1. Approval card appears inline near the latest context and composer.
2. Composer remains focused but the approval key map takes precedence while visible.
3. The card uses explicit action lines:
   - `y = approve this command once`
   - `s = approve this command/scope for the session`
   - `n = deny it`
4. The assistant/tool stream does not continue until a decision or auto-resolution is received.
5. Approval decided, cancelled, and auto-resolved notifications clear the pending card and leave a visible activity record.

### 3. Queued Follow-Up During Active Turn

Precondition: a turn is running.

Expected flow:

1. User presses Enter with a new prompt.
2. The prompt is staged near the composer and shown as queued for next turn.
3. The composer input is cleared.
4. The queued message is not inserted ahead of already-rendered chat content.
5. `Ctrl+U` clears queued messages.
6. `Esc` interrupts the active turn and preserves the queued message for submission when the turn stops.

### 4. Diff Review and Context Staging

Precondition: a task or approval exposes a `preview_id`.

Expected flow:

1. TUI requests diff preview through AppUI.
2. Inline diff preview shows file status, path, hunks, and line-level additions/removals.
3. Large diffs are collapsed by file/hunk with hidden-line counts.
4. `[` and `]` move selected hunk.
5. `c` stages selected hunk context into the composer or next-turn queue.

### 5. Background Task and Swarm Management

Precondition: server advertises AppUI task-control support.

Expected flow:

1. Task updates appear in activity and task views with stable task ids.
2. `/ps` or the task inspector lists running and completed background tasks.
3. Running tasks can be cancelled through `/stop` or an equivalent task command.
4. Cancelled, failed, completed, and running states are rendered distinctly.
5. Reconnect rehydrates visible task state through `task/list` or session snapshot rather than duplicating stale updates.

### 6. Long Output Inspection

Precondition: a tool produces output longer than the preview threshold.

Expected flow:

1. Transcript shows a collapsed tool card with first useful lines and hidden-line count.
2. `Ctrl+O` expands the focused card.
3. Expanded content is scrollable without moving the composer or status row.
4. `Ctrl+O` collapses the card again.
5. The current expansion state is stable across redraws.

## Required Test Coverage

### Unit and Reducer Tests

- Markdown parser/rendering for headings, bullets, numbered lists, checkboxes,
  tables, fenced code, inline code, and paragraph boundaries.
- Store ordering for user prompt, active turn, queued prompt, completion, and
  error.
- Approval request, approval decided, auto-resolved, cancelled, and hidden-modal
  flows.
- Task update states including pending, running, completed, failed, and
  cancelled.
- Tool label classification for shell, file, search, build, test, install,
  wait/watch, and unknown tools.
- Collapse threshold behavior for command output, generated files, and diffs.
- Status row state transitions: idle, working, blocked, error, done.

### Snapshot and Render Tests

- 80x24, 100x30, and 140x40 terminal sizes.
- Codex, Claude, Slate, Solarized, and terminal themes.
- Chat layout with and without inspector.
- Sticky plan/status/composer during transcript overflow.
- Long markdown table in assistant output.
- Long diff preview.
- Approval card with and without diff preview.
- Collapsed and expanded tool cards.

### Tmux Harness Tests

The parent Octos tmux harness should keep the state matrix from
`docs/M9_33_VISUAL_PARITY_HARNESS.md` and add checks for:

- exactly one composer cursor inside the composer input line
- no green forced cursor/theme artifact in terminal theme
- sticky plan/status/composer while transcript scrolls
- markdown table rendered as a table-like block
- `/ps` and `/stop` behavior if advertised
- `Ctrl+O` expandable tool cards
- approval card action text in the explicit `key = meaning` format
- no raw protocol progress spam in normal transcript
- queued user message position after active output

### Live Coding Parity Tests

Run a real long coding task against the same fixture in Codex and `octos-tui`.
The TUI passes when human review can confirm:

- user intent and current model state are easier to locate than in raw logs
- command cards preserve command, cwd, status, duration, and output preview
- approvals are impossible to miss and cannot be confused with normal chat
- plan progress visibly changes as work completes
- long outputs do not bury the transcript
- final recap gives enough information to decide whether to continue, stop, or
  inspect files

## Current Implementation Notes

The current implementation already has building blocks for several
requirements:

- Chat-first layout with composer/status regions in `src/app.rs`.
- Inline approval card and `y`/`s`/`n` handling in `src/app.rs` and
  `src/event_loop.rs`.
- Markdown basics for headings, bullets, numbered lists, checkboxes, and fenced
  code in `src/app.rs`.
- Tool activity cards and command labels in `src/app.rs`.
- Diff preview, hunk selection, and context staging in `src/store.rs`.
- Task output cursor handling in `src/store.rs`.
- Visual parity harness scope in `docs/M9_33_VISUAL_PARITY_HARNESS.md`.

Known gaps relative to this requirements document:

- Plan is still inferred from chat and rendered inline or in inspector, not yet
  guaranteed sticky above the composer/status region.
- Markdown table rendering is not yet a full table renderer.
- `Ctrl+O` expandable tool cards are not yet the documented primary interaction.
- `/ps` and `/stop` must either be implemented or removed from visible help.
- AppUI task-control client support depends on the server task-control API
  merge path.
- `--theme terminal` is not listed in the current README theme set and must be
  implemented before tests require it.

## Release Gate

No TUI release should be considered UX-complete unless:

- all P0 requirements have automated coverage or a documented manual harness
  assertion
- no advertised interaction is unsupported
- current `octos-core` AppUI protocol changes compile in `octos-tui`
- live coding parity harness produces retained artifacts and a human-readable
  summary
