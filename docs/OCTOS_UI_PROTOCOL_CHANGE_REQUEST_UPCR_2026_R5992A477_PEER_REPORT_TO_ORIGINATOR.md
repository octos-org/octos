# UPCR-2026-R5992A477 — a staged peer's reports in the session that staged it

Status: proposed (additive, no new capability, backward compatible)
PR: TBD

## Decision record

`UPCR-2026-R5992A477`. Builds on `UPCR-2026-R3DB2ECCA`, which left this
delivery for its own record.

### Problem

A staged peer works in its own session. Its `report_progress` rows land on
that session's stream, and its `peer_publish` checkpoints are read only by
another agent's `peer_gather`. The user is normally looking at the session
that staged the peer, so while a supervisor waits on peers the user sees
nothing of what the peers found.

### Decision

When a staged peer publishes a report, the host also emits the same
`background/activity` row on the stream of the session that staged it. Two
sources feed it:

- `report_progress` called in the peer's turn. The row on the peer's own
  stream is unchanged; a second row goes to the originator.
- `peer_publish`. The checkpoint is written exactly as before, and its text
  is also emitted to the originator as a `milestone` report. A peer whose
  own connection never negotiated `event.agent_report.v1` still reaches the
  originator this way.

The row on the originator's stream:

| Field | Value |
| --- | --- |
| `session_id` | The originator session. |
| `origin_kind` | `agent`. |
| `origin_id`, `origin_label` | `peer-<slug>` and `<slug>`. The origin stays the peer. |
| `text` | The report summary, or the checkpoint message. |
| `report.kind` | The peer's kind for `report_progress`. `milestone` for a checkpoint. |
| `report.turn_id` | The peer's turn, not a turn of the originator session. |

No field is new. A client tells a mirrored row from a session's own report
by `origin_id`: it names a peer other than the session itself.

### Contract

- **Audience.** The row goes to one session: the peer's recorded
  originator, read through `staged_peer_dir` and accepted only when it has
  the peer's profile and is not itself a peer session. That is the reading
  `peer_send_input`, `peer_respond` and the awaiting-input wake already
  trust. A missing, malformed, wrong-profile or peer originator means no
  row. Nothing is sent to other sessions in the folder, the profile or a
  workspace team.
- **Only a live peer.** The reporting session must be the trusted live
  session for its slug, and the peer must not be closed.
- **Host-owned app peers are excluded** (ADR 0007). Their person is in the
  app's own UI, not in the owning system agent's session.
- **Attribution.** The origin is the peer. The row is never presented as
  the supervisor's statement, and `checkpoint` text is a `milestone`: a
  checkpoint is intermediate by contract and is never a `verification`.
- **It wakes nobody.** The originator's model is not woken and the row
  enters no model context. Agents still learn of peer state only through
  `peer_gather`, `peer_list` and the existing wakes. Completion semantics of
  `peer_publish` are unchanged.
- **Size.** A checkpoint may hold 8,192 bytes and a report text 2,000. A
  longer checkpoint is cut and ends with
  ` [... cut; full checkpoint: peer_gather]`. The whole checkpoint stays in
  `checkpoint.json`.
- **Cleaning, cap, durability.** As in `UPCR-2026-R3DB2ECCA`: control
  characters and recognizable credentials are removed, the per-origin cap
  of 40 rows per 60 seconds applies to `(originator session, agent,
  peer-<slug>)`, and the row is appended to the originator's ledger and
  replayed by cursor.
- **Tool result.** When the row was admitted, the `peer_publish` result
  gains one sentence saying the checkpoint is also recorded as the peer's
  progress report for the user. `report_progress` results are unchanged.

### Compatibility

- Gated by the existing `event.agent_report.v1` on the originator's
  connection. A connection without it receives no `agent` row, mirrored or
  not, live or on replay. The row is still appended to the ledger.
- A client that negotiated the capability before this change starts
  receiving rows whose `origin_id` is a peer and whose `report.turn_id` is
  not one of the session's own turns. It must not treat such a row as
  belonging to the session's running turn.
- `peer_gather` output, `checkpoint.json` and the peer blackboard are
  unchanged.

### Not in this change

- Workspace-team members. They are independent sessions with no shared
  task, so their reports stay in their own session.
- Pacing, merging of a checkpoint and a report that say the same thing,
  and a host heartbeat.
