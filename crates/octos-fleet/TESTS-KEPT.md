# octos-fleet — tests kept

TEST-DIET (#3069) retention inventory for `crates/octos-fleet`, measured on `main@1d52f8106`. This is a keep-list, not a deletion list: every entry was checked against `cargo +1.97.1 test -p octos-fleet --lib -- --list` (181 discoverable lib tests). No test removed by the earlier fleet diet slice is restored here.

Categories: **1** cross-system user-visible chain; **2** cross-feature interaction; **3** security/authorization/bounds; **4** recovery/data/concurrency/protocol correctness; **5** intentional design decision.

## T-0584 second round: same-chain merges and shared setup

Current retained inventory: **82 lib tests** (first round 91 → second round 82). Only the retained tests were audited; no first-round deletion is restored. Nine duplicate scenarios were merged only after moving all of their assertions into the surviving chain:

| Absorbed scenario(s) | Surviving test | Assertions retained |
|---|---|---|
| `store::tests::reconcile_reclaims_foreign_owner_epoch` | `store::tests::reconcile_releases_interrupted_reservation_so_relaunch_is_admitted` | interrupted id/child, Ready/current-pointer/attempt status plus exact budget 100→0 and relaunch. |
| `store::tests::reconcile_reclaims_expired_lease_same_epoch` | `store::tests::reconcile_then_relaunch_gets_fresh_attempt` | one interruption + Ready before fresh attempt; distinct current id and attempts_used=2 remain. |
| `store::tests::reconcile_skips_children_of_cancelled_fleet` | `store::tests::reconcile_releases_reservations_of_terminal_fleets` | public cancel, reservation release, Interrupted/Cancelled/current-pointer assertions already cover the raw-status duplicate. |
| `store::tests::ack_is_fenced_against_a_stale_reclaimed_consumer` | `store::tests::outbox_append_claim_ack_and_expiry_reclaim` | old/new sequence/token comparison, stale ack rejection and new-owner ack added to seq2 reclaim; seq1/missing-seq checks remain. |
| `fleet::tests::ready_tasks_excludes_a_task_with_an_unmet_dep` | `fleet::tests::create_sets_initial_ready_and_planned_from_deps` | initial Ready/Planned/deps view plus ready_tasks=[a]. |
| `fleet::tests::is_complete_only_when_all_accepted` | `fleet::tests::record_outcome_accepted_unblocks_dependent` | initial/post-a incomplete, dependent Ready, then b accepted and final complete. |
| `sqlite_ledger::tests::commit_state_with_audit_is_atomic` + `commit_state_with_audit_rolls_back_on_cas_failure` | `sqlite_ledger::tests::commit_state_with_audit_atomicity` | two independent DB scopes: rev0 success/rev1+one finding, and stored rev5/expected0 error with active/rev5+zero findings. |
| `grant::tests::grant_minimal_is_todays_closed_worker` + `grant_missing_fields_default_to_minimal` + `write_paths_default_absent_and_back_compat` | `grant::tests::grant_defaults_and_legacy_shapes_stay_minimal_and_unfenced` | exact base tools/network/fs/default, missing/partial JSON, legacy unfenced shape and serialization omissions all retained. |

Shared setup is construction-only, never a live store shared across scenarios:

- Existing `store::tests::{fresh,fleet_with_ready_child,launch_running}` and `fleet::tests::{fresh,controller,budget,spec,run_and_record}` remain the canonical fixtures; no cosmetic wrapper was added.
- New `sqlite_ledger::test_support::{open_ledger,goal,task,finding}` constructors serve both ledger test modules with struct-update overrides. Success/CAS, migration/race, digest lifecycle and timestamps keep independent TempDir/DB and explicit values.

Wrong owner/attempt/generation, healthy vs stale lease, exact cap/overflow, grant validation, migration locking, cancellation, late messages and all five security subclasses remain independent; no assertion was weakened to reduce lines.

## Worker scheduling, outcomes and fleet-level behavior

| Test (exact `cargo test --list` name) | Category | Property guarded |
|---|---:|---|
| `store::tests::open_creates_tables_and_survives_reopen` | 4 | Fleet state survives a real close/reopen without losing schema or records. |
| `store::tests::launch_happy_path_reserves_budget_and_creates_leased_attempt` | 1 | Dispatch creates one leased attempt and its budget reservation atomically. |
| `store::tests::double_launch_is_rejected_and_does_not_double_reserve` | 4 | A duplicate dispatch cannot create a second attempt or double-charge budget. |
| `store::tests::budget_admits_exactly_at_the_cap` | 4 | The inclusive budget boundary admits exact-fit work without overflowing. |
| `store::tests::budget_exceeded_is_rejected_and_child_stays_ready` | 2 | Budget refusal leaves the child dispatchable rather than losing it. |
| `store::tests::launch_rejected_when_lease_expiry_would_overflow` | 3 | Lease-time arithmetic cannot wrap into an already-expired or unbounded lease. |
| `store::tests::launch_rejected_when_reservation_would_overflow` | 3 | Reservation arithmetic cannot wrap and admit work beyond the budget. |
| `store::tests::mark_running_sets_attempt_and_child_running_then_refuses_repeat` | 4 | Worker start is recorded once and duplicate start notifications are inert. |
| `store::tests::mark_running_rejected_when_attempt_generation_is_stale` | 4 | A stale attempt cannot move the current child into Running. |
| `store::tests::mark_running_no_mutation_when_child_current_attempt_differs` | 4 | A result from a different attempt cannot overwrite the current attempt state. |
| `store::tests::complete_happy_path_settles_budget_and_appends_outbox` | 1 | Worker completion records the outcome, settles reservation and queues result delivery together. |
| `store::tests::complete_rejected_for_wrong_owner_epoch` | 3 | A previous owner's completion cannot commit against the current lease. |
| `store::tests::complete_rejected_for_wrong_attempt_id_no_state_change` | 4 | A completion for the wrong attempt leaves current state untouched. |
| `store::tests::complete_rejected_for_stale_generation` | 4 | A pre-replan generation cannot complete the new task generation. |
| `store::tests::complete_rejected_when_not_running` | 4 | Completion is accepted only in the valid Running state. |
| `store::tests::complete_rejects_attempt_with_mismatched_stored_id` | 4 | Stored attempt identity is cross-checked, preventing result misattribution. |
| `store::tests::reconcile_leaves_healthy_lease_untouched` | 4 | Recovery does not interrupt a live healthy worker lease. |
| `store::tests::reconcile_releases_interrupted_reservation_so_relaunch_is_admitted` | 2 | Foreign epoch interruption restores Ready/clears pointer and releases exact reservation for relaunch. |
| `store::tests::reconcile_releases_reservations_of_terminal_fleets` | 2 | Terminal fleets cannot retain budget reservations forever. |
| `store::tests::reconcile_then_relaunch_gets_fresh_attempt` | 1 | Expired lease reports one interruption/Ready, then relaunch gets a fresh current id and attempts_used=2. |
| `store::tests::outbox_append_claim_ack_and_expiry_reclaim` | 1 | Claim/ack/expiry redelivery rejects the stale token and accepts the reclaimed consumer. |
| `store::tests::cancel_fleet_terminalizes_a_live_fleet_and_is_idempotent` | 1 | Fleet cancellation reaches a terminal state without duplicate side effects. |
| `store::tests::terminal_fleet_fences_child_operations` | 3 | Terminal fleets reject all later child mutations. |
| `store::tests::record_escalation_blocks_child_non_terminally_and_emits_childdone` | 1 | Escalation pauses the child and emits the wake needed for an operator decision. |
| `store::tests::record_escalation_is_a_no_op_for_a_stale_attempt` | 4 | Late escalation from an old attempt cannot block a replacement worker. |
| `store::tests::replan_interrupts_surviving_running_attempt_and_frees_reservation` | 2 | Replanning cancels obsolete workers and releases their budget. |
| `store::tests::replan_removed_live_child_releases_reservation` | 2 | Removing a live child cannot leave a reservation leaked. |
| `store::tests::replan_is_revision_fenced` | 4 | A stale plan revision cannot overwrite a newer plan. |
| `store::tests::replan_survivor_with_new_unmet_dep_becomes_planned` | 2 | New dependencies correctly remove a survivor from the ready set. |
| `store::tests::replan_readds_removed_task_as_fresh_child` | 4 | Reintroduced work gets fresh identity instead of inheriting stale attempt state. |
| `store::tests::replan_clears_pointer_so_a_failed_survivor_can_retry` | 1 | Replanning clears the stale continuation pointer so failed work can retry. |
| `store::tests::decision_log_appends_per_fleet_in_order` | 4 | Operator decisions remain ordered and isolated per fleet. |
| `store::tests::load_drops_row_with_higher_schema_version` | 4 | An older binary refuses unknown future records instead of misparsing them. |
| `store::tests::key_components_reject_control_chars_preventing_collision` | 3 | Crafted key components cannot collide with another fleet/child key. |
| `store::tests::append_event_rejects_control_char_event_id` | 3 | Control characters cannot forge or corrupt event identity. |
| `store::tests::escalating_a_legacy_v2_child_restamps_to_v3` | 4 | Legacy children migrate safely when escalated instead of retaining ambiguous Blocked semantics. |
| `fleet::tests::create_persists_controller_workspace_root` | 4 | Controller workspace binding survives persistence and reload. |
| `fleet::tests::create_sets_initial_ready_and_planned_from_deps` | 1 | Initial Ready/Planned/deps view and ready_tasks exclude the unmet dependent. |
| `fleet::tests::create_with_a_bad_graph_writes_nothing` | 4 | Invalid plans fail atomically with no partial fleet. |
| `fleet::tests::ready_tasks_excludes_a_launched_child` | 4 | The ready queue cannot relaunch an already-leased child. |
| `fleet::tests::ready_tasks_self_heals_a_missed_promotion` | 1 | Ready enumeration repairs a missed dependency promotion instead of stranding completed work. |
| `fleet::tests::record_outcome_accepted_unblocks_dependent` | 1 | Accepted a unblocks b while incomplete; accepting b then completes the fleet. |
| `fleet::tests::record_outcome_rejected_marks_failed_and_blocks_dependent` | 1 | Rejected work does not incorrectly unlock dependents. |
| `fleet::tests::record_outcome_superseded_for_wrong_attempt_is_typed_and_inert` | 4 | A wrong-attempt outcome is typed as superseded and cannot change plan state. |
| `fleet::tests::apply_edit_add_task_bumps_generation_and_adds_ready_child` | 2 | Plan edits publish a new generation and dispatchable child together. |
| `fleet::tests::apply_edit_remove_task_cancels_child_and_frees_reservation` | 2 | Removing work interrupts its worker and releases resources. |
| `fleet::tests::apply_edit_split_task_replaces_one_with_many` | 2 | Splitting preserves the plan graph while replacing one task with all children. |
| `fleet::tests::apply_edit_stale_revision_is_rejected` | 4 | Stale UI edits cannot overwrite a newer plan. |
| `fleet::tests::apply_edit_rejects_malformed_edits_without_mutating` | 4 | Malformed edits leave the existing plan intact. |
| `fleet::tests::concurrent_ready_tasks_and_completion_never_drop_the_successor` | 4 | Racing ready enumeration and completion cannot lose the dependent successor. |
| `fleet::tests::load_snapshot_is_internally_consistent_under_concurrent_replan` | 4 | A snapshot cannot combine spec and execution state from different revisions. |
| `fleet::tests::retarget_deps_racing_a_completion_never_mutates_the_terminal_task` | 4 | A dependency edit racing completion cannot reopen terminal work. |
| `fleet::tests::retitle_does_not_interrupt_a_running_task` | 2 | Metadata-only edits leave the active worker and attempt intact. |
| `fleet::tests::blocked_child_keeps_goal_active` | 5 | Blocked is intentionally a nonterminal operator pause (fleet v3), not completion/failure. |
| `fleet::tests::deny_emits_a_childdone_wake_and_bumps_revision` | 2 | Operator denial both publishes the new plan revision and wakes result consumers. |
| `fleet::tests::set_grant_edit_widens_task_grant_and_readies_it` | 1 | Grant expansion applies to the intended child and makes it dispatchable. |

## Ledger, migration, digest and grant safety

| Test | Category | Property guarded |
|---|---:|---|
| `sqlite_ledger::digest_integration_tests::digest_from_ledger_end_to_end` | 1 | Real ledger findings flow into the digest used by cross-worker consumers. |
| `sqlite_ledger::digest_integration_tests::digest_from_ledger_counts_mixed_lifecycles_and_supersession` | 4 | Digest accounting respects finding lifecycle and supersession instead of double-counting claims. |
| `sqlite_ledger::tests::append_finding_rejects_cross_goal_task` | 3 | A finding cannot attach another goal's task and leak evidence across goals. |
| `sqlite_ledger::tests::commit_state_with_audit_atomicity` | 4 | Independent DB scopes prove rev0 commit and rev5/expected0 CAS rollback of state+audit. |
| `sqlite_ledger::tests::sqlite_ledger_multi_process_access` | 4 | Real multi-process ledger access preserves committed data and locking behavior. |
| `sqlite_ledger::digest_integration_tests::should_behave_identically_on_fresh_and_migrated_schemas` | 4 | Fresh and migrated ledgers expose identical task/finding behavior. |
| `sqlite_ledger::digest_integration_tests::should_roll_back_whole_migration_when_a_step_fails` | 4 | A failed schema step cannot leave a half-migrated ledger. |
| `sqlite_ledger::digest_integration_tests::should_never_read_a_torn_goal_when_a_writer_migrates_concurrently` | 4 | Concurrent migration cannot expose a torn goal row. |
| `sqlite_ledger::digest_integration_tests::should_never_leave_terminal_rows_unranked_when_creation_races_migration` | 4 | Creation/migration races cannot leave terminal work outside deterministic ranking. |
| `sqlite_ledger::digest_integration_tests::cas_goal_status_is_defeated_by_an_intervening_same_status_write` | 4 | CAS detects intervening writes even when they use the same status value. |
| `sqlite_ledger::tests::list_decisions_round_trips_per_goal_in_order` | 4 | Decision history round-trips in order and remains goal-scoped. |
| `sqlite_ledger::tests::should_default_time_used_seconds_when_deserializing_a_pre_2068_goal` | 4 | Pre-#2068 rows retain safe time-accounting defaults after upgrade. |
| `digest::tests::watermark_streams_all_findings_under_constant_budget` | 4 | Streaming a full finding log stays within bounded memory. |
| `digest::tests::the_budget_is_enforced_and_every_cut_is_declared` | 3 | Digest cuts honor budget and declare every omission. |
| `digest::tests::an_unsatisfiable_budget_terminates_and_still_declares_the_loss` | 3 | Even an impossible budget terminates with explicit evidence loss. |
| `digest::tests::oversized_finding_does_not_freeze_stream` | 3 | One oversized finding cannot stall the stream indefinitely. |
| `digest::tests::overturns_not_lost_when_cut` | 4 | Budget trimming cannot hide claim reversals. |
| `digest::tests::the_watermark_makes_it_a_diff_not_a_corpus` | 5 | Watermark semantics intentionally return incremental changes, not a full repeated corpus. |
| `grant::tests::grant_defaults_and_legacy_shapes_stay_minimal_and_unfenced` | 5 | Minimal/default/missing/partial and legacy JSON stay closed and unfenced, with old-shape serialization. |
| `grant::tests::grant_unknown_tool_is_rejected` | 3 | Unknown tools cannot be smuggled into a worker grant. |
| `grant::tests::grant_web_tool_without_network_is_rejected` | 3 | Network-capable tools require an explicit network grant. |
| `grant::tests::grant_empty_hosts_allowlist_is_rejected` | 3 | Empty host allowlists cannot masquerade as valid network access. |
| `grant::tests::network_grant_egress_and_allowlist_semantics` | 3 | Egress behavior follows the declared host allowlist rather than implicit network access. |
| `grant::tests::write_paths_with_host_fs_is_rejected` | 3 | Host filesystem access cannot be combined with a supposedly constraining write fence. |
| `grant::tests::write_path_patterns_are_validated` | 3 | Write-fence patterns reject malformed or escaping paths. |
| `grant::tests::create_only_without_write_paths_is_rejected` | 3 | Create-only cannot be requested without an actual write fence (#1976). |

## Verification of this inventory

- `cargo +1.97.1 test -p octos-fleet --lib -- --list` — 181 lib tests listed; every exact name above is present in that output.
- `python3 wake/test-lines.py` at `main@1d52f810` — octos **435,216 lines / 11,120 counted tests**, octos-fleet **8,978 / 181**.
- This PR adds only this Markdown inventory; it neither deletes nor modifies tests or production code.
