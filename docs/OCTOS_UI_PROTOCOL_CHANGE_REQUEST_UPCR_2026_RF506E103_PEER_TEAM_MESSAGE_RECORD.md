# UPCR-2026-RF506E103 — Workspace peer message record

Status: proposed (implemented for review)
PR: https://github.com/octos-org/octos/pull/3201

## Decision record

A member of an automatic workspace team (`peer.workspace_team.v1`) can receive
a message from another member. Until now the recipient ran a turn on that
message and nothing recorded it: the recipient's history held a reply with no
cause, an attached client showed an answer nobody asked for, and later turns
of the recipient's own model no longer saw what it had replied to.

This change records each delivered message in the recipient's history and lets
a client show it as an attributed record. It is item 1 of
[`docs/adr/peer-collaboration-parity.md`](adr/peer-collaboration-parity.md).

### Stored record

When a workspace peer message is dispatched to its recipient, the prompt the
model is given is committed to the recipient's session history before the
provider is called, as one row:

| Field | Value |
| --- | --- |
| `role` | `user` |
| `source` | `workspace_peer_message` |
| `participant` | the sending agent id (`workspace-2`), when it is a valid participant label |
| `content` | the attributed prompt, unchanged: the `[workspace-peer-message]` guidance followed by the delivery envelope as the last line |
| `thread_id` | the thread of the recipient turn the message starts |

The row is written for every recipient, whatever clients are connected. A
failed write fails the turn before the model runs, the same rule a user's own
prompt follows. Interrupted-prompt recovery writes the same attributed row.

The content is deliberately the attributed prompt and not the bare message. A
later turn replays history to the model, and a bare `user` row would present
another agent's words as a direct human instruction.

### Wire contract

Capability: `peer.team_message_record.v1`, requested through
`X-Octos-Ui-Features` (or `client/hello` on stdio). It is never implied: not by
a missing feature header, not by the first-server slice, and not by the stdio
defaults.

**Notification `peer/team/message_received`**, durable, routed on the
recipient session:

```json
{
  "session_id": "team:local:tui#coding-b",
  "topic": "coding-b",
  "thread_id": "0193…",
  "message_id": "team:local:tui#coding-b:4:1762776000000000000",
  "seq": 4,
  "persisted_at": "2026-11-10T12:00:00Z",
  "peer_message": { "from_agent": "workspace-1", "text": "…", "assignment": true }
}
```

- Emitted once, when the row commits, so it precedes the reply it causes.
- `seq`, `thread_id` and `message_id` equal the hydrated row's, so a client
  that receives the notification and later hydrates the row shows it once.
- `assignment` is omitted when false.
- Delivered live and on reconnect replay only to connections that negotiated
  the capability.

**`session/hydrate` and `session/rollback` rows** gain an optional
`peer_message` object with the same three fields. It is present on the
recorded row for connections that negotiated the capability. The row's
`content` stays the stored prompt; clients render `peer_message` and never
present the row as typed input.

No request method is added and `peer/team/message` is unchanged.

### Compatibility

- **A connection that did not negotiate the capability sees no change.** The
  recorded row never becomes a `user_message` projection envelope for anyone,
  the notification is filtered, and `session/hydrate` / `session/rollback`
  omit the row. `seq` remains the history index, so an omitted row never
  renumbers another row; such a client sees a gap in `seq`, which the
  `after` cursor already tolerates.
- `HydratedMessage.peer_message` is additive and absent on every other row,
  which keeps its bytes.
- The legacy listings (`GET /api/sessions/{id}/messages` and
  `session/messages_page`) already expose each row's `source`; they return the
  recorded row with `source: "workspace_peer_message"` and no structured
  sender. They are not changed here.
- **The recorded row is not the person's prompt.** It roots no user turn for
  `session/rollback`, so `num_turns` counts the same prompts whether or not
  the client sees the record, and the turn the message started is kept like
  any other turn no person's message roots. It never titles the session and is
  never the `last_prompt` of `session/list`.
- Memory extraction reads the row with its participant label, the rule
  UPCR-2026-053 introduced for records that name who spoke.

### Not changed

Delivery, queueing, receipts, occurrence ids, the model-visible prompt text and
the `peer_*` tools are unchanged. Team membership still grants no privileges.
Staged peers (`peer_handoff`) keep their existing `peer_send_input` path, which
persists a plain user row by design.

### Verification

- `crates/octos-cli/tests/serve_workspace_team.rs`
  `should_record_attributed_peer_message_when_delivered_to_a_team_member`: a
  real `octos serve --shared --solo` process, three connections and a local
  model stub. The opted-in recipient window receives the notification before
  the terminal and no `user_message` envelope; a second window on the same
  session that did not opt in receives neither; each window's hydrate matches.
- `crates/octos-bus/src/resume_policy.rs`
  `should_not_count_a_recorded_peer_message_when_rolling_back_user_turns` and
  `crates/octos-bus/src/session_tests.rs`
  `should_not_title_or_preview_a_session_from_a_recorded_peer_message`.
- `crates/octos-cli/src/api/ui_protocol_peer_message_record_tests.rs`: the
  committed row and its projection through the real commit observer,
  interrupted-prompt recovery, the capability filter, hydrate visibility,
  reading the sender and body back from a row, participant label validation,
  capability advertisement, and the wire shape.
