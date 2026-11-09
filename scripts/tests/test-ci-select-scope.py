#!/usr/bin/env python3
"""Regression tests for conservative PR crate selection."""
from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "ci-select-scope.py"


class SelectCiScopeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.inventory = root / "inventory.json"
        self.inventory.write_text(json.dumps({
            "head": "test-inventory",
            "packages": [
                {"name": name, "manifest_path": f"crates/{name}/Cargo.toml"}
                for name in ("octos-core", "octos-agent", "octos-cli", "octos-bus", "weather")
            ],
            "declared_direct_dependents": {
                "octos-core": ["octos-agent"],
                "octos-agent": ["octos-cli"],
                "octos-cli": [],
                "octos-bus": [],
                "weather": [],
            },
        }))
        self.out = root / "selection.json"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def select(self, *files: str, check: bool = True, force_full: bool = False, deleted: tuple[str, ...] = (), repo: Path = REPO) -> dict:
        command = ["python3", str(SCRIPT), "--repo", str(repo), "--inventory", str(self.inventory),
                   "--out", str(self.out), "--files", *files]
        if force_full:
            command.append("--full-ci")
        if deleted:
            command += ["--deleted-files", *deleted]
        result = subprocess.run(
            command,
            text=True, capture_output=True,
        )
        if check and result.returncode:
            self.fail(f"selector failed: {result.stdout}\n{result.stderr}")
        if result.returncode:
            return {"error": result.stderr, "returncode": result.returncode}
        return json.loads(self.out.read_text())

    def test_docs_only(self) -> None:
        selected = self.select("docs/ARCHITECTURE.md", "README.md")
        self.assertEqual(selected["mode"], "docs")
        self.assertTrue(selected["docs_only"])
        self.assertEqual(selected["selected_packages"], [])

    def test_leaf_crate(self) -> None:
        selected = self.select("crates/weather/src/lib.rs")
        self.assertEqual(selected["mode"], "selective")
        self.assertEqual(selected["selected_packages"], ["weather"])

    def test_direct_dependents_are_not_transitive(self) -> None:
        selected = self.select("crates/octos-core/src/task.rs")
        self.assertEqual(selected["selected_packages"], ["octos-agent", "octos-core"])

    def test_cli(self) -> None:
        selected = self.select("crates/octos-cli/src/steer_return.rs")
        self.assertEqual(selected["selected_packages"], ["octos-cli"])

    def test_feature_gated_bus_forces_full_ci(self) -> None:
        selected = self.select("crates/octos-bus/src/matrix_channel.rs")
        self.assertTrue(selected["full_ci"])

    def test_feature_gated_cli_adapter_forces_full_ci(self) -> None:
        selected = self.select("crates/octos-cli/src/commands/gateway/adapters/telegram.rs")
        self.assertTrue(selected["full_ci"])

    def test_oup_runtime_forces_full_ci(self) -> None:
        selected = self.select("crates/octos-cli/src/commands/oup_session.rs")
        self.assertTrue(selected["full_ci"])

    def test_model_catalog_forces_full_ci(self) -> None:
        selected = self.select("model_catalog.json")
        self.assertTrue(selected["full_ci"])

    def test_unknown_root_asset_fails_closed_to_full_ci(self) -> None:
        selected = self.select("new_runtime_catalog.json")
        self.assertTrue(selected["full_ci"])

    def test_web_source_is_lightweight_but_not_docs(self) -> None:
        selected = self.select("dashboard/src/App.tsx")
        self.assertEqual(selected["mode"], "selective")
        self.assertFalse(selected["docs_only"])
        self.assertEqual(selected["selected_packages"], [])

    def test_bundled_skill_prompt_forces_full_ci(self) -> None:
        selected = self.select("crates/app-skills/weather/SKILL.md")
        self.assertTrue(selected["full_ci"])

    def test_resource_markdown_is_not_docs_only(self) -> None:
        selected = self.select("crates/octos-agent/resource-skills/pptx/reference.md")
        self.assertTrue(selected["full_ci"])

    def test_crate_readme_is_docs_only(self) -> None:
        for path in ("crates/octos-cli/README.md", "crates/octos-agent/README.md"):
            with self.subTest(path=path):
                selected = self.select(path)
                self.assertTrue(selected["docs_only"])
                self.assertEqual(selected["selected_packages"], [])

    def test_platform_cfg_source_forces_full_ci(self) -> None:
        for path in ("crates/octos-services/src/updater.rs", "crates/octos-llm/src/vision.rs"):
            with self.subTest(path=path):
                selected = self.select(path)
                self.assertTrue(selected["full_ci"])

    def test_crate_markdown_fixture_selects_owner(self) -> None:
        selected = self.select("crates/weather/tests/fixtures/sample.md")
        self.assertEqual(selected["selected_packages"], ["weather"])
        self.assertFalse(selected["docs_only"])

    def test_lockfile_forces_full_ci(self) -> None:
        selected = self.select("Cargo.lock")
        self.assertEqual(selected["mode"], "full-ci")
        self.assertEqual(len(selected["selected_packages"]), 5)

    def test_explicit_full_ci_recomputes_a_leaf_selection(self) -> None:
        selected = self.select("crates/weather/src/lib.rs", force_full=True)
        self.assertTrue(selected["full_ci"])
        self.assertFalse(selected["docs_only"])
        self.assertEqual(selected["mode"], "full-ci")
        self.assertEqual(len(selected["selected_packages"]), 5)
        self.assertEqual(selected["changed_packages"], ["weather"])

    def test_platform_path_forces_full_ci(self) -> None:
        selected = self.select("crates/octos-cli/src/process_manager.rs")
        self.assertTrue(selected["full_ci"])

    def test_unmapped_root_build_configs_fail_closed_to_full_ci(self) -> None:
        for path in ("typos.toml", "rustfmt.toml", "cliff.toml", "flake.lock", "future-tool.toml", "future-input.lock"):
            with self.subTest(path=path):
                selected = self.select(path)
                self.assertTrue(selected["full_ci"])
                self.assertEqual(selected["mode"], "full-ci")
                self.assertEqual(len(selected["selected_packages"]), 5)
                self.assertEqual(selected["reasons"][-1]["decision"], "full-ci")

    def test_unmapped_rust_source_fails_closed_to_full_ci(self) -> None:
        selected = self.select("unknown/src/lib.rs")
        self.assertTrue(selected["full_ci"])
        self.assertEqual(len(selected["selected_packages"]), 5)

    def test_explicit_full_ci_handles_unmapped_build_paths(self) -> None:
        for path in ("typos.toml", "flake.lock", "unknown/src/lib.rs"):
            with self.subTest(path=path):
                selected = self.select(path, force_full=True)
                self.assertTrue(selected["full_ci"])
                self.assertEqual(len(selected["selected_packages"]), 5)
                self.assertEqual(selected["reasons"][0]["path"], "--full-ci")

    def test_empty_diff_fails_closed(self) -> None:
        selected = self.select(check=False)
        self.assertNotEqual(selected["returncode"], 0)
        self.assertIn("empty changed-file list", selected["error"])

    def fixture(self, name: str, files: dict[str, str]) -> Path:
        root = Path(self.temp.name) / name
        for path, content in files.items():
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
        return root

    def commands(self, selected: dict, shard: str) -> list[list[str]]:
        return [list(map(str, command)) for command in selected[shard]]

    def include_research_package(self) -> None:
        inventory = json.loads(self.inventory.read_text())
        inventory["packages"].append({
            "name": "octos-research",
            "manifest_path": "crates/octos-research/Cargo.toml",
        })
        inventory["declared_direct_dependents"]["octos-research"] = []
        self.inventory.write_text(json.dumps(inventory))

    def test_nested_path_modules_follow_the_fixture_layout(self) -> None:
        root = self.fixture("nested-modules", {
            "crates/octos-cli/src/api/transport.rs":
                '#[cfg(test)]\n#[path = "transport_cases.rs"]\nmod tests;\n',
            "crates/octos-cli/src/api/transport_cases.rs":
                '#[path = "history_tests.rs"]\nmod history;\n',
            "crates/octos-cli/src/api/history_tests.rs":
                "#[test]\nfn history_works() {}\n",
            "crates/octos-bus/src/session.rs":
                '#[cfg(test)]\n#[path = "session_tests.rs"]\nmod tests;\n',
            "crates/octos-bus/src/session_tests.rs":
                "#[test]\nfn session_works() {}\n",
        })
        selected = self.select(
            "crates/octos-cli/src/api/history_tests.rs",
            "crates/octos-bus/src/session_tests.rs",
            repo=root,
        )
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--lib", "api::transport::tests::history"],
            self.commands(selected, "windows_cli_lib_commands"),
        )
        self.assertIn(
            ["test", "-p", "octos-bus", "--features", "api,matrix", "--lib", "session::tests"],
            self.commands(selected, "windows_workspace_commands"),
        )

    def test_inline_test_module_uses_its_fixture_source_path(self) -> None:
        path = "crates/octos-cli/src/api/handlers.rs"
        root = self.fixture("inline-tests", {
            path: "#[cfg(test)]\nmod tests {\n    #[test]\n    fn handles_request() {}\n}\n",
        })
        selected = self.select(path, repo=root)
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--lib", "api::handlers::tests"],
            self.commands(selected, "windows_cli_lib_commands"),
        )

    def test_changed_bus_feature_modules_enable_their_test_cfg(self) -> None:
        selected = self.select(
            "crates/octos-bus/src/api_channel.rs",
            "crates/octos-bus/src/matrix_user_channel.rs",
        )
        commands = self.commands(selected, "windows_workspace_commands")
        for module in ("api_channel::tests", "matrix_user_channel::tests"):
            self.assertIn(
                ["test", "-p", "octos-bus", "--features", "api,matrix", "--lib", module],
                commands,
            )

    def test_unattached_test_files_use_the_fixture_file_stems(self) -> None:
        paths = (
            "crates/octos-cli/src/api/recovery_tests.rs",
            "crates/octos-cli/src/api/budget_tests.rs",
        )
        root = self.fixture("detached-tests", {
            path: "#[test]\nfn declared_here() {}\n" for path in paths
        })
        selected = self.select(*paths, repo=root)
        commands = self.commands(selected, "windows_cli_lib_commands")
        for path in paths:
            module = Path(path).stem
            self.assertIn(
                ["test", "-p", "octos-cli", "--features", "api", "--lib", module],
                commands,
            )

    def test_include_test_file_resolves_to_parent_tests_module(self) -> None:
        path = "crates/octos-cli/src/api/undo_history_tests.rs"
        root = self.fixture("include-tests", {
            "crates/octos-cli/src/api/transport.rs":
                '#[cfg(test)]\n#[path = "transport_tests.rs"]\nmod tests;\n',
            "crates/octos-cli/src/api/transport_tests.rs":
                'include!("undo_history_tests.rs");\n',
            path: "#[test]\nfn undo_history_works() {}\n",
        })
        selected = self.select(path, repo=root)
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--lib",
             "api::transport::tests"],
            self.commands(selected, "windows_cli_lib_commands"),
        )

    def test_api_channel_bus_tests_select_the_api_feature(self) -> None:
        # The shared bus mapping enables both API and Matrix modules exactly
        # once, avoiding both zero-test selection and duplicate feature flags.
        selected = self.select("crates/octos-bus/src/api_channel_tests.rs")
        self.assertIn(
            ["test", "-p", "octos-bus", "--features", "api,matrix", "--lib", "api_channel::tests"],
            self.commands(selected, "windows_workspace_commands"),
        )

    def test_deleted_tests_file_emits_no_command_from_head_tree(self) -> None:
        # The diff mentions a `_tests.rs` path, but the PR head deleted the
        # file entirely. Emitting its stem would select zero tests forever.
        path = "crates/octos-cli/src/api/removed_tests.rs"
        root = self.fixture("head-deleted-tests-file", {
            "crates/octos-cli/src/api/transport.rs": "pub fn code() {}\n",
        })
        selected = self.select(path, repo=root)
        self.assertEqual(self.commands(selected, "windows_cli_lib_commands"), [])

    def test_emptied_tests_file_emits_no_command_from_head_tree(self) -> None:
        # The file survives the deletion PR but its tests are gone: no
        # marker, no command.
        path = "crates/octos-cli/src/api/emptied_tests.rs"
        root = self.fixture("head-emptied-tests-file", {
            path: "// tests moved to the WS suite\n",
        })
        selected = self.select(path, repo=root)
        self.assertEqual(self.commands(selected, "windows_cli_lib_commands"), [])

    def test_source_whose_inline_tests_were_deleted_emits_no_command(self) -> None:
        path = "crates/octos-cli/src/api/handlers.rs"
        root = self.fixture("head-inline-tests-removed", {
            path: "pub fn production_code() {}\n",
        })
        selected = self.select(path, repo=root)
        self.assertEqual(self.commands(selected, "windows_cli_lib_commands"), [])

    def test_deleted_integration_target_emits_no_command_from_head_tree(self) -> None:
        target = "crates/octos-cli/tests/removed_integration.rs"
        root = self.fixture("head-deleted-integration-target", {
            "crates/octos-cli/tests/other.rs": "#[test]\nfn other() {}\n",
        })
        selected = self.select(target, repo=root)
        self.assertEqual(self.commands(selected, "windows_cli_integration_commands"), [])

    def test_helper_of_deleted_integration_target_emits_no_command(self) -> None:
        helper = "crates/octos-cli/tests/removed_integration/helpers.rs"
        root = self.fixture("head-helper-outlives-target", {
            helper: "#[test]\nfn helper() {}\n",
        })
        selected = self.select(helper, repo=root)
        self.assertEqual(self.commands(selected, "windows_cli_integration_commands"), [])

    def test_tests_rs_module_file_uses_its_own_module_path(self) -> None:
        # `metasearch/tests.rs` is itself the parent's `#[cfg(test)] mod tests`
        # — the filter must not append a second `::tests` (zero selection).
        path = "crates/octos-core/src/metasearch/tests.rs"
        root = self.fixture("tests-rs-is-the-module", {
            path: "#[tokio::test]\nasync fn case() {}\n",
        })
        selected = self.select(path, repo=root)
        self.assertEqual(
            self.commands(selected, "windows_workspace_commands"),
            [["test", "-p", "octos-core", "--lib", "metasearch::tests"]],
        )

    def test_research_browser_tests_select_the_browser_feature(self) -> None:
        self.include_research_package()
        selected = self.select("crates/octos-research/src/browser.rs")
        self.assertEqual(
            self.commands(selected, "windows_workspace_commands"),
            [["test", "-p", "octos-research", "--features", "browser", "--lib", "browser::tests"]],
        )

    def test_research_impersonation_has_no_windows_test_command(self) -> None:
        # metasearch/mod.rs gates this module on feature=impersonate AND
        # not(windows), so no Windows feature selection can make it nonempty.
        self.include_research_package()
        selected = self.select("crates/octos-research/src/metasearch/impersonate.rs")
        self.assertEqual(self.commands(selected, "windows_workspace_commands"), [])
        self.assertEqual(selected["selected_packages"], ["octos-research"])

    def test_research_root_without_tests_has_no_windows_test_command(self) -> None:
        path = "crates/octos-research/src/lib.rs"
        root = self.fixture("research-root-without-tests", {
            path: "pub fn production_code() {}\n",
        })
        self.include_research_package()
        selected = self.select(path, repo=root)
        self.assertEqual(self.commands(selected, "windows_workspace_commands"), [])
        self.assertEqual(selected["selected_packages"], ["octos-research"])

    def test_include_source_file_resolves_to_including_module(self) -> None:
        # T-0225: a non-_tests include!()-ed source file (recovery) has no
        # module of its own; its tests live under the INCLUDING module's
        # submodule (recovery_write_tests). Emit the including module — a
        # substring filter covering every test inside it, whatever the
        # submodule is named. Regression for the zero-test trap.
        selected = self.select("crates/octos-cli/src/api/ui_protocol_ledger_recovery.rs")
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--lib",
             "api::ui_protocol_ledger"],
            self.commands(selected, "windows_cli_lib_commands"),
        )

    def test_helper_only_attached_module_fails_closed_to_parent_tests(self) -> None:
        # T-0225 (alan0x): ui_protocol_retained_references.rs is a #[path]
        # module with `#[cfg(test)]` HELPER methods but no runnable tests — its
        # tests live in the parent's tests. Emitting `...::retained_references`
        # selects zero tests on Windows; fail closed to the parent's tests.
        selected = self.select("crates/octos-cli/src/api/ui_protocol_retained_references.rs")
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--lib",
             "api::ui_protocol_ledger::tests"],
            self.commands(selected, "windows_cli_lib_commands"),
        )

    def test_changed_integration_test_selects_only_its_target(self) -> None:
        target = "crates/octos-cli/tests/session_history.rs"
        helper = "crates/octos-cli/tests/session_history/helpers.rs"
        root = self.fixture("integration-target", {
            target: "#[test]\nfn integration_case() {}\n",
            helper: "#[test]\nfn helper_case() {}\n",
        })
        selected = self.select(target, repo=root)
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--test", "session_history"],
            self.commands(selected, "windows_cli_integration_commands"),
        )
        self.assertEqual(selected["selected_packages"], ["octos-cli"])
        nested = self.select(helper, repo=root)
        self.assertIn(
            ["test", "-p", "octos-cli", "--features", "api", "--test", "session_history"],
            self.commands(nested, "windows_cli_integration_commands"),
        )

    def test_changed_test_selection_keeps_direct_dependents(self) -> None:
        path = "crates/octos-core/src/task_tests.rs"
        root = self.fixture("direct-dependents", {
            path: "#[test]\nfn task_case() {}\n",
        })
        selected = self.select(path, repo=root)
        self.assertEqual(selected["selected_packages"], ["octos-agent", "octos-core"])
        self.assertIn(
            ["test", "-p", "octos-core", "--lib", "task_tests"],
            self.commands(selected, "windows_workspace_commands"),
        )

    def test_deleted_test_files_do_not_emit_zero_test_commands(self) -> None:
        lib = "crates/octos-cli/src/api/removed_tests.rs"
        integration = "crates/octos-cli/tests/removed_integration.rs"
        selected = self.select(lib, integration, deleted=(lib, integration))
        self.assertEqual(selected["windows_cli_lib_commands"], [])
        self.assertEqual(selected["windows_cli_integration_commands"], [])
        self.assertIn("octos-cli", selected["selected_packages"])

    def test_rename_only_selects_the_new_test_path(self) -> None:
        old = "crates/octos-cli/src/api/old_tests.rs"
        new = "crates/octos-cli/src/api/new_tests.rs"
        root = self.fixture("rename-at-head", {
            new: "#[test]\nfn renamed_case() {}\n",
        })
        selected = self.select(old, new, deleted=(old,), repo=root)
        self.assertEqual(
            self.commands(selected, "windows_cli_lib_commands"),
            [["test", "-p", "octos-cli", "--features", "api", "--lib", "new_tests"]],
        )

    def test_real_git_diff_skips_deleted_and_rename_old_paths(self) -> None:
        root = Path(self.temp.name) / "git-fixture"
        old_lib = root / "crates/octos-cli/src/api/old_tests.rs"
        old_integration = root / "crates/octos-cli/tests/old_integration.rs"
        old_lib.parent.mkdir(parents=True)
        old_integration.parent.mkdir(parents=True)
        old_lib.write_text("#[test] fn old() {}\n")
        old_integration.write_text("#[test] fn old() {}\n")

        def git(*args: str) -> None:
            subprocess.run(["git", *args], cwd=root, check=True, capture_output=True)

        git("init", "-q")
        git("config", "user.email", "test@example.invalid")
        git("config", "user.name", "CI selector test")
        git("add", ".")
        git("commit", "-qm", "base")
        old_lib.unlink()
        old_integration.rename(old_integration.with_name("new_integration.rs"))
        git("add", "-A")
        git("commit", "-qm", "delete and rename")
        subprocess.run([
            "python3", str(SCRIPT), "--repo", str(root), "--inventory", str(self.inventory),
            "--base", "HEAD~1", "--out", str(self.out),
        ], check=True, capture_output=True)
        selected = json.loads(self.out.read_text())
        self.assertEqual(selected["deleted_files"], [
            "crates/octos-cli/src/api/old_tests.rs",
            "crates/octos-cli/tests/old_integration.rs",
        ])
        self.assertEqual(selected["windows_cli_lib_commands"], [])
        self.assertEqual(
            self.commands(selected, "windows_cli_integration_commands"),
            [["test", "-p", "octos-cli", "--features", "api", "--test", "new_integration"]],
        )

    def test_windows_changed_test_steps_are_wired_without_renaming_required_gate(self) -> None:
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        for output in (
            "windows_cli_lib_commands",
            "windows_cli_integration_commands",
            "windows_workspace_commands",
        ):
            self.assertIn(f"{output}: ${{{{ steps.scope.outputs.{output} }}}}", workflow)
            self.assertIn(f"{output}=$(jq -c '.{output}'", workflow)
        self.assertIn("Test changed octos-cli modules (PR)", workflow)
        self.assertIn("Test changed octos-cli integration targets (PR)", workflow)
        self.assertIn("Test changed workspace modules (PR)", workflow)
        self.assertIn("for name in research_only windows_neutral full_ci docs_only selective packages light_packages light_package_names agent_selected cli_selected research_selected dashboard swarm_app windows_cli_lib_commands windows_cli_integration_commands windows_workspace_commands", workflow)

    def test_workflow_runs_selector_self_test(self) -> None:
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        check = workflow.split("  check:\n", 1)[1].split("  test-octos-agent:\n", 1)[0]
        self.assertIn(
            "- name: CI selector self-test\n        run: python3 scripts/tests/test-ci-select-scope.py",
            check,
        )

    def test_full_ci_label_contract_is_wired(self) -> None:
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        self.assertIn("types: [opened, synchronize, reopened, labeled, unlabeled]", workflow)
        reset = '''if [ "$full_ci" = true ]; then
            docs_only=false
            research_only=false
            windows_neutral=false
          fi'''
        self.assertIn(reset, workflow)
        # Documentation PRs still execute the required repo-lint job.
        self.assertIn("  check:\n    needs: scope\n    runs-on: ${{ vars.OCTOS_CI_SELF_HOSTED", workflow)

    def test_full_ci_retains_dynamic_light_and_feature_coverage(self) -> None:
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        check = workflow.split("  check:\n", 1)[1].split("  test-octos-agent:\n", 1)[0]
        self.assertIn("- name: Test selected light crates and feature variants", check)
        self.assertIn("light_package_names: ${{ steps.scope.outputs.light_package_names }}", workflow)
        self.assertIn("needs.scope.outputs.light_package_names != ''", check)
        self.assertNotIn("- name: Test octos-core\n", check)
        self.assertNotIn("- name: Test research crates\n", check)
        for package, feature in (
            ("octos-diagnostics", "github"),
            ("octos-embed-llama", "embed-llama"),
            ("octos-pyo3", "python"),
        ):
            self.assertIn(f'{package}) feature={feature} ;;', check)
        self.assertIn('cargo test -p "$package" --features "$feature" --locked', check)
        self.assertIn("workspace light shard completed:", check)
        self.assertIn("--out /tmp/octos-ci-selection.json --full-ci", workflow)

    def test_cli_bins_have_exactly_one_consumer(self) -> None:
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        cli = workflow.split("  test-octos-cli:\n", 1)[1].split("  check-matrix:\n", 1)[0]
        self.assertIn("run: cargo test -p octos-cli --lib --bins", cli)
        self.assertIn("run: bash scripts/cargo-test-glob.sh octos-cli", cli)
        self.assertNotIn("run: cargo test -p octos-cli --tests", cli)


if __name__ == "__main__":
    unittest.main()
