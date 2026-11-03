# Post-turn context checkpoint follow-up

This is a separate follow-up to `61bcb72d4`, not a change to the frozen A/B executables or running producers.

## Observed problem

MiniMax-new returned `EndTurn` at 2026-10-11 19:38:50 UTC, but the runtime continued working before delivering the terminal. A read-only sample of PID 90717 at 19:40:57 showed 1,384 of 1,567 foreground samples inside `record_appui_context_manager_message` and full context snapshot persistence; there were no `progress_delivery` frames. The process used about one CPU core. Its snapshot contained 12,594 canonical items and occupied 27.78 MB.

The session journal advanced from 2,195 rows at 19:41:56 to 2,202 at 19:42:21. This was slow forward progress, not a blocked progress flush. The existing completion loop normalized, derived semantic blocks, hashed and serialized the full context after every appended response row. The unoptimized experiment build and long inherited history made this repeated work visible; no release-profile speed comparison was performed.

## Change and correctness boundaries

`AppUiContextCommitBatch` merges each successfully committed transcript row immediately, in its original sequence order, and checkpoints the derived context once after the foreground response batch. Journal writes, per-row sequence/cursor advancement, segment identity, tool-call normalization and authorization paths remain unchanged. The success/error terminal and self-paced continuation stay after the final checkpoint attempt and session-lock scope. Failed/truncated response carriers use this same path.

The guard retains no mutex across asynchronous journal appends. If the task is cancelled or unwinds, its destructor checkpoints rows already merged. The existing immediate interrupted-terminal acknowledgement still precedes waiting for the aborted task; this follow-up does not delay that acknowledgement or alter its authorization gate.

The canonical transcript remains authoritative. If a process terminates before destructors run, the derived checkpoint may lag; existing journal-tail reconciliation/rebuild recovers committed rows. A compaction depending on uncommitted rows must still be rejected/rebuilt. This does not add an fsync/power-loss guarantee, an all-or-nothing transaction, or a guarantee that every in-flight cache epoch/summary survives a hard kill. Checkpoint I/O failures retain the existing warning-and-rebuild behavior.

Only the foreground batch checkpoint is coalesced. Background writers retain their existing independent checkpoints. Per-row context merging and journal appends still have costs, and the final full snapshot can still be expensive. No measured production speedup is claimed.

## Verification

Four focused tests cover: deferred single checkpoint with the same normalized projection/source hash as the former per-row path; task cancellation after real transcript appends; stale-checkpoint recovery retaining committed compaction and tool pairing; and recovery after a checkpoint-write failure without fabricating a final answer.

Seventeen existing regression tests cover segment identities, terminal FIFO order, interruption races, background merges, committed usage, truncated voice directives, and recovery from stale/aborted/uncommitted context. The production-only library is checked separately with default features disabled and `api` enabled. Exact results and commands live in the experiment root's `postturn-batch-results.json` and referenced logs. No global executable is built or installed, and no active A/B binary is replaced.

## Opt-in synthetic checkpoint diagnostic

`benchmark_long_postturn_context_batch` is ignored in ordinary tests. It creates deterministic tool-call/result history in temporary directories, installs a summary retaining complete recent tool pairs, and compares the former per-row context checkpoint with the batch helper on the same completed response rows. The legacy phase stops after roughly 45 seconds at a complete tool-pair boundary and appends a final response if necessary; this is a bounded diagnostic, not a timing assertion.

Run it with:

```sh
cargo test --offline --locked -p octos-cli --lib --no-default-features --features api benchmark_long_postturn_context_batch -- --ignored --nocapture --test-threads=1
```

The 2026-10-11 debug run used 12,582 history rows, producing 12,583 canonical and 2,902 active items after compaction. Its complete response batch contained 12 rows: a user message, five tool-call/result pairs and a final assistant message. Both resulting snapshots contained 12,595 canonical and 2,914 active items, occupying 26,098,077 bytes each.

| Context update/checkpoint path | Snapshot installs | Measured seconds |
| --- | ---: | ---: |
| Former per-row checkpoint | 12 | 37.297 |
| One checkpoint after the batch | 1 | 2.930 |

The normalized model prompts and source-head hashes were equal in memory and after reloading both snapshots. Fixture construction took 5.915 seconds outside the measured paths; the full test, including equality checks and reloads, took 55.36 seconds. There were no provider calls. The timed paths include context merge, status publication and snapshot persistence; they do not measure model inference, tools, journal appends, UI delivery or the full turn.

The snapshot-count reduction is the direct result. The approximately 12.7-fold elapsed-time difference is one diagnostic observation, not a production speedup claim: this was an unoptimized build, legacy ran first, filesystem/cache order was not controlled, and three other debug runtimes each used about one CPU core on the 18-logical-CPU host. It is not a release-profile benchmark or repeated statistical experiment. Exact output and host observations are retained in the experiment root's `benchmark-postturn-batch.log`, `postturn-benchmark-results.json` and `postturn-benchmark-contention.json`.
