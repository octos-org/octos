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
| 3 | `api::handlers::session_file_security_tests::session_file_read_rejects_traversal_directories_and_symlinks` | Real filesystem reads, for a web session's workspace and for a chat's own folder (UPCR-2026-R1EFC56A3), serve the session's own file and reject `..`, absolute paths, directories, leaf symlinks, and symlinked ancestors without outside side effects. |
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
| 4 | `api::ui_protocol_transport::tests::spawn_report_announcement_cap_boundary_is_exact_and_over_cap_recovers` | Exact char-cap off-by-one: at-cap inlines full, over-cap truncates AND carries the read_task_output recovery pointer — the boundary cannot be substituted by a large-report-only test. |
| 1 | `api::ui_protocol_transport::tests::generic_interrupt_routes_to_owner_and_terminates_real_producer` | Generic turn interrupt routes through the session store to the live execution owner; duplicate interrupts terminate the real producer, and the restored replica learns the terminal only through persistence refresh. |
| 4 | `api::ui_protocol_transport::tests::generic_interrupt_after_real_completion_leaves_owner_completed` | Normal producer completion wins a late generic interrupt without a cross-supervisor terminal rewrite. |
| 4 | `api::ui_protocol_transport::tests::prompt_durability::should_commit_disconnected_prompt_before_terminal_without_duplicates` | Disconnect cannot drop or duplicate the durable prompt around terminal delivery. |
| 4 | `context_manager::tests::should_not_record_history_twice_when_a_turn_opens_with_a_new_runtime_note` | A turn that opens with a new runtime note commits its prompt behind that note, so the model prompt holds each row once instead of the whole conversation twice. |
| 1 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ingress_lifetime_tests::ordinary_ws_disconnect_preserves_turn_and_pending_prompts_over_real_ws` | Real WebSocket disconnect preserves ordinary work and queued prompts for reconnect. |
| 3 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ingress_lifetime_tests::valid_work_secret_disconnect_aborts_turn_and_pending_prompts_over_real_ws` | Revoked work-secret ingress cannot leave detached execution alive. |
| 3 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ingress_lifetime_tests::revoked_work_secret_prebind_cancels_only_matching_generation_over_real_ws` | A deterministic post-handshake/pre-bind revocation closes 1008 and retires only the presented grant revision's old generation, preserving different/same-token replacements, their automatic continuations, and post-revocation browser work. |
| 3 | `api::ui_protocol_transport::tests::ws_lifetime_tests::ws_detached_approval_and_question_wait_for_an_authorized_reconnect` | Detached requests remain unavailable until an authorized connection resumes them. |
| 4 | `api::ui_protocol_transport::tests::event_ledger_recovery_runs_exactly_once_across_concurrent_initializers` | Concurrent ledger initialization cannot execute recovery twice. |
| 4 | `api::ui_protocol_transport::tests::grant_io_tests::slow_grant_read_does_not_hold_admission_lock_across_sessions` | A 5.3-second grant-file read cannot hold the global admission lock or stall another session on a current-thread executor; the slow bind still completes honestly. |
| 3 | `api::ui_protocol_transport::tests::grant_io_tests::stale_grant_snapshot_cannot_retire_replacement_generation` | A delayed old-token snapshot is observed before a same-session replacement, so it cannot overwrite or retire the replacement generation. |
| 3 | `api::ui_protocol_transport::tests::grant_io_tests::revoke_after_snapshot_cannot_admit_revoked_generation`; `api::ui_protocol_transport::tests::ws_lifetime_tests::ingress_lifetime_tests::turn_admission_holds_grant_fence_through_active_turn_insert`; `api::ui_protocol_transport::session_turn_service::tests::grant_read_error_fails_closed_for_ingress_but_not_browser_turns` | Stable cross-process shared/exclusive grant locks fence both bind and real WS turn admission through ActiveTurn insertion, and an unavailable snapshot fails closed for a bound ingress caller while retaining the browser-only fallback. |
| 4 | `api::ui_protocol_transport::tests::turn_terminal_recovery_orphans_reused_turn_generation` | Cold recovery cannot attach a reused turn id to the wrong generation. |
| 4 | `api::ui_protocol_transport::tests::unified_terminal_sink_failure_with_ack_queues_recovery_under_profile` | A failed acknowledged terminal write is retried in the owning profile. |
| 3 | `api::ui_protocol_transport::tests::stdio_multi_profile_open_status_reads_isolated_runtime_policy_stamps` | Stdio profile routing cannot mix runtime policy or session state. |
| 3 | `api::ui_protocol_transport::tests::should_drop_cross_profile_session_opened_frames_when_connection_scopes_another_profile` | Session lifecycle frames cannot cross the authenticated profile boundary. |
| 3 | `api::ui_protocol_transport::tests::should_drop_cross_profile_background_activity_frames_when_connection_scopes_another_profile` | Background activity cannot leak to another profile's connection. |
| 3 | `api::ui_protocol_transport::session_turn_service::tests::approval_respond_matches_the_handler_for_each_caller_and_a_late_repeat` | External approval authority is owner-scoped and single-use (no session-wide scope), an external caller cannot decide a host-owned peer's or peer context's approval, a refused approval stays parked, and `approval/respond` decides alike through its WebSocket handler and `SessionTurnService` (T-0636; replaces the two external-approval rows). |
| 4 | `api::ui_protocol_transport::tests::should_fail_approval_send_when_ledger_persistence_fails` | Approval delivery cannot report success when its durable ledger write failed. |
| 4 | `api::ui_protocol_transport::tests::session_hydrate_file_mutations_honor_include_after_and_profile_scope` | Recovery hydration applies file mutations only in the requested cursor and profile scope. |
| 1 | `api::ui_protocol_transport::tests::peer_send_input_rehomes_to_reopened_peer_wire_on_reconnect` | Peer input delivery follows the reopened wire rather than a dead connection. |
| 4 | `api::ui_protocol_transport::tests::bc9_sf1_disconnect_releases_build_cache_slot` | Disconnect releases the per-session build-cache admission slot. |
| 5 | `api::ui_protocol_transport::tests::spec_section6_catalog_lists_every_advertised_method` | The OUP §6 specification and advertised method catalog remain deliberately synchronized (`api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`). |
| 3 | `api::ui_protocol_transport::tests::stdio_capabilities_omit_auth_bound_methods_and_report_unsupported` | Stdio neither advertises auth-bound methods as callable nor omits their explicit unsupported report (M18 contract). |

## Session turn service (T-0636, `docs/IM_REMOTE_CONTROL.md` §14)

Each test sends the same requests through the WebSocket handler and straight to `SessionTurnService` (no socket) on twin fixtures and compares replies, ledger rows and stores.

| Cat. | Exact test | Guard |
|---|---|---|
| 5 | `api::ui_protocol_transport::session_turn_service::tests::turn_interrupt_matches_the_handler_on_each_registry_state_and_a_late_repeat` | Unknown, mismatched, owned, foreign-owned (external) and finished turns, and a repeated interrupt, are decided alike by the handler and the service. |
| 5 | `api::ui_protocol_transport::session_turn_service::tests::turn_start_matches_the_handler_on_admission_and_each_refusal` | Admission (registry owner and profile, connection turn, the turn run once through the caller's runner) and each refusal — no text, no runtime, foreign profile, occupied for ordinary and external callers, revoked work-secret grant — match between handler and service. |
| 5 | `api::ui_protocol_transport::session_turn_service::tests::session_open_matches_the_handler_for_ordinary_ingress_and_foreign_scope` | Ordinary, work-secret ingress (filtered capabilities) and foreign-profile opens reply, record and write the same through the handler and the service. |

## External turns (T-0637, `docs/IM_REMOTE_CONTROL.md` §14)

`ExternalTurns` runs turns on the ledger-only connection against a real serve runtime, its durable ledger and existing chats, with a scripted model.

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `api::ui_protocol_transport::external_turns::tests::only_an_existing_chat_of_a_served_profile_can_be_bound` | An unknown profile, another profile's or an unprofiled session, a topic, a Code session, a chat that was never made and a broken chat cannot be bound; nothing falls back to `_main` or another session. |
| 1 | `api::ui_protocol_transport::external_turns::tests::a_turn_on_the_ledger_connection_writes_what_an_external_socket_turn_writes` | A turn with no socket writes the same durable rows, reply included, as an external WebSocket client's turn, and is offered no tool beyond the external allowlist. |
| 4 | `api::ui_protocol_transport::external_turns::tests::a_turn_id_runs_once_and_only_in_its_own_chat` | Starting a running or ended turn id again returns its record and runs nothing; the same id names no turn in another chat. |
| 4 | `api::ui_protocol_transport::external_turns::tests::after_a_restart_each_recorded_turn_is_settled_by_its_exact_id` | After a restart, a recorded turn that never started may run, one cut off mid-turn is orphaned and never rerun, an ended one is reported once and forgotten. |
| 3 | `api::ui_protocol_transport::external_turns::tests::the_entry_answers_and_stops_only_its_own_turns_and_records_no_scope` | The entry cannot answer another connection's approval or interrupt another connection's turn; its own approval is answered without recording the requested session scope. |

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

## Peer purge State 1 durable rescan (T-0398)

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `api::ui_protocol_transport::peer_host_tools_tests::should_rescan_state1_residue_with_the_persisted_owner_intent` | A real partial erase leaves State 1 residue; a fresh owner retry uses the durable scope, concurrent retries do not block or double-erase, State 2 stays refused, and completion is idempotent. |
| 3 | `peers::purge::tests::state1_intent_requires_owner_token_and_the_same_unfinished_generation` | Originators, forged/unknown tokens and a later finalized generation cannot turn a durable intent into deletion authority. |
| 3 | `peers::purge::tests::display_name_retry_skips_another_owners_intent` | A same-named intent belonging to another originator/token cannot shadow the authorized generation during recovery. |
| 4 | `peers::purge::tests::expired_state1_intent_is_removed_and_cannot_authorize_retry` | An expired durable intent is cleaned up and cannot resume deletion. |
| 4 | `api::ui_protocol_transport::peer_host_tools_tests::should_finish_only_metadata_when_restart_follows_the_token_tombstone` | A restart after the token tombstone but before slug finalization completes only the missing metadata and never repeats destructive erasure. |
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_refuse_an_old_intent_when_the_slug_is_a_new_generation` | An old purge intent cannot erase a different peer generation staged under the same slug. |
| 3 | `api::ui_protocol_transport::peer_host_tools_tests::should_refuse_re_staging_while_purge_recovery_is_unfinished` | Live intents and unfinalized tombstones block even binding-less re-staging; finalization and intent cleanup reopen a safe new generation. |

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
| 4 | `session_actor::tests::starting_delete_fails_queue_and_ignores_late_construction` | Deleting a Starting actor fails its queued messages and a late construction cannot resurrect it. |
| 4 | `session_actor::tests::starting_dispatch_releases_api_admission_for_delete` | Real ApiChannel HTTP DELETE acquires its write lease, removes Starting, and leaves the late construction disconnected. |
| 4 | `session_actor::tests::construction_replays_fifo_before_actor_start_and_bounds_queue` | Construction replay is FIFO and gated, and the 33rd queued message gets a retryable busy error without deadlocking. |
| 4 | `session_actor::tests::actor_run_waits_for_start_gate_before_inbox` | A real SessionActor does no model work for queued input until the dispatcher opens its start gate. |
| 4 | `session_actor::tests::publish_panic_fails_queued_turn_retryable` | A publish panic closes and drains the queued inbox into explicit retryable error completions instead of success or silent loss. |
| 4 | `session_actor::tests::delete_during_publish_cancels_before_model_and_allows_replacement` | Delete during accepted publication fences hydration/model work and permits a fresh generation after cleanup. |
| 4 | `session_actor::tests::deleting_session_rejects_without_parking_unrelated_dispatch` | Deletion-held construction fencing rejects immediately without factory consumption or dispatcher wait while an unrelated session still dispatches and constructs. |
| 4 | `session_actor::tests::publish_and_factory_share_construction_permits` | Factory and publish blocking I/O share the same eight permits in both directions; neither can bypass the other. |
| 4 | `session_actor::tests::sixty_fifth_start_is_busy_and_only_eight_factories_run` | The 65th Starting actor is rejected immediately, deleted starts still consume outstanding capacity, and at most eight blocking factories run concurrently. |
| 4 | `session_actor::tests::closed_respawn_respects_outstanding_capacity` | A closed Running actor cannot bypass the global construction limit when respawning. |
| 4 | `session_actor::tests::cancelled_parts_fail_queue_and_allow_retry` | Cancelled construction parts never become Running; queued work is retryable and a later generation succeeds. |
| 4 | `session_actor::tests::factory_panic_fails_queue_and_dispatcher_recovers` | A factory panic fails queued work, clears the slot, and does not kill later dispatch or retries. |
| 4 | `session_actor::tests::factory_panic_returns_correlated_api_completion`; `session_actor::tests::saturated_starting_actor_rejects_without_blocking_other_sessions`; `session_actor::tests::stalled_outbound_bounds_rejection_delivery_and_backpressures_admission`; `session_actor::tests::factory_panic_holds_delivery_permit_until_outbound_drains`; `session_actor::tests::startup_rejection_preserves_shared_api_stream_for_earlier_thread` | Startup rejection carries a correlated API terminal completion, bounded delivery permits cap stalled-outbound fallback tasks/senders and preserve construction lifetime, admission resumes after drain, and a rejected thread cannot close earlier requests on the shared real `/chat` stream. |
| 4 | `session_actor::tests::shutdown_fails_starting_queue_and_discards_late_parts` | Shutdown fails Starting queues and discards late parts without publishing them. |
| 4 | `session_actor::tests::stale_generation_never_consumes_replacement_queue` | A stale construction cannot consume or overwrite the replacement generation's queued work. |

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
| 4 | `autonomy::agent_orchestrator::goal_task_row_reconcile_tests::should_leave_the_binding_alone_when_a_live_incarnation_still_owns_the_task` | A real post-fence settle attempt is positively acknowledged and parked while restore reconcile runs; its generation survives and the released chain completes the owed row. |
| 4 | `api::ui_protocol_transport::tests::should_reconcile_and_settle_goal_rows_in_m9_task_output_fixture` | The real M9 fixture restores an unobserved terminal JSONL task into its owed SQLite row and also creates/settles its own goal task row; deleting the fixture observer fails both effects. |

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
| 3 | `commands::skills::reviewed_install_tests::should_not_let_a_dev_registration_outlive_the_skill_it_was_made_for` | A dev registration ends with its skill: a reviewed replacement moves the record away with the old folder, a receipt overrides a record that is still there, a rollback restores the dev skill, a commit leaves no record, and a plain forced install (also over the record of a hand-deleted folder) installs an installed skill even when its tree carries the same marker. A registration that cannot be removed or moved (read-only registry folder) makes the plain and the reviewed install fail and leaves the dev skill as it was. |
| 3 | `commands::skills::reviewed_install_tests::should_reject_traversal_and_unknown_packages` | Traversal-shaped package names and unknown packages are refused before any filesystem or network work. |
| 5 | `commands::skills::reviewed_install_tests::should_project_review_provenance_into_the_search_result` | The registry search projection exposes exactly the identity fields the install request must bind. |
| 3 | `api::ui_protocol_transport::tests::reviewed_install_capability_gate_and_announcement` | The reviewed-install method is strict opt-in: unnegotiated/old-core callers get method_not_supported, unknown fields are rejected, and the feature/method are announced only where the skills surface exists. |
| 3 | `api::ui_protocol_transport::tests::reviewed_install_keeps_the_existing_profile_isolation_scope` | The reviewed install path keeps the shared authenticated profile-scope gate: a connection can never install into a foreign profile. |
| `api::ui_protocol_transport::tests::registry_search_reports_the_core_host_platform_for_reviewed_install` | 3 | `server_platform` equals the `platform_key()` the install gate checks — the web builds a legal reviewed-install request for source-only packages without guessing from the browser; wrong platform fails closed. |

