# octos-llm — tests kept

TEST-DIET (#3069) retention inventory for `crates/octos-llm`, measured on `main@ed914b5fc`. This is a keep-list, not a deletion list: every entry below was checked against the actual test binary with `cargo +1.97.1 test -p octos-llm --lib -- --list` (807 discoverable lib tests). No test deleted by an earlier diet slice is resurrected here.

Categories: **1** cross-system user-visible chain; **2** cross-feature interaction; **3** security/credentials/bounds; **4** recovery/data/concurrency/protocol-frame correctness; **5** intentional design decision.

Concrete `EmbedQueueFull` shedding/backfill tests live in `octos-embed-llama` and `octos-agent` (T-0317), not in this crate; the llm-side bounded scheduling/queue interactions that do exist are listed below.

## Provider request/response and streaming protocols

| Test (exact `cargo test --list` name) | Category | Property guarded |
|---|---:|---|
| `sse::tests::test_chunked_data` | 4 | Coalescing arbitrary TCP chunk boundaries cannot change SSE event parsing. |
| `sse::tests::test_done_sentinel` | 4 | The terminal sentinel is recognized without being emitted as assistant content. |
| `sse::tests::test_multiple_events` | 4 | Multiple events in one read retain order and boundaries. |
| `sse::tests::test_utf8_split_across_byte_chunks` | 4 | A UTF-8 codepoint split across reads is never corrupted or dropped. |
| `sse::tests::test_utf8_cjk_multiple_events_bytes` | 4 | CJK streaming remains byte-exact across event and chunk boundaries. |
| `stream_accumulator::tests::should_accumulate_text_deltas` | 4 | Streaming text deltas assemble into the same final answer as the non-streamed path. |
| `stream_accumulator::tests::should_accumulate_reasoning_deltas` | 4 | Reasoning deltas stay in the reasoning lane instead of leaking into answer text. |
| `stream_accumulator::tests::should_accumulate_tool_calls` | 4 | Fragmented tool-call names/arguments reassemble into executable calls. |
| `stream_accumulator::tests::should_capture_usage` | 4 | Streaming usage reaches billing/accounting rather than disappearing at completion. |
| `openai::tests::should_surface_invalid_openai_sse_json` | 4 | Malformed OpenAI SSE is a typed visible error, not a silent partial answer. |
| `openai::lane_attributed_operational_errors::should_reject_malformed_tool_arguments_with_call_context` | 4 | Bad tool JSON names the failing call and cannot be executed as a different call. |
| `openai::lane_attributed_operational_errors::should_classify_length_truncated_tool_arguments_as_retryable_truncation` | 2 | Truncated tool arguments enter retry/failover instead of executing corrupted arguments. |
| `openai::tests::should_retry_without_video_when_the_endpoint_refuses_the_video_part` | 1 | An endpoint modality refusal degrades to the same user request without unsupported video. |
| `openai::tests::should_not_retry_text_only_when_failfast_on_image_modality_400_stream` | 2 | FailFast prevents a second hidden attempt and its duplicate side effects/cost. |
| `openai::tests::build_request_drops_reserved_keys_from_sampling_params` | 3 | User-supplied sampling maps cannot override provider-owned protocol fields. |
| `openai::tests::should_preserve_deepseek_reasoning_when_tools_are_sent` | 2 | DeepSeek tool turns retain required reasoning context instead of breaking the next request. |
| `openai::cache_usage_tests::should_parse_cache_write_tokens_from_sse_usage_disjointly` | 4 | Cached read/write tokens remain disjoint so cost accounting cannot double-count output. |
| `anthropic::tests::tool_loop_history_round_trips_tool_use_and_tool_result` | 1 | Anthropic tool-use/tool-result history round-trips as a valid multi-turn conversation. |
| `anthropic::tests::parallel_tool_results_merge_into_one_user_message` | 4 | Parallel results preserve provider role pairing and every result body. |
| `anthropic::tests::orphan_tool_result_falls_back_to_plain_text` | 4 | An orphaned result degrades visibly to text rather than creating invalid tool pairing. |
| `anthropic::tests::should_send_cache_breakpoints_and_parse_cache_usage_end_to_end` | 1 | Cache markers sent on the request and cache usage returned on the stream agree end to end. |
| `anthropic::tests::should_parse_redacted_thinking_block` | 4 | Redacted provider thinking is preserved as opaque protocol data, not mistaken for answer text. |
| `anthropic::tests::should_surface_invalid_anthropic_sse_json` | 4 | Malformed Anthropic frames fail explicitly instead of truncating the turn silently. |
| `anthropic::tests::should_drop_modeled_keys_when_smuggled_via_sampling_params_on_glm` | 3 | GLM sampling overrides cannot smuggle provider-managed reasoning fields onto the wire. |
| `gemini::tests::test_gemini_sse_function_call_captures_thought_signature` | 4 | Gemini function calls retain the thought signature required for the following tool turn. |
| `gemini::tests::gemini_3_limits_signature_fallback_to_the_current_user_turn` | 4 | Synthetic signatures never leak backward into older user turns or corrupt history. |
| `gemini::tests::should_reject_response_schema_when_array_items_are_empty` | 4 | Unsupported array schemas are refused instead of sending a schema Gemini misinterprets. |
| `gemini::tests::tool_schema_projection_removes_unsupported_constraints_recursively` | 4 | Recursive schema projection preserves the usable tool contract while removing unsupported fields. |
| `gemini::tests::should_surface_gemini_data_error_payload` | 4 | Provider error payloads reach the caller with the real classification and message. |
| `gemini::tests::should_surface_invalid_gemini_sse_json` | 4 | Invalid Gemini SSE cannot be accepted as a successful partial response. |
| `openai_responses::tests::test_sse_function_call_flow` | 4 | Responses-API function-call deltas assemble into the correct completed call. |
| `openai_responses::tests::should_ignore_error_events_when_stream_already_terminal` | 4 | Late errors cannot overwrite a terminal successful stream result. |
| `openai_responses::tests::should_still_surface_error_events_before_a_terminal` | 4 | Pre-terminal errors remain visible and prevent false success. |
| `openai_responses::continuation::tests::should_only_retry_definite_missing_continuations_on_400_or_404` | 2 | Continuation retry is limited to definite missing-checkpoint errors, avoiding duplicate execution. |
| `openai_responses::continuation::tests::should_not_abbreviate_when_the_history_rewrites_the_past` | 4 | A rewritten history invalidates the checkpoint rather than mixing unrelated turns. |
| `openai_responses::continuation::tests::should_keep_session_checkpoints_isolated` | 3 | One session cannot reuse another session's server-side continuation state. |
| `openai_responses::continuation::tests::should_evict_the_whole_table_when_full_and_a_new_session_checks_in` | 3 | Checkpoint memory stays bounded and a full table cannot grow without limit. |
| `openrouter::tests::replayed_assistant_tool_calls_carry_the_function_type` | 4 | Replayed OpenRouter calls retain function identity required by the next tool request. |
| `openrouter::tests::test_api_response_cached_tokens_normalized_to_disjoint_accounting` | 4 | OpenRouter cached-token details normalize into disjoint usage accounting. |
| `openrouter::tests::should_surface_invalid_json_through_openrouter_sse_mapper` | 4 | OpenRouter malformed stream JSON surfaces as an error rather than false completion. |
| `config::tool_choice_wire_tests::should_render_every_provider_wire_form_for_explicit_choices` | 1 | Explicit tool choice maps to each provider's actual wire form. |
| `config::tool_choice_wire_tests::should_omit_tool_choice_for_the_default_and_for_tool_less_requests` | 1 | Default/tool-less requests omit the field, preserving provider compatibility. |

## Retry, failover, routing and bounded resources

| Test | Category | Property guarded |
|---|---:|---|
| `retry::tests::test_chat_stream_retries_on_503` | 1 | A transient provider 503 retries the stream without losing the user turn. |
| `retry::tests::test_chat_stream_exhausts_retries` | 4 | Retry exhaustion returns an error instead of hanging or reporting false success. |
| `retry::tests::should_retry_and_failover_on_connection_closed_before_completed` | 1 | A pre-completion connection close retries/fails over to finish the request safely. |
| `retry::tests::should_call_inner_once_when_failfast_on_stream` | 2 | FailFast suppresses same-lane retries even for otherwise retryable stream errors. |
| `retry::tests::test_rate_limit_delay_clamps_to_max_delay` | 3 | Hostile/absurd retry hints cannot produce unbounded sleeps or arithmetic overflow. |
| `retry::tests::test_not_retryable_typed_quota` | 2 | Quota exhaustion fails over rather than burning same-provider retries. |
| `failover::tests::should_failover_to_healthy_lane_when_chat_hangs` | 1 | A hung primary chat moves to a healthy lane within the bounded request policy. |
| `failover::tests::should_failover_stream_init_when_chat_stream_hangs` | 1 | A hung stream initialization fails over instead of leaving the user waiting forever. |
| `failover::tests::should_failover_after_report_late_failure` | 2 | Late transport failures update circuit state and affect subsequent routing. |
| `failover::tests::test_circuit_breaker_resets_on_success` | 4 | A recovered lane re-enters service rather than remaining permanently quarantined. |
| `fallback::provider_index_tests::should_prepend_provider_index_event_when_fallback_serves_stream` | 2 | Stream consumers learn which fallback lane actually served the response. |
| `fallback::tests::nested_fallback_as_primary_resolves_inner_fallback_lane` | 2 | Nested wrappers report the concrete serving lane, not the wrapper identity. |
| `fallback::tests::should_skip_fallback_that_resolves_too_small_at_dispatch` | 2 | A fallback whose resolved context is too small is skipped before request dispatch. |
| `adaptive::tests::test_hedged_racing_picks_faster_provider` | 1 | Hedged racing returns the faster valid provider response. |
| `adaptive::tests::test_hedged_racing_survives_one_failure` | 1 | One failed hedge does not discard the other lane's valid response. |
| `adaptive::tests::test_circuit_breaker_skips_degraded` | 2 | Adaptive routing excludes a degraded lane before sending new user traffic. |
| `adaptive::tests::failover_broadcast_does_not_block_router_under_backpressure` | 3 | A slow event consumer cannot stall routing or grow an unbounded queue. |
| `adaptive::tests::auto_escalation_state_is_session_scoped` | 2 | One session's latency/mode changes cannot silently reroute another session. |
| `throttle::tests::chat_calls_share_the_configured_concurrency_limit` | 3 | Concurrent chat calls obey the configured global provider concurrency bound. |
| `throttle::tests::streaming_calls_hold_permit_until_stream_is_dropped` | 3 | Stream permits live for the stream's full lifetime and release on drop. |
| `local_context_probe::tests::should_bound_readiness_and_keep_floor_when_load_outlives_deadline` | 3 | Local model probing honors its deadline and retains the safe context floor. |
| `local_context_probe::tests::should_adopt_allocation_after_deadline_and_after_retry_cap` | 4 | A late successful allocation can be adopted without violating the retry bound. |
| `responsiveness::tests::test_degradation_detection` | 1 | Sustained user-visible latency degradation activates protection only after evidence. |
| `responsiveness::tests::test_recovery_detection` | 1 | Recovered latency restores normal routing instead of leaving protection stuck on. |
| `lane::tests::default_candidates_match_rfc3_spec` | 5 | RFC-3 (#1292) lane defaults remain the deliberate operator-visible routing contract. |
| `lane::tests::instruction_strong_orders_k25_before_k26` | 5 | PR #1305 intentionally orders k2.5 before the k2.6 fallback in the instruction-strong lane. |
| `registry::tests::should_resolve_local_engine_aliases_to_the_local_family` | 5 | The unified local-family alias design keeps llama.cpp/LM Studio/custom local servers keyless and zero-config. |
| `registry::tests::should_keep_anthropic_api_type_routes_on_the_anthropic_root_for_zai_lanes` | 5 | Saved Anthropic-protocol ZAI routes intentionally stay on the Anthropic endpoint rather than being rewritten to OpenAI. |
| `registry::zai::tests::should_migrate_a_saved_anthropic_root_and_keep_other_overrides` | 5 | The deliberate ZAI OpenAI migration changes only the legacy root and preserves unrelated custom route settings. |
| `pricing::tests::should_price_unknown_provider_cache_reads_at_full_input_rate_and_writes_never_free` | 5 | Unknown providers intentionally price cache reads conservatively and never invent free cache writes. |
| `pricing::tests::should_change_cache_cost_when_provider_changes` | 5 | The same model under a different provider uses that provider's cache pricing, by design. |
| `pricing::tests::should_price_kimi_k3_before_generic_moonshot_branch` | 5 | Kimi K3 intentionally matches its specific price before generic Moonshot pricing. |

## Security, credentials, redaction and bounded data

| Test | Category | Property guarded |
|---|---:|---|
| `vision::tests::should_refuse_a_symlinked_out_of_root_leaf` | 3 | A media symlink cannot escape the authorized workspace root. |
| `vision::tests::should_refuse_a_swapped_symlink_ancestor_when_the_scope_root_is_given` | 3 | Ancestor swaps between validation and open cannot redirect a media read. |
| `vision::tests::should_never_read_redirected_bytes_when_parent_changes_before_open` | 3 | TOCTOU parent replacement cannot leak bytes from outside the scope. |
| `vision::tests::should_refuse_a_symlink_and_an_oversized_file_at_encode_time` | 3 | Symlink and size restrictions hold when bytes are actually encoded, not only at path validation. |
| `vision::tests::should_refuse_non_image_bytes_arriving_with_an_image_extension` | 3 | File extensions cannot disguise arbitrary content as an accepted image. |
| `vision::tests::should_reject_tampered_snapshot_instead_of_changing_historical_pixels` | 3 | A changed source cannot silently rewrite the bytes associated with an earlier media snapshot. |
| `openai::tests::build_request_never_ships_bytes_behind_a_swapped_symlink_ancestor` | 3 | Provider request construction cannot read media through a swapped symlink ancestor. |
| `openai::tests::prompt_cache_key_is_capability_gated_to_official_openai_endpoint` | 3 | Prompt-cache affinity state is never sent to an unofficial/custom endpoint accidentally. |
| `cache_manifest::tests::observer_is_bounded_isolates_streams_and_never_retains_prompt_or_identity` | 3 | Cache observation is bounded per stream and retains neither prompt text nor identity. |
| `cache_manifest::tests::observer_serializes_concurrent_writers_and_keeps_a_bounded_window` | 3 | Concurrent manifest writers cannot corrupt state or grow an unbounded observation window. |
| `cache_manifest::tests::redacted_manifest_proves_append_only_prefix_without_prompt_text` | 3 | Cache-prefix evidence remains useful without persisting prompt contents. |
| `cache_manifest::tests::correlation_uses_hashed_oup_ids_and_scoped_affinity` | 3 | OUP identifiers are hashed and affinity scoped, preventing raw-id leakage/cross-scope correlation. |
| `credential_pool::tests::should_not_hand_same_credential_to_concurrent_fillfirst_acquirers` | 3 | Concurrent FillFirst acquisition cannot double-assign one credential. |
| `credential_pool::tests::should_quarantine_corrupt_state_for_all_four_strategies` | 3 | Corrupt credential state fails closed for every allocation strategy. |
| `credential_pool::tests::should_keep_corrupt_row_and_quarantine_when_repair_persist_fails` | 4 | Failed repair persistence cannot erase evidence or mark corrupt credential state healthy. |
| `credential_pool::tests::should_reject_future_schema_instead_of_accepting_fresh_state` | 4 | A future credential schema is rejected rather than silently resetting credentials. |
| `vertex_auth::tests::should_sign_and_verify_rs256_assertion_with_pinned_key` | 3 | Vertex JWT signing uses real RS256 verification against the pinned service-account key. |
| `vertex_auth::tests::should_error_when_private_key_invalid_pem` | 3 | Invalid private-key PEM fails explicitly instead of producing an unauthenticated request. |
| `provider::tests::should_not_panic_when_truncating_multibyte_utf8_body` | 3 | Provider error redaction/truncation cannot panic on non-ASCII input. |
| `provider::tests::streaming_client_is_not_capped_by_total_timeout` | 1 | Long-lived valid streams are not killed by a total-request timeout. |
| `discovery::tests::should_map_401_to_authentication_failed_and_404_to_unsupported` | 4 | Discovery distinguishes authentication failure from unsupported capability. |
| `discovery::tests::should_map_rate_limit_transport_and_malformed_responses_distinctly` | 4 | Rate limit, transport failure and malformed discovery data remain separately actionable. |
| `discovery::tests::should_return_declared_unsupported_without_any_outbound_request` | 3 | A declared-unsupported provider is never probed over the network. |
| `types::tests::semantic_checkpoint_report_round_trips_without_prompt_content` | 3 | Checkpoint telemetry round-trips without embedding prompt contents. |
| `types::tests::token_usage_without_checkpoint_report_remains_backward_compatible` | 4 | Older usage records lacking checkpoint metadata still deserialize correctly. |
| `context::tests::route_fit_uses_provider_request_estimate_through_wrappers` | 2 | Context-fit decisions use the wrapped provider's real request estimate. |
| `context::tests::should_count_tool_schemas_in_route_fit` | 2 | Tool schema tokens count toward route fit, preventing context overflow after tool injection. |
| `embedding::tests::request_serializes_dimensions_only_when_set` | 4 | Embedding dimension overrides reach the provider protocol only when explicitly configured. |

## Verification of this inventory

- `cargo +1.97.1 test -p octos-llm --lib -- --list` — 807 lib tests listed; every exact name above is present in that output.
- `python3 wake/test-lines.py` at `main@ed914b5f` — octos **458,756 lines / 12,113 counted tests**, octos-llm **24,398 / 875** (attribute-count baseline; the test binary reports 807 discoverable lib tests).
- This PR adds only this Markdown inventory; it neither deletes nor modifies tests or production code.
