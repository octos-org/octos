spec: task
name: "Synthetic tool aborts survive context snapshot reload"
tags: [context-manager, compaction, cache, recovery]
---

## Contract

When the trusted new-user boundary closes a previous incomplete tool batch,
the generated missing-result output has `TranscriptItemSource::Synthetic`.
It has no canonical session source sequence and is not an in-flight runtime
result. Its semantic group, envelope, and canonical session coverage stay
unchanged. Reloading its snapshot must preserve the row and any installed
compaction, active projection, and cache epoch that depend on it.

Real source-less `ToolRuntime` rows remain uncommitted crash leftovers. The
loader must not infer synthetic provenance from an abort-looking text body.
Pre-fix snapshots that encoded an abort as `ToolRuntime` without a source
reference still take the conservative rebuild path if a compaction consumed
that row. This fix prevents recurrence; it does not claim to recover the
unprovable compaction authority in those legacy snapshots.

## Boundaries

Changes are limited to `crates/octos-cli/src/api/context_manager.rs`, its inline
tests, and this specification. No caller API, source-sequence assignment,
ghost-discard policy, provider behavior, or unrelated ledger format changes.

## Regression mapping

- `should_preserve_aborted_tool_compaction_when_snapshot_reloads`: persist and
  reload a partial parallel batch, close its missing result at the next durable
  user row, install compaction, and reload from disk with status `Loaded`;
  preserve compaction, projection, canonical hash, coverage, and cache epoch.
- `should_preserve_synthetic_abort_across_repeated_snapshot_loads`: an
  uncompacted closed batch retains exactly one abort, stable canonical hash
  and generation, and a closed semantic tool group over two disk reloads.
- `should_reject_legacy_uncommitted_runtime_abort_in_compacted_snapshot`:
  legacy runtime provenance is not upgraded by matching text; reject the
  tainted snapshot and rebuild from the exact durable history.
- Existing `should_drop_uncommitted_conversation_rows_when_reloading_after_a_crash`
  and `should_rebuild_from_history_when_a_snapshot_compaction_depends_on_uncommitted_rows`
  continue to reject real uncommitted conversation state.
