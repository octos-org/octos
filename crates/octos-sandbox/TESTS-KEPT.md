# octos-sandbox — tests kept

TEST-DIET retention inventory for `crates/octos-sandbox`, measured on `main@7c16f6c3b`. This PR is inventory-only; deletion may start only after approval and merge.

Categories: **1** cross-system chain; **2** cross-feature interaction; **3** security/authorization/bounds; **4** recovery/data/concurrency/format correctness; **5** intentional design decision.

| Test / target | Category | Property guarded |
|---|---:|---|
| `tests::should_reject_missing_executable_when_host_managed_command_requested` | 3 | A nonexistent worker executable fails before confinement instead of producing an ambiguous escape/exec path. |
| `tests::should_reject_relative_executable_when_host_managed_command_requested` | 3 | Relative worker paths are rejected so cwd changes cannot redirect the confined executable. |
| `linux_probe_succeeds_when_kernel_supports_landlock_seccomp` (`--test linux_sandbox`) | 3 | The real Linux helper verifies Landlock/seccomp support before tests rely on it. |
| `linux_sandbox_allows_cwd_write_and_blocks_sibling_write` (`--test linux_sandbox`) | 3 | A real sandboxed shell can write its workspace but cannot write a sibling path. |
| `linux_sandbox_readonly_cwd_blocks_cwd_write` (`--test linux_sandbox`) | 3 | Read-only cwd denies new writes while preserving legitimate reads. |
| `linux_sandbox_readonly_cwd_under_tmp_blocks_cwd_write` (`--test linux_sandbox`) | 3 | The allowed `/tmp` exception cannot reopen a read-only workspace located under `/tmp`. |
| `linux_sandbox_blocks_internet_socket_when_network_disabled` (`--test linux_sandbox`) | 3 | Network-disabled confinement blocks a real outbound socket. |
| Integration target `host_managed` (`harness=false`, `tests/host_managed.rs`) | 3 | Real parent/worker/standalone subprocesses cannot read private files, inherit sensitive FDs, use TCP/UDP/Unix sockets, spawn/exec another process or read parent memory, while confined IPC still works. |

| `linux_command_env_tests::should_not_set_tmpdir_when_no_scratch_created` | 3 | No scratch directory means no TMPDIR rewrite or hidden writable location. |
| `linux_command_env_tests::should_still_strip_blocked_env_vars_when_scratch_present` | 3 | Scratch setup cannot reintroduce blocked environment variables. |
| `linux_command_env_tests::should_tie_planned_scratch_to_child_tmpdir_for_readonly_cwd_under_tmp` | 3 | A readonly `/tmp` workspace gets a separate child TMPDIR rather than a writable ancestor of cwd. |
| `write_grant_plan_tests::should_drop_tmp_write_grant_when_readonly_cwd_under_tmp` | 3 | Readonly cwd under `/tmp` loses the normally writable `/tmp` grant. |
| `write_grant_plan_tests::should_drop_var_tmp_write_grant_when_readonly_cwd_under_var_tmp` | 3 | The same containment rule holds under `/var/tmp`. |
| `write_grant_plan_tests::should_drop_grant_when_canonical_root_contains_cwd_even_if_literal_does_not` | 3 | Canonicalization cannot bypass readonly cwd containment. |
| `write_grant_plan_tests::should_drop_both_grants_when_two_roots_canonicalize_to_same_tree_over_cwd` | 3 | Multiple aliases of one containing tree are all removed from the write plan. |
| `write_grant_plan_tests::should_fail_closed_and_drop_dir_roots_when_canonicalizer_maps_them_over_cwd` | 3 | Canonicalizer failures drop suspicious grants instead of failing open. |
| `write_grant_plan_tests::should_not_grant_write_to_any_path_containing_readonly_cwd_under_tmp` | 3 | Every ancestor of readonly cwd is excluded from write grants. |
| `write_grant_plan_tests::should_not_place_scratch_as_an_ancestor_of_a_read_only_cwd` | 3 | Scratch placement cannot make readonly cwd writable through its parent. |
| `write_grant_plan_tests::should_not_place_scratch_inside_a_read_only_temp_root_cwd` | 3 | Scratch cannot be nested inside a readonly temp-root workspace. |
| `write_grant_plan_tests::should_keep_grants_when_canonical_roots_do_not_contain_cwd` | 3 | Legitimate non-containing write roots remain usable. |
| `write_grant_plan_tests::should_grant_all_system_write_paths_when_cwd_writable` | 3 | Writable cwd retains the expected system write grants. |
| `write_grant_plan_tests::should_grant_all_system_write_paths_when_readonly_cwd_outside_tmp` | 3 | Readonly cwd outside temp roots retains only safe system grants. |
| `macos::tests::should_escape_backslash_and_quote_when_profile_literal_built` (macOS cfg) | 3 | Seatbelt path literals cannot be escaped through quotes or backslashes. |
| `macos::tests::should_reject_control_characters_when_profile_literal_built` (macOS cfg) | 3 | Control characters cannot inject additional Seatbelt rules. |
| `macos::tests::should_reject_non_utf8_path_when_profile_literal_built` (macOS cfg) | 3 | Non-UTF-8 paths fail closed instead of producing a malformed profile. |
| `macos::tests::should_pin_worker_profile_grants_when_command_built` (macOS cfg) | 5 | The intentional deny-default worker profile allows only immutable runtime reads and this exact executable's reexec. |
| `macos::tests::should_report_stdio_descriptors_when_descriptor_table_inspected` (macOS cfg) | 4 | Descriptor inspection retains the stdio needed for confined IPC. |

