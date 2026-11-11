# octos-diagnostics — tests kept

TEST-DIET retention inventory for `crates/octos-diagnostics`, measured on `main@e174e76a8`. Default features discover 45 lib tests; `--features github` discovers 55 including offline GitHub and ignored live tests. Every retained name is present in the corresponding real list. This PR is inventory-only; deletion may start only after approval and merge.

Categories: **1** cross-system chain; **2** cross-feature interaction; **3** security/authorization/bounds; **4** recovery/data/concurrency/format correctness; **5** intentional design decision.

## Doctor checks, reporting and install diagnosis

| Test | Category | Property guarded |
|---|---:|---|
| `checks::tests::protocol_skew_fails_on_older_schema` | 1 | Doctor fails when the installed host speaks an older protocol schema. |
| `checks::tests::protocol_skew_passes_for_full_protocol` | 1 | A fully compatible protocol passes the skew check. |
| `checks::tests::protocol_skew_warns_when_feature_missing` | 2 | Missing optional features warn without being misreported as schema incompatibility. |
| `checks::tests::locale_check_requires_utf8` | 4 | Non-UTF-8 locales are diagnosed before they corrupt text/protocol output. |
| `checks::tests::writability_check_fails_when_path_is_a_file` | 4 | A data path occupied by a file fails rather than being misreported as a writable directory. |
| `checks::tests::writability_check_warns_for_missing_dir` | 4 | A missing data directory produces the intended warning/remedy path. |
| `install_method::tests::is_ancestor_is_component_wise_not_substring` | 3 | Install roots are compared by path components, preventing sibling-prefix confusion. |
| `install_method::tests::only_cargo_dist_is_self_updating` | 5 | Only the installer-based cargo-dist method is offered the self-update path by design. |
| `install_method::tests::upgrade_hints_are_method_and_spec_specific` | 1 | Upgrade instructions select the actual install method and product package rather than a generic wrong command. |
| `install_method::tests::should_classify_cellar_as_homebrew_not_npm_with_real_npm_root` | 1 | A real npm root cannot misclassify a Homebrew Cellar installation as npm. |
| `install_method::tests::should_classify_cargo_git_when_source_is_git` | 1 | Git-based cargo installations retain the correct upgrade provenance. |
| `locate::tests::npm_install_does_not_false_warn_on_path_or_shadow` | 1 | A valid npm install on PATH does not produce a false shadow warning. |
| `locate::tests::on_path_check_warns_when_ran_by_abs_path_and_dir_not_on_path` | 1 | Invoking an absolute binary whose directory is not on PATH yields the actionable warning. |
| `locate::tests::shadow_check_passes_for_single_and_warns_for_multiple` | 2 | One resolvable binary passes while multiple shadowing binaries are surfaced. |
| `report::tests::exit_code_is_one_on_failure_zero_on_warnings` | 1 | Doctor's process exit status distinguishes hard failure from warnings for scripts/CI. |
| `report::tests::json_carries_caller_version_not_crate_version` | 1 | JSON diagnostics report the host product version, not the shared crate's version. |
| `report::tests::renderer_groups_by_category_and_shows_fix_lines` | 1 | Human-readable diagnostics group findings and preserve actionable fix lines. |
| `spec::tests::asset_selector_joins_prefix_and_triple` | 1 | Release asset selection combines the product prefix and host target triple correctly. |
| `spec::tests::binary_file_name_is_bare_on_unix` | 4 | Unix update paths use the extensionless executable name. |

## Update planning and offline GitHub protocol

| Test | Category | Property guarded |
|---|---:|---|
| `update::tests::is_newer_compares_by_precedence` | 4 | Update availability follows semantic precedence rather than string ordering. |
| `update::tests::parse_tolerates_v_prefix_and_metadata` | 4 | Version parsing accepts real release prefixes/metadata without miscalculating the plan. |
| `update::tests::plan_defers_to_package_manager_for_brew` | 1 | Homebrew upgrades defer to the package manager instead of self-replacing the binary. |
| `update::tests::plan_self_update_for_cargo_dist_installer` | 1 | Installer-based cargo-dist installations select the self-update path. |
| `update::tests::plan_unknown_install_is_update_available_not_package_manager` | 4 | Unknown provenance reports an available update without guessing a package-manager command. |
| `update::tests::plan_up_to_date_on_unparseable` | 4 | Unparseable versions fail safe to UpToDate rather than triggering an unintended downgrade/update. |
| `update::tests::plan_up_to_date_when_current_ge_latest` | 4 | Current or newer installations do not downgrade. |
| `github::tests::host_target_triple_is_dash_joined_arch_first` | 4 | The GitHub asset target triple uses the actual host architecture and dash-joined shape. |
| `github::tests::parse_release_errors_without_tag_name` | 4 | A release response missing its required tag is an explicit error, not a fabricated version. |
| `github::tests::parse_release_prefix_siblings_may_not_shadow_the_bundle` | 3 | A decoy asset with an extended archive suffix cannot shadow the real bundle. |
| `github::tests::parse_release_returns_none_asset_when_no_triple_matches` | 4 | No matching host asset yields no URL rather than downloading another platform's binary. |
| `github::tests::parse_release_selects_asset_for_this_host_triple` | 1 | Release parsing selects the actual host's downloadable asset. |
| `github::tests::token_reads_only_the_specs_env_var` | 3 | GitHub authentication reads only the explicitly configured env var and never falls back to an unrelated token source. |

## Security subclass audit (category 3)

- **Path traversal:** `is_ancestor_is_component_wise_not_substring` pins path-component ancestry; locate/PATH tests cover the related executable-resolution boundary. The crate does not accept arbitrary archive extraction paths.
- **Sandbox escape:** there is no process sandbox or archive extractor here. Update planning and asset selection are the applicable boundaries, so no OS-sandbox claim is made.
- **Auth/permissions:** no login/authorization policy exists in this crate. `token_reads_only_the_specs_env_var` pins token source selection; writability checks diagnose filesystem access without claiming to enforce it.
- **Secret/token redaction:** there is no log-redaction routine or secret-bearing renderer fixture. The token test proves only the explicitly named env var is read and does not print or fall back to other credentials; no stronger redaction property is claimed.
- **External input size bounds:** no response-byte/asset-size cap exists in this crate. Prefix-sibling rejection is asset-name correctness, not a size-limit test.

## Cross-feature/failure/platform audit

Cross-feature chains are protocol compatibility→doctor report/exit code, install method→upgrade command, and release JSON→host asset→update plan. Failure/late paths include older/missing protocol features, unwritable/missing paths, missing tags/assets, unparseable versions and unknown provenance. There is no cancellation token or background worker lifecycle; blocking GitHub calls are not tested against shared live deployment here. Recovery means safe diagnosis/planning rather than persistent crash recovery; this crate owns no database or lock table. Exact schema, version, suffix and path-component boundaries are retained. Release JSON is the only wire-like frame. Host triple, Unix executable naming, PATH shadowing and package-manager differences are the platform coverage. Three `live_*` GitHub tests are intentionally not retained in this offline task; no live/shared endpoint or credentials are used.

## Verification

- Default `cargo +1.97.1 test -p octos-diagnostics -- --list` — 45 tests.
- `cargo +1.97.1 test -p octos-diagnostics --features github -- --list` — 55 tests including offline GitHub and ignored live tests.
- **32 retained names**, all present in the appropriate list.
- `test-lines.py` accounting for the full scope: **774 lines / 55 attributes**; post-deletion numbers will be measured on the deletion PR.
- This PR adds only this Markdown inventory and changes no tests or production code.
