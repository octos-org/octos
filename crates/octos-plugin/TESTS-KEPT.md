# octos-plugin TESTS-KEPT

Tracking: T-0576. Baseline is octos main `7c16f6c3`: `wake/test-lines.py --json` reports **1,890 test lines / 75 tests**. Every name below was checked with `cargo test -p octos-plugin --all-targets -- --list`.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design. Integration rows use `tests/<target>.rs::<exact libtest name>`. Manifest field mirrors and duplicate validator branches are excluded.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative, or code-backed reason there is no such boundary |
|---|---|
| 路径穿越 | **本 crate 无独立路径穿越边界。** Discovery searches caller-configured plugin roots and fixed manifest filenames; it does not canonicalize untrusted request paths. Binary execution confinement belongs to the lifecycle sandbox tests below. |
| 沙箱逃逸 | `tests/lifecycle_sandbox.rs::should_run_preflight_steps_through_sandbox`; `tests/lifecycle_sandbox.rs::pick_and_place_lifecycle_example_runs_end_to_end` covers the real sandboxed lifecycle chain. |
| 鉴权/权限 | `integrity::tests::missing_or_bad_proof_is_fail_closed`; `tests/lifecycle_sandbox.rs::should_not_dispatch_when_safepolicy_denies_with_extra_whitespace` pins command-policy denial before dispatch. |
| 密钥/令牌脱敏 | `tests/lifecycle_sandbox.rs::should_apply_blocked_env_vars_to_lifecycle_steps` strips forbidden secret/injection environment variables before child execution; this crate has no separate log-output redactor. |
| 外部输入大小上限 | **本 crate 无字节数拒绝边界。** `protocol_v2::tests::very_long_legacy_line_is_returned_as_is` deliberately preserves long legacy lines for compatibility (category 5); manifest validation bounds structure, not input bytes. |