## Skill development from source (Relax JIT, experimental)

| Cat. | Exact test | Guard |
|---|---|---|
| 5 | `commands::skills_dev::tests::the_binary_carries_exactly_the_patches_the_setup_script_applies` | The patch list compiled into the binary for `octos skills jit setup` equals the `*.patch` files `scripts/jit/setup-relax.sh` applies; a patch added to the folder alone would give an installed Octos a different Relax than a checkout builds. |
| 2 | `commands::skills_dev::tests::a_new_skill_passes_manifest_validation_and_never_overwrites` | The starter crate from `octos skills new` passes the same strict manifest validation an install applies, with every placeholder filled in; an existing folder and a name with a path separator are refused. |
| 3 | `skill_dev_tool::tests::skill_dev_stays_in_the_workspace_and_does_nothing_without_an_approval` | The agent's `skill_dev` tool refuses a crate reached through a link out of the workspace, an absolute path outside it and the folder above, for every action; and with no approval channel or a denied approval it compiles, runs and registers nothing. |
| 3 | `skill_dev_tool::tests::check_and_run_reach_the_crate_only_through_the_session_sandbox` | `check` and `run` hand the Relax command to the session's sandbox with the session workspace as cwd and return what the sandbox's process printed; without a sandbox on the call, or with one that refuses, nothing is started; a stand-in Relax that leaves a trace proves the tool never starts it itself. |
| 4 | `skill_dev_tool::tests::an_early_end_stops_every_process_the_call_started` | Real processes: at the time limit, on an interrupt and when the call's future is dropped, a grandchild that would write a marker a second later is killed with its process group. |
| 3 | `skill_dev_tool::tests::skill_dev_is_offered_only_on_an_opted_in_server_with_a_runtime` | Through the real registry: the tool is absent for an unset, `0`, empty or other flag value, without a Relax binary, and on a host-scoped server; it is present only for `1` with a runtime. |

