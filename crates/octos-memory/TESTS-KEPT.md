# octos-memory — tests kept

TEST-DIET retention inventory for `crates/octos-memory`, measured on `main@7c16f6c3b`. Every entry was checked against `cargo +1.97.1 test -p octos-memory -- --list` (172 discoverable lib tests). This PR is inventory-only; deletion may start only after this file is approved and merged.

Categories: **1** cross-system user-visible chain; **2** cross-feature interaction; **3** security/authorization/bounds; **4** recovery/data/concurrency/format correctness; **5** intentional design decision.

## Persistence, recovery, concurrency and isolation

| Test (exact `cargo test --list` name) | Category | Property guarded |
|---|---:|---|
| `memory_request::tests::concurrent_instances_admit_once_and_profiles_are_independent` | 3 | Concurrent publication is single-admit and one profile cannot consume another profile's request. |
| `memory_request::tests::failed_publication_is_not_success_and_retry_recovers_after_repair` | 1 | A failed publication is not reported as success and a repaired retry completes it. |
| `memory_request::tests::invalid_or_guarded_notes_leave_no_receipt` | 3 | Rejected content leaves no success receipt that could be replayed as accepted memory. |
| `memory_request::tests::receipt_before_publication_recovers_once` | 4 | A crash between receipt and publication recovers exactly once. |
| `memory_request::tests::replay_survives_consumption_and_reopen_and_conflicts_fail` | 4 | Replay survives reopen, consumed requests are not duplicated and conflicting reuse fails. |
| `store::tests::test_open_creates_db` | 1 | A real redb store opens and initializes its durable schema. |
| `store::tests::test_reopen_persists_data` | 1 | Episodes survive a real close/reopen. |
| `store::tests::in_memory_stores_are_functional_and_isolated` | 3 | Independent in-memory stores cannot read each other's episodes. |
| `store::tests::should_error_on_strict_open_when_redb_already_held` | 4 | Strict open fails when another handle holds the redb lock. |
| `store::tests::strict_open_lock_error_is_typed_and_names_the_remedy` | 4 | Lock contention returns an actionable typed error rather than data corruption. |
| `store::tests::should_return_degraded_store_when_redb_already_held_by_another_handle` | 2 | Non-strict open degrades safely instead of stealing the lock or losing read access. |
| `store::tests::non_lock_open_failures_are_not_flagged_as_locked` | 4 | Real I/O failures are not mislabeled as lock contention. |
| `store::tests::should_not_find_deleted_episode_after_reopen` | 4 | Deletion remains durable after reopen. |
| `store::tests::delete_by_id_salvages_corrupt_cwd_index_and_keeps_other_ids` | 4 | Corrupt cwd state cannot prevent deletion or destroy unrelated episode ids. |
| `store::tests::parse_episode_ids_salvages_corrupt_json` | 4 | Corrupt JSON index data degrades to salvageable state rather than losing the store. |
| `store::tests::reopening_at_a_different_dimension_reports_the_mismatch` | 4 | Reopen reports embedding-dimension mismatch instead of silently mixing vectors. |
| `store::tests::open_with_dimension_still_drops_mismatched_vectors` | 4 | Non-default dimensions still reject incompatible persisted vectors. |
| `store::tests::re_embedding_restores_vector_coverage` | 1 | Re-embedding repairs missing/mismatched vector coverage. |
| `store::tests::reindexing_the_worklist_restores_full_coverage` | 1 | The durable worklist drives full vector recovery. |
| `store::tests::episodes_needing_vectors_lists_absent_and_mismatched_only` | 4 | Backfill selects exactly absent/mismatched vectors, avoiding duplicate work. |
| `store::tests::find_relevant_filtered_falls_through_to_db_scan_when_no_floor` | 1 | Without a cwd floor, search still finds durable episodes beyond the in-memory candidate set. |
| `store::tests::find_relevant_filtered_returns_empty_when_floor_set_and_no_cwd_match` | 3 | A cwd-scoped search cannot leak another cwd's episodes. |
| `store::tests::find_relevant_filtered_returns_local_match_through_many_foreign_with_floor` | 3 | Many foreign-cwd records cannot hide the authorized local match. |
| `recall::tests::should_detect_unpersisted_vector_writes_on_reopen` | 4 | Reopen detects vector writes missing from the durable graph manifest. |
| `recall::tests::adopted_embedder_keeps_deferred_backfill_valid_on_reopen` | 4 | Deferred backfill remains valid across embedder adoption and restart. |
| `recall::tests::should_drop_stale_vector_when_text_changes_without_replacement` | 4 | Text replacement cannot leave the old vector serving stale semantic matches. |
| `recall::tests::should_drop_vectors_from_another_embedder_on_open` | 4 | Vectors from an incompatible embedder are excluded on open. |
| `recall::tests::should_keep_foreign_vectors_on_disk_but_ignore_them` | 5 | Foreign embedder vectors are intentionally ignored, not destructively deleted from shared storage. |
| `recall::tests::should_index_post_dump_records_when_reloading_a_matching_manifest` | 4 | Records written after a snapshot are included when its manifest is reloaded. |
| `recall::tests::should_not_roll_back_generation_when_persisting_a_stale_snapshot` | 4 | A stale snapshot cannot move durable generation backward. |
| `recall::tests::should_rank_by_vector_and_persist_graph_across_reopen` | 1 | Vector ranking and its graph survive reopen. |
| `recall::tests::should_restore_tombstone_accounting_from_a_persisted_graph` | 4 | Tombstones remain accounted after graph recovery, preventing deleted results from reappearing. |
| `recall::tests::should_fall_back_to_memory_when_lock_is_held` | 4 | Recall remains available in memory when the durable lock is held elsewhere. |
| `recall::tests::should_keep_vector_capacity_when_records_are_updated_repeatedly` | 3 | Repeated updates cannot grow vector storage beyond capacity. |
| `recall::tests::should_evict_and_delete_by_heat_when_aging` | 2 | Aging evicts cold records and deletes by heat without arbitrary loss. |
| `recall::tests::should_keep_hottest_records_resident_when_aging` | 2 | Hot records remain resident while cold records age out. |
| `recall::tests::should_keep_cold_records_bm25_only_and_revive_on_touch` | 2 | Cold records remain BM25-searchable and regain vectors on touch. |
| `recall::tests::should_search_bm25_only_without_vectors` | 1 | Recall still returns useful results when no vectors are available. |
| `recall::tests::should_filter_by_kind_source_and_time_and_nominate_hot_documents` | 1 | Recall filters and hot-document nomination work across the real query path. |
| `recall::tests::should_widen_candidates_until_filter_is_satisfied` | 1 | Filtering does not return premature empty results while later candidates match. |
| `recall::rebuild_tests::should_preserve_bm25_insert_racing_with_rebuild` | 4 | An insert racing rebuild is not lost from BM25. |
| `recall::rebuild_tests::should_preserve_bm25_text_update_racing_with_rebuild` | 4 | A text update racing rebuild cannot resurrect stale content. |

