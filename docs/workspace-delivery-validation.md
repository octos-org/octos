# Root artifact delivery checks

`validation.validators` accepts `phase = "delivery"`. These checks are loaded
before a root conversation turn or root task starts, and run before an ordinary
terminal model response is accepted. They use the existing `ValidatorRunner`,
sandbox, timeouts, typed outcomes and durable evidence. They do not depend on a
plugin name, a dedicated editing tool, or a `slides/` project directory.

```toml
[[validation.validators]]
id = "artifact.final"
phase = "delivery"
required = true
kind = "command"
cmd = "/usr/bin/python3"
args = ["/host-owned/check_delivery.py"]
timeout_ms = 600000
```

A required failure is returned to the agent as concrete validator feedback in
its ordinary conversation. The agent can repair and retry within its existing
budget. The policy is pinned for the turn: editing its file mid-turn cannot waive
checks. The budget grace response cannot override a failed delivery check.
Whole-deliverable checks do not run against spawned page workers; their existing
completion checks remain in effect. Without delivery validators, existing behavior
is unchanged. Cancellation and runtime failures can still end an unfinished task;
this gate does not convert them into a delivery.

A completed delivery check refreshes loop activity, including a failed check:
new validation evidence is progress the agent can act on. A check that runs
longer than the idle window must not cause an immediate idle stop on the next
repair iteration. This does not reset explicit token/iteration budgets or
generate synthetic heartbeats; validator timeouts and cancellation still apply.

This is an opt-in artifact workflow, not a generic requirement for every chat.
The host should protect the contract and checker code against producer writes,
bind checks to actual artifact bytes, and keep trusted evidence outside producer
write permissions. Changes to artifacts invalidate previous evidence. The kernel
does not interpret model-written acceptance flags, and contains no presentation
specific quality policy. Expensive checker implementations should cache immutable
evidence and distinguish infrastructure failures from actionable content defects.

Native application validation and renderer validation are different evidence.
An independent visual model can improve feedback coverage but is not proof that
every visual defect has been found. Hosts must preserve this uncertainty.

Keep the kernel, artifact adapter and project acceptance policy separate. The
kernel invokes a command and consumes its result; it must not contain KDN, PPTX,
page-count, typography or model-specific quality rules. A PPTX adapter can render
and inventory a presentation, while a host-owned project contract supplies its
expected text, page order and optional typography/reference requirements. A code
project can instead use build/test validators; a data artifact can use schema and
reconciliation checks. Enabling delivery checks does not automatically impose
presentation checks on other tasks.

For example, the generic `pptx_host_check.py` adapter in the PPTX skill reads a
project contract, supports decks without reference images, and selects the host
renderer/reviewer from configuration. A structure-only pass must not be reported
as visual acceptance. Missing required capabilities are errors, not skipped
successes. Project contracts may specify different consistency groups (for
example, centered covers and left-aligned body titles) without changing code.
Changing a checker or contract during a measured run changes the treatment;
stage the next version separately and preserve the active run's policy.

The context projection also uses the catalog history-replay capability, rather
than the sampling-control style. Kimi continuation fields must survive both
ContextManager reconstruction and the wire adapter; serialization tests alone
do not prove that the earlier projection retained them. Regression coverage
includes snapshot/resume and keeps private reasoning out of child forks.

## Recovery and observation

A producer's `blocked` declaration while a required checker is pending is not
an accepted delivery. Continuous hosts should keep observing, retain per-artifact
evidence, and deliver each distinct completed check result once. Steer an active
root turn through its native input queue, or start a continuation when the root
is idle. Do not start a second turn while the runtime reports an active one. Failed checker
reports are infrastructure outcomes; valid findings remain artifact feedback.

The supervisor event store uses a persistent OS advisory guard and a compatible
append marker. Process termination releases the guard automatically. An orphaned
new-protocol marker can then be recovered; a legacy marker is recovered only
after a positive dead-owner probe. Live or unknown legacy ownership is never
inferred stale from age alone. The guard file must not be unlinked while writers
may use it. Crash and contending-recovery tests cover this persistence path.

The task ledger also serializes appends with an OS lock and writes each JSON
record and its newline together. Readers share the same lock, recover adjacent
complete records from legacy interleaving, and ignore incomplete fragments.
Before a new append, a torn tail is separated from the new record so a crash
cannot hide later task updates. This preserves task identity and latest-state
ordering; it does not infer completion from a partial record.

Tool feedback must preserve content-addressed artifact paths and checksums. The
tool-output sanitizer no longer treats every 64-character hex string as a
credential. Recognizable credential prefixes and secret assignments remain
redacted, including JSON and escaped JSON fields. A loop-level regression checks
the exact evidence locator received by the next model request, alongside a
redacted credential, so a passing checker cannot become an unreadable path.

Waiting on a restored task also refreshes owned task rows from the shared ledger
while polling. A later turn must see completion persisted by the original owner
without importing foreign tasks or cancelling the original worker. Regression
coverage writes a terminal result through the durable channel after waiting has
started, and requires the waiter to return that result and its artifact path.
