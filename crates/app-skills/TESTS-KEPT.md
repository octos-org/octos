# app-skills tests kept

Tracking: T-0571. Base: `7c16f6c3b1bff2cec9a64d984d9fd33f0a40cbac`. This is the allow-list for the later crate-wide deletion pass, not a request to preserve every test in a named file. Every existing row was discovered from the test body and its production call chain; the single new row is marked **NEW**.

`python3 wake/test-lines.py` baseline: **10,648 lines / 346 measured tests** in `crates/app-skills`. A source audit finds 347 test attributes: the platform-gated `evidence_lock_releases_when_a_duplicate_handle_remains_open` uses `cfg(all(test, ...))`, which the current line counter does not recognize. One deep-search demo is ignored and deep-search/deep-crawl contain Unix-only tests. `send-email`, `time`, `weather`, and `wechat-bridge` have zero tests.

The allow-list retains **83 tests: 82 existing representatives plus one NEW security representative**, with category counts **9 / 18 / 20 / 25 / 11** for categories 1–5. Categories: **1** real cross-system/process/file/network chain; **2** cross-feature interaction; **3** security/authorization; **4** failure, cancellation, recovery, data/concurrency/protocol integrity; **5** intentional design invariant. `src/...::name` and `tests/...::name` are source-qualified exact test names, not file-level retention.

A row is retained only for the behavior in its guard column. Manifest-field literals, slug/format helpers, parameter permutations, repeated lifecycle copies, and single-module implementation details are not retained merely because they exist.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact representative and boundary |
|---|---|
| 路径穿越 | `office/src/pptx.rs::should_refuse_workspace_escape_for_output_and_image` and `office/src/ooxml.rs::should_not_create_parent_directories_outside_workspace` exercise real `..` output/input containment and assert no outside artifact/directories. |
| 沙箱逃逸 | These skills do not implement an OS sandbox backend. Their actual containment boundaries are represented by `pptx-runtime/tests/runtime.rs::refuses_symlink_escape_and_dangling_targets` (Unix symlink/dangling workspace escape) and `deep-crawl/src/main.rs::should_block_browser_requests_and_navigations_to_private_addresses` (network/metadata/file-scheme scope escape). Do not infer OS sandbox coverage from these rows. |
| 鉴权/权限 | `deep-search/src/main.rs::test_synthesis_config_args_path_takes_precedence_over_env`, `deep-search/src/research.rs::should_refuse_scrapers_without_opt_in`, and `pptx-runtime/tests/visual_proof_gates.rs::independent_route_preserves_unknowns_and_rejects_self_approval_and_native_failures` exercise credential routing and real deny paths. `smart-home/tests/binary_e2e.rs::should_list_devices_via_env_config_with_bearer_auth` separately verifies the bearer reaches the mock bridge on the wire. |
| 密钥/令牌脱敏 | **NEW** `smart-home/tests/binary_e2e.rs::should_not_echo_bridge_token_in_error_output`: the real binary receives a bearer and a failing loopback bridge; its tool-protocol error output must not echo that bearer. This fills the only qualifying redaction representative found by the audit; credential-resolution/header tests alone were rejected as redaction evidence. |
| 外部输入大小上限 | `office/src/pptx.rs::should_enforce_total_image_budget` checks exact cap, cap+1, and overflow; `deep-search/src/main.rs::synthesis_prompt_trims_cjk_sources_by_characters` and `build_synthesis_prompt_omits_sources_beyond_cap` check Unicode/shared prompt bounds and source count; `harness-starter-audio/src/lib.rs::should_clamp_duration_to_safe_bounds` bounds generated audio allocation. |