## Authenticated catalog bootstrap (T-0404, UPCR-2026-R9631FCFB)

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_page_globally_and_survive_a_cold_reload` | Cross-profile keyset pages stay stable when a newer row lands ahead of a returned page, and the same server-minted refs resolve after a cold-state reload (new browser cannot depend on local recents). |
| 5 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_return_an_empty_session_page_with_workspace_summaries` | An empty catalog returns a valid empty page with zero counts and no cursor, and a registered Workspace without sessions is still bootstrapped. |
| 5 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_default_to_fifty_rows` | The bootstrap page is bounded by default (50) and the cursor continues past the first 50 rows. |
| 3 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_should_filter_authorized_profiles_without_enumerating_others` | A scoped or authenticated connection cannot widen its profile projection, while an unscoped admin receives the all-profile catalog instead of an implicit `_main` subset. |
| 3 | `api::workspace_protocol::bootstrap_tests::catalog_bootstrap_rpc_should_preserve_external_and_profile_denials` | External connections and unauthorized profile requests are denied with typed errors before any catalog I/O. |
| 3 | `api::ui_protocol_transport::tests::catalog_bootstrap_requires_explicit_capability_and_preserves_ingress_scope` | `catalog/bootstrap` is strict opt-in (old clients get method_not_supported and never see it advertised), its typed codec round-trips, and a bound session-ingress credential cannot enumerate the catalog. |

