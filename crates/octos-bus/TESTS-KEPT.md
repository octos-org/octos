# octos-bus tests kept

Tracking: #3069. This is the allow-list for the later deletion pass, not a request to add or restore tests. Every row names a test discovered on current main (`b823eb0ae`) with `cargo +1.97.1 test -p octos-bus --all-features -- --list`.

Baseline: **882 tests / 28,574 test lines** measured by `/home/shu/kamico/octos/wake/test-lines.py` on the same main. Default-feature discovery (`cargo +1.97.1 test -p octos-bus -- --list`) finds 369 tests; all-feature discovery is authoritative here so optional channel security and transport coverage cannot disappear by accident.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security/input bounds; **4** recovery/data/concurrency/protocol framing; **5** intentional design invariant. A test belongs here only when its row states the behavior that would be lost. Structural shape, constants, getters, formatting-only checks, and parameter permutations do not qualify.

## 1. Cross-system

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `api_channel::tests::delete_stream_tests::deleted_turn_output_cannot_enter_or_close_a_new_thread` | Output from a deleted incarnation cannot contaminate or close a fresh thread. |
| 1 | `api_channel::tests::delete_stream_tests::topic_requests_have_independent_streams_and_delete_scope` | Topic deletion closes only its own stream and leaves unrelated chats live. |
| 1 | `api_channel::tests::spawn_only_file_delivery_is_visible_to_watcher_replay_after_reconnect` | Actor-persisted background media remains replayable after reconnect with a downloadable profile-relative handle. |
| 1 | `cron_service::tests::serve_delivery_deletes_one_shot_only_after_ack` | A one-shot serve job is removed only after delivery acknowledgement. |
| 1 | `deleted_session_rejects_late_writes_from_all_transcript_writers` | Deletion revokes stale handle, manager, canonical, rewrite, and title writers without resurrection. |
| 1 | `deletion_barrier_survives_reopen_and_scopes_store_and_topic` | Cold writers observe the tombstone while sibling topics and independent roots remain writable. |
| 1 | `feishu_channel::tests::should_use_reply_endpoint_when_reply_to_present` | Replies use Feishu's parent-message endpoint instead of losing threading. |
| 1 | `fresh_user_can_reopen_without_admitting_old_deliveries` | A fresh user incarnation can reopen while old handles and deliveries remain revoked. |
| 1 | `matrix_channel::tests::should_fan_out_allbots_to_bound_child_bots` | `/allbots` fans out distinct inbound messages with each room-bound child's correct profile binding and filters a globally registered target bound only to another room. |
| 1 | `matrix_channel::tests::test_handle_transaction_drops_ignored_senders_even_as_text` | The real transaction-to-inbound path drops ignored bot senders and prevents bot reply loops. |
| 1 | `matrix_user_channel::tests::should_stay_silent_when_group_message_mentions_another_user` | Sync parsing and membership probing suppress group messages addressed only to another user. |
| 1 | `refused_seal_preserves_sealed_history_on_the_handle_write_path` | Handle writes refuse to overwrite an occupied segment and preserve its bytes and new rows. |
| 1 | `touch_over_recoverable_seal_state_does_not_rearm_segment_replacement` | Gateway touch recovers the true segment count so the next rollover cannot replace old history. |

## 2. Cross-feature

| Cat. | Exact test | Guard |
|---|---|---|
| 2 | `api_channel::tests::session_messages_default_source_returns_recent_window_with_absolute_seq` | History windows preserve absolute session sequence numbers. |
| 2 | `dedup::tests::should_accept_id_again_after_forget` | Failed processing can revoke dedup admission so a platform retry is processed. |
| 2 | `heartbeat::tests::test_heartbeat_sends_on_content` | Actionable heartbeat content becomes a synthetic system inbound message. |
| 2 | `heartbeat::tests::test_heartbeat_skips_empty` | A header-only heartbeat file produces no spurious inbound turn. |
| 2 | `matrix_user_channel::tests::should_answer_mention_of_other_user_inside_direct_room` | `m.direct` data overrides group-style suppression so direct-room messages reach the dispatcher. |
| 2 | `should_preserve_sealed_segments_and_control_rows_on_rename` | Title mutation preserves sealed bytes, control rows, unknown metadata, and folded history. |
| 2 | `wecom_bot_channel::tests::should_store_req_id_during_parse_callback` | Callback ingestion records request IDs for later streamed-response correlation. |