The three lib tests were listed with `cargo +1.97.1 test -p octos-sandbox --lib -- --list`. The five Linux tests were listed with `cargo +1.97.1 test -p octos-sandbox --test linux_sandbox -- --list` after building the helper; a temporary rustc wrapper supplied the compile-time `CARGO_BIN_EXE_octos-sandbox` value that this checkout's Cargo did not define. Binary tests were listed separately with `--bin octos-sandbox` (15 on Linux); macOS-only tests are named from the compiled source cfg and reviewed against `src/macos.rs`. `host_managed` is a harness-free integration target rather than a `#[test]` function, so it is named by its exact Cargo target.

## Security subclass audit (category 3)

- **Path traversal:** the relative/missing executable checks plus cwd/sibling and readonly-cwd probes pin executable and filesystem boundaries.
- **Sandbox escape:** `host_managed` and all five Linux integration scenarios exercise real kernel boundaries rather than configuration mirrors.
- **Auth/permissions:** readonly versus writable cwd and network-disabled mode are the exact permission contracts; no user/account authenticator exists in this helper crate.
- **Secret/token redaction:** there is no log-redaction routine. `host_managed` instead proves that private file contents/metadata and an intentionally inheritable sensitive descriptor cannot escape, and that host files remain unchanged.
- **External input size bounds:** there is no crate-specific byte/message limit; inputs are argv and OS resources. No size-limit test is claimed. Path shape and resource permissions are covered above.

## Cross-feature/failure/platform audit

Cross-feature coverage is the executable→confinement→stdio/process lifecycle chain in `host_managed`, and cwd↔`/tmp` plus network/cwd permission interactions in Linux probes. Failure paths cover missing/relative executables, denied reads/writes/sockets/exec and unsupported kernels. This crate owns no long-running worker or cancellation token; process lifetime/cancellation belongs to the caller, so no cancellation test is claimed. No persistent database, crash-recovery journal, concurrent lock table or wire protocol exists here; the relevant integrity property is that hostile children cannot mutate host state. Exact readonly/network/path boundaries and Linux/macOS/Unix host-managed behavior are retained. No shared deployment or credentials are used.

## Verification

- Lib list: 3 tests; keep 2 security-boundary representatives and drop only the directory-vs-missing duplicate during the later approved deletion.
- Binary list: 15 Linux tests; keep 14 distinct grant/env boundaries and drop only a duplicated TMPDIR setup assertion.
- macOS cfg: keep 5 of 6 tests (drop only the plain-path duplicate of the escaping test).
- Linux integration list: 5 tests; keep all 5 because each proves a distinct OS permission boundary.
- Harness-free host integration target: keep its real escape/IPC chain.
- `python3 wake/test-lines.py` at `main@7c16f6c3` — octos **87,915 lines / 1,994 counted tests**; sandbox is part of the small-crate tail measured again at deletion time.
- This PR adds only this Markdown inventory and changes no tests or production code.