## Ranking, bounded data and format behavior

| Test | Category | Property guarded |
|---|---:|---|
| `hybrid_search::tests::test_hybrid_ranking` | 1 | BM25 and vector evidence combine into the intended user-facing ranking. |
| `hybrid_search::tests::search_scored_combined_matches_search_for_ranking_parity` | 4 | Scored and unscored APIs produce the same ranking. |
| `hybrid_search::tests::search_scored_exposes_per_modality_breakdown` | 2 | Score attribution separates lexical and vector contributions. |
| `hybrid_search::tests::search_scored_filtered_admits_bm25_only_winner_across_large_noise` | 1 | A BM25-only winner is not lost among many vector matches. |
| `hybrid_search::tests::search_scored_filtered_pool_decoupled_from_limit_when_floor_supplied` | 4 | Candidate pooling honors the floor rather than truncating valid filtered results. |
| `hybrid_search::tests::add_embedding_returns_false_when_hnsw_at_capacity` | 3 | HNSW insertion fails cleanly at capacity instead of growing or corrupting the graph. |
| `hybrid_search::tests::vector_fetch_count_saturates_at_hnsw_capacity` | 3 | Fetch-count arithmetic cannot exceed graph capacity or overflow. |
| `hybrid_search::tests::should_drop_vector_and_keep_bm25_when_insert_dimension_mismatches` | 4 | Bad vector dimensions degrade only the vector lane, preserving lexical recall. |
| `hybrid_search::tests::vector_coverage_separates_absent_vectors_from_mismatched_ones` | 4 | Coverage accounting distinguishes missing from incompatible vectors. |
| `hybrid_search::tests::vector_coverage_excludes_removed_episodes` | 4 | Removed episodes cannot inflate vector coverage or be backfilled. |
| `hybrid_search::tests::should_return_empty_when_limit_is_zero` | 4 | The exact zero-limit boundary performs no results or extra work. |
| `hybrid_search::tests::should_select_true_top_k_when_matches_exceed_limit` | 4 | Truncation preserves true top-k rather than insertion order. |
| `quant::tests::should_encode_zero_vector_without_nan` | 4 | Quantizing a zero vector cannot produce NaN/corrupt persisted bytes. |
| `quant::tests::should_round_trip_within_quantisation_error` | 4 | Quantized vectors round-trip within the declared error bound. |
| `record::tests::should_clamp_title_and_abstract_on_utf8_boundaries` | 3 | External text is bounded without splitting UTF-8 codepoints. |
| `episode::tests::test_episode_schema_version_defaults_on_missing` | 4 | Legacy episodes without a schema version still load safely. |

