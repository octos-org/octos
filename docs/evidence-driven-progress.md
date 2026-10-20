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
- At convergence checkpoints, read optional `.octos/progress.json` evidence.
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
- Use durable job receipts with `watch_external_job` for completion. Its result
  is a job outcome, not artifact acceptance.

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
