# Incremental post-turn context persistence

This follow-up starts from `ea7b4e9c7` in its own worktree. It does not replace the frozen experiment binaries or modify producer workspaces.

## Problem and bounded change

The canonical session transcript already appends messages. The expensive duplicate operation was deriving, cloning and rewriting the entire context ledger after those messages committed. The batch fix reduced many full rewrites to one. This change makes that ordinary post-turn operation an appended context transition, retaining the full checkpoint as the recovery baseline.

This follows the append-and-replay principle reviewed in the local Codex source; it is not Codex format compatibility or architectural parity. Existing mid-turn, manual compaction and other general context writers still install full checkpoints. There is no new scheduler, database or provider behavior.

## Format and acknowledgement

A v2 checkpoint gains an optional first-field `journal_checkpoint_id`; legacy snapshots without it still load. Each checkpoint owns a separate ID-derived JSONL sidecar. New checkpoint installation first creates its empty sidecar, then atomically replaces the root snapshot, then removes the obsolete sidecar. Interruption before root replacement leaves the old root authoritative; interruption after replacement leaves the new root authoritative, even if the obsolete journal is corrupt or incomplete. Unreferenced sidecars may remain after a hard crash.

Each checksummed, versioned transition binds the checkpoint identity, preceding byte offset and acknowledged canonical item count. It records an exact replacement suffix from the first dirty canonical index, the ordered active IDs, source-history chain, context state, compaction records and cache epoch. Dirty tracking includes in-flight source stamping, ordered insertion, source reclassification, normalization/deletion and media attachment. It is independent of generation and ledger length, which are insufficient mutation identities.

The writer appends a complete line and awaits `write_all` and `flush` before acknowledging the cursor. It does not call `sync_all` or promise power-loss durability. The existing session transcript append still happens first; the success/error terminal remains after the batch finish attempt. Cooperative task cancellation checkpoints the merged prefix through the existing guard destructor. There is no new per-tool crash guarantee or all-or-nothing transaction.

All context file entry points use one in-process lock per root path. Both full and incremental writers reject an obsolete checkpoint/offset rather than replacing newer acknowledged context. Scratch clones own independent dirty/cursor state. The prompt bridge transfers acknowledgements only for identical state; after a background merge it verifies preserved canonical tail rows, compaction/recovery facts and cache-route state before rebasing lineage. Metadata-only source classification updates are adopted even when generation and source watermark do not advance. Semantic tool-group IDs keep the existing local remapping; normalized call/result pairing remains the contract.

## Recovery and migration

Load validates the root, then applies complete checksummed transitions in order. An incomplete final line is ignored and only that unacknowledged suffix may be truncated by a subsequent writer. A complete corrupt record, mismatched cursor, missing acknowledged bytes or foreign checkpoint identity is rejected. Root normalization and removal of uncommitted rows remain dirty relative to the original disk vector, so the next transition cannot accidentally retain removed rows.

The existing committed-only rule is unchanged: source-less conversation rows are discarded, and compactions depending on uncommitted rows invalidate recovery from the context files. Exact canonical transcript coverage remains the final consistency check; mismatched source history rebuilds from the transcript. Valid checkpoint summaries/cache epochs survive replay. Derived source and semantic indexes are rebuilt on load instead of being rewritten on every transition.

A missing/legacy checkpoint is promoted by the first incremental commit using one full save. Older runtimes understand only the root checkpoint and will not replay the new sidecar. A downgrade therefore requires successful full-checkpoint promotion with this runtime first; it is not safe to assume an old runtime preserves transition-only metadata. No automatic production migration/deployment is performed by this experiment.

## Costs and limitations

Ordinary transition writes avoid historical semantic-index derivation and full payload rewrite. They still cost the changed canonical suffix plus active-ID and compaction metadata, and `state()` still derives active state. Per-row source-history witnessing, in-memory merging, status publication and the final synchronous write remain. An early insertion or classification can make the replacement suffix large. This is not a claim that the whole turn or every persistence path is O(delta).