## Memory writers and guard/security boundaries

| Test | Category | Property guarded |
|---|---:|---|
| `memory_store::tests::should_guard_store_boundary_writers` | 3 | Every public memory writer applies the same injection guard. |
| `memory_store::tests::should_reject_staging_note_when_content_guard_flags_it` | 3 | Guarded content cannot reach the staging file. |
| `memory_store::tests::should_drop_only_poisoned_items_when_writing_extraction` | 3 | One poisoned extraction item does not cause unrelated valid items to be written. |
| `memory_store::tests::should_return_none_and_write_nothing_when_all_items_dropped` | 4 | A fully rejected batch reports no artifact and writes nothing. |
| `memory_store::tests::should_cap_ids_per_call_and_skip_oversized` | 3 | External extraction batches have bounded ids and oversized entries are skipped. |
| `memory_store::tests::should_drop_oldest_daily_notes_first_when_over_budget` | 3 | Daily-note retention stays within budget while preferring newer context. |
| `memory_store::tests::should_truncate_long_term_at_paragraph_when_alone_over_budget` | 3 | Long-term memory is bounded at a coherent paragraph boundary. |
| `memory_store::tests::should_return_empty_when_first_paragraph_exceeds_budget` | 4 | The exact impossible-budget case returns empty rather than exceeding the cap. |
| `memory_store::tests::should_create_backup_when_rewriting_long_term` | 4 | Rewriting durable long-term memory preserves a recovery backup. |
| `memory_store::tests::should_leave_no_temp_files_when_writes_succeed` | 4 | Atomic-write temp files do not accumulate after success. |
| `memory_store::tests::should_use_opaque_extraction_artifact_filenames` | 3 | Model-controlled content cannot choose a path or reveal information through artifact filenames. |
| `memory_store::tests::should_use_opaque_filename_when_note_sensitive` | 3 | Sensitive note text cannot leak through the filename. |
| `memory_store::tests::should_write_and_backup_entity_when_name_is_near_filename_limit` | 3 | Near-limit entity names are safely mapped to files and backed up without path overflow. |
| `memory_store::tests::should_reject_reserved_registry_names_at_the_write_boundary` | 3 | Reserved registry names cannot overwrite control metadata. |
| `memory_store::tests::should_mark_sensitive_when_host_forget_note` | 3 | Host-originated forget notes are marked sensitive and cannot leak as ordinary memory. |
| `memory_store::tests::should_force_sensitive_on_forget_note_quoting_poison` | 3 | Quoting hostile text does not bypass sensitive forget-note handling. |
| `memory_store::tests::test_strip_frontmatter_crlf` | 4 | CRLF frontmatter frames parse without corrupting the body. |
| `memory_store::tests::test_strip_frontmatter_closing_fence_at_eof` | 4 | EOF at the closing fence is handled as a complete frame, not trailing body data. |
| `guard::tests::should_flag_instruction_override_when_phrased_directly` | 3 | Direct instruction overrides are blocked before memory admission. |
| `guard::tests::should_flag_prompt_stack_overrides_without_middle_qualifier` | 3 | Prompt-stack override wording cannot bypass the guard. |
| `guard::tests::should_flag_role_hijack_markers` | 3 | Role-hijack markers are rejected as hostile memory. |
| `guard::tests::should_flag_concealment_and_exfil` | 3 | Concealment/exfiltration instructions are rejected rather than stored for later recall. |
| `guard::tests::should_defeat_separator_evasion` | 3 | Separator manipulation cannot hide a hostile override. |
| `guard::tests::should_defeat_zero_width_and_whitespace_evasion` | 3 | Zero-width/whitespace evasion is normalized before the security decision. |