## Retained representatives

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `account-manager/tests/sandbox_config.rs::should_return_config_error_and_preserve_profile_when_update_has_non_object_sandbox` | Real binary/stdin/stdout/temp-profile chain rejects malformed update and preserves the original profile byte-for-byte. |
| 1 | `account-manager/tests/sandbox_config.rs::should_update_existing_object_sandbox` | Real profile update preserves enabled/mode/network and unknown sandbox data rather than dropping fields. |
| 1 | `harness-starter-audio/tests/harness_smoke.rs::tool_produces_wav_satisfying_size_validator` | Actual WAV artifact, documented path, and 4096-byte policy threshold (not a claim of full PluginTool delivery coverage). |
| 5 | `harness-starter-audio/src/lib.rs::should_emit_valid_riff_wave_header` | RIFF/WAVE wire format remains valid. |
| 3 | `harness-starter-audio/src/lib.rs::should_clamp_duration_to_safe_bounds` | Audio duration cannot request an unbounded generated buffer. |
| 2 | `harness-starter-coding/tests/harness_smoke.rs::tool_produces_diff_and_preview_artifacts` | Diff and preview stay paired and satisfy their declared artifact paths and 64-byte gate. |
| 4 | `harness-starter-coding/src/lib.rs::should_emit_hunk_for_each_file` | A multi-file change cannot silently drop a file's hunk. |
| 2 | `harness-starter-generic/tests/harness_smoke.rs::tool_produces_primary_artifact_matching_policy_glob` | Generated body/path agrees with the declared primary artifact. |
| 5 | `harness-starter-generic/tests/harness_smoke.rs::lifecycle_state_transitions_queued_running_verifying_ready` | Sole retained copy of the supervisor Queued→Running→Verifying→Ready API mapping; three duplicate copies are excluded. |
| 2 | `harness-starter-report/tests/harness_smoke.rs::tool_produces_primary_markdown_artifact` | Real report artifact preserves topic, declared path, and minimum-size contract. |
| 4 | `harness-starter-report/src/lib.rs::should_respect_caller_supplied_body` | Caller content is not silently replaced by generated boilerplate. |
| 4 | `deep-crawl/src/main.rs::cleanup_chrome_is_idempotent_with_zero_pid` | The no-child cleanup path returns without issuing a process kill; retained as the safe counterpart to the dangerous PID-1 test. |
| 3 | `deep-crawl/src/main.rs::should_block_browser_requests_and_navigations_to_private_addresses` | Browser requests, redirects, metadata/loopback/mapped addresses, and file schemes fail closed. |
| 4 | `deep-crawl/src/main.rs::should_propagate_evaluate_error_with_page_and_operation` | Real loopback WebSocket/CDP response correlation preserves page and `Runtime.evaluate` failure context. |
| 4 | `deep-crawl/src/main.rs::should_reject_non_json_link_extraction_result` | Malformed browser output is rejected rather than treated as a trusted link list. |
| 4 | `deep-crawl/src/main.rs::should_preserve_legitimate_empty_link_list` | A real empty link list remains valid, distinguishing a leaf page from malformed output. |
| 5 | `deep-crawl/src/main.rs::should_not_hide_automation_in_chrome_args_or_scripts` | Deliberate no-stealth/no-webdriver-hiding and identifiable-UA product policy. |
| 1 | `deep-search/src/main.rs::should_stop_a_timed_out_deep_crawl_gracefully` | Unix timeout drives a real crawl child through SIGTERM, browser termination, reaping, and a bounded partial result. |
| 3 | `deep-search/src/research.rs::should_discard_browser_render_that_js_redirects_to_a_private_ip` | Sibling deep-crawl stdout passes through shared-reader revalidation; a late private redirect discards content. |
| 2 | `deep-search/src/research.rs::should_report_deep_crawl_page_error_when_browser_hit_a_challenge` | A sibling-process browser challenge becomes a classified page error, not empty article content. |
| 3 | `deep-search/src/research.rs::should_refuse_scrapers_without_opt_in` | Direct provider and planner/dispatch both refuse scraper access without explicit operator opt-in. |
| 5 | `deep-search/src/research.rs::should_not_consult_robots_txt_by_default` | Real Reader configuration neither consults nor requests robots by default. |
| 2 | `deep-search/src/research.rs::should_search_each_language_in_its_own_words` | Localized queries, language filters, and the language set compose without dropping the additional language. |
| 3 | `deep-search/src/main.rs::test_synthesis_config_args_path_takes_precedence_over_env` | Host/session endpoint, credential, model, and provider routing beat process-global credentials. |
| 3 | `deep-search/src/main.rs::synthesis_prompt_trims_cjk_sources_by_characters` | CJK character counting and the exact shared 4000/4001 boundary prevent prompt-budget bypass. |
| 3 | `deep-search/src/main.rs::build_synthesis_prompt_omits_sources_beyond_cap` | Sources beyond the 12-source cap are omitted and disclosed rather than sent unbounded to the model. |
| 1 | `deep-search/src/main.rs::length_cut_off_reply_is_retried_and_never_success` | Real loopback provider retries truncated replies, records cost/diagnostics, and never auto-delivers incomplete synthesis. |
| 4 | `deep-search/src/main.rs::mid_sentence_reply_without_length_flag_is_caught_and_retry_can_fix_it` | A provider `stop` response that is actually mid-sentence is detected and recovered by a real retry. |
| 2 | `deep-search/src/main.rs::failed_synthesis_is_partial_and_not_auto_delivered` | Failed synthesis remains partial and is never reported as a completed delivered answer. |
| 4 | `deep-search/src/main.rs::uncited_sentences_are_flagged_in_a_complete_reply` | Complete model output still marks unsupported prose `[citation needed]` without falsely calling it truncated. |
| 4 | `deep-search/src/main.rs::test_structured_progress_events_match_fixture` | Actual NDJSON sink bytes preserve exact progress frames and normalize checkout CRLF versus emitted LF. |
| 4 | `deep-search/src/main.rs::should_fail_and_not_count_an_artifact_whose_parent_is_a_file` | Real `ENOTDIR` propagates failure without appending a phantom source or synthesis artifact. |
| 4 | `deep-search/src/main.rs::unique_report_path_adds_suffix_on_collision` | Existing reports receive a suffix instead of being overwritten; this is sequential preservation, not race-safety proof. |
| 5 | `deep-search/src/main.rs::should_treat_a_bing_challenge_as_a_miss_not_as_results` | Lookalike links in a real challenge are not promoted to search results or used to bypass CAPTCHA policy. |
| 2 | `office/src/docx.rs::should_generate_title_paragraph_table_and_image_in_docx` | Real DOCX ZIP/XML preserves escaping, table, image relationship/dimensions, and original image bytes. |
| 2 | `office/src/xlsx.rs::should_write_multiple_sheets_and_primitive_cells_when_generating` | Real multi-sheet package preserves primitive cell types, XML escaping, and canonical output containment. |
| 2 | `office/src/xlsx.rs::should_write_formulas_and_styles_when_cells_are_formatted` | Formula normalization/recalculation and deduplicated effective styles survive export. |
| 2 | `office/src/pptx.rs::should_generate_two_slides_with_bullets_image_and_relationships` | Real editable two-slide PPTX preserves bullets, image relationships, and well-formed XML. |
| 3 | `office/src/ooxml.rs::should_not_create_parent_directories_outside_workspace` | Rejected `../` output creates no external directories. |
| 3 | `office/src/pptx.rs::should_refuse_workspace_escape_for_output_and_image` | Output and input-image traversal are refused with no published deck. |
| 3 | `office/src/pptx.rs::should_refuse_more_than_one_hundred_slides` | 101 slides fail with no output. |
| 3 | `office/src/pptx.rs::should_enforce_total_image_budget` | Exact 64 MiB succeeds; cap+1 and usize overflow fail without over-allocation. |
| 3 | `office/src/docx.rs::should_reject_xml_forbidden_control_characters` | XML-forbidden input produces no corrupt DOCX package. |
| 4 | `office/src/docx.rs::should_refuse_workspace_escape_and_overwrite` | Existing output is not replaced and traversal fails; unchanged-byte/crash atomicity remains a disclosed gap. |
| 1 | `pptx-runtime/tests/runtime.rs::build_reopen_render_patch_and_preserve_editable_text_without_external_programs` | Real binary with empty PATH builds/reopens/renders/patches, preserving editable text/old attempts and refusing overwrite. |
| 1 | `pptx-runtime/tests/object_review.rs::native_crop_pixels_and_final_evidence_are_exercised_through_public_tools` | Public cycle→crop→pack→submit→record→status preserves real pixels, unknowns, and hash binding. |
| 1 | `pptx-runtime/tests/runtime.rs::ocr_localizes_actual_text_in_image_coordinates` | macOS Apple Vision OCR localizes real English/Chinese pixels and rejects workspace escape. |
| 2 | `pptx-runtime/tests/authoring_operations.rs::split_keeps_canonical_copy_and_tracks_every_unicode_character_through_render` | Split preserves every canonical Unicode range through actual render inspection and source hash. |
| 2 | `pptx-runtime/tests/semantic_authoring.rs::graphic_mapping_preserves_copy_exports_native_labels_and_requires_visual_symbol_review` | Text/artwork mapping preserves copy/native labels and requires fresh visual symbol review. |
| 2 | `pptx-runtime/tests/semantic_authoring.rs::artwork_layers_share_the_checked_stack_and_bind_their_source_bytes` | Artwork stays below editable text and mutated source bytes invalidate the review cycle. |
| 2 | `pptx-runtime/tests/placement.rs::search_avoids_slanted_artwork_and_qa_rejects_regression_and_relaxation` | Exported layout is re-measured; regressions and constraint relaxation fail. |
| 2 | `pptx-runtime/tests/fitting_policy.rs::target_size_policy_uses_the_target_not_the_old_large_frame` | Target-box fit followed by real build/inspect proves selected size, one line, and no overflow. |
| 3 | `pptx-runtime/tests/runtime.rs::validates_copy_occurrences_locked_styles_paths_and_geometry` | `../`, unauthorized copy/style edits, and rotated geometry escape are refused. |
| 3 | `pptx-runtime/tests/runtime.rs::refuses_symlink_escape_and_dangling_targets` | Unix outside-symlink/dangling-target writes fail without external content. |
| 3 | `pptx-runtime/tests/runtime.rs::evidence_uses_host_identity_and_reports_misbound_policies` | Forged/missing owner cannot initialize and another runtime cannot consume misbound evidence. |
| 3 | `pptx-runtime/tests/visual_proof_gates.rs::independent_route_preserves_unknowns_and_rejects_self_approval_and_native_failures` | Maker self-approval, stale/tampered attestation, native failures, and route changes fail closed. |
| 3 | `pptx-runtime/tests/qa_gates.rs::should_not_accept_request_level_ignore_or_threshold_overrides` | Caller cannot ignore/dismiss/relax native QA or inject a pass. |
| 3 | `pptx-runtime/tests/qa_gates.rs::should_bind_qa_to_actual_pptx_not_a_forged_clean_report_or_reused_cycle` | Forged clean reports and reviewer dismissal cannot replace fresh measurement of the actual PPTX. |
| 4 | `pptx-runtime/src/evidence.rs::evidence_lock_releases_when_a_duplicate_handle_remains_open` | Live writer excludes another lock; dropping it releases despite a cloned descriptor on supported desktop platforms. |
| 4 | `pptx-runtime/src/placement.rs::emu_roundtrip_noise_does_not_expand_ink_into_adjacent_grid_cell` | EMU roundoff is tolerated at every edge while genuine subpixel overlap still fails. |
| 4 | `pptx-runtime/src/ocr.rs::converts_bottom_left_normalized_quads_without_losing_rotation` | Vision bottom-left normalized coordinates retain rotation and reject NaN on every host. |
| 4 | `pptx-runtime/tests/runtime.rs::mcp_shutdown_has_no_extra_plugin_envelope` | Real MCP initialize then stdin EOF emits only newline JSON-RPC frames and no plugin-envelope leakage. |
| 4 | `pptx-runtime/tests/qa_gates.rs::should_use_nonzero_cli_exit_for_failed_or_unknown_qa` | Real binary returns exit 2 plus structured unknown QA, preventing false success. |
| 4 | `pptx-runtime/tests/feedback_parity.rs::operations_returns_all_results_pixels_and_stops_before_later_writes` | Successful prefix/pixels are returned, failure is explicit, and later writes never occur. |
| 4 | `pptx-runtime/tests/semantic_authoring.rs::invalid_artwork_patch_does_not_save_partial_text_changes` | Invalid/missing/hash-mismatched artwork leaves source bytes unchanged with no partial text patch. |
| 4 | `pptx-runtime/tests/runtime.rs::completed_batch_can_expand_without_replaying_mutated_old_sources` | Completed scope expands only explicitly, preserves history/findings, is idempotent, and rejects shrink/replay. |
| 4 | `pptx-runtime/tests/runtime.rs::delivery_retarget_and_source_preview_bindings_preserve_existing_history` | Retarget preserves history while wrong preview/blank/source/owner fails instead of reusing evidence. |
| 5 | `pptx-runtime/tests/runtime.rs::evidence_never_silently_passes_missing_reviews_or_drops_unresolved_findings` | Missing review stays unknown, unresolved findings stay failed, identical records are idempotent, and changed pixels fail. |
| 5 | `pptx-runtime/tests/qa_gates.rs::should_fail_actual_overflow_despite_claimed_pass_and_forged_inspection` | Rehashed forged inspection plus claimed pass cannot hide actual off-slide ink. |
| 5 | `pptx-runtime/tests/reference_fidelity.rs::actual_render_repair_requires_pixel_change_not_a_rewritten_review` | Rewritten verdicts cannot close an unchanged defect; repaired pixels can, with immutable hash-bound originals. |
| 5 | `pptx-runtime/tests/typography_contract.rs::typography_constraints_cannot_disappear_during_a_repair` | Deleting typography groups is a constraint change, not a repair; frozen groups survive. |
| 2 | `skill-evolve/src/main.rs::should_find_skill_in_manifest` | Real manifest-directory reverse mapping associates a failed plugin tool with its owning skill. |
| 4 | `skill-evolve/src/main.rs::should_truncate_at_utf8_boundary` | Multibyte prompt/error truncation cannot split UTF-8 or exceed the byte budget plus marker. |
| 4 | `skill-evolve/src/main.rs::should_split_learned_notes` | Original SKILL.md body and existing learned notes remain distinguishable for non-destructive rewrite. |
| 5 | `skill-evolve/src/main.rs::should_detect_cooldown` | A fresh patch suppresses another LLM evolution call during cooldown. |
| 5 | `skill-evolve/src/main.rs::should_not_cooldown_when_old` | Expired cooldown permits learning again; the exact 600-second edge remains a disclosed gap. |
| 3 | `skill-evolve/src/main.rs::should_skip_self_evolution` | Real manifest scan refuses recursive self-modification. |
| 1 | `smart-home/tests/binary_e2e.rs::should_list_devices_via_env_config_with_bearer_auth` | Real skill process sends the expected GET and bearer to a TCP mock bridge and returns device data. |
| 1 | `smart-home/tests/binary_e2e.rs::should_control_device_via_env_config_posting_form_body` | Real control command preserves POST path, Authorization, content type, and encoded form values. |
| 2 | `smart-home/tests/binary_e2e.rs::should_fall_back_to_profile_json_when_env_config_absent` | Profile JSON supplies bridge config when env is absent through the real process chain. |
| 2 | `smart-home/tests/binary_e2e.rs::should_prefer_env_config_over_profile_json_when_both_present` | Env bridge/token precedence cannot accidentally use stale profile credentials. |
| 4 | `smart-home/tests/binary_e2e.rs::should_surface_bridge_reported_error_body` | Bridge failure reaches the tool error contract rather than being reported as success. |
| 3 | `smart-home/tests/binary_e2e.rs::should_not_echo_bridge_token_in_error_output` | **NEW** Real transport failure does not echo the configured bearer into tool output. |