## Durable session work mode (T-0595, UPCR-2026-R6982BEFC)

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `api::ui_protocol_transport::tests::session_work_mode_is_fixed_at_creation_and_read_back_after_a_restart` | The open that creates a session fixes its Chat / Code mode, and open, hydrate and list read it back from the record across connections, a `/clear` and a restart. An existing session (including a legacy one) is never moved to another mode, an unknown stored value reads as Code, and a client that did not negotiate `session.work_mode.v1` can neither set the mode nor see it. |
| 3 | `api::ui_protocol_transport::tests::chat_workspace_tests::chat_workspace_is_created_once_bound_to_its_chat_and_deleted_with_it` | A chat gets a private folder of its own only through the negotiated `session/chat/create`, for an unused API id of its profile; it opens only there, no other session may open a chat folder or below it, it stays read-only and offline unless set to at most workspace-write, only negotiated clients list chats, and deleting the session deletes the folder and binding. |
| 3 | `api::ui_protocol_transport::tests::chat_workspace_tests::chat_tools_cannot_leave_the_chat_folder` | A chat's runtime is bound to its own folder whatever hint it is given, keeps its transcript in the profile store and only the chat tools, and `read_file`, `write_file` and `send_file` refuse `../`, absolute paths, the profile's shared zones, the uploads root and symlinks into another chat, without writing outside. |
| 3 | `runtime::chat_workspace::server_root_tests::should_refuse_server_root_overlapping_profile_or_workspace` | Startup refuses a server-level chats root overlapping an existing profile data directory or a workspace at/below that root, while allowing a standard ancestor workspace whose precomputed hidden set covers the root; a failed legacy-root reservation propagates. |
| 3 | `runtime::chat_workspace::server_root_tests::should_unreserve_deleted_legacy_profile_root` | Deleting a legacy profile removes its process-wide chats-root reservation, so strict hidden-root resolution does not refuse unrelated profiles' commands after the deletion. |
| 3 | `runtime::chat_workspace::server_root_tests::should_refuse_no_sandbox_command_started_before_runtime_profile` | With no sandbox, the fixed shared server root makes a command started before a runtime-created Profile refuse instead of reading that profile's first chat; a reverse legacy-root mutation makes the test fail. |
| 3 | `runtime::chat_workspace::server_root_tests::should_hide_new_profile_chat_from_bwrap_command` | On Linux hosts with bubblewrap, a command started in an ancestor of the shared root writes a readiness marker, then still cannot see the runtime Profile's first chat; hosts without namespace support report the backend unavailable instead of claiming a pass. |
| 3 | `runtime::chat_workspace::server_root_tests::should_hide_new_profile_chat_from_macos_command` | On macOS, a command started in an ancestor of the shared root writes a readiness marker, then remains under the precomputed seatbelt denial while the first chat appears. |

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
| 4 | `memory_consolidate::stuck_ask_tests::should_park_an_ask_the_model_never_honors_after_three_backed_off_merges` | A user's ask the merge model never honors (#3253) is retried at most three times with backoff that survives restarts, then parked visibly (`memory/request` answers `failed`) and kept, while token use stops and no other note is quarantined. |
| 3 | `api::ui_protocol_transport::tests::peer_checkpoint_is_owned_intermediate_and_never_consumed_as_a_final_result` | Only the owning peer wire can publish an intermediate checkpoint; it creates no final result/receipt and closes reject later writes. |
| 2 | `api::ui_protocol_transport::tests::should_edit_memory_entry_verbatim_through_rpc_with_refresh_disabled` | Session-scoped memory list/edit persists verbatim content and a new version without a runtime, LLM, or refresh service. |
| 4 | `api::ui_protocol_transport::tests::should_conflict_on_stale_memory_edit_version_and_change_nothing` | RPC stale edit returns RESOURCE_CONFLICT/current_version and leaves bytes unchanged. |
| 3 | `api::ui_protocol_transport::tests::should_report_not_found_for_another_profiles_memory_entry` | A session cannot delete another profile's memory id; malformed ids remain invalid_params and the owner's bytes are untouched. |
| 4 | `api::ui_protocol_transport::tests::should_delete_memory_entry_through_rpc_and_cascade_to_archives` | Authorized RPC delete returns its receipt and scrubs the id from current and archived memory. |
| 4 | `commands::oup_peers::tests::terminal_restore_reconnects_same_peer_without_replaying_brief` | Terminal restore reconnects the same peer without replaying its original brief. |
| 3 | `commands::oup_peers::tests::terminal_restore_ignores_foreign_closed_and_other_workspace_peers` | Restore excludes closed, foreign, and other-workspace peers instead of crossing tenant/workspace boundaries. |
| 4 | `commands::oup_peers::tests::terminal_restore_never_guesses_legacy_or_missing_fenced_workspace` | Missing or legacy fenced workspace evidence fails closed rather than guessing a restore target. |
| 2 | `commands::gateway::profile_factory::tests::should_wire_pool_and_classifier_in_profile_factory_router` | Profile-factory wiring attaches the pool/classifier and fail-closed fallback construction preserves operation/provider context. |
| 3 | `commands::gateway::profile_factory::tests::should_isolate_credentials_when_profile_falls_back` | Real profile/strong builders, shared serve/gateway builder and the constructor used by gateway/child auto-registration must not send primary credentials to a different provider (chat or local discovery) or accept missing fallback credentials; own/default/OAuth keys, keyless local, same-family aliases and explicit sharing remain valid. Distinct endpoints within a family, including registered OpenAI proxies and aliases, require an exact selector unaffected by family login or sibling key names; two loopback services check chat/probe headers, path-only changes, missing/empty/reversed aliases, exact process-env selection over a sibling config-map key, same-endpoint alias compatibility and anonymous local versus missing explicit local credentials. |
| 3 | `qos_catalog::tests::should_scope_inherited_keys_to_effective_fallback_endpoints` | Implicit fallback credential sharing follows resolved endpoint defaults, trailing slashes, aliases and protocol roots; omitted URLs must not equate a proxy with a public endpoint. No external requests. |
| 4 | `tests/serve_ws_liveness.rs::serve_ws_liveness::serve_ws_liveness_keeps_ponging_client_open_past_deadline` | A real application-idle WS client answers each binary Ping with Pong and remains open past the idle deadline, proving inbound Pong refreshes `last_inbound`; fully-silent close and pipelined ids stay separate. |