## Mandatory security-boundary review (non-additive)

These five review pointers make the security boundaries explicit before category 3. They reference existing retained tests and do not add entries to the 81-test inventory.

| Boundary | Exact retained test or no such boundary | Guard |
|---|---|---|
| Path traversal | `absolute_input_with_dotdot_through_missing_component_rejected` | Lexically collapses `..` before containment checks so missing components cannot be used to escape allowed roots. |
| Sandbox escape | `workspace_relative_symlink_resolution_is_lexical_not_canonical` | Preserves the lexical symlink path for downstream `O_NOFOLLOW` enforcement instead of resolving to an external target. |
| Authentication/authorization | `env_token_arms_the_whole_session_surface` | Real TCP probes prove the bearer token protects the entire session surface, not only selected routes. |
| Secret/token redaction | `matrix_channel::tests::media_download_redirect_does_not_leak_as_token` | Refuses cross-origin media redirects so the appservice token cannot be disclosed to another host. |
| External input size bound | `media::tests::download_media_with_cap_rejects_oversized_body` | Rejects oversized response bodies at the byte cap, bounding attacker-controlled memory and disk use. |

## Cross-feature and late-failure review pointers (non-additive)

| Focus | Exact retained test | Guard |
|---|---|---|
| Feature crossover | `matrix_user_channel::tests::should_answer_mention_of_other_user_inside_direct_room` | `m.direct` account data overrides group-style mention suppression so direct-room delivery still reaches the dispatcher. |
| Late failure | `bus::tests::failed_outbound_send_preserves_the_unsent_message` | A closed outbound bus returns the original unsent message for recovery or requeuing instead of losing it after dispatch fails. |

## 3. Authorization, security, and input bounds

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `absolute_input_with_dotdot_through_missing_component_rejected` | Lexical `..` collapse prevents allowed-root escape through missing components. |
| 3 | `env_token_arms_the_whole_session_surface` | Over real TCP, the environment token protects every session-surface route. |
| 3 | `feishu_channel::tests::feishu_webhook_empty_encrypt_key_no_longer_accepts_publicly_keyed_forgeries` | Empty encrypt-key configuration cannot arm a publicly computable forgery path. |
| 3 | `feishu_channel::tests::feishu_webhook_http_path_enforces_armed_token_over_real_tcp` | Real armed webhooks reject missing credentials and forward only valid events. |
| 3 | `feishu_channel::tests::feishu_webhook_replayed_capture_is_refused_on_the_wire` | A validly signed but stale capture is rejected without inbound delivery. |
| 3 | `matrix_channel::tests::media_download_redirect_does_not_leak_as_token` | Media redirects cannot disclose the appservice token to another origin. |
| 3 | `matrix_channel::tests::should_enforce_target_cap_when_allbots_invoked` | Broadcast target caps bound amplification and dispatcher load. |
| 3 | `matrix_channel::tests::test_matrix_registration_file_mode_is_owner_only` | Registration material containing connection secrets is owner-only on disk. |
| 3 | `matrix_channel::tests::test_matrix_send_rejects_unregistered_sender` | Outbound metadata cannot impersonate an unregistered Matrix sender. |
| 3 | `matrix_channel::tests::test_private_bot_message_blocked_for_non_owner` | A non-owner cannot dispatch work to a private bot through room routing. |
| 3 | `media::tests::download_media_with_cap_rejects_oversized_body` | Media download caps bound attacker-controlled disk and memory use. |
| 3 | `session_artifacts::tests::should_fail_closed_when_initialized_manifest_is_missing` | Missing initialized manifest does not trigger reinitialization or deletion. |
| 3 | `session_artifacts::tests::should_never_delete_files_when_manifest_is_corrupt` | Corrupt manifest state fails closed without copying, eviction, downloads, or deletion. |
| 3 | `session_artifacts::tests::should_not_read_through_a_symlinked_delivered_ancestor` | Downloads refuse symlinked artifact ancestors instead of disclosing external bytes. |
| 3 | `session_artifacts::tests::should_reject_non_delivered_manifest_keys_without_deleting` | A manifest entry targeting a non-delivered path is rejected without deleting the target. |
| 3 | `should_reject_untrusted_metadata_before_mutation_in_both_layouts` | Rename/delete reject foreign identity, malformed metadata, and future schemas before mutation. |
| 3 | `the_wire_refuses_a_rebound_host_and_a_cross_site_origin` | Raw HTTP rejects DNS-rebinding Host and cross-site Origin attacks. |
| 3 | `workspace_relative_symlink_resolution_is_lexical_not_canonical` | Lexical symlink paths are preserved for downstream `O_NOFOLLOW` checks. |

