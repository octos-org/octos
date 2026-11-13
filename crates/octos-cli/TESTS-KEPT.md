# octos-cli tests kept

Tracking: #3069. This is the allow-list for the later deletion pass, not a general request to add or restore tests. Except for explicitly marked restoration rows, every row names a test discovered on main (`ed914b5fc`). All rows, including the T-0550 security representatives and T-0562 contract restorations, are verified with `cargo +1.97.1 test -p octos-cli --tests --no-default-features --features api -- --list`.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design invariant. Unit rows are exact libtest names. Integration rows use `tests/<target>.rs::<exact libtest name>` so equally short names in different targets cannot be confused.

A test belongs here only when its row states the behavior that would be lost. Structural shape, constants, getters, parameter permutations, and duplicate branch enumeration do not qualify.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative |
|---|---|
| 路径穿越 | `api::handlers::session_file_security_tests::session_file_read_rejects_traversal_directories_and_symlinks`; `tests/build_output_dir_validation.rs::should_reject_traversal_and_absolute_paths_without_creating_output` |
| 沙箱逃逸 | `tests/mcp_serve_integration.rs::should_block_shell_write_outside_workspace_via_sandbox` |
| 鉴权/权限 | `tests/preview_auth.rs::preview_auth_and_profile_header_boundaries_fail_closed`; `api::solo_auth::tests::solo_create_403_when_opt_in_disabled` |
| 密钥/令牌脱敏 | `api::ui_protocol_ledger::tests::should_redact_tool_started_arguments_before_persist_and_replay`; `api::admin::tests::serve_log_redaction_masks_credentials` |
| 外部输入大小上限 | `api::memory_panel::tests::oversized_memory_md_is_refused_not_truncated`; `api::ui_protocol_transport::hydrate_response_security_tests::hydrate_budget_unshrinkable_changes_fail_without_partial_result` |

## Restored security representatives (T-0550)

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `commands::skills::execution_gate_tests::npm_source_scripts_do_not_run_without_the_operator_opt_in` | A skill shipping `package.json` does not spawn `npm install` (arbitrary postinstall scripts) without `OCTOS_SKILLS_NPM_INSTALL=1`, leaving no node_modules or lockfile artifacts. |
| 3 | `commands::skills::execution_gate_tests::cargo_source_build_is_denied_without_spawning_or_mutating_the_manifest` | A remote skill shipping `Cargo.toml` does not spawn `cargo build` (arbitrary build.rs) without `OCTOS_SKILLS_CARGO_BUILD=1`, leaving the manifest and target/ untouched. |
| 3 | `commands::skills::execution_gate_tests::explicitly_allowed_cargo_source_build_still_produces_the_local_binary` | The explicit local-development opt-in still performs the source build and produces the expected platform binary (`main`/`main.exe`). |
| 3 | `commands::skills::execution_gate_tests::allowed_lifecycle_failure_rolls_back_the_partial_install` | A local-path skill whose allowed lifecycle fails rolls back the partial install directory so re-install starts clean. |
| 3 | `api::handlers::session_file_security_tests::session_file_read_rejects_traversal_directories_and_symlinks` | Real filesystem reads reject `..`, absolute paths, directories, leaf symlinks, and symlinked ancestors without outside side effects. |
| 3 | `api::solo_auth::tests::solo_create_403_when_opt_in_disabled` | Loopback alone cannot bypass the explicit solo opt-in; the real handler returns 403. |
| 3 | `api::memory_panel::tests::oversized_memory_md_is_refused_not_truncated` | The current memory query service refuses an over-cap MEMORY.md instead of returning a truncated prefix. |
| 3 | `api::ui_protocol_ledger::tests::should_redact_tool_started_arguments_before_persist_and_replay` | Tool credentials are absent from the appended event, durable bytes, and replay while non-secret fields survive. |
| 3 | `api::admin::tests::serve_log_redaction_masks_credentials` | The real serve-log filter output masks bearer, query-token, and key credentials. |
| 3 | `api::ui_protocol_transport::hydrate_response_security_tests::hydrate_budget_unshrinkable_changes_fail_without_partial_result` | An oversized hydrate response returns a same-request error contract rather than a partial/truncated result. |
| 3 | `tests/preview_auth.rs::http_otp_login_bearer_and_invalid_credentials` | Real HTTP static-OTP verification mints a bearer accepted by `/api/auth/me`; missing/wrong bearer gets 401. |
| 3 | `tests/preview_auth.rs::preview_auth_and_profile_header_boundaries_fail_closed` | Real preview routes allow own bearer, reject missing auth/cross-tenant access, and deny a trusted-hop forged X-Profile-Id without content leak. |
| 3 | `tests/preview_auth.rs::preview_signed_unknown_expired_and_revoked_tokens_fail_closed` | Signed-preview unknown/expired tokens return 404 and a revoked issuer returns 403 through the real router. |
| 3 | `tests/preview_auth.rs::preview_signed_cap_returns_429_with_retry_after` | The same-bearer token cap fails at 429 with `Retry-After: 60` after all in-cap mints succeed. |
| 3 | `tests/build_output_dir_validation.rs::should_reject_traversal_and_absolute_paths_without_creating_output` | Build-output metadata rejects relative/normalized traversal and absolute paths without creating output. |
| 3 | `tests/build_output_dir_validation.rs::should_reject_symlink_escape_after_build_without_following_target` | A built-output symlink to an outside directory is rejected without following or modifying the target. |
| 3 | `tests/build_output_dir_validation.rs::should_return_400_not_200_on_invalid` | The real preview build error mapping returns 400 without leaking local paths. |
| 3 | `tests/auth_work_secret_cli.rs::issue_stdout_stays_the_secret_and_list_never_shows_the_token` | Real CLI issue stdout is exactly the encoded secret and list output shows only the grant, never its raw token. |
| 3 | `tests/session_ingress_work_secret.rs::work_secret_ws_rejects_malformed_wrong_session_and_expiry` | Real TCP/WebSocket handshakes reject malformed tokens with 401, wrong-session tokens with 403, and expired tokens with 401. |

