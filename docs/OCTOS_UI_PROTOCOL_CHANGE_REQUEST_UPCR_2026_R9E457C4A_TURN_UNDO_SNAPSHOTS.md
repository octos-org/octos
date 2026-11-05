# UPCR-2026-R9E457C4A — Turn undo snapshots on the serve path

Status: proposed (implemented for review)
PR: TBD

## Decision record

`UPCR-2026-R9E457C4A`.

The #1768 git-backed workspace snapshots (`snapshots.enabled`, opt-in, default
OFF) never took effect for `octos serve` turns: the WS turn path rebuilds a
per-turn agent from `Agent::new_shared` and dropped the session agent's
`snapshot_manager`, so `snapshot/list` stayed empty after real write_file
rounds. The per-turn agent now re-attaches the manager, restoring the existing
per-batch pre-mutation snapshots on the serve path.

Additionally, each WS turn records ONE turn-level undo point before the agent
runs, labelled `pre-turn <turn_id>`. Restoring it reverts everything the round
changed — file-tool writes and shell deletions alike — regardless of how many
tool batches the round contained. Snapshot failure (missing git, index
contention) is logged and never blocks the turn.

`snapshot/list` rows gain an optional `turn_id` field, present only when the
label matches the `pre-turn <turn_id>` convention; the field is absent (not
null) for older `pre-tool:`/`pre-restore` rows. `snapshot/restore` is
unchanged: it refuses during an active turn, takes its own `pre-restore`
snapshot first, so undoing a restore ("undo the undo") returns the workspace
to the post-round state. Conversation history is ledger-backed and untouched
by restores.

No new methods, capabilities, or permission changes; the `snapshots` config
remains opt-in and default OFF. Older cores simply never emit `turn_id` rows.
