# AskUserQuestion — Structured Mid-Turn User Questions

Date: 2026-06-03

Status: design draft (Phase 1: synchronous tool-block).

Owner: octos-agent + octos-cli (AppUI/UI Protocol) + octos-tui.

Related contract surfaces:

- [api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md](../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md)
  (§7 `user_question/respond`, §8 `user_question/requested`, §4.1 UPCR-2026-023)
- [api/OCTOS_SERVER_FEATURE_REQUIREMENTS.md](../../api/OCTOS_SERVER_FEATURE_REQUIREMENTS.md) (`SRV-041`)
- [api/OCTOS_APP_FEATURE_REQUIREMENTS.md](../../api/OCTOS_APP_FEATURE_REQUIREMENTS.md) (`APP-037`)
- [api/OCTOS_TUI_FEATURE_REQUIREMENTS.md](../../api/OCTOS_TUI_FEATURE_REQUIREMENTS.md) (`TUI-037`)

Existing machinery this design reuses (read these before implementing):

- `crates/octos-agent/src/tools/mod.rs` — `ToolApprovalRequester` trait,
  `TOOL_APPROVAL_CTX` task-local, `ToolApprovalRequest` / `ToolApprovalDecision`.
- `crates/octos-cli/src/api/ui_protocol_approvals.rs` — `PendingApprovalStore`,
  oneshot response channel, turn-scoped cancellation.
- `crates/octos-cli/src/api/ui_protocol.rs` — `UiProtocolApprovalRequester`
  impl, `AppState.approvals`, the `TOOL_APPROVAL_CTX.scope(...)` turn wrapper,
  and the `approval/respond` dispatch path.
- `crates/octos-agent/src/tools/coding_tools.rs` — the `request_user_input`
  codex tool (`request_user_input_body`) whose structured-metadata shape is the
  graceful-fallback template.

---

## 1. Motivation

octos agents frequently reach a point mid-turn where they must make an
assumption the user could resolve in one tap: which framework, which target
environment, which of two ambiguous files, opt-in to a destructive cleanup, and
so on. Today the agent has two poor options:

1. Ask the question as free assistant prose and hope the user types a usable
   reply on the next turn. This loses the turn's context, forces the user to
   read and re-type, and produces unstructured answers the model has to
   re-parse.
2. Guess. This is the dominant failure mode behind "the agent did the wrong
   thing confidently" — the model picks a default because there is no
   first-class way to surface a bounded choice.

Frontier coding agents (codex, Claude) solve this with a structured
`AskUserQuestion` tool: the agent emits 1–4 multiple-choice questions, the
client renders selectable options plus a free-text escape hatch, and the
selected answer routes straight back into the same turn. octos already has every
primitive needed to do this — the approval flow is exactly "pause the turn at a
tool boundary, surface a typed decision point to the client, route the client's
answer back, resume." AskUserQuestion is **approval + choices + free-text**.

This document specs the Phase-1 implementation: a synchronous tool-block that
mirrors the approval flow.

## 2. Goals

- Let the agent ask the user a **structured multiple-choice question** mid-turn
  and receive a structured answer that continues the same turn.
- Carry 1–4 questions per call; each question has a short `header`, a `question`
  string, 2–4 `options` (`label` + `description`), and a `multi_select` flag.
- The client always also offers a free-text "Other" so the user is never boxed
  in by the agent's options.
- Reuse the **proven approval machinery** (pending store + oneshot + task-local
  requester + turn-scoped cancellation) rather than inventing a new turn-state
  model.
- **Never hard-block a non-supporting client**: if no capable client is
  attached, degrade gracefully to structured-metadata + generic text and
  continue the turn (the `request_user_input` behavior).
- **Turn-interrupt cancels pending questions**, exactly like approval
  cancellation.

## 3. Non-Goals

- This is not a generic forms/wizard engine. It is a bounded multiple-choice +
  free-text question, nothing more.
