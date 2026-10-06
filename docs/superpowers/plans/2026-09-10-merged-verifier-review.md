# 2026-09-10 merged-verifier review repair

Branch `fix/merged-verifier-review-20260910` (base 329566d3). Scope: #2273
merged-review audit gaps. Review artifacts live in the outer repair
directory (verifier-red2.log / verifier-green.log and the two peer reports).

## Fixes

1. **Interactive sentinel failure warning carries the WIRE session id.**
   The shared constructor `goal_verifier_failure_warning` normalizes via
   `wire_key_from_goal_key` internally — one production boundary shared by
   the interactive call site and the tests. Goal lookups keep the scoped
   key. Plain sessions pass through unchanged. The AUTONOMOUS station's
   `goal_verifier_warning_event` intentionally does NOT strip: its argument
   is the turn's plain wire `session_id`; the scoped goal key
   (`goal_ctx.goal_session_key`) is a separate, independent value in that
   context.
2. **A replayed verifier failure no longer re-appends the durable /
   in-memory note.** `session_actor` appends only when `!outcome.replayed`;
   the `tracing::warn!` still fires on every refusal; goal status,
   charging, and TTL are untouched. Changed evidence produces a fresh
   verdict and a fresh note.
3. **Spec/plan sync.** Allowed Changes gained the two test files and this
   plan; Decision 2(f) documents the NOT_DONE delimiter (fused tokens fall
   through to InvalidResponse); Decision 5 documents the optional `reason`
   column (serde default; reader precedence reason → missing_evidence →
   error → bare outcome; old v3 rows keep the legacy fallback) and the
   no-GC retention policy — semantic verdicts are permanent replay
   evidence and any lifecycle bound is an operator decision (wontdo this
   round, rationale in the spec). Status moved from draft to implemented;
   personal/machine paths removed.

## Verification

- Behavioral RED → GREEN with real exit codes: a replay duplicated the
  durable note (2 != 1) before the fix; after the fix the targeted four
  tests pass. A first-pass compile error (unicode escape) and an early
  wrong-path note counter (legacy flat layout) are recorded as test bugs,
  not behavioral RED.
- The warning test exercises the shared constructor plus a real
  WsConnection / `send_notification_ephemeral` send — an integration of the
  boundary, not a full interactive turn dispatch (stated honestly).
- Cargo window (2026-09-10, real exit codes, logs in the recovery
  directory): fmt clean; the targeted set INCLUDING the newly added
  in-memory duplicate assertion passes 4/4 (verifier-final-targeted.log,
  EXIT0); clippy `-p octos-cli --features api --all-targets -- -D
  warnings` EXIT0 (verifier-final-clippy.log). All-targets belongs to the
  outer integration run.

## Handoff

Product and tests are frozen on this branch. The outer combined
integration run (workspace-wide clippy --all-targets and test
--all-targets) is owned by ROOT on the review-combined tree; this worktree
runs no further cargo. Final delivery report with per-item dispositions,
RED/GREEN exits, frozen SHAs, and the dual-model review trail lives in the
recovery directory (`verifier-final.md`).

## Dispositions

- Ledger GC: wontdo — permanent semantic replay evidence; see Decision 5.
- The `diagnostic` field already covers composite outcomes; no new kind,
  five-value outcome compatibility unchanged.

## Evening round (PR #2283 post-review follow-up, 2026-09-10)

Defects (verified by the review): the session-actor failure note was gated
on `!outcome.replayed` and swallowed its own persist error — a crash window
between the ledger append and the note persist (or a failed note append)
lost the note forever, and a failed append left a phantom in-memory note.

Fix: `persist_system_note_once_through_canonical_path` (octos-bus) — one
per-key-locked canonical critical section doing a STRICT read (metadata
NotFound-only absence, bounded size, valid meta header + supported schema,
every line a Message or control record, valid UTF-8, mandatory trailing
newline, rollback-aware visibility via `assemble_session_messages`) and an
append that returns the durable row (original timestamp/content for an
existing note id). The actor keys the note as
`goal-verifier-note:v1:{goal_id}:{evidence_digest}` (digest via the existing
pub(crate) `verifier_evidence_digest`), mirrors the returned durable row
into RAM only when missing, and records (does not swallow) persist errors —
the next same-evidence verification retries naturally. Charging, goal
status and TTL are untouched.

RED→GREEN (real exits, logs in the evening round dir): T2 phantom RED
(1 != 0) held from the first round; T1's original RED was retracted as a
fixture artifact (empty mirror → no claim) and re-established correctly
after the durable-seed-then-fresh-open rework — old actor behavior
restored momentarily for `verifier-restart-valid-red.log` (notes 0 != 1,
provider-count preconditions passing), then the candidate actor returned
for GREEN. Actor suite 5/5 EXIT0; bus suite 9/9 EXIT0 (idempotency,
concurrency, bad header/body/newline/UTF-8/directory all fail closed with
bytes unchanged); clippy -D warnings EXIT0; fmt clean.


### 最终整合：持久化恢复边界

- canonical 零字节普通文件按尚未初始化处理；真实空文件回归先失败（exit 101），修复后与其余 helper 回归共 10 项通过。目录目标仍报错。
- 坏 header 反例保留合法 body 和末尾换行并断言完整字节不变；非法 UTF-8 反例损坏 Message JSON 字符串内部，确认 lossy 解码仍能解析，以隔离严格解码门。
- RAM 修复测试原候选在注记落盘后才打开第二个 handle，未制造内存缺失。最终版先打开第二个 handle，再由第一个 actor 落盘，断言旧 handle 有 claim、无 note 后驱动恢复。
- 合约新增 12 个场景，分别绑定实际 actor/helper 测试；agent-spec parse 与 lint --min-score 0.7 均通过。
- 本轮完整验证与实际 GLM/K3 最终互审结果在提交说明中记录；不是跨进程重启测试，也未修改计费或 TTL 语义。
