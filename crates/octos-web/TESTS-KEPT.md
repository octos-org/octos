# octos-web TESTS-KEPT

Tracking: T-0579. Baseline is octos main `7c16f6c3`: `wake/test-lines.py --json` reports **749 test lines / 34 tests**. Names were verified with Node 22 `npx vitest list`, whose 34 entries match the same-algorithm count. Rows use `file > suite > exact Vitest name`; parameterized `%s`/`%j` and `${fixture.name}` placeholders are part of the discovered name and are not expanded or split.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative, or code-backed reason there is no such boundary |
|---|---|
| 路径穿越 | `src/runtime/__tests__/usage-api.test.ts > UsageApiClient > loads own session totals from the persistent usage API` — session ids are percent-encoded as one URL path segment (`session%2Fa%20b`), preventing path-splitting. |
| 沙箱逃逸 | **本 crate 无进程沙箱边界。** It contains SPA API/reducer/protocol code and spawns no tools or child processes; sandbox policy stays in the agent backend. |
| 鉴权/权限 | `src/runtime/__tests__/usage-api.test.ts > UsageApiClient > loads own session totals from the persistent usage API` pins bearer-authenticated own-session access; `raises typed errors for non-2xx responses` preserves 403 failure propagation. |
| 密钥/令牌脱敏 | `src/runtime/__tests__/usage-api.test.ts > UsageApiClient > loads own session totals from the persistent usage API` keeps the session token in the Authorization header rather than the URL; this crate has no separate log-output redactor. |
| 外部输入大小上限 | **本 crate 无原始帧字节上限边界。** The bridge validates envelope structure and sequence, while transport byte caps belong to the server; no test here should claim a byte quota. |

## Retained tests