### Equivalent duplicate scenarios (not separately retained)

| Duplicate removed in T-0589 | Stronger retained coverage | Assertion-level equivalence |
|---|---|---|
| `qos_catalog::tests::should_isolate_credential_pools_between_profiles` | `qos_catalog::tests::should_restore_profile_pool_state_across_restart_via_production_builder` | The restart test performs the same production-builder A/401 rotation and initial B acquisition, then reopens both pools and asserts A-only failure state and B-only ids/zero failures. The removed test's additional Arc-pointer/file-exists checks are structural, not another behavioral outcome. |
| `memory_consolidate::direct::tests::should_list_entries_with_stable_id_full_text_and_version` | `api::ui_protocol_transport::tests::should_edit_memory_entry_verbatim_through_rpc_with_refresh_disabled` | The RPC test asserts the same stable id, full MEM_E1 text, and on-disk content hash, plus `total=2` and `truncated=false`, before proving the edit chain. |
| `memory_consolidate::direct::tests::should_reject_stale_version_and_change_nothing` | `api::ui_protocol_transport::tests::should_conflict_on_stale_memory_edit_version_and_change_nothing` | Both drive the same direct edit precondition and require the actual current version plus byte-identical MEMORY.md; the RPC test additionally verifies RESOURCE_CONFLICT and `version_conflict` wire mapping. |