## OUP transport and real connection lifecycle

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `api::ui_protocol_transport::tests::cancelled_approval_replays_on_reconnect` | A cancelled approval survives reconnect instead of disappearing from the client. |
| 4 | `api::ui_protocol_transport::tests::reconnect_after_decision_replays_decided_event` | A decision made around disconnect is replayed exactly once after reconnect. |
| 4 | `api::ui_protocol_transport::tests::session_open_replays_pending_approval_after_reconnect_without_cursor` | Reopening without a cursor still restores the actionable approval. |
| 4 | `api::ui_protocol_transport::tests::prompt_durability::should_commit_disconnected_prompt_before_terminal_without_duplicates` | Disconnect cannot drop or duplicate the durable prompt around terminal delivery. |
| 4 | `context_manager::tests::should_not_record_history_twice_when_a_turn_opens_with_a_new_runtime_note` | A turn that opens with a new runtime note commits its prompt behind that note, so the model prompt holds each row once instead of the whole conversation twice. |
| 1 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ingress_lifetime_tests::ordinary_ws_disconnect_preserves_turn_and_pending_prompts_over_real_ws` | Real WebSocket disconnect preserves ordinary work and queued prompts for reconnect. |
| 3 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ingress_lifetime_tests::valid_work_secret_disconnect_aborts_turn_and_pending_prompts_over_real_ws` | Revoked work-secret ingress cannot leave detached execution alive. |
| 3 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ws_detached_approval_and_question_wait_for_an_authorized_reconnect` | Detached requests remain unavailable until an authorized connection resumes them. |
| 4 | `api::ui_protocol_transport::tests::event_ledger_recovery_runs_exactly_once_across_concurrent_initializers` | Concurrent ledger initialization cannot execute recovery twice. |
| 4 | `api::ui_protocol_transport::tests::turn_terminal_recovery_orphans_reused_turn_generation` | Cold recovery cannot attach a reused turn id to the wrong generation. |
| 4 | `api::ui_protocol_transport::tests::unified_terminal_sink_failure_with_ack_queues_recovery_under_profile` | A failed acknowledged terminal write is retried in the owning profile. |
| 3 | `api::ui_protocol_transport::tests::stdio_multi_profile_open_status_reads_isolated_runtime_policy_stamps` | Stdio profile routing cannot mix runtime policy or session state. |
| 3 | `api::ui_protocol_transport::tests::should_drop_cross_profile_session_opened_frames_when_connection_scopes_another_profile` | Session lifecycle frames cannot cross the authenticated profile boundary. |
| 3 | `api::ui_protocol_transport::tests::should_drop_cross_profile_background_activity_frames_when_connection_scopes_another_profile` | Background activity cannot leak to another profile's connection. |
| 3 | `api::ui_protocol_transport::tests::should_let_an_external_client_answer_only_its_own_turns_approvals_once` | External approval authority is owner-scoped and single-use. |
| 3 | `api::ui_protocol_transport::tests::should_refuse_an_external_answer_to_a_host_owned_peer_approval` | An external caller cannot decide a host-owned approval. |
| 4 | `api::ui_protocol_transport::tests::should_fail_approval_send_when_ledger_persistence_fails` | Approval delivery cannot report success when its durable ledger write failed. |
| 4 | `api::ui_protocol_transport::tests::session_hydrate_file_mutations_honor_include_after_and_profile_scope` | Recovery hydration applies file mutations only in the requested cursor and profile scope. |
| 1 | `api::ui_protocol_transport::tests::peer_send_input_rehomes_to_reopened_peer_wire_on_reconnect` | Peer input delivery follows the reopened wire rather than a dead connection. |
| 4 | `api::ui_protocol_transport::tests::bc9_sf1_disconnect_releases_build_cache_slot` | Disconnect releases the per-session build-cache admission slot. |
| 5 | `api::ui_protocol_transport::tests::spec_section6_catalog_lists_every_advertised_method` | The OUP §6 specification and advertised method catalog remain deliberately synchronized (`api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`). |
| 3 | `api::ui_protocol_transport::tests::stdio_capabilities_omit_auth_bound_methods_and_report_unsupported` | Stdio neither advertises auth-bound methods as callable nor omits their explicit unsupported report (M18 contract). |

## Peer host tools and shared history

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_run_a_destructive_tool_only_after_the_persons_approval` | The destructive tool executes only through the person's real approval decision. |
| 5 | `api::ui_protocol_transport::peer_host_tools_tests::should_show_the_person_lane_a_system_agent_turn_waiting_for_approval` | UPCR-2026-034's live request → in-progress → approval projection remains ordered without persisting runtime markers. |
| 5 | `api::ui_protocol_transport::peer_host_tools_tests::should_show_each_lane_the_others_recent_turns_without_persisting_them` | UPCR-2026-034 shares a bounded projection between separate transcripts, never a merged transcript. |
| 2 | `api::ui_protocol_transport::peer_host_tools_tests::should_run_the_person_lane_while_the_system_agent_lane_is_busy` | Person interaction remains usable while the system-agent lane is occupied. |
| 2 | `api::ui_protocol_transport::peer_host_tools_tests::should_cancel_an_in_flight_host_call_when_the_turn_is_interrupted` | Turn interruption propagates into the in-flight host-tool call. |
| 4 | `api::ui_protocol_transport::peer_host_tools_tests::should_time_out_and_cancel_a_call_the_host_never_answers` | An unanswered host call is bounded and cancelled rather than leaked. |
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_answer_a_host_tool_approval_only_on_its_connection_when_turn_ids_collide` | Reused turn ids cannot redirect approval authority across host connections. |
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_never_answer_or_remember_a_host_tool_approval_by_scope` | Scope alone never grants or retains host-tool approval authority. |
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_refuse_host_tool_registration_and_results_when_the_connection_is_external` | External connections cannot register host tools or inject their results. |
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_never_give_an_external_turn_a_host_routed_tool_when_one_is_registered` | Host-routed tools never enter an external turn's executable set. |
| 4 | `api::ui_protocol_transport::peer_host_tools_tests::should_finish_a_retry_when_an_earlier_attempt_erased_the_provisioned_workspace` | Peer retry recreates its required workspace and reaches completion after destructive failure. |