- **Turn-suspension** (releasing the turn while the question is outstanding and
  resuming it from a durable checkpoint when the answer arrives) is explicitly
  **out of scope for Phase 1**. See §8 (Phase 2 / future work). Phase 1 keeps
  the turn paused at the tool's await boundary, identical to `approval/requested`.
- Persisting answers as durable policy ("always pick X") is out of scope.
- Pipeline human-gate integration (`octos-pipeline` already has
  `HumanInputType::Choice`) is a separate, already-existing surface; see §7.

## 4. Chosen Approach: Synchronous Tool-Block Mirroring Approvals

### 4.1 Why this shape

The approval flow has already solved the hard problem AskUserQuestion needs:
**deterministically pausing a turn at a tool boundary, surfacing a typed
decision point to whichever client is attached, routing the client's reply back
to the exact waiting tool invocation, and cancelling cleanly on interrupt.**
The relevant invariants are already documented and tested:

- `approval/requested` (§8 of the protocol spec): "While this is unresolved, the
  turn remains paused at a deterministic boundary."
- `turn/interrupt` (`SRV-006`): "drains pending approvals, cancels active work
  safely, and emits terminal state."
- `approval/requested` is durable enough for reconnecting clients to render true
  state (`SRV-010`).

Building AskUserQuestion as a parallel of this flow means we inherit all of that
machinery and its test coverage. We do **not** redesign turn state, we do not add
a suspend/resume checkpoint, and we do not introduce a new way for a turn to be
"alive but waiting." The agent tool simply `await`s a oneshot, exactly as the
approval-gated tools do today.

The cost of this choice is that the turn holds its execution context (and any
upstream connection/turn budget) while the user thinks. That is an accepted
trade-off for Phase 1 and is identical to the existing approval behavior. The
turn-suspension variant that removes this cost is deferred to Phase 2 (§8).

### 4.2 End-to-end flow

```
agent loop (turn active)
  └─ model emits ask_user_question tool call (1–4 questions)
       └─ ask_user_question tool .execute()
            ├─ reads USER_QUESTION_CTX task-local (the UserQuestionRequester)
            │    ├─ if NOT present  → graceful fallback (§4.4): emit
            │    │     structured_metadata + generic text, return immediately,
            │    │     turn continues with no answer
            │    └─ if present → requester.request_user_question(req).await
            │         (this is the await boundary; the turn is now PAUSED)
            │
            │  ── server side (octos-cli) ──
            │   SessionUserQuestionRequester::request_user_question:
            │     1. mint question_id, store pending in PendingQuestionStore
            │        keyed by (session, turn, question_id) with a oneshot Sender
            │     2. emit user_question/requested notification (structured
            │        questions + mandatory generic title/body fallback)
            │     3. .await the oneshot Receiver
            │
            │   client renders the question picker (single/multi-select + Other)
            │   client → user_question/respond { session_id, question_id,
            │                                     answers, client_note? }
            │
            │   server handler:
            │     - validate session/turn/question_id (typed errors on stale)
            │     - PendingQuestionStore: take the oneshot Sender, send answers
            │     - (this resolves the await in request_user_question)
            │
            └─ tool .execute() receives answers, returns them as the ToolResult
       └─ tool result appended to the message history
  └─ agent loop continues the SAME turn with the answer in context
```

Turn interrupt path (mirrors approval cancellation): `turn/interrupt` calls
`PendingQuestionStore::cancel_pending_for_turn`, which drops/cancels each
pending oneshot. The blocked tool's `await` resolves to a `Cancelled` outcome;
the tool returns a cancelled result and the turn terminates. The server emits a
`user_question/cancelled`-equivalent terminal signal (Phase-1 reuses the warning
/ turn-error path; see §6 test plan).

### 4.3 Type shapes

The wire types live in `octos-core` (the protocol source of truth). Names below
are the proposed Rust struct/field names; the JSON field names are the
snake_case serde renames.

Request — emitted by the agent, carried in `user_question/requested`:

```rust
/// One AskUserQuestion call. 1–4 questions.
pub struct UserQuestionRequestedEvent {
    pub session_id: SessionKey,
    pub topic: Option<String>,          // skip_serializing_if None
    pub question_id: QuestionId,        // newtype over Uuid, like ApprovalId
    pub turn_id: TurnId,
    pub title: String,                  // MANDATORY generic fallback text
    pub body: String,                   // MANDATORY generic fallback text
    pub questions: Vec<UserQuestion>,   // 1..=4
}

pub struct UserQuestion {
    pub header: String,                 // short label, ≤ 12 chars (over-long truncated, not rejected)
    pub question: String,
    pub options: Vec<UserQuestionOption>, // 2..=4
    pub multi_select: bool,
    pub allow_free_text: bool,          // server forces true: "Other" always offered
}

pub struct UserQuestionOption {
    pub label: String,
    pub description: String,
}
```

Response — sent by the client in `user_question/respond`:

```rust
pub struct UserQuestionRespondParams {
    pub session_id: SessionKey,
    pub question_id: QuestionId,
    pub answers: Vec<UserQuestionAnswer>, // one per question, in order
    pub client_note: Option<String>,      // optional, audit/display
}

pub struct UserQuestionAnswer {
    /// Selected option label(s). Empty when the user only supplied free_text.
    /// For single-select questions this is 0 or 1 entries; for multi_select,
    /// 0..N. Labels MUST match the option labels from the request.
    pub selected_labels: Vec<String>,
    /// Optional free-text from the "Other" escape hatch.
    pub free_text: Option<String>,
}
```

The agent-facing tool surfaces the same answer shape back to the model as its
`ToolResult.output` JSON, so the model sees `{ answers: [{ selected_labels,
free_text }, ...] }`.

Agent-side bridge types in `octos-agent` (mirror `ToolApprovalRequest` /
`ToolApprovalDecision`):

```rust
pub struct UserQuestionRequest {
    pub questions: Vec<UserQuestion>, // validated 1..=4, options 2..=4
    pub title: String,
    pub body: String,
}

pub enum UserQuestionOutcome {
    Answered(Vec<UserQuestionAnswer>),
    Cancelled,        // turn interrupted / pending drained
    Unsupported,      // no capable client; tool degrades to fallback
}

#[async_trait]
pub trait UserQuestionRequester: Send + Sync {
    async fn request_user_question(&self, req: UserQuestionRequest)
        -> UserQuestionOutcome;
}
```

### 4.4 Graceful fallback (no capable client)

If `USER_QUESTION_CTX` is not present in the task-local context (no interactive
client supporting `user_question.v1` is attached for this turn), the
`ask_user_question` tool does **not** block. It returns immediately with a
`request_user_input`-style result:

- `success: true`
- `output`: a JSON object describing the question(s) and noting that no
  synchronous host response channel is attached.
- `structured_metadata`: `{ "codex_tool": "ask_user_question", "request": {…},
  "host_response_channel": "not_attached" }`.

This is exactly the degradation the existing `request_user_input` tool performs
(`request_user_input_body` in `coding_tools.rs`). The model receives the
fallback result, sees that no answer is available, and continues the turn (it
can fall back to its own best guess and state the assumption in prose). A
non-supporting client therefore never sees a hung turn.

The server is also responsible for the wire-level fallback: a client that does
NOT negotiate `user_question.v1` but is otherwise attached must still receive an
actionable rendering. The `user_question/requested` event carries **mandatory**
generic `title`/`body` text (just like `approval/requested`) so that a client
which does not understand the structured `questions` field can render the
generic text and the user can still respond (e.g. via free-text). Unknown fields
fall back to generic rendering and stay actionable.

## 5. Component / File Checklist (Phase-B implementation)

This is the implementation map for the follow-on coding stack. It is not built
in this docs-only change.

**octos-core** (protocol source of truth):