## Host-scoped execution follow-up (#3260 / T-0606)

| Category | Exact test | Boundary retained |
| --- | --- | --- |
| 3 | `api::ui_protocol_transport::tests::active_only_steer_never_starts_a_replacement_turn` | Idle/terminal active-only steer returns the precise rejection instead of starting a replacement turn; legacy capability advertisement stays opt-in. |
| 3 | `api::ui_protocol_transport::tests::host_scope_denies_an_already_loaded_foreign_profile` | The process-wide host pin excludes even an already-loaded foreign runtime; run in an isolated child to avoid contaminating ordinary profiles. |
| 3 | `runtime::host_scope::tests::shared_host_sandbox_discards_profile_overrides_and_credential_paths` | Host constraints replace ambient overrides; the actual sandbox refuses unsupported non-macOS hosts instead of falling back to unconfined execution. |
| 3 | `tests/serve_host_scoped.rs::should_reject_host_scope_without_network_fence_or_independent_instance` | The real stdio server requires no-network and a separate canonical instance directory. |
| 3 | `tests/serve_host_scoped.rs::should_confine_real_stdio_sessions_to_the_host_profile_and_workspace` | Real session dispatch pins the profile, installs confined read/list/write tools, rejects ancestor symlinks, and refuses network-widened bootstrap while ordinary workspace I/O succeeds. |