## Session actor recovery, persistence, and cancellation

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `session_actor::tests::supervisor_failure_signal_generates_recovery_continuation_end_to_end` | A real supervisor failure reaches the durable recovery continuation path. |
| 4 | `session_actor::tests::session_actor_note_persist_failure_no_phantom_and_retry_recovers` | Failed note persistence neither fabricates success nor prevents a later retry. |
| 4 | `session_actor::tests::test_persist_child_session_lifecycle_marks_orphaned_terminal_events` | Terminal child output cannot be lost when its parent no longer exists. |
| 4 | `session_actor::tests::test_persist_child_session_lifecycle_repairs_join_when_terminal_arrives_first` | Out-of-order terminal arrival still repairs the parent/child lifecycle join. |
| 2 | `session_actor::tests::cancel_task_reaches_earlier_turn_supervisor_after_a_later_turn_registers` | Cancellation targets the original supervisor even after later turn registration. |
| 4 | `session_actor::tests::test_speculative_overflow_concurrent` | Concurrent speculative overflow cannot corrupt or reorder committed session output. |
| 4 | `session_actor::tests::late_background_result_persists_with_originating_thread_id_not_derived_from_latest_user` | Late results stay bound to their originating thread rather than a newer user turn. |
| 4 | `session_actor::tests::recovery_turn_preserves_originating_client_message_id_from_failure_signal` | Recovery retains client-message correlation across the failure boundary. |
| 3 | `session_actor::tests::session_task_query_store_hides_absolute_output_paths` | Task query responses do not disclose server-local absolute paths. |
| 4 | `session_actor::tests::background_failure_recovery_capped_at_max_retries` | Persistent background failure cannot create an unbounded recovery loop. |
| 4 | `session_actor::tests::test_profile_session_keys_are_persisted_separately` | Equal session text in different profiles cannot share durable history. |
| 4 | `session_actor::tests::test_agent_error_persists_to_history` | Agent failure remains visible in durable history instead of vanishing. |

