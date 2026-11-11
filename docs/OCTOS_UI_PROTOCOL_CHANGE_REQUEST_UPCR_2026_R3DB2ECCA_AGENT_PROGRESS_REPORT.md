# UPCR-2026-R3DB2ECCA — agent progress reports on `background/activity`

Status: proposed (additive, capability-gated, backward compatible)
PR: TBD

## Decision record

`UPCR-2026-R3DB2ECCA`.

### Problem

During a long turn the user sees tool activity but no statement of what the
agent found, what it will do next, or what is still unverified. Whether any
of that reaches the transcript before the turn ends depends on the model
volunteering it in reply text. `peer_publish` (#3142) lets a staged peer
leave a checkpoint, but only another agent's `peer_gather` reads it.

### Decision

A running agent can publish its own progress report to the user of its own
session. The report rides on the existing human-only `background/activity`
event (#2019) instead of a new notification, because that event already
provides everything a report needs: a required session routing key, origin
attribution, a per-origin cap with a visible drop marker, durable ledger
replay, and the guarantee that the payload never enters model context.

New capability `event.agent_report.v1`. It is effective only together with
`event.background_activity.v1`.

When both are negotiated by the connection that starts a turn:

- the turn's tool registry gains `report_progress`
  (`{kind, summary, next_step?, evidence?}`), and the stable system prompt
  gains one paragraph telling the model when and how to use it. The tool is
  optional, so it is reached through `run_tool`; the paragraph carries the
  call form, and the model needs no discovery step;
- each accepted call emits one `background/activity` row with
  `origin_kind: "agent"` and a new optional `report` object.

Wire additions on `background/activity` params:

| Field | Meaning |
| --- | --- |
| `origin_kind: "agent"` | New value in the open origin registry. |
| `origin_id` | `main` for a session's main agent. The session topic (`peer-<slug>`) for a staged peer. |
| `origin_label` | The peer slug for a staged peer. Absent for a main agent. |
| `text` | The agent's summary, at most 2,000 bytes. |
| `report.kind` | `milestone`, `blocker`, `correction` or `verification`. A client draws an unknown kind as `milestone`. |
| `report.turn_id` | The turn the report was published from. |
| `report.next_step?` | What the agent says it will do next, at most 500 bytes. |
| `report.evidence?` | A path or command the agent points to, at most 500 bytes. |

`report` is absent on `monitor` and `fleet` rows, so their serialized form is
byte-identical to before.

### Contract

- **Attribution is host-derived.** `session_id`, `profile_id`, `origin_id`,
  `origin_label` and `report.turn_id` come from the turn's own runtime state.
  The tool schema has no identity field and rejects unknown fields, so a
  report cannot claim another session, agent or turn.
- **A report is the agent's own statement.** It is not an approval, not an
  independent verification, and not a claim that the task is complete.
  `verification` means the agent ran a check itself. Clients must not draw
  any kind as a pass.
- **It wakes nobody.** No model turn is started and no agent is notified. The
  row is never routed into any model context, the reporting agent's
  included. The only thing the reporting model reads is the tool result.
- **Scope.** A report is delivered only on the reporting session's own
  ledger stream. Sessions that share a folder or a workspace team do not
  receive it. Delivery of a staged peer's reports to its originator session
  is a later change and will get its own record.
- **Cleaning.** Control characters other than newline and tab are removed
  from `text`, `next_step` and `evidence`, recognizable credentials are
  replaced through `octos_core::secret_redaction`, and each field is cut at
  its limit with a visible ` [...]` marker. A summary that is empty after
  cleaning is refused.
- **Cap.** The existing per-origin budget applies: 40 rows per 60 seconds per
  `(session, origin_kind, origin_id)`. When it trips, one marker row
  (`suppressed: true`, no `report`) is emitted and the tool result tells the
  model the report was not shown.
- **Durability and replay.** Unchanged from `background/activity`: the row is
  appended to the session ledger and replayed by cursor on reconnect. The
  ledger cursor of the row is its stable position; clients deduplicate on it.

### Compatibility

- A connection without `event.agent_report.v1` never receives a row with
  `origin_kind: "agent"`, live or on replay, cap marker included, and its
  turns are not offered the tool or the prompt sentence. Nothing changes for
  it.
- The capability is not part of the no-header baseline and is not a stdio
  default. A client must ask for it.
- `report` is optional and additive. A client that negotiated the capability
  but ignores the object still shows the summary text under the agent origin.

### Known limit

Live, with `deepseek-v4-flash` in `octoscode`: asked for a report, the model
called `run_tool(report_progress)` and the report was stored and drawn while
the turn was still running. Unprompted, it sent none in five multi-stage
tasks (12 to 22 tool calls each) across four wordings of the prompt
paragraph, and listing the tool directly made no difference. Its reasoning
shows it weighed the instruction only at the end of the task, where a report
is redundant with the reply. So this change delivers the path and makes the
tool usable on request. It does not make reporting spontaneous. That needs a
reminder from the host inside the turn, which belongs with the pacing change.

### Not in this change

- A host reminder that prompts the model to report during a long turn.

- Routing a staged peer's reports or `peer_publish` checkpoints to the
  originator session.
- Pacing above the hard cap, and a host heartbeat for a quiet task.
- Any task or goal association. The session and turn identify the report.