## Retained tests

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `discovery::tests::distinct_ids_with_the_same_display_name_are_discovered_independently` | Display-name equality cannot merge distinct plugin identities or their state. |
| 2 | `discovery::tests::higher_priority_wins_dedup` | Duplicate plugin resolution follows the configured source priority chain. |
| 3 | `discovery::tests::rfc2_discovery_skips_manifest_with_invalid_schema` | Invalid external manifests are excluded from discovery instead of reaching execution. |
| 3 | `discovery::tests::should_reject_all_same_root_plugins_when_manifest_ids_duplicate` | Duplicate ids in one trust root reject the ambiguous set rather than permitting shadowing. |
| 4 | `discovery::tests::should_reject_valid_sibling_when_same_root_duplicate_manifest_is_invalid` | One malformed duplicate cannot leave a seemingly valid sibling loaded from ambiguous input. |
| 3 | `gating::tests::missing_env_var_fails` | Required environment/credential presence fails closed before plugin activation. |
| 4 | `gating::tests::os_check_wrong_platform` | Platform gating refuses activation on an unsupported host OS. |
| 3 | `integrity::tests::bad_signature_or_hash_is_rejected` | A signature or binary hash mismatch is rejected before plugin use. |
| 3 | `integrity::tests::missing_or_bad_proof_is_fail_closed` | Missing or malformed provenance proof cannot bypass integrity verification. |
| 3 | `integrity::tests::registry_commit_and_review_epoch_are_signed` | Registry commit and review epoch are cryptographically bound to the signed provenance. |
| 4 | `integrity::tests::registry_stale_corrupt_and_future_reviews_are_rejected` | Stale, corrupt, and future-dated review metadata cannot resurrect trust. |
| 1 | `integrity::tests::signed_matching_binary_is_accepted` | The positive real signed-binary chain succeeds when every proof matches. |
| 2 | `lifecycle::tests::run_phase_injects_octos_skill_dir_env` | Lifecycle phases receive the intended skill-directory environment across the process boundary. |
| 3 | `manifest::tests::rfc2_rejects_bare_anyof_branch_at_load_time` | Bare anyOf external schema input is rejected at manifest load rather than later misinterpreted. |
| 3 | `manifest_validator::tests::additional_properties_recursion_catches_invalid_anyof` | Recursive additionalProperties cannot hide an invalid anyOf branch. |
| 3 | `manifest_validator::tests::defs_recursion_catches_invalid_anyof` | `$defs` indirection cannot hide an invalid anyOf branch from validation. |
| 3 | `manifest_validator::tests::dollar_ref_rejected_without_explicit_allow` | Schema `$ref` is refused unless the explicit draft-07 compatibility mode allows it. |
| 3 | `manifest_validator::tests::empty_input_schema_rejected` | Empty external input schemas fail closed under the validating profile. |
| 5 | `manifest_validator::tests::lenient_profile_accepts_bare_anyof_branch` | The intentional lenient compatibility profile accepts legacy bare anyOf input. |
| 3 | `manifest_validator::tests::root_schema_must_be_object_type` | Non-object root schemas cannot be confused with valid tool input objects. |
| 5 | `protocol_v2::tests::malformed_json_falls_back_to_legacy` | Malformed JSON deliberately follows the legacy text compatibility path rather than dropping output. |
| 4 | `protocol_v2::tests::unknown_event_type_is_unknown_event` | Unknown protocol event types remain explicitly unknown instead of being executed as a known event. |
| 5 | `protocol_v2::tests::very_long_legacy_line_is_returned_as_is` | Long legacy text is intentionally passed through unchanged for wire compatibility. |
| 1 | `tests/bundled_manifests_validate.rs::bundled_app_skills_manifests_pass_strict_validator` | Real bundled app-skill manifests all pass the strict production validator. |
| 3 | `tests/lifecycle_sandbox.rs::blocked_env_vars_match_core_canonical_list` | Plugin lifecycle and core share the canonical forbidden-environment list. |
| 1 | `tests/lifecycle_sandbox.rs::pick_and_place_lifecycle_example_runs_end_to_end` | A real lifecycle example runs through the sandboxed process chain end to end. |
| 4 | `tests/lifecycle_sandbox.rs::should_abort_phase_when_critical_step_fails` | A critical lifecycle step failure aborts the phase instead of continuing into an invalid state. |
| 5 | `tests/lifecycle_sandbox.rs::should_continue_phase_when_non_critical_step_fails` | The intentional non-critical failure policy continues without hiding the step outcome. |
| 3 | `tests/lifecycle_sandbox.rs::should_apply_blocked_env_vars_to_lifecycle_steps` | Forbidden environment values are stripped before real lifecycle child execution. |
| 4 | `tests/lifecycle_sandbox.rs::should_kill_child_when_step_timeout_exceeded` | Timeout kills the child process instead of leaving orphaned work after cancellation. |
| 3 | `tests/lifecycle_sandbox.rs::should_not_dispatch_when_safepolicy_denies_with_extra_whitespace` | Whitespace-mangled dangerous commands cannot bypass SafePolicy dispatch denial. |
| 3 | `tests/lifecycle_sandbox.rs::should_run_preflight_steps_through_sandbox` | Preflight commands execute through the sandbox rather than an unconfined host path. |

共 **32 项**。

## Cross-cutting audit

| Area | Audit result |
|---|---|
| 跨功能交互 | Discovery/gating, signed integrity, real manifest validation, and lifecycle sandbox/process tests are retained; isolated field getters are not. |
| 失败/迟到路径 | Invalid discovery input, critical abort, stale/corrupt/future review rejection, and bad signatures cover failure propagation. |
| 取消/超时 | `should_kill_child_when_step_timeout_exceeded` is the explicit cancellation representative. |
| 崩溃恢复 | Plugin lifecycle has no durable restart/recovery store; integrity stale/corrupt metadata tests are the relevant durable-trust recovery boundary. |
| 数据完整性 | Distinct ids, duplicate-root rejection, signed registry epochs, and review-time validation prevent identity/provenance confusion. |
| 并发/锁 | No plugin-owned lock protocol exists; discovery is read-only and duplicate-id determinism is the applicable shared-state boundary. |
| 精确边界 | Missing env, unsupported OS, empty schema, and proof mismatch test exact fail-closed edges without field permutations. |
| 协议帧 | Unknown event typing and malformed-JSON legacy fallback are retained; BOM/CRLF/simple line-shape cases are excluded. |
| 平台差异 | Wrong-OS gating is explicit; sandbox lifecycle tests remain subject to Windows/macOS/Linux CI, with no live/canary access. |
