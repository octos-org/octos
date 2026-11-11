# octos-workflows TESTS-KEPT

Tracking: T-0579. Baseline is octos main `7c16f6c3`: `wake/test-lines.py --json` reports **652 test lines / 43 tests**. Names were verified with `cargo test -p octos-workflows --all-targets -- --list`.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design. This crate defines/compiles workflow metadata; DOT/node execution is explicitly outside it. Pure serde, registry-name, output-string, per-field, and per-template mirrors are not retained as behavioral coverage.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative, or code-backed reason there is no such boundary |
|---|---|
| 路径穿越 | `workflow_runtime::workflow_families::site::tests::from_slug_strict_rejects_unknown_slugs` — unknown, empty, and output-dir-lookalike template slugs return `None`, preventing the preview validator from treating arbitrary input as the Docs fallback. |
| 沙箱逃逸 | **本 crate 无进程沙箱边界。** It emits workflow/workspace definitions and spawns no commands; execution confinement belongs to the agent/pipeline runtime. |
| 鉴权/权限 | **本 crate 无鉴权主体或权限判定。** Strict template parsing is input validation, not an authorization decision; no credential/peer capability is processed here. |
| 密钥/令牌脱敏 | **本 crate 无 secret 或日志脱敏边界。** Workflow definitions contain no credential resolution or secret-output redactor. |
| 外部输入大小上限 | **本 crate 无字节数上限边界。** Detection and slug mapping use string matching/trimming but define no byte quota; transport/input caps belong to callers. |

## Retained tests

| Cat. | Exact test | Guard |
|---|---|---|
| 2 | `workflow_runtime::tests::detects_deep_research_across_zh_synonyms` | Non-ASCII research wording reaches the intended background workflow rather than silently staying foreground. |
| 5 | `workflow_runtime::tests::podcast_signal_takes_precedence_over_deep_research` | Combined research+podcast intent follows the intentional podcast precedence rule. |
| 5 | `workflow_runtime::tests::english_foreground_overrides_each_suppress_detection` | An explicit foreground request overrides every background-detection signal. |
| 4 | `workflow_runtime::tests::with_phase_replaces_only_current_phase` | Phase transition clones the workflow and changes only `current_phase`, preserving all other execution metadata. |
| 2 | `workflow_runtime::tests::workflow_kind_plan_compiles_via_registry` | A detected workflow kind compiles through the real family registry into the matching plan. |
| 3 | `workflow_runtime::workflow_families::site::tests::from_slug_strict_rejects_unknown_slugs` | Security-sensitive strict parsing rejects unknown/empty/Docs-lookalike slugs instead of default-allowing them. |
| 5 | `workflow_runtime::workflow_families::site::tests::from_slug_strict_normalizes_case_and_surrounding_whitespace` | Strict parsing intentionally normalizes ASCII case and surrounding whitespace before the known-slug check. |
| 5 | `workflow_runtime::workflow_families::tests::bounded_site_template_defaults_to_docs` | The non-security lossy parser intentionally defaults unknown templates to Docs, distinct from the strict validator's fail-closed behavior. |

共 **8 项**。

## Cross-cutting audit

| Area | Audit result |
|---|---|
| 跨功能交互 | Detection → registry plan compilation and strict/lossy template parsing are retained; metadata shape serialization is not mistaken for a system chain. |
| 失败/迟到路径 | No node executor exists here. Strict unknown-slug rejection is the applicable fail-closed input path; there is no late node result to test. |
| 取消 | No task/process/cancellation token exists in this definition-only crate; cancellation belongs to the executing runtime. |
| 崩溃恢复 | No durable execution state or restart store exists here; recovery cannot be honestly claimed by these metadata tests. |
| 数据完整性 | `with_phase` preserving all non-phase fields and strict/lossy template distinctions protect definition integrity. |
| 并发/锁 | No shared mutable executor or lock protocol exists in this crate. |
| 精确边界 | Explicit foreground override, podcast precedence, unknown/empty slug rejection, and normalization are retained. |
| 协议帧 | No transport frame codec exists here; serde field-shape mirrors are excluded rather than mislabeled as protocol framing. |
| 平台差异 | Pure Rust definitions have no OS branch. UTF-8 Chinese detection and ASCII case normalization remain covered without live/canary access. |
