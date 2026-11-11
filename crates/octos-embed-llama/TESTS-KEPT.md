# octos-embed-llama TESTS-KEPT

Tracking: T-0579. Baseline `wake/test-lines.py --json`: **612 test lines / 22 tests**. Names were verified both with default `cargo test -p octos-embed-llama --all-targets -- --list` (10 pure-Rust tests) and `--features embed-llama` (all 22 including cfg-gated/model-backed tests). Integration rows use `tests/<target>.rs::<exact libtest name>`.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design. Model-backed tests remain `#[ignore]` unless a cached GGUF is explicitly supplied; no live/canary access or credential request is needed for this inventory.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative, or code-backed reason there is no such boundary |
|---|---|
| 路径穿越 | **本 crate 无请求路径穿越沙箱边界。** The caller supplies a local GGUF path which is canonicalized for model loading/cache identity; there is no archive/request-path resolution or output-directory join. |
| 沙箱逃逸 | **本 crate 无进程沙箱边界。** It runs llama.cpp in-process and spawns no shell/tool child; host sandboxing is outside this provider. |
| 鉴权/权限 | **本 crate 无鉴权主体或权限决策。** Model/vector APIs carry no peer credential or capability check. |
| 密钥/令牌脱敏 | **本 crate 无 secret 输出/日志脱敏边界。** It has no credential resolver or secret-specific redactor; embedding text is caller data, not an auth token channel. |
| 外部输入大小上限 | `prompt::tests::batch_plan_never_exceeds_the_token_budget`; `imp::tests::should_prioritize_foreground_and_bound_background_waiters` covers the bounded background queue. Production also truncates each tokenized sequence to `MAX_TOKENS_PER_SEQ`. |

## Retained tests

| Cat. | Exact test | Guard |
|---|---|---|
| 2 | `prompt::tests::prompts_are_asymmetric_and_prefix_the_text` | Query/document task prefixes remain asymmetric and are applied before tokenization, preserving retrieval semantics. |
| 4 | `prompt::tests::l2_normalize_produces_a_unit_vector` | Post-pooling vectors are normalized with correct direction and unit norm. |
| 4 | `prompt::tests::l2_normalize_leaves_a_zero_vector_alone` | Zero vectors cannot produce NaN/Infinity during normalization. |
| 4 | `prompt::tests::mrl_truncate_shrinks_and_renormalizes` | MRL output is truncated to the requested dimension and renormalized, with no-op upper bounds preserved. |
| 4 | `prompt::tests::mrl_truncate_survives_a_zero_prefix` | A zero MRL prefix cannot produce non-finite output. |
| 4 | `prompt::tests::batch_plan_covers_every_index_once_and_respects_the_cap` | Every input index appears exactly once while sequence-count caps hold. |
| 3 | `prompt::tests::batch_plan_never_exceeds_the_token_budget` | Combined batch tokens never exceed the context/decode budget. |
| 4 | `prompt::tests::batch_plan_keeps_an_oversized_sequence_in_its_own_group` | An oversized sequence is isolated rather than dropped or corrupting neighboring outputs. |
| 4 | `prompt::tests::batch_plan_handles_empty_and_zero_cap` | Empty input and zero sequence cap degrade to exact safe behavior. |
| 4 | `imp::tests::should_load_a_cached_model_only_once_and_share_its_arc` | Concurrent model loads single-flight to one allocation and shared Arc. |
| 3 | `imp::tests::should_prioritize_foreground_and_bound_background_waiters` | Foreground recall preempts background work and excess background waiters are refused rather than queued without bound. |
| 4 | `imp::tests::should_wake_remaining_waiters_when_a_notified_waiter_is_cancelled` | Cancelling a notified waiter hands wakeup to the next class instead of stranding capacity. |
| 1 | `imp::tests::should_lazy_load_share_weights_and_decode_off_the_async_worker` | Real lazy model initialization shares weights and moves llama decode off the async worker. |
| 4 | `tests/embed.rs::should_match_one_at_a_time_when_batched` | Real model batching cannot change individual embeddings when long and short texts share a batch. |
| 4 | `tests/embed.rs::should_truncate_and_renormalize_for_mrl` | The real model chain returns a unit-normalized 256-dimensional MRL embedding. |
| 4 | `tests/golden_agreement.rs::should_agree_with_golden_on_direction` | Real embeddings retain direction agreement with the frozen verified reference, catching pooling/prefix/normalization regressions. |
| 1 | `tests/golden_agreement.rs::should_match_golden_retrieval_ranking` | Query-to-document top-1 ranking matches the frozen verified reference across the real model chain. |
| 2 | `tests/golden_agreement.rs::should_separate_relevant_from_irrelevant_on_its_own` | The provider independently ranks relevant above irrelevant, so agreement cannot hide a shared semantic bug. |

共 **18 项**。

## Cross-cutting audit

| Area | Audit result |
|---|---|
| 跨功能交互 | Lazy load/shared weights/blocking decode, batch-vs-solo, MRL, and golden retrieval chains are retained. |
| 失败/迟到路径 | Queue-full refusal, tokenization/batch/decode error propagation remain in the reviewed call chain; no existing test injects a model decode failure, so this inventory does not claim one. |
| 取消 | The cancelled-notified-waiter handoff is retained. Blocking llama decode itself is not asynchronously abortable; no test claims mid-decode cancellation. |
| 崩溃恢复 | No durable provider state exists; RAII guards return scheduler capacity on drop, but process crash recovery is outside this crate. |
| 数据完整性 | Index-once batching, batch-vs-solo equality, normalization, MRL, and golden direction/ranking protect vector/index integrity. |
| 并发/锁 | Single-flight model loading, bounded priority scheduling, and cancellation wakeup are explicit retained lock/Notify boundaries. |
| 精确边界 | Zero vectors, zero MRL prefix, empty input, zero cap, oversized sequence, and exact token budget are retained. |
| 协议帧 | No wire protocol/frame codec exists here; the provider boundary is text in and vectors out. |
| 平台差异 | Pure arithmetic runs in default CI; feature-gated CPU/model tests remain for cached-GGUF runs. Metal/CUDA only change GPU offload selection and are not claimed as locally executed evidence. |