- `crates/octos-core/src/ui_protocol.rs`:
  - `QuestionId` newtype (mirror `ApprovalId`).
  - `UserQuestion`, `UserQuestionOption`, `UserQuestionRequestedEvent`.
  - `UserQuestionRespondParams`, `UserQuestionRespondResult`,
    `UserQuestionAnswer`.
  - Add `user_question/respond` to `UI_PROTOCOL_COMMAND_METHODS`.
  - Add `user_question/requested` (and any terminal/cancelled variant) to
    `UI_PROTOCOL_NOTIFICATION_METHODS`.
  - Add `user_question.v1` to the known feature registry surfaced via
    `supported_features` in `UiProtocolCapabilities`.
  - Add the new method/notification + a representative wire payload to the
    golden contract tests (gated by accepted UPCR-2026-023).

**octos-agent** (tool + bridge):

- `crates/octos-agent/src/tools/mod.rs`:
  - `UserQuestionRequest`, `UserQuestionOutcome`, `UserQuestionRequester`
    trait, and the `USER_QUESTION_CTX` task-local (mirror
    `ToolApprovalRequester` / `TOOL_APPROVAL_CTX`).
- `crates/octos-agent/src/tools/coding_tools.rs` (or a dedicated module):
  - `AskUserQuestionTool` implementing `Tool`; `spec()` declares the
    `questions[]` schema (1–4, `header`/`question`/`options`/`multi_select`);
    `execute()` validates, reads `USER_QUESTION_CTX`, blocks on the requester,
    falls back when absent (§4.4).
- `crates/octos-agent/src/tools/registry.rs`: register `AskUserQuestionTool`
  (mirror `registry.register(RequestUserInputTool)`).
- `crates/octos-agent/src/role_template.rs`: add `ask_user_question` to the
  coding tool roster alongside `request_user_input`.
- Tool policy: ensure `ask_user_question` is allowed under the relevant
  groups/provider policy (`tools/policy.rs`).

**octos-cli** (server: store + handler + requester + wiring):

- `crates/octos-cli/src/api/ui_protocol_approvals.rs` (or a sibling
  `ui_protocol_user_questions.rs`): `PendingQuestionStore` (mirror
  `PendingApprovalStore`): pending map keyed by `(session, turn, question_id)`,
  oneshot `Sender`, `register_pending`, `take`/respond, and
  `cancel_pending_for_turn`.
- `crates/octos-cli/src/api/ui_protocol.rs`:
  - `SessionUserQuestionRequester` impl of `UserQuestionRequester` (mirror
    `UiProtocolApprovalRequester`): mint id, store pending, emit
    `user_question/requested`, await oneshot.
  - `AppState.user_questions: PendingQuestionStore` field (mirror
    `AppState.approvals`).
  - Scope `USER_QUESTION_CTX` around the turn (alongside the existing
    `TOOL_APPROVAL_CTX.scope(...)` wrapper).
  - `user_question/respond` dispatch handler: validate, resolve oneshot,
    return result; typed errors on stale/unknown/closed.
  - `turn/interrupt`: also drain `user_questions.cancel_pending_for_turn`.
  - `session/hydrate`: include pending questions for reconnecting clients
    (mirror `pending_approvals`).
  - Capability negotiation: advertise `user_question.v1` in
    `supported_features` when the client requests it.

**octos-tui** (rendering):

- `crates/octos-tui/src/store.rs` + `src/app.rs` (mirror the approval-card
  handling): render the question picker — one card per question, single-select
  vs multi-select, each option as a selectable line, plus an "Other" free-text
  entry. Send `user_question/respond` with the correct `question_id` and
  per-question `answers`. Handle stale/expired as decided/expired (mirror the
  approval-cancelled handling).

## 6. Test Plan

Follow the project RED → GREEN → REFACTOR cycle. Tests by layer:

**octos-core (golden contract):**