## DOT retirement and native-research replacement

These rows were restored from #2691's focused runtime validation after T-0562
found that the later broad deletion pass had removed them. They pin a deliberate
product contract, not the deleted DOT implementation. See
`docs/DOT_PIPELINE_RETIREMENT.md`.

| Cat. | Exact test | Guard |
|---|---|---|
| 5 | `runtime::profile::tests::profile_runtime_does_not_expose_dot_pipeline` | #2691's retirement survives old on-disk graphs, profile bootstrap, plugin reload, and session construction; model-facing `discover_tools(name=run_pipeline)` reports unknown/unavailable. |
| 2 | `runtime::profile::tests::native_research_synthesis_is_retained_for_plugin_reload` | Profile credential resolution and plugin reload still compose to supply the native-research synthesis provider/model/credential after a real plugin-layer rebuild. Matching-route and credential-isolation units remain retained in `octos-services/TESTS-KEPT.md`. |

## ACP, MCP, work-secret, and real-process integration

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `tests/acp_integration.rs::should_stream_assistant_message_and_end_turn_when_driven_through_acp_initialize_new_session_prompt` | The real ACP initialize/new/prompt chain streams the assistant update and terminal stop reason. |
| 4 | `tests/acp_integration.rs::should_accumulate_conversation_history_across_multiple_prompts` | ACP multi-turn prompts retain the actual accumulated conversation history. |
| 4 | `tests/acp_integration.rs::should_restore_a_conversation_through_session_load` | ACP session/load restores an existing conversation rather than starting empty. |
| 3 | `tests/acp_integration.rs::should_sanitize_stored_history_when_loading_a_session` | ACP replay sanitizes persisted history before exposing it to the client. |
| 1 | `tests/mcp_serve_integration.rs::should_execute_real_agent_session_via_mcp_dispatch_and_return_artifact` | MCP dispatch drives a real agent session through artifact production. |
| 4 | `tests/mcp_serve_integration.rs::should_return_typed_failure_on_session_failed_via_mcp` | Agent failure crosses MCP as a typed failure rather than a false successful result. |
| 3 | `tests/mcp_serve_integration.rs::should_not_leak_internal_iteration_messages_via_dispatch` | Internal agent iterations are not exposed through the MCP dispatch boundary. |
| 3 | `tests/mcp_serve_integration.rs::should_block_shell_write_outside_workspace_via_sandbox` | A real MCP-driven shell call cannot write outside its sandboxed workspace. |
| 4 | `tests/serve_host_managed.rs::serve_host_managed::serve_host_managed_stops_when_the_host_closes_stdin` | Closing the host's stdin stops the real host-managed server (UPCR-2026-036). |
| 4 | `tests/serve_host_managed.rs::serve_host_managed::serve_host_managed_stops_when_its_host_is_sigkilled` | Even SIGKILL of the host cannot leave an orphaned host-managed server. |
| 3 | `tests/serve_host_managed.rs::serve_host_managed::serve_host_managed_refuses_an_external_turn_reusing_a_host_turn_id` | An external process cannot take over host work by reusing its turn id. |
| 3 | `tests/serve_host_managed.rs::serve_host_managed::serve_host_managed_gives_an_external_turn_only_the_allowlisted_tools` | The real server enforces the external turn tool allow-list. |
| 4 | `tests/serve_sigterm.rs::serve_sigterm::serve_sigterm_reaps_gateway_children` | SIGTERM of the real server reaps steady-state gateway children (#2086). |
| 4 | `tests/serve_sigterm.rs::serve_sigterm::serve_sigterm_during_startup_reaps_gateways` | SIGTERM during the auto-start window neither orphans gateways nor starts the remainder. |
| 4 | `tests/serve_sigterm.rs::serve_sigterm::serve_sigterm_with_open_sse_stream_still_reaps_gateways` | An open SSE stream cannot prevent process shutdown and child reaping. |
| 3 | `tests/session_ingress_work_secret.rs::work_secret_ws_round_trip_and_revocation_close` | One real work-secret socket rejects raw `session/goal/set` with -32600, allows typed `session/status.get` with `active=false`, then closes with Policy/`session_ingress_revoked` after revocation. |
| 3 | `tests/session_ingress_work_secret.rs::work_secret_ws_client_hello_advertises_only_callable_methods` | Work-secret capability advertisement matches the methods the connection may actually call. |
| 3 | `api::ui_protocol_transport::tests::ws_origin_gate_allows_absent_or_empty_origin` | #924 allow-side: no/empty/whitespace Origin must allow TUI/gateway/scripts — deny tests cannot substitute; losing it silently regresses legitimate non-browser clients. |
| 3 | `api::ui_protocol_transport::tests::should_normalize_safe_tool_context_at_protocol_boundary` | Protocol-boundary sanitizer: tool_context rejects path separators, >64 chars, and empty/whitespace — the only test pinning `normalize_tool_context`'s rejection branches. |
| 1 | `tests/serve_workspace_team.rs::serve_workspace_team_shared_identity_two_clients_message_and_disconnect` | Two real workspace-team clients share identity and survive one client's disconnect. |
| 1 | `tests/steer_integration.rs::olp_ctrl_steer_subprocess_cli_writes_sidecar_and_marker` | The real CLI subprocess writes the steer sidecar and completion marker. |
| 4 | `tests/serve_ws_liveness.rs::serve_ws_liveness::serve_ws_liveness_closes_fully_silent_connection` | The real server closes a fully silent WebSocket instead of leaking it. |
| 4 | `tests/serve_ws_liveness.rs::serve_ws_liveness::serve_ws_liveness_answers_every_pipelined_request_id` | Real pipelined WebSocket requests retain one response per request id. |

| 2 | `api::ui_protocol_transport::tests::mcp_status_list_fallback_projects_config_oauth_true_and_false` | No-runtime fallback projects config.oauth true/always-present false onto mcp/status rows so a fresh page can offer re-authorization. |
| 3 | `contracts::diff::tests::hunk_content_lines_starting_with_dashes_and_pluses_are_not_file_headers` | Hunk content `--`/`++` (SQL/Lua/Haskell comments) is consumed by declared counts, not swallowed as a file header; line numbers stay correct. |
| 3 | `contracts::diff::tests::consecutive_special_lines_and_multi_file_do_not_cross_hunks` | Consecutive special lines and a real next file header do not cross hunks. |
| 3 | `contracts::diff::tests::empty_hunk_is_kept_without_lines` | An empty hunk (0,0 counts) is kept with no lines and no spurious file header. |
| 3 | `contracts::diff::tests::git_diff_paths_preserve_spaces_renames_and_quoted_utf8` | Git header with ordinary spaces / ` b/` substring / rename keeps correct files.path/old_path, not split at the first space. |
| 3 | `contracts::diff::tests::diff_git_paths_decode_quoted_paths_before_stripping_prefixes` | Quoted and mixed unquoted-a/+quoted-b/ headers decode without chopping the quoted path at a space. |
| 3 | `contracts::diff::tests::diff_git_paths_reject_malformed_quoted_headers` | Malformed quoted headers (trailing garbage, unterminated) fail closed instead of mislabelling. |
| 1 | `contracts::diff::tests::real_git_preview_preserves_tracked_untracked_and_rename_paths` | Real `git diff` on a space-bearing repo keeps files.path/old_path true to the actual file. |
| 3 | `contracts::sanitize::tests::sanitize_display_path_does_not_fold_dotdot_formed_by_control_removal` | Control removal cannot create a new `..` label collision; newline/CR/ESC/C1 and nested traversal stay visibly separate. |
| 3 | `contracts::sanitize::tests::sanitize_display_path_combines_traversal_and_unicode` | Literal `..` folds before bidi control removal; a control-bearing segment is not collapsed a second time. |
| 3 | `contracts::sanitize::tests::sanitize_display_path_preserves_spaces_and_utf8` | Ordinary spaces and UTF-8 path labels are not over-sanitized. |
| 2 | `api::ui_protocol_transport::tests::session_history_reports_broken_flat_but_keeps_users_sibling_read_only` | Broken flat branch reports unavailable_workspaces while legal users sibling still lists; read-only, no mkdir. |
## Office path validation

| Cat. | Exact test | Guard |
|---|---|---|
| 2 | `commands::office::tests::accept_changes_rejects_non_utf8_input_with_context` | Non-UTF-8 Office paths produce the structured Office-path/UTF-8 error without creating output; APFS still exercises the command when fixture creation returns EILSEQ. |

## Follow-up restored representative chains

These five rows close independent real-path gaps identified during lead re-review. They restore only representative behavior, not the deleted parameter variants.

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `tests/serve_solo_gateway_guard.rs::serve_solo_gateway_guard::solo_serve_refuses_manual_gateway_starts` | Solo serve rejects every manual gateway-start route and never spawns a child that could steal the profile's single-writer data-dir lock (#2546). |
| 2 | `tests/serve_solo_gateway_guard.rs::serve_solo_gateway_guard::non_solo_serve_still_starts_gateways_manually` | Non-solo manual start still spawns the disabled profile's gateway, proving the solo guard does not over-block. |
| 4 | `tests/serve_broken_pipe.rs::serve_broken_pipe::serve_shutdown_broken_pipe_cleanup_marker_observed` | Real EPIPE during shutdown still runs cleanup and exits cleanly; the same test also pins graceful-shutdown-before-gateway-stop ordering. |
| 1 | `tests/smart_home_ws_e2e.rs::smart_home_device_command_posts_to_real_bridge` | A real UI-protocol WebSocket drives profile smart-home configuration through the real bridge and returns the device response. |
| 4 | `api::handlers::session_mutation_tests::session_mutation_unreachable_gateway_preserves_only_actual_local_success` | Port 0 guarantees gateway refusal without a bind/drop race; actual local success survives, while any 404/non-502 mapping still fails. |

## Reviewed skill install (T-0556, UPCR-2026-R48CB467B)

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `commands::skills::reviewed_install_tests::should_install_exactly_the_reviewed_commit_and_report_it_on_reread` | The installed tree is the reviewed commit even after the branch drifts, and the on-disk receipt + list re-read report that actual commit. |
| 3 | `commands::skills::reviewed_install_tests::should_fail_closed_when_the_registry_entry_is_swapped` | Six labeled cases (swapped repo/commit/review, uncovered artifact, stale review, foreign platform) fail closed with their exact error kind and no published tree. |
| 3 | `commands::skills::reviewed_install_tests::should_install_signed_raw_and_archived_binaries_with_actual_digests` | One reviewed signing chain installs both raw and archived payloads, reports their actual digest/source commit on receipt + list, sets executable perms, and always extracts archive payloads to `main` without honoring member paths. |
| 3 | `commands::skills::reviewed_install_tests::should_fail_closed_on_artifact_substitution` | Served-bytes swaps (digest) and rogue-key signatures both fail closed before any publish. |
| 3 | `commands::skills::reviewed_install_tests::should_refuse_a_skill_tree_containing_symlinks` | A symlink in the reviewed tree aborts the install, so no escape target is ever read or published (unix). |
| 4 | `commands::skills::reviewed_install_tests::should_protect_the_existing_install_on_refusal_or_failure` | An existing install survives both a no-force refusal and a failed forced reinstall byte-for-byte. |
| 4 | `commands::skills::reviewed_install_tests::should_roll_back_a_published_install_and_restore_the_previous_one` | A post-publish failure restores the previous install; confirming consumes the backup; remove is the explicit inverse and reinstall uses a fresh non-forced review-bound request. |
| 3 | `commands::skills::reviewed_install_tests::should_reject_traversal_and_unknown_packages` | Traversal-shaped package names and unknown packages are refused before any filesystem or network work. |
| 5 | `commands::skills::reviewed_install_tests::should_project_review_provenance_into_the_search_result` | The registry search projection exposes exactly the identity fields the install request must bind. |
| 3 | `api::ui_protocol_transport::tests::reviewed_install_capability_gate_and_announcement` | The reviewed-install method is strict opt-in: unnegotiated/old-core callers get method_not_supported, unknown fields are rejected, and the feature/method are announced only where the skills surface exists. |
| 3 | `api::ui_protocol_transport::tests::reviewed_install_keeps_the_existing_profile_isolation_scope` | The reviewed install path keeps the shared authenticated profile-scope gate: a connection can never install into a foreign profile. |
| `api::ui_protocol_transport::tests::registry_search_reports_the_core_host_platform_for_reviewed_install` | 3 | `server_platform` equals the `platform_key()` the install gate checks — the web builds a legal reviewed-install request for source-only packages without guessing from the browser; wrong platform fails closed. |

## Authenticated catalog bootstrap (T-0404, UPCR-2026-R9631FCFB)

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_page_globally_and_survive_a_cold_reload` | Cross-profile keyset pages stay stable when a newer row lands ahead of a returned page, and the same server-minted refs resolve after a cold-state reload (new browser cannot depend on local recents). |
| 5 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_return_an_empty_session_page_with_workspace_summaries` | An empty catalog returns a valid empty page with zero counts and no cursor, and a registered Workspace without sessions is still bootstrapped. |
| 5 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_default_to_fifty_rows` | The bootstrap page is bounded by default (50) and the cursor continues past the first 50 rows. |
| 3 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_filter_authorized_profiles_without_enumerating_others` | A scoped or authenticated connection cannot widen its profile projection, while an unscoped admin receives the all-profile catalog instead of an implicit `_main` subset. |
| 3 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_rpc_should_preserve_external_and_profile_denials` | External connections and unauthorized profile requests are denied with typed errors before any catalog I/O. |
| 3 | `api::ui_protocol_transport::tests::catalog_bootstrap_requires_explicit_capability_and_preserves_ingress_scope` | `catalog/bootstrap` is strict opt-in (old clients get method_not_supported and never see it advertised), its typed codec round-trips, and a bound session-ingress credential cannot enumerate the catalog. |

## T-0589 inventory closure and healthy WS liveness

The earlier 28-item grouping had five OUP entries: four are direct memory RPC tests and `peer_checkpoint...` is a separate ownership/protocol guard below. Every previously unlisted test now has an explicit retained category and behavior guard; no file-level grouping or deletion is used.

| Cat. | Exact test | Guard |
|---|---|---|
| 2 | `qos_catalog::tests::should_preserve_per_profile_credential_pool_in_config_from_profile` | Profile flattening preserves the profile's own credential ids, redb state path, strategy, and cooldown instead of sharing the primary pool. |
| 5 | `qos_catalog::tests::should_leave_credential_pool_absent_for_profile_without_override` | Missing or empty pool config intentionally preserves legacy single-credential behavior. |
| 5 | `qos_catalog::tests::should_select_profile_pool_deterministically_and_prefer_default` | Ambiguous pools select the lexicographically smallest key, while an explicit `default` takes precedence. |
| 4 | `qos_catalog::tests::should_restore_profile_pool_state_across_restart_via_production_builder` | Reopening production builders restores the owning profile's cursor, cooldown, and failure counts without cross-profile credential leakage. |
| 2 | `qos_catalog::tests::should_wire_pool_and_classifier_into_router_when_configured` | Every provider slot receives the configured pool and router-level classifier, including the actual chat routing callback. |
| 4 | `qos_catalog::tests::should_rotate_credential_on_401_and_429_through_production_builder` | Both 401 and 429 cool the failed credential and rotate to another while recording the failure. |
| 5 | `qos_catalog::tests::should_leave_router_unwired_when_config_absent_or_disabled` | Absent credentials and disabled content routing intentionally leave acquisition and classification unwired. |
| 5 | `memory_consolidate::direct::tests::should_list_no_entries_for_missing_or_legacy_files` | Missing and legacy unlabelled memory files intentionally produce an empty direct-entry list. |
| 2 | `memory_consolidate::direct::tests::should_land_edit_verbatim_with_id_unchanged` | Direct edits preserve the id and sibling entries, append the kernel date stamp, and chain the returned version. |
| 4 | `memory_consolidate::direct::tests::should_report_not_found_for_unknown_id_and_change_nothing` | Unknown direct edit/delete ids return NotFound without changing memory. |
| 3 | `memory_consolidate::direct::tests::should_reject_content_that_cannot_form_one_entry_block` | Empty or multi-block caller content is refused without changing memory. |
| 4 | `memory_consolidate::direct::tests::should_delete_entry_and_cascade_to_archives_and_backups` | Direct delete scrubs matching archive/backup copies while retaining unrelated entries. |
| 4 | `memory_consolidate::direct::tests::should_keep_other_entries_intact_when_delete_scrubs_shared_lines` | Shared fact lines are scrubbed without deleting another entry's independent content. |
| 4 | `memory_consolidate::direct::tests::should_refuse_stale_delete_and_change_nothing` | A stale direct delete fails with VersionConflict and leaves memory unchanged. |
| 4 | `memory_consolidate::direct::tests::should_never_clobber_a_direct_delete_with_a_concurrent_consolidation` | A direct delete serialized behind consolidation conflicts honestly, then succeeds with the fresh version instead of being lost. |
| 4 | `memory_consolidate::direct::tests::should_keep_a_delete_that_landed_before_the_consolidation_started` | A delete completed before consolidation is not resurrected by the older model snapshot. |
| 3 | `api::ui_protocol_transport::tests::peer_checkpoint_is_owned_intermediate_and_never_consumed_as_a_final_result` | Only the owning peer wire can publish an intermediate checkpoint; it creates no final result/receipt and closes reject later writes. |
| 2 | `api::ui_protocol_transport::tests::should_edit_memory_entry_verbatim_through_rpc_with_refresh_disabled` | Session-scoped memory list/edit persists verbatim content and a new version without a runtime, LLM, or refresh service. |
| 4 | `api::ui_protocol_transport::tests::should_conflict_on_stale_memory_edit_version_and_change_nothing` | RPC stale edit returns RESOURCE_CONFLICT/current_version and leaves bytes unchanged. |
| 3 | `api::ui_protocol_transport::tests::should_report_not_found_for_another_profiles_memory_entry` | A session cannot delete another profile's memory id; malformed ids remain invalid_params and the owner's bytes are untouched. |
| 4 | `api::ui_protocol_transport::tests::should_delete_memory_entry_through_rpc_and_cascade_to_archives` | Authorized RPC delete returns its receipt and scrubs the id from current and archived memory. |
| 4 | `commands::oup_peers::tests::terminal_restore_reconnects_same_peer_without_replaying_brief` | Terminal restore reconnects the same peer without replaying its original brief. |
| 3 | `commands::oup_peers::tests::terminal_restore_ignores_foreign_closed_and_other_workspace_peers` | Restore excludes closed, foreign, and other-workspace peers instead of crossing tenant/workspace boundaries. |
| 4 | `commands::oup_peers::tests::terminal_restore_never_guesses_legacy_or_missing_fenced_workspace` | Missing or legacy fenced workspace evidence fails closed rather than guessing a restore target. |
| 2 | `commands::gateway::profile_factory::tests::should_wire_pool_and_classifier_in_profile_factory_router` | Profile-factory wiring attaches the pool/classifier and fail-closed fallback construction preserves operation/provider context. |
| 4 | `tests/serve_ws_liveness.rs::serve_ws_liveness::serve_ws_liveness_keeps_ponging_client_open_past_deadline` | A real application-idle WS client answers each binary Ping with Pong and remains open past the idle deadline, proving inbound Pong refreshes `last_inbound`; fully-silent close and pipelined ids stay separate. |

### Equivalent duplicate scenarios (not separately retained)

| Duplicate removed in T-0589 | Stronger retained coverage | Assertion-level equivalence |
|---|---|---|
| `qos_catalog::tests::should_isolate_credential_pools_between_profiles` | `qos_catalog::tests::should_restore_profile_pool_state_across_restart_via_production_builder` | The restart test performs the same production-builder A/401 rotation and initial B acquisition, then reopens both pools and asserts A-only failure state and B-only ids/zero failures. The removed test's additional Arc-pointer/file-exists checks are structural, not another behavioral outcome. |
| `memory_consolidate::direct::tests::should_list_entries_with_stable_id_full_text_and_version` | `api::ui_protocol_transport::tests::should_edit_memory_entry_verbatim_through_rpc_with_refresh_disabled` | The RPC test asserts the same stable id, full MEM_E1 text, and on-disk content hash, plus `total=2` and `truncated=false`, before proving the edit chain. |
| `memory_consolidate::direct::tests::should_reject_stale_version_and_change_nothing` | `api::ui_protocol_transport::tests::should_conflict_on_stale_memory_edit_version_and_change_nothing` | Both drive the same direct edit precondition and require the actual current version plus byte-identical MEMORY.md; the RPC test additionally verifies RESOURCE_CONFLICT and `version_conflict` wire mapping. |