## CLI cron identity (T-0303)

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `commands::cron::tests::should_create_distinct_ids_and_remove_only_one_job` | Sequential creations have distinct IDs and removing one keeps all other schedules/state. |
| 4 | `commands::cron::tests::should_refuse_ambiguous_historical_ids_without_writing` | Duplicate historical IDs and missing targets fail before any write. |
| 4 | `commands::cron::tests::should_preserve_other_jobs_raw_bytes_across_add_and_remove` | Add/remove preserve unrelated legacy job bytes, omitted defaults and unknown fields. |
| 4 | `commands::cron::tests::should_retry_an_occupied_id_and_refuse_unreadable_snapshots` | An occupied candidate is regenerated, while corrupt/unreadable snapshots cannot be treated as an empty ID namespace. |

## Explicit chat → profile routing (#1506)

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `commands::gateway::tests::should_refuse_only_misresolved_explicit_profile_routes` | Only an operator-pinned (explicit map) route whose target failed to resolve is refused; heuristic routes keep their legacy fallback and a resolved target dispatches — an explicit mapping can never silently land on the default profile. |

## Standard cron weekdays (T-0263 / UPCR-058)

Creation admission also retains these persistence regressions (T-0323):

| Category | Test | Contract |
| --- | --- | --- |
| 4 | `commands::cron::creation_tests::creation_rejects_unschedulable_cli_cron_without_writing` | Rejected CLI expressions leave empty/existing stores byte-identical; spaced fields, shorthand and valid explicit years remain supported. |
| 4 | `cron_tool::tests::creation_rejects_unschedulable_tool_cron_without_writing` | Actual tool creation rejects impossible dates, exhausted years, DST gaps and invalid zones without changing disk or live job count. |

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `cron_tool::tests::cron_tool_and_natural_requests_keep_the_requested_weekday` | Raw tool (including internally spaced fields) and Matrix natural-language input persist the requested dates; invalid weekday leaves disk unchanged. |

## Invalid cron timestamps (T-0302)

| Cat. | Exact test | Guard |
|---|---|---|
| 5 | `api::cron_panel::timestamp_tests::should_render_invalid_deadlines_without_overflowing` | List invalid persisted deadlines without overflow or misleading delay; retain raw timestamp and legal overdue display. |