- `user_question/respond` is in `UI_PROTOCOL_COMMAND_METHODS`;
  `user_question/requested` is in `UI_PROTOCOL_NOTIFICATION_METHODS`.
- `user_question.v1` is in the known-feature registry.
- Representative `UserQuestionRequestedEvent` / `UserQuestionRespondParams`
  serialize to the expected wire JSON (snake_case, mandatory `title`/`body`,
  `questions[]` with `header`/`question`/`options`/`multi_select`/
  `allow_free_text`).
- Round-trip: a generic-fallback event with an unknown extra field still
  deserializes and keeps `title`/`body`.

**octos-agent (tool):**

- `should_emit_structured_metadata_when_no_requester` — with no
  `USER_QUESTION_CTX`, `execute()` returns the fallback result with
  `codex_tool = "ask_user_question"` and `host_response_channel =
  "not_attached"` (mirrors `request_user_input_emits_structured_metadata_event`).
- `should_block_and_return_answers_when_requester_present` — with a recording
  requester scoped via `USER_QUESTION_CTX`, `execute()` awaits and returns the
  injected answers.
- `should_reject_when_questions_out_of_range` — 0 or >4 questions, or an option
  count outside 2..=4, is a tool input error.

**octos-cli (server + e2e):**

- `should_emit_user_question_requested_with_structured_questions`.
- `should_resume_tool_when_user_question_respond_matches`.
- `should_reject_stale_user_question_respond_with_typed_error`.
- `should_cancel_pending_questions_on_turn_interrupt` (mirror the approval-drain
  test).
- `should_replay_pending_user_question_on_hydrate` (reconnect path).
- `should_degrade_to_generic_when_client_lacks_capability`.

**octos-tui:**

- Render snapshot of single-select and multi-select cards + the "Other" entry.
- Reducer test: `user_question/respond` carries the right `question_id` and
  per-question `answers`; stale renders as decided/expired.

## 7. Alternatives Considered

**A. Turn-suspension (Phase 2 candidate).** Instead of holding the turn at the
tool await boundary, release the turn, persist a checkpoint, and resume from it
when `user_question/respond` arrives. This frees the execution context and any
upstream connection/budget while the user thinks, and degrades better for slow
human responses. Rejected for Phase 1 because it requires a durable turn-state
checkpoint/resume mechanism octos does not yet have; that is real new turn-state
machinery, not a reuse of the approval flow. Deferred to §8.

**B. Pipeline human-gate.** `octos-pipeline` already has a human-input gate with
`HumanInputType::Choice` (`crates/octos-pipeline/src/human_gate.rs`, re-exported
from `lib.rs`). That solves a structurally similar problem — a pipeline node
pauses for a typed human choice. It was considered as the home for this feature
but rejected for the interactive-chat path: the pipeline gate is node-scoped and
checkpoint-based (it fits the DOT-graph pipeline engine), whereas the chat agent
loop needs a mid-turn, in-place block that the existing approval flow already
provides. The two can converge later (the pipeline `Choice` gate and the chat
AskUserQuestion could share the `octos-core` wire types), but Phase 1 keeps the
chat path on the approval-mirror mechanism.

**C. Free-text only (status quo `request_user_input`).** The existing codex
`request_user_input` tool already emits a structured-metadata request but has no
synchronous response channel and no structured options. AskUserQuestion is the
superset: it keeps the same graceful fallback (§4.4) but adds the bounded
options, multi-select, and the synchronous answer route. We keep
`request_user_input` as-is; AskUserQuestion is the new, richer tool.

## 8. Phase 2 / Future Work (not specced here)

- **Turn-suspension variant** (Alternative A): release the turn while a question
  is outstanding and resume from a durable checkpoint when the answer arrives.
  Requires turn-state checkpoint/resume infrastructure. Tracked as a possible
  future UPCR; not part of `user_question.v1`.
- **Durable answer policy** ("remember this choice").
- **Convergence with the pipeline `HumanInputType::Choice` gate** on shared
  `octos-core` wire types.