Recovery reads the root and current sidecar, validates source coverage and normalizes context. This slice does not implement a SQLite projection, fast restart index, cross-process file locking, per-tool journaling or bounded log rotation independent of existing full checkpoints. Multiple runtime processes must not concurrently own the same context store. Full checkpoint writers normally retire the previous journal; crash leftovers require later housekeeping.

The narrow media fix makes the canonical ToolOutput carry the same media as its active copy. Without that coherence, either the old snapshot or a transition could lose model-visible media on restart.

## Verification

Focused regressions exercise immutable baseline + transition replay; stamped twins and ordered insertion; metadata-only updates; compaction/cache/supervisor facts; media; legacy migration; stale full and incremental writes; independent clone tracking; incomplete and corrupt writes; checkpoint promotion; committed source mismatch; removal of crash rows; retry after write failure; and bridge iterations with background metadata and effective-route changes followed by commit/restart. Existing context, tool-pairing, cancellation, background-merge, terminal-order and recovery tests are run separately.

The ignored `benchmark_long_postturn_context_batch` uses a deterministic long history and the same completed response rows for three paths: per-row full checkpoint, one full batch checkpoint, and one transition from a previously installed baseline. Baseline construction is outside timing. It verifies equal normalized prompts and source heads after reload and reports bytes written, checkpoint rewrites and transition appends. This is a provider-free debug diagnostic under host contention, not a production performance estimate. Exact commands, results and caveats are stored in the experiment root alongside the logs.

## Resume and rewind

The existing `session/rollback` API appends a canonical rollback marker and trims the visible conversation; it does not undo workspace files or tools. Its rebuilt context and the session actor reset helper explicitly start a new persistence lineage, retiring later summaries/cache state. The stale-writer check continues to reject ordinary old scratch clones.

There is a crash window after the canonical rollback marker commits and before the new context checkpoint is installed. Existing exact source-history count/digest (or exact source head) validation rejects the surviving future context during normal session-open recovery; no new rollback identity protocol is needed. The regression simulates this boundary using the real SessionManager rollback and session-open context path. A separate handler-level test rewinds from transitions plus compaction, reopens the folded transcript, appends a new branch at reused sequence positions, reloads, then promotes/reloads a new full compaction checkpoint. Dropped messages, supervisor facts, summaries and cache route must not reappear.

Cooperative cancellation now has a pre-existing checkpoint in its fixture, proving the guard appends a partial completed-row transition without a root rewrite. Partial-line recovery and failed-write retry are separate tests. These checks do not promise resuming uncommitted tool side effects or transactionally undoing filesystem changes.

## Recorded diagnostic results

The focused regression selection passed **152 tests, 0 failures, 1 ignored** in 2.19 seconds. The ignored benchmark passed separately in 63.07 seconds with no provider calls. It used 12,582 history rows (12,583 canonical items after compaction, 2,902 active items), then the same 12 completed response rows for all paths.

| Persistence path | Measured time | Full checkpoint installations | Transition appends | Written context size |
| --- | ---: | ---: | ---: | ---: |
| Legacy per-row full checkpoint | 35.785 s | 12 | 0 | Final checkpoint 26,098,136 bytes |
| One full batch checkpoint | 3.003 s | 1 | 0 | Checkpoint 26,098,136 bytes |
| Incremental batch from installed baseline | 0.951 s | 0 | 1 | Transition 278,909 bytes |

Final normalized prompt and source head were equal across all paths and after reload. The incremental baseline was unchanged byte-for-byte. Fixture setup took 7.355 seconds outside the measured paths. Sequential execution order was legacy, full batch, then incremental; the raw log's historic two-arm `order` label was corrected in the benchmark source afterward. This correction changes diagnostic metadata only.

This single debug-profile measurement ran on a shared 18-logical-CPU host. The four A/B runtime CPU samples at benchmark start were 0%, 7.5%, 0% and 18.6%; other host load and cache/order effects remain uncontrolled. Timings cover context handling/persistence of synthetic completed rows, not provider, GPU, UI or total agent-turn latency. Snapshot counts and semantic equivalence are the principal evidence. The new binary is staged separately for a subsequently identified comparison phase; earlier experiment binaries and results remain frozen.
