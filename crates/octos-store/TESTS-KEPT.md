# octos-store — tests kept

TEST-DIET retention inventory for `crates/octos-store`, measured on `main@cf7fe9b19`. Every entry was checked against `cargo +1.97.1 test -p octos-store -- --list` (43 discoverable lib tests). This PR is inventory-only; deletion may start only after approval and merge.

Categories: **1** cross-system chain; **2** cross-feature interaction; **3** security/authorization/bounds; **4** recovery/data/concurrency/format correctness; **5** intentional design decision.

## Durable identity, credentials and audit

| Test | Category | Property guarded |
|---|---:|---|
| `admin_token_store::tests::hashes_and_verifies_a_token` | 3 | Admin tokens are verified through the salted hash path rather than stored/compared as plaintext. |
| `admin_token_store::tests::salts_are_unique_per_record` | 3 | Two equal tokens receive independent salts, preventing cross-record hash correlation. |
| `admin_token_store::tests::save_and_load_round_trip` | 4 | Token state survives a real save/reload without losing verification data. |
| `admin_token_store::tests::save_uses_0600_permissions` | 3 | Persisted admin credentials are not group/world-readable. |
| `admin_token_store::tests::clear_removes_file` | 4 | Clearing credential state removes the durable file rather than leaving a reusable token. |
| `setup_state_store::tests::updates_last_step_round_trip` | 4 | First-install progress survives reload with the correct last completed step. |
| `setup_state_store::tests::mark_complete_and_mark_skip` | 1 | Setup completion and explicit skip transitions remain distinct across the user journey. |
| `setup_state_store::tests::save_uses_0600_permissions` | 3 | Setup state is persisted without broadening filesystem permissions. |
| `smtp_secret_store::tests::save_and_load_round_trip` | 1 | SMTP secret state survives a real save/reload. |
| `smtp_secret_store::tests::save_overwrites_previous_value` | 4 | Secret rotation replaces, rather than appends or resurrecting, the previous value. |
| `smtp_secret_store::tests::save_uses_0600_permissions` | 3 | SMTP credentials are written with owner-only permissions. |
| `user_store::tests::test_user_roundtrip` | 1 | User records persist and reload through the real store path. |
| `user_store::tests::test_user_file_permissions` | 3 | User records cannot become group/world-readable. |
| `user_store::tests::test_validate_user_id` | 3 | User ids reject malformed/path-like shapes before they can address store files. |
| `user_store::tests::should_disambiguate_reserved_channel_names_when_minting_user_ids` | 3 | Reserved channel names cannot collide with or impersonate another durable user namespace. |
| `login_allowlist::tests::save_claim_and_delete_normalize_email` | 3 | Email normalization is consistent across allowlist save/claim/delete, preventing duplicate identities or authorization bypass. |
| `admin_audit_store::tests::records_lists_and_filters_entries` | 1 | Admin audit entries persist and remain queryable through the real list/filter path. |
| `admin_audit_store::tests::parses_rfc3339_and_date_bounds` | 4 | RFC3339 timestamps and exact date bounds select the intended audit records. |
| `approvals_audit::tests::audit_log_writes_one_line_per_decision` | 4 | Approval decisions append one complete JSONL frame without merging or splitting audit events. |
| `approvals_audit::tests::audit_log_rotates_on_size_threshold` | 3 | Audit logs rotate at the configured size threshold instead of growing without bound. |
| `approvals_audit::tests::audit_log_retention_sweeps_old_files` | 3 | Retention removes expired audit files while retaining current evidence. |

## Usage ledger recovery, corruption and locking

