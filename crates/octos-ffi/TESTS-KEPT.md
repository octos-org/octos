# octos-ffi TESTS-KEPT

Tracking: T-0577. Baseline is octos main `7c16f6c3`: `wake/test-lines.py --json` reports **1,689 test lines / 61 tests**. Every name below was checked with `cargo test -p octos-ffi --all-targets -- --list`.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design. Integration rows use `tests/<target>.rs::<exact libtest name>`. Null handling is retained per ABI entry family, not per field permutation.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative, or code-backed reason there is no such boundary |
|---|---|
| 路径穿越 | **本 crate 无独立路径穿越沙箱边界。** Model/data paths are caller-owned local configuration, not archive/request paths. `model_tests::should_keep_explicit_model_path_semantics_when_download_is_opted_out` is retained for explicit-path semantics, not mislabeled as traversal rejection. |
| 沙箱逃逸 | **本 crate 无直接进程沙箱边界。** It exposes model/memory/agent FFI and does not spawn sandboxed tools itself; execution policy remains in octos-agent. `tests/dependency_boundary.rs::should_exclude_cli_from_all_dependencies_when_building_ffi` preserves that architectural separation. |
| 鉴权/权限 | `memory_tests::should_force_untrusted_and_reset_counters_on_ingest` marks external ingest untrusted and resets trust-sensitive counters instead of inheriting prior authority. |
| 密钥/令牌脱敏 | `tests::resolved_secret_uses_canonical_name_so_authstore_key_is_found_and_scrubbed`; `tests::sanitizer_redacts_secret_shaped_tokens_and_caps_length` covers shaped-token redaction plus bounded error output. |
| 外部输入大小上限 | `memory_tests::should_reject_knowledge_kind_empty_ids_and_oversized_batches`; `tests::sanitizer_redacts_secret_shaped_tokens_and_caps_length` covers batch and error-string caps. |

## Retained tests