## Cross-cutting audit and explicit non-claims

- **Cross-function/cancel/late/failure:** The retained set covers real account/smart-home subprocesses, deep-search cancellation and partial/failed synthesis, deep-crawl malformed browser output, PPTX late/stale evidence, and failure-atomic artifact paths. Starter tests do **not** execute PluginLoader/PluginTool/verifier/delivery, cancellation tokens, or real glob resolution; their retained artifact/lifecycle rows are narrower than full Octos E2E.
- **Crash recovery/data integrity:** Retain profile byte preservation, `ENOTDIR` no-phantom-artifact behavior, invalid-patch atomicity, completed-batch expansion, and delivery/history binding. Account-manager still bypasses `ProfileStore` validation/atomic rename/mode enforcement; starter writes are non-atomic; PPTX pending/detail/history/snapshot writes have no crash-injection test. These are uncovered production risks, not reasons to keep structural CRUD tests.
- **Concurrency/locks/exact boundaries:** Retain evidence duplicate-handle locking and exact image/slide/prompt/audio/EMU limits. The discarded `browser_semaphore_caps_concurrency` acquires only one permit and is not genuine concurrency evidence. `exclusive` manifest assertions and hand-stepped duplicate lifecycle tests do not prove detached writers serialize. No same-slug writer, concurrent-record, crashed-writer, or lost-update test exists.
- **Protocol frames/platform:** Retain smart-home argv/stdin/stdout plus real HTTP, deep-search NDJSON, PPTX MCP shutdown/CLI exit, and RIFF. PPTX symlink is Unix-only; evidence lock is platform-gated for Linux/macOS/Windows; deep-search cancellation is Unix-only; real OCR is macOS-only with an all-host coordinate-math counterpart. No target-specific Windows/macOS matrix exists for the other skills.
- **Zero-test crates:** `send-email`, `time`, `weather`, and `wechat-bridge` have no tests to retain or delete. Weather retries can exceed its manifest timeout and its HTTP bodies are uncapped; clock malformed input defaults to local time. WeChat WS clients have no bridge-level auth, token/context data can be exposed through argv/stdout/WS, HTTP/WS input is uncapped, and send/cursor/dedup/crash recovery are untested. These need separate production/test work, not fabricated retention claims.
- **Other uncovered security risks:** account-manager raw IDs/path writes and cross-parent/secret handling lack adversarial coverage; skill-evolve `find_skill_dir` accepts unvalidated tool-controlled paths. News feed-controlled deep-fetch has no SSRF guard and all 11 news tests are isolated parser/config helpers, so none is retained merely to manufacture category coverage. No existing test covers these boundaries, and this deletion task will not change production behavior.
- **PPTX packaging scope:** `octos-pptx` direct CLI/MCP chains are real, but this base does not list the binary in `BUNDLED_APP_SKILLS` or release `BINARIES`; the separate bundled Python PPTX resource skill is not the same implementation. Retained rows guard the direct runtime, not an assertion that automatic bundled shipping exists.
- **Dangerous excluded test:** `deep-crawl/src/main.rs::cleanup_chrome_clears_pid_after_call` sets `CHROME_PID=1` and reaches a real `kill -9 1`/`taskkill` path. It is not a safe representative and must be deleted, not run as part of an unfiltered local suite. `cleanup_chrome_is_idempotent_with_zero_pid` is retained because `pid <= 0` returns before spawning `kill`.
- **External/live:** No retained-test validation requires shared deployment, credentials, or network access. The ignored deep-search demo is not retained.

After approval and merge of this manifest, exactly one crate-wide deletion PR may remove every test/fixture/helper outside this list. Before deletion, run a machine inventory of all current test functions and the retained actual run set. After deletion, `kept_set - actual_set` and `actual_set - kept_set` must both be empty; record final lines/tests with the same `wake/test-lines.py` command. No production, required-check, check-arm, or generic selector change is allowed in this task.