| 3 | `session::tests::broken_flat_sessions_branch_keeps_store_users_without_redirect_or_mkdir` | A plain-file/file-link/dangling/loop flat `sessions` branch must not redirect users discovery to the link target parent; legal own users survive and no mkdir replaces the broken branch. |
| 3 | `cron_types::tests::every_interval_overflow_is_unschedulable_without_panicking` | every_ms=i64::MAX/0/negative and 2099-overflow intervals are unschedulable (checked_add, no debug panic/release wrap, no disk write); normal interval and 2099 boundary still schedule. |
## 4. Recovery, data, concurrency, and protocol framing

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `api_channel::tests::concurrent_bus_side_persists_get_distinct_seqs` | Concurrent API-channel persists receive distinct committed sequence numbers for watcher correlation. |
| 4 | `api_channel::tests::concurrent_same_chat_streams_do_not_cross_talk_via_last_content` | Same-chat turns compute deltas from per-thread state without cross-turn content leaks. |
| 4 | `api_channel::tests::delete_stream_tests::delete_failure_keeps_pending_and_the_error_retryable` | Storage failure retains pending streams and allows a later successful retry. |
| 4 | `api_channel::tests::delete_stream_tests::deletion_waits_for_actor_admission_without_blocking_other_chats` | Deletion fences admitted actor work without stalling unrelated chats. |
| 4 | `api_channel::tests::should_close_incomplete_completion_with_error_not_success_done` | A truncated completion closes with an honest terminal error rather than successful `done`. |
| 4 | `api_channel_preserves_thread_binding_under_generated_completion_orders` | Generated completion orderings preserve thread binding and per-thread event sequences. |
| 4 | `bus::tests::failed_outbound_send_preserves_the_unsent_message` | A closed outbound bus returns the original message for recovery instead of silent data loss. |
| 4 | `channel::tests::should_dispatch_next_message_when_channel_send_is_stuck` | A permanently blocked channel send times out without head-of-line blocking later outbound dispatch. |
| 4 | `coalesce::tests::test_hard_cut_unicode` | Hard splitting respects UTF-8 boundaries at the byte limit. |
| 4 | `coalesce::tests::test_truncation_on_massive_input` | Massive output is capped at 50 chunks plus an explicit truncation marker. |
| 4 | `cron_service::tests::concurrent_adds_and_reconciling_toggles_lose_nothing` | Racing additions and reload-based toggles lose no jobs and remain consistent with disk. |
| 4 | `cron_service::tests::corrupt_cron_store_is_quarantined_not_silently_discarded` | Corrupt cron bytes are quarantined rather than silently destroyed. |
| 4 | `cron_service::tests::serve_delivery_failure_retains_then_disables_after_five_attempts` | Failed delivery retains retry state and disables only after five attempts. |
| 4 | `cron_service::tests::should_deliver_reserved_fire_when_rearm_aborts_timer_mid_tick` | A committed firing is delivered even when schedule mutation aborts the timer mid-tick. |
| 4 | `cron_service::tests::should_fire_due_job_exactly_once_when_rearm_races_mid_fire` | Re-arming during delivery cannot duplicate a due job. |
| 4 | `feishu_channel::tests::token_fetch_runs_concurrently_without_holding_cache_lock` | Slow token fetches do not hold the cache lock or deadlock concurrent callers. |
| 4 | `http_client::tests::channel_http_client_times_out_stalled_response` | The shared channel client fails a stalled peer within its total deadline. |
| 4 | `matrix_channel::tests::test_handle_transaction_bad_json_does_not_poison_txn_id` | A malformed transaction does not consume its ID, so a valid retry still reaches the inbound bus. |
| 4 | `matrix_channel::tests::test_matrix_dedup_txn_id` | Replayed Matrix transaction IDs are acknowledged without forwarding duplicates inbound. |
| 4 | `matrix_channel::tests::test_matrix_register_bot_fails_when_route_persist_fails` | Failed route persistence rolls back the route and sender authorization. |
| 4 | `matrix_user_channel::tests::initial_sync_retries_until_cursor_obtained` | Initial-sync failure is retried until a cursor exists instead of starting without recovery state. |
| 4 | `media::tests::download_media_with_cap_fails_when_server_stalls` | A stalled media server fails within the bound instead of blocking ingress indefinitely. |
| 4 | `resume_policy::tests::retry_pinned_unresolved_call_does_not_delete_resolved_sibling_result` | Mixed retry and resolved calls are sanitized without breaking call/result pairing. |
| 4 | `session::contract_read_tests::canonical_contract_rejects_corrupt_transcripts_and_retries` | Corrupt metadata, schema, UTF-8, and rows are rejected without modifying bytes, then retry works after repair. |
| 4 | `session::contract_read_tests::strict_contract_load_preserves_legacy_and_sealed_history` | Strict updates detect corruption in legacy and sealed history instead of rewriting a partial snapshot. |
| 4 | `session::contract_read_tests::strict_contract_load_recovers_interrupted_seal_and_replays_rollback` | Interrupted-seal recovery rebuilds active metadata and replays rollback correctly. |
| 4 | `session::contract_read_tests::strict_tail_receipt_does_not_excuse_other_corruption` | A valid torn-tail receipt does not whitelist unrelated corruption. |
| 4 | `session::lifecycle::tests::slow_acquire_does_not_block_the_executor` | Slow lifecycle acquisition stays off the async executor so unrelated tasks proceed. |
| 4 | `session::replay_window_tests::append_retry_beyond_the_loaded_window_returns_original_receipt` | Retry finds durable records beyond the bounded replay window without duplication. |
| 4 | `session::replay_window_tests::independent_managers_serializing_the_same_record_commit_once` | Independent managers racing one idempotency key commit one row and return the original sequence. |
| 4 | `session::sequence_tests::independent_handles_keep_global_sequences_across_repeated_rollover` | Stale handles preserve global sequences and history across rollover and bounded reload. |
| 4 | `session::tests::concurrent_canonical_contract_upserts_lose_no_updates` | Per-key locking covers read-modify-write so concurrent completions cannot erase sibling state. |
| 4 | `session::tests::should_not_split_a_live_persist_lock_while_pruning` | Lock-cache pruning cannot replace a live mutex and admit two writers. |
| 4 | `session::tests::should_rebuild_after_a_seal_when_the_newest_segment_meta_is_unreadable` | Interrupted-seal and metadata damage still recover rows, active state, and the next sequence. |
| 4 | `session::tests::should_surface_active_read_error_instead_of_committing_past_tail` | Unreadable active bytes fail sequence discovery before committing past the damaged tail. |
| 4 | `session::tests::torn_tail_complete_row_is_recovered` | A complete row missing only its newline survives crash-tail sealing and later reload. |
| 4 | `session_artifacts::tests::should_not_resurrect_evicted_entry_after_interrupted_redelivery` | Interrupted redelivery cannot resurrect an evicted entry. |
| 4 | `session_artifacts::tests::should_recover_new_key_after_rename_crash_and_restore_quota` | Recovery adopts an artifact whose rename landed before its manifest entry and restores quota. |
| 4 | `wecom_bot_channel::tests::should_build_stream_frame_final` | The final WeCom stream frame carries terminal semantics and final content. |
| 4 | `wecom_bot_channel::tests::should_clear_req_id_map_on_overflow` | Request-correlation state is cleared at overflow rather than growing or retaining stale routes. |

## 5. Intentional design invariants

| Cat. | Exact test | Guard |
|---|---|---|
| 5 | `cron_types::tests::cron_expression_upcoming_check_uses_requested_timezone` | Schedulability uses the requested timezone across DST gaps and the intentional [2099 product boundary](src/cron_types.rs#L195-L223). |
| 5 | `matrix_user_channel::tests::should_fail_open_when_membership_probe_hangs` | A hung membership probe intentionally [fails open after `DM_PROBE_TIMEOUT`](src/matrix_user_channel.rs#L2254-L2256) instead of indefinitely stalling delivery. |


## Standard cron weekdays (T-0263 / UPCR-058)

| Cat. | Exact test | Guard |
|---|---|---|
| 1 | `cron_expression::tests::standard_weekdays_schedule_in_local_time` | User weekday normalization feeds real timezone scheduling, including spring DST. |
| 4 | `cron_expression::tests::legacy_weekdays_survive_reload_toggle_and_copy` | Persisted v1 numeric weekdays, internally spaced fields and explicit years retain execution dates after reload, toggle, display and copy. |