| Cat. | Exact test | Guard |
|---|---|---|
| 4 | `incomplete_tests::should_clear_c_partial_on_new_failure_or_success` | A new C call cannot return a stale partial result after success or failure. |
| 4 | `incomplete_tests::should_keep_c_partial_thread_local_and_clear_it_on_panic` | Partial state is thread-local and panic cleanup cannot leak it into another call. |
| 4 | `incomplete_tests::should_preserve_partial_when_native_conversation_is_truncated` | Native truncation preserves the actual partial output rather than losing work. |
| 4 | `incomplete_tests::should_transfer_c_partial_once_without_putting_body_in_last_error` | C partial transfer is single-use and does not confuse output bytes with the error channel. |
| 3 | `memory_tests::should_accept_caller_vectors_and_reject_length_mismatch` | Caller vector length must exactly match its declared dimension, preventing ABI overread/misinterpretation. |
| 3 | `memory_tests::should_force_untrusted_and_reset_counters_on_ingest` | External ingest cannot inherit trusted status or stale usage counters. |
| 3 | `memory_tests::should_handle_null_arguments_in_c_memory_entry_points` | Null C memory arguments fail safely without dereferencing invalid pointers. |
| 4 | `memory_tests::should_parse_time_bounds` | Time-filter boundaries survive the C input parsing contract exactly. |
| 4 | `memory_tests::should_persist_stores_under_data_dir_across_runtimes` | Memory stores persist under the configured data dir across independent runtime lifetimes. |
| 3 | `memory_tests::should_reject_bad_search_requests` | Malformed search requests fail at the external ABI boundary instead of reaching the store. |
| 3 | `memory_tests::should_reject_knowledge_kind_empty_ids_and_oversized_batches` | Empty ids and oversized knowledge batches are rejected before allocation/persistence. |
| 3 | `memory_tests::should_reject_zero_recall_dimension` | Zero-dimensional recall input is rejected instead of creating invalid vector state. |
| 4 | `memory_tests::should_report_no_such_record_on_load_miss` | Load misses propagate a distinct no-such-record result across the ABI. |
| 5 | `model_tests::should_build_keyword_only_runtime_when_download_is_opted_out` | The intentional offline/no-download mode remains keyword-only. |
| 5 | `model_tests::should_keep_explicit_model_path_semantics_when_download_is_opted_out` | Offline mode preserves explicit caller model-path semantics instead of silently downloading. |
| 3 | `model_tests::should_refuse_ensure_without_download_when_absent` | Ensure fails closed when the model is absent and downloading is disabled. |
| 3 | `model_tests::should_reject_null_and_invalid_data_dir_in_c_model_entry_points` | Null/invalid C data-dir strings are rejected before filesystem/runtime use. |
| 4 | `model_tests::should_report_partial_file_as_present_but_incomplete` | A partial model artifact is reported as incomplete, never as ready. |
| 1 | `model_tests::should_serve_model_status_and_ensure_through_c_without_a_runtime` | Real C model status/ensure calls work without preconstructing a native runtime. |
| 1 | `tests::build_agent_exposes_memory_tools_over_the_runtime_recall_store` | Agent construction exposes memory tools over the same runtime recall store. |
| 4 | `tests::c_free_removes_scratch_dir` | C free releases the runtime scratch directory and its owned resources. |
| 3 | `tests::cstr_rejects_null` | The base C-string boundary rejects null instead of dereferencing it. |
| 3 | `tests::factory_consumes_the_single_resolved_pinned_key` | Factory resolution consumes one pinned credential without ambiguous duplicate secrets. |
| 4 | `tests::guard_contains_panic_and_returns_default` | Panics cannot unwind across the ABI; the guard returns the documented default. |
| 4 | `tests::native_drop_removes_scratch_dir` | Native Drop and C free share the same scratch-directory cleanup guarantee. |
| 4 | `tests::owned_string_round_trips_through_free` | Owned FFI strings remain valid through their matching free function. |
| 3 | `tests::resolved_secret_uses_canonical_name_so_authstore_key_is_found_and_scrubbed` | Canonical credential lookup succeeds and resolved secret state is scrubbed. |
| 3 | `tests::sanitizer_redacts_secret_shaped_tokens_and_caps_length` | Error sanitization redacts secret-shaped tokens and enforces an output length cap. |
| 1 | `tests/dependency_boundary.rs::should_exclude_cli_from_all_dependencies_when_building_ffi` | FFI builds cannot accidentally pull in the CLI dependency tree or bypass the host boundary. |
| 1 | `tests/ffi.rs::e2e_run_task_returns_output_containing_ok` | The real public FFI task chain returns executable output to the host. |
| 3 | `tests/ffi.rs::empty_config_returns_null_and_sets_error` | Empty external config returns a null handle with an error instead of a half-initialized runtime. |
| 3 | `tests/ffi.rs::invalid_config_json_returns_null_and_sets_error` | Malformed external JSON fails across the ABI with a readable error. |
| 4 | `tests/ffi.rs::last_error_reflects_most_recent_failure` | The public error channel reports the latest real failure rather than stale success state. |
| 2 | `tests/ffi.rs::memory_upsert_search_load_round_trip_bm25_only` | Public C memory calls round-trip through keyword-only search/load without an embedder. |
| 3 | `tests/ffi.rs::missing_required_field_returns_null` | Missing required external fields are rejected at runtime construction. |
| 3 | `tests/ffi.rs::run_task_null_brief_is_safe` | A null task brief fails safely at the task ABI boundary. |
| 3 | `tests/ffi.rs::run_task_null_runtime_is_safe` | A null runtime handle fails safely at the same task boundary without a dereference. |
| 4 | `tests/ffi.rs::runtime_free_null_is_safe` | Freeing a null runtime is a safe no-op. |
| 1 | `tests/ffi.rs::valid_config_returns_nonnull_handle_and_frees` | Valid public config returns a real handle that is safely released. |

共 **39 项**。

## Cross-cutting audit

| Area | Audit result |
|---|---|
| 跨功能交互 | Agent/memory store wiring, C model entry points, keyword-only public memory search, and the real task chain are retained. |
| 失败/迟到路径 | Stale partial clearing, latest-error propagation, panic containment, absent/partial model handling, and malformed config failures are retained. |
| 取消 | The synchronous C ABI has no async cancellation token; panic guards and Drop/free cleanup are the applicable early-exit representatives. |
| 崩溃恢复 | Persistent stores across runtimes, partial native output, and panic-safe partial cleanup cover process/runtime reconstruction. |
| 数据完整性 | Vector dimensions, record misses, model partial files, and single-use partial transfer prevent stale or misinterpreted bytes. |
| 并发/锁 | Partial state is explicitly thread-local; no additional FFI-owned lock protocol exists. |
| 精确边界 | Null per ABI family, zero dimension, vector-length mismatch, empty ids, oversized batches, and output caps are retained as distinct boundaries. |
| 协议帧 | Public config/error/task/memory calls are the ABI wire boundary; no separate OUP frame codec lives in this crate. |
| 平台差异 | C strings, null handles, Drop/free, and scratch-dir cleanup are retained for Windows/macOS/Linux CI rather than treated as interchangeable host cases. |
