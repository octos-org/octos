# Task-scoped acceptance

The task's authoritative copy, page inventory, shared typography and editable
scope form its acceptance criteria. Keep those criteria independent of a candidate
so the producer cannot redefine success by editing a checker. Never hard-code
another deck's page numbers, strings or geometry into tools or skills.

Use [Rust runtime](rust-runtime.md) for deterministic file/layout reports and
[continuation evidence](continuation-evidence.md) for hash-bound review records.
The runtime rejects copy mismatches, stale artifacts and missing required inputs.
It does not contain an autonomous visual judge or the former private HTTP review
server. A host can supply its own independent review service through normal tools;
its permissions and credentials stay host-owned.

Declare actual check coverage. Package validity does not establish visual quality
or native PowerPoint acceptance. Missing semantic/visual review is unknown, not
pass. Keep real unresolved finding IDs when a new candidate is reviewed. The
existing host continuation observer owns stop/yield decisions.
