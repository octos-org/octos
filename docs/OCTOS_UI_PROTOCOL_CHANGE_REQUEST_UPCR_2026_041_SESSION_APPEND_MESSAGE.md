# Octos UI Protocol Change Request: `session/append_message`

## Header

- Request id: `UPCR-2026-041`
- Issue: #2355
- Date: 2026-10-08
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: additive typed command `session/append_message` + additive optional
  `source` field on persisted `Message` rows

## Problem

Conversation history is only written when an agent turn executes. Upper-layer
products that drive skills directly through `SKILL_ACTION_INVOKE` — or run
entirely non-agent pipelines — leave no trace in session history: memory
reads, compaction, and episodic recall never see those interactions. The only
workaround, writing session JSONL files directly, bypasses seq derivation,
thread binding, title derivation, context bookkeeping, and event projection.
`session/btw` acknowledges that not every message must go through turn
execution; there is no mirror for the write side.

## Contract

`session/append_message` appends one record to a session's persisted history
and returns its committed sequence. It NEVER starts a turn, calls a model, or
runs a tool — the write goes through the same canonical persist path as
turn-written rows (`SessionManager::add_message_with_seq`), so seq derivation,
the first-user-message title derivation, per-key write serialization, segment
rolling, and the post-commit projection fan-out all behave exactly as they do
for turn rows.

Params (`SessionAppendMessageParams`):

- `session_id: string` (required) — addressed session. Created implicitly
  in the store the connection resolves to when it does not exist yet,
  mirroring `turn/start`.
- `role: "user" | "assistant" | "system"` (required) — `tool` is rejected:
  tool rows are turn machinery, not conversational records.
- `content: string` (required, non-blank, ≤ 1,000,000 chars).
- `media: string[]` (optional) — stored verbatim as the row's media
  references; entries must be non-empty.
- `source: string` (required, non-empty, ≤ 200 chars) — provenance tag
  persisted on the row (e.g. `external_record:whiteboard`). Turn-written
  rows leave `Message::source` absent.
- `client_message_id: string` (optional) — correlation token. When supplied
  and a live row in the addressed session already carries it, the call is an
  idempotent retry: nothing is appended and the EXISTING row's seq is
  returned. The existence scan runs inside the same per-key persist lock as
  the append (the `persist_system_note_once` pattern), so a retry racing the
  original can never double-append. Two honest scopes on this guarantee:
  the scan sees the session's LOADED window, so a retry of a row sealed
  beyond the load budget appends a duplicate; and the namespace is shared
  with turn-written rows, so a caller reusing a turn's `client_message_id`
  gets the turn row's seq back with nothing appended — mint distinct ids per
  producer. Assistant retries whose deriving user row is no longer visible
  should carry `thread_id` explicitly (see below).
- `thread_id: string` (optional) — only valid with `role: "assistant"`;
  caller-supplied value wins (same rule as the turn path). When omitted, an
  assistant record binds to the most recent user row's thread and is
  rejected with `invalid_params` when the session has no user row — a public
  wire surface must not silently mint threads. For `role: "user"` the row
  roots its own thread (`client_message_id`, else a synthesized UUIDv7, the
  canonical new-write rule); supplying `thread_id` there is
  `invalid_params`. `role: "system"` rows are not thread-scoped and reject
  `thread_id` the same way.

Result (`SessionAppendMessageResult`): `{ session_id, seq, thread_id }` —
`thread_id` echoes the resolved binding so callers can correlate without a
follow-up read.

Mid-turn appends are allowed and deliberate: the record-only write adds
history, it does not rewrite it, so — unlike `session/rollback` — there is
no turn-in-progress refusal. The in-flight turn does not see the row (its
prompt was already built); the per-key persist lock serializes the row
between the turn's own writes. The assistant thread fallback reads the
loaded history outside that lock, so under a concurrent writer it can bind
to a user row that is no longer the newest by commit time — a caller
writing into a concurrently active session should pass `thread_id`
explicitly.

Live projection reuses the existing post-commit observer: user and assistant
records emit the canonical v2 envelopes (`UserMessage` /
`AssistantPersisted`) on the durable ledger, so reconnecting clients replay
them and live clients render them like any persisted row. `system` records
emit no chat envelope, matching turn-written system rows. The `source` tag
is visible on the row (JSONL, `session/messages_page` passthrough) and on
the context-ledger source refs of the recording path; it is NOT added to the
v2 envelope payload in this UPCR.

Context accounting: the handler records the committed row into the session's
AppUI `ContextManager` — the live manager when the session is open, the
durable snapshot otherwise — with source kind `external_record`, tagged via
the same `mark_source_event_kind` path background rows use, so generation,
transcript hash, and token estimates stay consistent with durable history.

Capability gating: none — `session/rollback`, `session/fork`, and
`session/btw` all ship ungated and this method is their family member. It is
NOT added to the external-client allowlist (UPCR-2026-036): external
connections keep read-only history access plus turn execution. Host-owned
app-peer sessions (UPCR-2026-035) treat it as a write: the
`HOST_PEER_SESSION_WRITE_METHODS` gate confines it to the peer's host
connection, exactly like `turn/start` and `session/rollback`, because an
appended record lands in the peer's next turn's prompt. Gateway-attached
deployments where the addressed session lives only in the gateway are out
of scope for this UPCR: the method writes the serve-resolved stores, and a
session that no serve store knows is simply created there (the write never
proxies to the gateway).

Wire changes are strictly additive: one new command constant, one
`UiCommand` variant, two DTOs, and one optional `serde(default,
skip_serializing_if)` field on `Message`. Legacy JSONL rows parse unchanged
(the field is absent on them and absent on serialize for turn-written rows).

## Tests

- octos-core: `Message` source serde round-trip + legacy-row absence; new
  method in the §wire golden contract; params/result JSON goldens; envelope
  round-trip; `UiCommand::method()` transport-name table.
- octos-bus: idempotent retry returns the original seq without a second
  row; user/assistant/system thread-binding rules; source persists to the
  JSONL row.
- octos-cli: handler validation matrix (role/content/source/media/
  thread_id rules, including the whitespace forms); idempotent retry
  through the handler; user-row thread rooting; assistant unbound-thread
  refusal and explicit-binding rescue; context-manager recording of
  assistant records for the live-manager and durable-snapshot branches
  (the recording path skips system rows, matching the background-row
  path); host-peer write-gate confinement.
- e2e (model-independent lane): live serve with NO provider configured —
  `session/append_message` persists user + assistant records, a
  `session/messages_page` read sees them with `source`, a retry returns the
  same seq, and a second connection receives the live projection envelope
  (proving no agent loop was involved: the serve cannot run turns at all).