| Cat. | Exact Vitest name | Guard |
|---|---|---|
| 3 | `src/runtime/__tests__/usage-api.test.ts > UsageApiClient > loads own session totals from the persistent usage API` | Own-session usage uses a bearer header and safely encoded session path with exact time filters. |
| 2 | `src/runtime/__tests__/usage-api.test.ts > UsageApiClient > queries admin aggregate usage with optional session filters` | Admin aggregate usage preserves optional session filtering on the real client path. |
| 4 | `src/runtime/__tests__/usage-api.test.ts > UsageApiClient > raises typed errors for non-2xx responses` | HTTP authorization failures become typed UsageApiError values with status. |
| 3 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge.decodeAndValidate > rejects non-object params` | Non-object external protocol input is rejected before routing. |
| 3 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge.decodeAndValidate > rejects envelope missing thread_id` | Envelopes without thread identity cannot enter a thread reducer. |
| 3 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge.decodeAndValidate > rejects envelope with seq=0 (per-thread seq is 1-based)` | The exact one-based sequence lower bound is enforced. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge.decodeAndValidate > rejects envelope with unknown payload.type` | Unknown payload variants fail as malformed rather than reaching listeners. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge.decodeAndValidate > accepts a well-formed envelope and routes to listeners` | A valid envelope reaches exactly one listener without false rejection. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge.decodeAndValidate > ignores methods other than projection/envelope` | Other RPC methods cannot inject projection envelopes through this bridge. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge hard barrier (spec § 14.6) > drops envelopes that arrive after turn_terminal on the same thread` | Post-terminal envelopes are dropped by the spec §14.6 hard barrier. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge hard barrier (spec § 14.6) > counts duplicate turn_terminal under the \"duplicate_completed\" label` | Duplicate completion is counted separately from ordinary late deltas. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge hard barrier (spec § 14.6) > barriers are per-thread — completion on tA does not affect tB` | One thread's completion cannot suppress another thread's events. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge seq monotonicity assertion > counts non-monotonic seqs under seqGaps but still surfaces the envelope` | Backward sequences are measured without silently losing the event. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > ProjectionEnvelopeBridge seq monotonicity assertion > counts forward gaps under seqGaps but still surfaces the envelope` | Forward sequence gaps are measured independently of backward replay. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > canonical v2 contract > closes a thread on %s` | Every parameterized terminal outcome closes its thread consistently. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > canonical v2 contract > delivers a late child result after its parent closes, and closes only the child` | Late child completion survives parent closure without closing the parent again. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > canonical v2 contract > keeps terminals isolated across sessions and topics` | Terminal state cannot leak across session/topic identities. |
| 3 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > canonical v2 contract > rejects malformed or retired payload $type` | Retired/malformed v2 payload tags fail closed rather than executing as current events. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > canonical v2 contract > requires turn identity and preserves cursor/routing and additive fields` | Turn identity is mandatory while cursor/routing and additive protocol fields survive. |
| 4 | `src/runtime/__tests__/ui-protocol-bridge.test.ts > decodes every published canonical v2 spec example with %j line endings` | Every published v2 example decodes under LF and CRLF host line endings. |
| 4 | `src/state/__tests__/json-loader.test.ts > Layer 1: JSON fixture loader > loads flat-form fixtures (round-trips with replay engine)` | Flat fixture JSON round-trips through the real replay engine. |
| 4 | `src/state/__tests__/json-loader.test.ts > Layer 1: JSON fixture loader > normalizes wrapped-form (UI Protocol method/params) to flat` | Wrapped UI Protocol method/params normalize to the canonical flat fixture form. |
| 3 | `src/state/__tests__/json-loader.test.ts > Layer 1: JSON fixture loader > rejects unknown UI Protocol methods with a clear message` | Unknown fixture methods are rejected with an actionable path/error. |
| 4 | `src/state/__tests__/json-loader.test.ts > Layer 1: JSON fixture loader > rejects malformed top-level fixture with a path` | Malformed fixture input reports its exact failing path. |
| 3 | `src/state/__tests__/json-loader.test.ts > Layer 1: JSON fixture loader > rejects events with both type and envelope (ambiguous)` | Ambiguous dual-form events cannot be interpreted twice. |
| 5 | `src/state/__tests__/fixtures.test.ts > Layer 1: fixtures fail against the current (buggy) reducer > ${fixture.name} fails against buggy reducer (proves teeth)` | Every retained fixture must fail against the deliberately broken sticky-thread reducer, proving negative coverage. |
| 2 | `src/state/__tests__/fixtures.test.ts > Layer 1: fixtures pass against the fixed (reference) reducer > ${fixture.name} passes against fixed reducer (contract is achievable)` | The same recovery/replay/interleave fixtures pass against the executable reference reducer. |
| 4 | `src/state/__tests__/fixtures.test.ts > Layer 1: replay engine determinism > produces identical final state across two replays of the same fixture` | Replaying identical input produces identical final state. |
| 4 | `src/state/__tests__/fixtures.test.ts > Layer 1: replay engine determinism > orders events by t, tie-broken by authored index` | Event ordering is deterministic by timestamp then authored index. |

共 **29 项**。两个动态 fixture suite 按 Vitest 实际发现的模板名整体保留，因此其引用的 7 个 fixture 与共享 reducer/replay helper 均属于保留集合，不做无法核名的单 fixture 删除。

## Cross-cutting audit

| Area | Audit result |
|---|---|
| 跨功能交互 | Usage API, JSON loader, projection bridge, and executable reducer fixtures are retained as distinct real chains. |
| 失败/迟到路径 | Typed HTTP errors, malformed envelopes, post-terminal drops, duplicate terminals, and late child results are retained. |
| 取消 | No AbortController/cancellation API exists in these clients; late-event barriers are the applicable boundary and are not mislabeled as cancellation. |
| 崩溃恢复 | The dynamic m89-recovery and reload-replay fixtures remain in both negative-sentinel and fixed-reducer suites. |
| 数据完整性 | Turn/thread identity, cursor preservation, deterministic replay, and multi-attachment dedup fixtures protect state integrity. |
| 并发/锁 | Per-thread barriers and sequence-gap accounting are the applicable shared-state boundaries; no browser-side lock protocol exists. |
| 精确边界 | seq=0, forward/backward gaps, duplicate completion, and exact URL encoding are retained. |
| 协议帧 | Published v2 examples, malformed/retired payloads, and wrapped/flat fixture normalization cover the wire contract. |
| 平台差异 | The canonical spec-example test explicitly covers LF and CRLF; other code is host-independent TypeScript and needs no live/canary access. |