## Security subclass audit (category 3)

- **Path traversal:** `should_use_opaque_extraction_artifact_filenames`, `should_use_opaque_filename_when_note_sensitive`, `should_write_and_backup_entity_when_name_is_near_filename_limit`, and `should_reject_reserved_registry_names_at_the_write_boundary` pin model/name-to-file boundaries beneath the configured memory directory.
- **Sandbox escape:** this crate has no process execution or OS-sandbox boundary. Its filesystem boundary is the memory directory; the path tests above are the applicable coverage, so no process-sandbox test is claimed.
- **Auth/permissions:** there is no network authenticator in this crate. The applicable authorization boundaries are profile/cwd/store isolation, covered by `concurrent_instances_admit_once_and_profiles_are_independent`, `in_memory_stores_are_functional_and_isolated`, and the two floor-scoped `find_relevant_filtered` tests.
- **Secret/token redaction:** there is no token-specific redaction routine. The crate instead marks sensitive content and avoids content-derived filenames, covered by `should_mark_sensitive_when_host_forget_note`, `should_force_sensitive_on_forget_note_quoting_poison`, and `should_use_opaque_filename_when_note_sensitive`.
- **External input size bounds:** `should_cap_ids_per_call_and_skip_oversized`, `should_drop_oldest_daily_notes_first_when_over_budget`, `should_clamp_title_and_abstract_on_utf8_boundaries`, `add_embedding_returns_false_when_hnsw_at_capacity`, and `should_keep_vector_capacity_when_records_are_updated_repeatedly` cover batch, text, graph and update bounds.

## Cross-feature/failure/platform audit

Cross-feature interactions are the memory-request→guard→receipt, recall→embedder→BM25, and aging→hot/cold revival chains above. Failure/late paths include failed publication, stale snapshots, corrupt indexes, lock contention, post-dump writes and rebuild races. Cancellation is not an exposed operation in this crate; there is no cancellation token or worker lifecycle to preserve. Crash/recovery is covered by real reopen, receipt, manifest, tombstone and migration cases. Data integrity and redb locking are covered by strict/degraded open, multi-handle behavior and durable worklists. Exact zero/capacity/budget boundaries are listed explicitly. Frontmatter CRLF/EOF are the only wire-like frame boundary; no network protocol exists here. No OS-specific branch is present; redb locking, UTF-8 and filename-limit behavior are the relevant platform-sensitive cases.

## Verification

- `cargo +1.97.1 test -p octos-memory -- --list` — 172 lib tests listed; every exact name above is present.
- `python3 wake/test-lines.py` at `main@7c16f6c3` — octos **87,915 lines / 1,994 counted tests**, octos-memory **4,230 / 172**.
- This PR adds only this Markdown inventory and changes no tests or production code.
