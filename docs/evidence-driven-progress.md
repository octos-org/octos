# Evidence-driven progress

This change addresses the KDN comparison's repeated shell edits, unverified
checkpoint summaries, and unnecessary whole-deck rebuilds. The kernel remains
task independent: presentation-specific checks belong to the shared skill.

## Implementation contract

- Observe bounded workspace file changes independently of tool names. Observe
  shell changes even after a nonzero exit. Coalesce notifications and compare
  contents so no-op writes do not count. Exclude dependency/runtime directories;
  expose incomplete observation rather than treating missing events as proof.
- Associate observations with the workspace/turn, not a particular shell process
  when concurrent workers make attribution ambiguous. Do not widen file scope.
- At convergence checkpoints, read `.octos/progress.json` evidence and adapter
  reports in `.octos/tool-progress/*.json`.
  Check reports identify their input artifact hashes and findings. Validate
  confinement, hashes, report status and freshness. These checks establish
  evidence consistency, not the correctness of an arbitrary user-written test
  or the truth of a visual judgment.
- Feed measured changes and current/stale checks into reflection. Do not record
  empty or truncated reflection as a successful review. Periodic checkpoints
  with fresh, improved evidence can proceed without another model call.
- Repeated edits trigger scoped diagnosis, never a forced quality pass, a model
  substitution, or automatic task abandonment. Preserve ordinary tasks that do
  not supply a progress contract.
- Automatically register durable job receipts returned through the adapter
  metadata contract with `watch_external_job`. Its result is a job outcome,
  not artifact acceptance.

## Closing the first-trial integration gap (PR #2842)

The first full-deck trial did not use the optional helper or watcher. It made
65 reflection calls and published no index; passing the isolated smoke tests
did not establish full-workflow effectiveness.

The follow-up improves three specific behaviors:

1. Tool results carrying `octos_external_job` register the existing native
   watcher through the ordinary dispatcher, without another model call.
   The integration test `real_shell_receipt_automatically_registers_one_watch`
   uses an actual shell result; duplicate receipt observations share one watch.
   Session policies, hooks, path confinement and background-task limits apply.
2. Tool adapters publish hash-bound operation reports directly, without a
   model-authored plan/index. `should_discover_adapter_report_without_model_index_and_not_call_it_quality`
   proves discovery, stale rejection and unchanged-input deduplication.
   Operations use `kind: "operation"`; their execution success is **not** a
   content/visual check pass. `role: "output"` bindings are freshness-checked
   but do not make a changed timestamp or job id look like new input progress.
3. File-churn diagnostics accompany an ordinary action request. They do not
   spend a tools-disabled reflection call and are coalesced across files for
   at least 20 action calls by default. Periodic checks remain available when
   evidence is missing or stale. Existing asynchronous-peer polling behavior
   remains unchanged. `should_coalesce_churn_diagnostics_without_spending_reflection_calls`
   pins the bounded cadence.

The service-side Python example is
[`receipt_adapter.py`](../examples/external-jobs/receipt_adapter.py). The KDN
tool service calls it for every job; the same adapter/helper files go to both
experimental arms. No fixed slide text, layouts or quality judgments are
introduced. Preserve trial 1 and record the new release as a different version
before comparing requests and delivered quality.

Automatic reports are bounded to 64 files of 256 KiB each. Adapters should
replace a small, stable set of latest-operation reports, not append one report
per job forever. Individual outcomes remain in their durable job receipts.

## Optional report format

`.octos/progress.json` contains a versioned list of report references:

```json
{"schema_version":1,"checks":[{"id":"copy","report":"verification/copy.json","sha256":"<report SHA-256>"}]}
```

Each report binds its checker/manuscript and candidate artifacts, and explicitly
closes previously observed findings:

```json
{"schema_version":1,"check_id":"copy","status":"pass",
 "artifacts":[{"path":"exports/deck.pptx","sha256":"<artifact SHA-256>"},
              {"path":"source/check_copy.py","sha256":"<checker SHA-256>"}],
 "findings":[{"id":"missing-copy","status":"closed"}]}
```

Reports allow `pass`, `fail`, or `unknown`; findings allow `open` or `closed`.
Reports and references must be workspace-relative regular files. Reads reject
symlinks and traversal on Unix. Limits are 64 checks, 256 KiB per report, 16
artifacts per check, 256 MiB per artifact, and 512 MiB total hashing per refresh.
Unchanged Unix inode/size/mtime/ctime stamps reuse hashes; a metadata change
during hashing invalidates that read. This is a bounded consistency check, not
an adversarial attestation service or an independently trusted checker.

Observation is per turn and bounded to 512 directories, 8,192 small source files,
1,024 pending paths, 1 MiB per source and a 32 MiB scan budget. Hidden paths,
dependency trees and binary artifacts are outside source-churn counting. Native
events may be coalesced or delayed. Incomplete observation is exposed in the
checkpoint summary, never interpreted as proof of no edits. Task reports and
findings remain on disk for resume; in-memory churn/history counters restart.

## Validation and comparison

Regression coverage must include shell and failed-command changes, no-op writes,
atomic replacement, symlink confinement, bounded observation, stale evidence,
unchanged evidence, truncated reflections and progress that really improves.
Run focused tests, existing convergence/execution tests, clippy and formatting.

The shared PPTX helpers will batch build/render/check work, support changed-page
inspection and calibrate representative page types before scaling. No fixed
page numbers, layouts or producer-specific quality feedback. Pi and Octos get
identical skill/helper inputs. Keep old deliveries and traces immutable.

Run three fresh isolated full-production trials per harness after smoke tests,
plus a separate comparison using common generated images to isolate conversion.
Record first-delivery quality, completeness, requests, tokens, actual cost,
render work, waiting and wall time. Old Pi transport-recovery time is not a
clean timing baseline. Do not claim a winner before reviewing final artifacts.
