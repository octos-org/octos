# Native spawned-worker execution policy

The native `spawn` tool uses `AgentConfig`'s iteration convention in both sync
and background mode: omitted `max_iterations` and explicit `0` mean unlimited.
Positive values are preserved; there is no implicit 150-iteration default or
300-iteration ceiling. Unlimited iterations do not disable cancellation, token
or cost budgets, tool permissions, sandboxing, timeouts, or concurrency policies.

## Host-owned iteration policy

Set `OCTOS_SPAWN_MAX_ITERATIONS` before constructing the runtime's spawn tools:

```sh
OCTOS_SPAWN_MAX_ITERATIONS=0 octos serve
```

The value is captured at tool construction, inherited by nested native spawns,
and cannot be changed by model-authored spawn arguments. A host value of `0`
requires unlimited iterations even if the model requests `45` or `50`. A positive
host value is a ceiling: an omitted or zero model value uses that ceiling, while
a smaller positive request remains smaller. Invalid values fail dispatch before
a worker is registered. Restart/reconstruct the tools to change the process
setting; it is not a live environment toggle.

Embedders may instead call `SpawnTool::with_host_max_iterations(0)` (or a positive
ceiling). This explicit builder setting overrides the construction-time process
setting. `with_agent_config` continues to carry token budgets, timeouts and the
other worker configuration; it does not silently turn a parent's default
iteration count into a child policy.

These controls apply to Octos's native worker loop. An external `agent_mcp`
backend owns its own execution limits; path references can be forwarded to it,
but this setting does not rewrite the external backend's iteration policy.

## Explicit skill references

An orchestrator can preserve canonical instructions without copying its entire
conversation into the worker:

```json
{
  "task": "Read jobs/worker-a/TASK.md and produce its declared handoff.",
  "skill_paths": [
    "skills/domain/SKILL.md",
    "skills/domain/references/worker-contract.md",
    "tools/TOOLS.md"
  ],
  "mode": "background"
}
```

`skill_paths` is an optional array of nonempty strings. The task carries escaped
source paths and a requirement to read them through the child's permitted tools.
Relative paths resolve against the child's working directory, including a
dedicated worktree when one is used. The harness performs no privileged reads
and does not grant access merely because a path appears in the list.

Path delivery is not evidence that the worker actually read or applied a skill.
The caller must supply an appropriate read capability and accessible inputs, and
the workflow must validate the worker's actual evidence. A task packet or local
summary must preserve the referenced instructions instead of replacing them.

## Model-visible tool output

The production ContextManager uses each tool's execution byte budget by default.
It preserves an already bounded `read_file` page or Bash result, including the
tool's continuation advice, warnings and full-output log reference, instead of
applying a second preview-sized cap. UI previews remain independently bounded.
An explicit caller projection limit still applies exactly; a cut native read
names `start_line` and `limit` for a usable continuation. `recall` is optional
and is not assumed to exist in a restricted worker's tool set.

If diagnostics appended after execution make a result exceed its default tool
budget, projection retains a bounded head and tail and identifies the omitted
bytes. Context-pressure compaction can still reduce older output. These rules
do not grant tools or file access, and they do not make an earlier truncated
provider request complete retroactively.

## File-read knowledge belongs to the worker

When file-read caching is enabled, inline and background workers start with an
empty cache using the parent's capacity settings. This also applies to shared
workspaces, dedicated worktrees and nested spawns. A parent's or sibling's read
must never make a worker's first read return `FILE_UNCHANGED`: context forks can
omit the original tool-result body. The worker's own repeat reads still benefit
from caching. Fresh recovery messages reset only that worker's read claims.
Output routing and summary services remain shared; they do not imply shared
knowledge of file contents.

Native task loops apply the inherited workspace compaction policy before their
first model request and after subsequent tool results, including when the
workspace budget is smaller than the provider context window. A caller-owned
ContextManager remains the sole prompt owner when present. Compaction or prompt
repair that removes visible tool-result bodies invalidates cached read claims;
an unchanged or append-only prompt projection preserves them. This prevents an
unchanged file from becoming unreadable after its previous body leaves context.

