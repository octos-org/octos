# UPCR-2026-023: AppUI Structured User Question (AskUserQuestion)

Status: proposed
Date: 2026-06-03

## Summary

Let an Octos agent ask the user a structured multiple-choice question mid-turn
and block on the answer, instead of guessing or emitting an unstructured text
prompt the client cannot reliably render or route a reply for.

This is the codex/Claude `AskUserQuestion` capability for Octos. The agent calls
a model-visible `ask_user_question` tool carrying 1–4 questions, each with a
short `header`, a `question`, 2–4 labeled `options`, and a `multi_select` flag;
the server forces a free-text "Other" escape hatch on every question. The server
emits a `user_question/requested` notification and pauses the turn at the
blocking-tool boundary (the same deterministic boundary as `approval/requested`).
The client renders a single/multi-select picker plus "Other" and replies with
`user_question/respond`, which resolves the waiting tool and resumes the turn.

This UPCR reuses the proven approval machinery (a pending-store keyed by id +
oneshot resume + a task-local requester + turn-scoped cancellation). It does NOT
introduce turn suspension/serialization, a new `TurnState` variant, or a
client-driven survey/wizard runtime — the turn stays `Active`, paused at the
tool's await point. A turn-suspension generalization is a possible future UPCR;
the pipeline `HumanInputType::Choice` gate remains a separate workflow-scoped
mechanism and is not wired to agent turns here.

## Decision

Do add a model-visible `ask_user_question` backend tool, resolved through the
same profile runtime factory as every other Octos tool (profile, memory, MCP,
skill, sandbox, approval, QoE, and model-portfolio policy apply).

Do add one server→client notification (`user_question/requested`) and one
client→server command (`user_question/respond`), correlated by `question_id`,
mirroring `approval/requested` + `approval/respond`.

Do require generic `title`/`body` fallback text on every request so a client
that does not understand the structured `questions` field stays actionable.

Do NOT let clients construct or invoke the tool directly, define new turn
lifecycle states, or persist answers as durable policy (a question answer is a
one-shot turn input, not an allow rule).

## Capabilities

Servers that support this contract advertise:

- `user_question.v1`

Advertised through optional `supported_features` in `UiProtocolCapabilities`;
clients request it through `X-Octos-Ui-Features` (comma/space-separated tokens).
Capability-gated fields must be omitted when the capability is not negotiated.

## AppUI Surface

Defined in the v1 spec §7 (command) and §8 (event):

### `user_question/requested` (event)

Pauses the turn at the blocking-tool boundary. Carries:

- Required fallback: `session_id`, `question_id`, `turn_id`, `title`, `body`
  (generic, mandatory).
- Structured (gated by `user_question.v1`): `questions` — 1–4 entries, each with
  `header` (≤ 12 chars — a longer header is **truncated** server-side, not
  rejected; live-soak hardening 2026-06-04), `question`, `options` (2–4 of
  `label` + `description`), `multi_select` (bool), and `allow_free_text` (bool,
  server-forced `true`).

### `user_question/respond` (command)

Answers a `user_question/requested` event. Carries:

- `session_id`, `question_id`.
- `answers` — one entry per question, in question order, each with
  `selected_labels` (0..1 for single-select, 0..N for multi-select; empty when
  only free text was supplied; labels must match the request's option labels)
  and optional `free_text` (the "Other" escape hatch).
- Optional `client_note` (audit/display; servers must not require it).

## Backend Tool Contract

- `ask_user_question` — model-visible structured user-question tool. Input is the
  `questions` array above. It runs inside the server-owned session runtime and,
  when a capable client is attached, blocks on a oneshot until
  `user_question/respond` resolves it, then returns the per-question answers to
  the model.
- This is the synchronous extension of UPCR-2026-020's `request_user_input`
  primitive (which emits structured metadata but does not block). Where
  `user_question.v1` is negotiated, `ask_user_question` is the blocking,
  answer-routed form.

## Security And Runtime Rules

- Question/answer routing happens inside the server-owned runtime; the client
  cannot invoke `ask_user_question` or forge a `question_id`.
- A `user_question/respond` is accepted only for a pending `question_id` on the
  caller's session; stale/unknown/duplicate ids return typed errors and do not
  resume a turn.
- Answers are one-shot turn input, never persisted as an allow rule or policy.
- Turn interrupt cancels all pending questions for the turn (mirroring approval
  cancellation), and the waiting tool observes a cancelled result.
- Question/answer state must be identical over WebSocket and stdio.
- Secret values must never be embedded in `title`/`body`/options.

## Error Model

Reuse the existing AppUI error taxonomy; add where more specific data helps:

- `user_question_unknown` — no pending question for the id/session.
- `user_question_stale` — the question was already answered or cancelled.

Structured error `data` should include `session_id`, `question_id`, and
`recoverable` when applicable. Secret values must be omitted.

## Compatibility

- Generic `title`/`body` remain mandatory fallback text for v1alpha1; a client
  that does not understand `questions` renders them and the user can still answer
  (e.g. via free text).
- Clients that do NOT advertise `user_question.v1` receive the agent tool's
  structured-metadata / generic-text fallback instead of a blocking question, so
  the turn never hard-blocks on a non-supporting client — the tool degrades
  exactly like `request_user_input`.
- Older servers omit `user_question.v1` and never emit `user_question/requested`;
  agents on those servers fall back to plain text prompts.

## Tests

- Protocol round-trip: `user_question/requested` and `user_question/respond`
  serialize/deserialize with the structured `questions`/`answers` shapes and the
  generic fallback fields.
- Server: a pending question resumes the waiting tool exactly once on
  `user_question/respond`; stale/unknown ids return typed errors; turn interrupt
  cancels pending questions; payloads identical over WebSocket and stdio.
- Agent tool: `ask_user_question` blocks and returns the selected answers when a
  requester context is attached; with no context it degrades to
  structured-metadata + generic text and continues (no hard block).
- Client: renders single/multi-select + "Other"; sends `user_question/respond`
  with correct ids; renders stale/expired as decided/expired, not pending.
- Live tmux soak: an agent calls `ask_user_question`, the TUI renders the picker,
  the user selects, and the agent receives the answer and continues the turn.
