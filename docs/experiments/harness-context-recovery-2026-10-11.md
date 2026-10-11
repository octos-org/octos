# Harness context and recovery A/B patch (2026-10-11)

This experimental branch addresses the observed long-running deck agents repeatedly revisiting stale reflections, losing the host-visible completed action log on provider failure, over-counting transcript metadata during compaction, and dropping progress events when the UI queue filled. The control is the same upstream revision (`a4ef9e88a`) without this patch, not the previously installed binary.

## Behavior

- `octos serve` enables LLM compaction by default. Existing `--llm-compaction` remains valid; `--no-llm-compaction` and `--llm-compaction=false` select heuristic summaries. Per-session mode overrides retain precedence.
- The visible summary size is capped independently from generation tokens. A summary call uses low reasoning and at least 16,384 generation tokens when supported by the model, allowing reasoning plus actual handoff text. Model output limits and the existing 60-second timeout still apply. Logs distinguish a successful LLM summary from heuristic fallback; session status already exposes effective mode in the upstream baseline.
- A convergence reflection is appended once, at its original position in the continuing model history. It is consumed from the pending slot and is not moved to the tail on later actions. New reflections do not erase completed tool results.
- Wrapped reqwest request timeouts and statusless transport failures are classified into the existing bounded timeout/network retry buckets, preserving current turn history. No new provider/model fallback is introduced. With a single configured lane, retries remain on that lane.
- Handled provider/dispatch failures carry the conversation's completed append-only action log and usage to the host. The existing indexed persistence path saves those rows before emitting an error terminal, without inventing a final assistant answer. Inner error classification remains available. This is not a rewrite of every specialized error exit or the separate task runner.
- Compaction budgets count visible text, tool arguments and media rather than JSON ledger metadata, hidden reasoning or raw output sidecars. The bridge additionally reserves the provider's estimate for runtime system instructions and tool declarations. Snapshot loading accepts legacy token estimates while continuing to validate projection IDs and hashes.
- Foreground progress is queued in order, adjacent text deltas coalesce, and a flush barrier precedes the directly sent terminal result. Excess ephemeral text beyond 4 MiB is explicitly counted as lossy; tool/lifecycle events are not arbitrarily dropped on a full transport channel. Closed receivers are recorded, and reporter teardown drains pending events.

## Limits

This does not establish exactly-once external tool effects or crash-safe per-action session commits. A request retry can be billed twice by a provider after an ambiguous connection failure. The error carrier preserves completed work on handled errors; process termination still depends on the existing canonical snapshots and persistence machinery.

Token counts remain estimates, not the provider's tokenizer or a calibration against its latest actual usage. Different serialization and media policies can change the actual count. Compaction still summarizes history and cannot guarantee semantic fidelity.

The critical-event queue is deliberately non-dropping and has no hard byte cap. A persistently stalled live consumer can accumulate critical metadata; text is bounded. This patch does not redesign all background/WS transport queues or guarantee every event survives a disconnected client. Ordinary awaited channel delivery and durable ledger replay remain separate mechanisms.

The combined A/B compares the patch bundle, including default-on LLM summaries, not the isolated contribution of each fix. Keep model/provider, task snapshot, skills and bridge service identical within each old/new pair; record wall time, provider usage, accepted artifacts, repeated actions, fallback summaries and recovery failures.

Both experiment executables use the debug profile with `--no-default-features --features api`. The installed production executable includes additional default features; these experimental binaries are not a feature-equivalent replacement for that global installation.

## Validation

Focused tests cover one-shot reflection consumption and chronological retention, real local HTTP header timeouts after one completed tool call, bounded exhaustion preserving completed rows, generation-versus-summary budgets, legacy snapshot loading, tool/media estimates, ordered progress under channel saturation, receiver closure and reporter teardown. Exact command results are recorded in the parent experiment's test logs. This document does not claim an unexecuted full workspace suite or a measured performance improvement.

The completed focused run passed 106 agent tests and 130 CLI tests. CLI tests were run from the library harness, including 108 context-manager tests, four ordered-delivery tests, three bridge tests, five segment-identity tests, eight OUP compaction/terminal checks, one flag-default test and one reporter thread-ID test. An initial recovery fixture used a noncanonical tool-call ID; it was corrected to the normal `call_` form before the passing recovery run. An initial CLI `--bin` filter selected zero tests and is not counted.