File-read caches are not locks or cross-worker write authorization. Writes
invalidate the writer's cache; other contexts normally notice changed disk
mtime on their next read. Same-mtime rewrites and conflicting concurrent writes
still require appropriate artifact ownership and stronger validation where
needed. Do not restore shared read claims as a substitute for that coordination.

## Recovery boundaries and completion ownership

A cancelled or interrupted native child does not start a fresh recovery run.
Structured stop events and a typed provider quota error also prevent recovery.
Unbounded workers retain one repair attempt after an eligible error.

Fresh runs currently reset run-local usage counters. Therefore, when a finite
iteration or token budget is configured, a failed worker returns its
original failure rather than silently receiving a second allowance. This applies
even before exhaustion: errors do not always expose exact remaining usage, and
the task API does not yet accept a remaining-budget continuation. This is a
conservative stop, not seamless continuation under the original budget.

`max_timeout` is an inactivity guard, not a cumulative wall-clock allowance.
Merely configuring it does not disable recovery. An actual activity-timeout
or interruption event is terminal and is never retried by this wrapper.

Background spawn replies include the actual supervisor task ID. Wait/read calls
must use that ID, not the display label. Native task registration atomically
preserves the original parent workspace and ledger lineage so later completion
notifications retain the same workspace identity as the originating session.

Implementation: [spawn.rs](../crates/octos-agent/src/tools/spawn.rs).
Behavioral coverage: [spawn_tests.rs](../crates/octos-agent/src/tools/spawn_tests.rs).

## Exclusive output claims for shared workers

Native shared `spawn` accepts `owned_paths`: workspace-relative literal files
or subtrees. `spawn_agent` and the `delegate` alias forward those claims. Before
launching a worker, the runtime atomically reserves all paths. An overlap with
another live owner returns `PATH_OWNERSHIP_CONFLICT`, its supervisor task ID
(or an explicitly separate lease ID during admission), label and paths. No
model request starts for that rejected worker. Distinct outputs remain parallel;
this adds no iteration, render or concurrency quota.

Claims use canonical existing ancestors and normalized missing suffixes;
file/subtree ancestors intersect by path components, not string prefixes.
Globs, traversal, the workspace root, symlink escapes and unresolved/dangling
symlink ancestors are rejected. Existing directory symlink aliases still resolve
to the same claim. The
registry is shared across native spawn objects and parent turns in this runtime
process. Each lease has an independent identity, so dropping an old owner cannot
release a successor. Claim ownership spans the actual sync/background future,
including recovery, validators and delivery cleanup. A terminal status, cancel
request or old timestamp does not release it while that future is still alive.
Ordinary native tool tasks also retain the lease: dropping a synchronous spawn
future on timeout cannot admit a successor while its already-running writer
can still modify the output. Release follows the last native execution owner.
The `spawn_agent` and `delegate` aliases use the native response's exact task ID;
they never infer ownership from the newest supervisor entry or a snapshot diff.

Hosts may set `OCTOS_REQUIRE_SHARED_WRITE_CLAIMS=1` before constructing the tools,
or use `SpawnTool::with_require_shared_write_claims(true)`. The policy is captured
and inherited by nested native spawns. Default is off for existing callers.
Strict mode rejects omitted claims for shared writable workers; only a closed
allow-list of known read-only file/query tools is exempt. Role names do not
establish read-only capability. Worktree-isolated workers keep their independent
directories and omit parent-workspace claims. External `agent_mcp` writers cannot
satisfy this contract and fail closed in strict mode. The separate legacy
`delegate_task` path also fails closed under strict mode or explicit claims;
use native `spawn`, `spawn_agent` or the `delegate` alias instead. Embedded hosts
using builder policies must configure every exposed dispatch surface consistently.

This is cooperative admission coordination, not a security sandbox. It does not
prevent undeclared writes, coordinate independent runtime processes or guarantee
that detached shell/PTY jobs and external services have stopped when the agent
future ends. Those writers need their own lifecycle leases or isolated outputs.
Existing symlink aliases are resolved; case folding is conservative on detected
case-insensitive filesystems. Hard links, later symlink changes and unusual
filesystem aliasing are outside this non-security contract. Related/nested
workers get no implicit exemption from overlapping claims. Use separate outputs
or wait for the real owner to stop; do not spawn a replacement writer merely
because it has a different label or its predecessor was marked cancelled.