## Native peer reopen/fork durable resume (octos#2026, T-0411)

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `peers::reopen_fork_tests::should_reopen_a_closed_peer_preserving_work_and_refusing_unauthorized_or_open` | Reopen clears ONLY the close marker: brief, committed results, unfinished notes and originator survive; non-originator refused with no state change; reopening an OPEN peer refused (crash/restart consistency); synthesis stamp restored at the committed round so old results do not re-fire. |
| 4 | `peers::reopen_fork_tests::should_stamp_a_legacy_bare_result_peer_on_reopen` | A legacy peer with only the bare `result.md` (no numbered versions) is stamped at delivered round 1 on reopen, so its old result cannot re-arm synthesis after the last-peer close reset. |
| 3 | `peers::reopen_fork_tests::should_fork_a_new_identity_with_scoped_inheritance_and_no_authority_carryover` | Fork mints a NEW identity inheriting brief/workspace/owner but NOT closed state, result history or the task-id binding; source untouched; non-originator refused; independent lifecycles. |
| 4 | `peers::reopen_fork_tests::should_fork_unique_addresses_inherit_budget_and_refuse_a_fenced_source` | Two unnamed forks get distinct slugs and NO shared display name (unambiguous name resolution); the budget LIMIT is inherited without the usage ledger; a fenced source is refused with no half-staged dir. |
| 3 | `peers::reopen_fork_tests::should_refuse_reopen_and_fork_when_the_source_is_purge_owned` | A slug tombstone (unfinalized OR finalized) or a live purge intent/recovery blocks BOTH reopen and fork; the purge's close marker survives and no residue brief is inherited. |
| 5 | `peers::reopen_fork_tests::should_fork_with_an_explicit_new_name` | An explicit fork name is honored and a duplicate explicit name is refused with no side effects. |

## First-turn task inputs (UPCR-2026-RBC0874AA)

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `api::ui_protocol_transport::first_inputs_tests::should_put_the_page_image_and_its_data_in_a_handed_off_peers_first_request` | Through the real `peer_handoff` tool, staging, `turn/start` and the OpenAI-compatible provider: the peer's first HTTP request carries the image bytes, the page data and a 120 KB file whole in one round with no `read_file` call, where the same handoff without `inputs` takes two rounds and two calls. |
| 4 | `api::ui_protocol_transport::first_inputs_tests::should_replay_a_peers_inputs_once_in_later_turns_as_they_were_first_read` | A later turn carries the inputs once, with the bytes the first message read, after the files on disk changed; the inputs are not attached again. |
| 5 | `api::ui_protocol_transport::first_inputs_tests::should_refuse_the_handoff_when_a_required_input_is_missing` | A handoff whose required input is not in the peer's working directory fails with the per-input report, leaves nothing staged, emits no `peer/staged` and makes no model call. |
| 4 | `api::ui_protocol_transport::first_inputs_tests::should_tell_the_peer_nothing_was_delivered_when_a_required_input_changed_or_went` | A required input deleted, or rewritten against its `sha256` pin, between handoff and first turn: no partial set is sent, the first request says which input and why, and the recorded report agrees. |
| 5 | `api::ui_protocol_transport::first_inputs_tests::should_withhold_a_peers_inputs_when_they_exceed_the_models_context` | A required input larger than the model's input budget is not truncated: no part of it reaches the request, which names it as over budget. |
| 3 | `api::ui_protocol_transport::first_inputs_tests::should_not_read_for_a_peer_what_its_own_session_may_not_read` | A declared file outside what the peer session's own file tools may read is not read on its behalf; its content never reaches the provider and the report says denied. |
| 4 | `peers::first_inputs::tests::should_record_the_declaration_as_given_when_every_required_input_is_there` | The declaration recorded beside the brief reads back equal (kind, optional, pin), and the staging note states each input's readiness. |
| 5 | `peers::first_inputs::tests::should_tell_a_fenced_peers_master_why_an_uncommitted_input_is_not_there` | Refusing a fenced peer's missing required input says the worktree holds only committed files, and records nothing. |
| 4 | `peers::first_inputs::tests::should_report_a_corrupt_declaration_instead_of_reading_it_as_none` | An unreadable recorded declaration is an error, never "no inputs", so a peer is not started silently without them. |
| 2 | `goal_tool::declared_inputs_tests::should_offer_task_inputs_to_the_planner_and_carry_them_onto_the_task` | `goal_plan` tells the keeper what declaring a task input does, and a declared input reaches the planned task. |
| 5 | `goal_tool::declared_inputs_tests::should_reject_plan_when_a_tasks_inputs_are_malformed` | A plan with a malformed `inputs` (not a list, no path, unknown field) is rejected naming the task, instead of planning a task that silently lacks them. |