| Test | Category | Property guarded |
|---|---:|---|
| `usage_ledger::tests::usage_events_survive_reopen_and_roll_up_session_totals` | 1 | Usage events survive reopen and aggregate to correct session totals. |
| `usage_ledger::tests::concurrent_records_all_persist` | 4 | Concurrent writers cannot lose usage events or corrupt the ledger. |
| `usage_ledger::tests::record_retries_while_another_opener_holds_the_file` | 4 | A writer retries real lock contention and eventually persists without corrupting another opener. |
| `usage_ledger::tests::open_gives_up_and_surfaces_the_error_once_the_budget_is_exhausted` | 4 | Lock retry is bounded and surfaces a real error instead of hanging forever. |
| `usage_ledger::tests::backfill_is_idempotent` | 4 | Re-running a backfill cannot duplicate usage events or totals. |
| `usage_ledger::tests::legacy_rows_without_schema_version_still_read` | 4 | Legacy rows without schema metadata remain readable after upgrade. |
| `usage_ledger::tests::should_decode_pre_cache_field_records_as_zero` | 4 | Pre-cache records decode with zero cache reads instead of failing or inventing cost. |
| `usage_ledger::tests::should_decode_rows_predating_cache_write_field_as_zero_writes` | 4 | Intermediate rows with `cache_read_tokens=640` but no cache-write field keep the reads and default writes to zero. |
| `usage_ledger::tests::should_round_trip_cache_write_tokens_when_recorded` | 4 | Cache-write token counts round-trip without losing billing data. |
| `usage_ledger::corruption_tests::should_fail_closed_on_malformed_event_rows` | 4 | Malformed event rows fail closed rather than silently dropping or fabricating usage. |
| `usage_ledger::corruption_tests::should_fail_closed_on_malformed_profile_index` | 4 | Corrupt profile indexes cannot redirect or erase another profile's accounting. |
| `usage_ledger::corruption_tests::should_fail_closed_on_malformed_session_index` | 4 | Corrupt session indexes cannot redirect or erase another session's accounting. |
| `usage_ledger::corruption_tests::should_preserve_missing_key_initialization_and_duplicate_dedupe` | 4 | Recovery initializes absent keys and deduplicates repeats without double-counting. |
| `usage_ledger::tests::analytics_group_by_time_profile_provider_model_and_channel` | 1 | Analytics groups the same durable events correctly across time, profile, provider, model and channel. |

## Security subclass audit (category 3)

- **Path traversal:** `test_validate_user_id`, `should_disambiguate_reserved_channel_names_when_minting_user_ids`, and normalized allowlist identity guard the user-controlled name→store path boundary. There is no arbitrary caller-supplied filesystem root.
- **Sandbox escape:** this crate has no process execution or OS sandbox. The applicable containment boundary is the configured store directory; no process-sandbox test is claimed.
- **Auth/permissions:** token verification, reserved identity names, allowlist normalization and all 0600 file-permission tests guard access and durable ownership boundaries.
- **Secret/token redaction:** there is no log-redaction routine. `hashes_and_verifies_a_token` and `salts_are_unique_per_record` prove plaintext-equivalent storage is not used; SMTP/admin/setup/user 0600 tests prevent filesystem disclosure.
- **External input size bounds:** no request-byte or token-size limit exists in this crate. Approval-log size rotation is bounded storage, not a claim of input rejection; `open_gives_up...` separately bounds lock waiting.

## Cross-feature/failure/platform audit

Cross-feature chains are token/setup/SMTP persistence, user/allowlist identity, approval→JSONL audit and event→session/provider analytics. Failure/late paths cover lock contention, exhausted retry budgets, malformed rows/indexes, retention and rotation. There is no cancellation token or long-running worker lifecycle in this crate; bounded lock retry is the applicable late-path behavior. Crash/recovery is covered by real reload, pre-cache and intermediate cache-write legacy decode, backfill idempotence and corruption handling. Data integrity/concurrency is pinned by concurrent writes and cross-opener locking. Exact date bounds, audit size thresholds and duplicate boundaries are retained. JSONL decisions and versioned usage rows are the only wire-like frames. Unix permission bits and real file-lock behavior are the platform-sensitive coverage; no OS-specific network protocol exists.

## Verification

- `cargo +1.97.1 test -p octos-store -- --list` — 43 lib tests; every exact name above is present (34 retained).
- `test-lines.py` accounting for the current scope: **1,075 lines / 43 tests**; post-deletion numbers will be measured on the deletion PR.
- This PR adds only this Markdown inventory and changes no tests or production code.

## Lead-directed restoration follow-up

- Restored exactly `should_decode_rows_predating_cache_write_field_as_zero_writes`, using the shared legacy-row fixture and retaining assertions for 640 reads, zero writes and 100 input tokens. The retained lib set is now **35**.
- Repeated legacy-row and common app-ui event setup was factored into helpers; no retained test or assertion was removed.
- `test-lines.py` accounting on `main@f0e0631b0`: octos-store **946 / 34 → 921 / 35** (**−25 lines**, +1 compatibility test).
