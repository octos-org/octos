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
                for name in ("octos-core", "octos-agent", "octos-cli", "weather")
            ],
            "declared_direct_dependents": {
                "octos-core": ["octos-agent"],
                "octos-agent": ["octos-cli"],
                "octos-cli": [],
                "weather": [],
            },
        }))
        self.out = root / "selection.json"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def select(self, *files: str, check: bool = True, force_full: bool = False) -> dict:
        command = ["python3", str(SCRIPT), "--repo", str(REPO), "--inventory", str(self.inventory),
                   "--out", str(self.out), "--files", *files]
        if force_full:
            command.append("--full-ci")
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
        for path in ("crates/octos-services/src/updater.rs", "crates/octos-cli/src/config.rs"):
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
        self.assertEqual(len(selected["selected_packages"]), 4)

    def test_explicit_full_ci_recomputes_a_leaf_selection(self) -> None:
        selected = self.select("crates/weather/src/lib.rs", force_full=True)
        self.assertTrue(selected["full_ci"])
        self.assertFalse(selected["docs_only"])
        self.assertEqual(selected["mode"], "full-ci")
        self.assertEqual(len(selected["selected_packages"]), 4)
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
                self.assertEqual(len(selected["selected_packages"]), 4)
                self.assertEqual(selected["reasons"][-1]["decision"], "full-ci")

    def test_unmapped_rust_source_fails_closed_to_full_ci(self) -> None:
        selected = self.select("unknown/src/lib.rs")
        self.assertTrue(selected["full_ci"])
        self.assertEqual(len(selected["selected_packages"]), 4)

    def test_explicit_full_ci_handles_unmapped_build_paths(self) -> None:
        for path in ("typos.toml", "flake.lock", "unknown/src/lib.rs"):
            with self.subTest(path=path):
                selected = self.select(path, force_full=True)
                self.assertTrue(selected["full_ci"])
                self.assertEqual(len(selected["selected_packages"]), 4)
                self.assertEqual(selected["reasons"][0]["path"], "--full-ci")

    def test_empty_diff_fails_closed(self) -> None:
        selected = self.select(check=False)
        self.assertNotEqual(selected["returncode"], 0)
        self.assertIn("empty changed-file list", selected["error"])

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
        self.assertIn("  check:\n    needs: scope\n    runs-on: ubuntu-latest", workflow)

    def test_full_ci_retains_dynamic_light_and_feature_coverage(self) -> None:
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        check = workflow.split("  check:\n", 1)[1].split("  test-octos-agent:\n", 1)[0]
        self.assertIn("- name: Test selected light crates and feature variants", check)
        self.assertIn("light_package_names: ${{ steps.scope.outputs.light_package_names }}", workflow)
        self.assertIn("if: needs.scope.outputs.light_package_names != ''", check)
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
        self.assertIn("run: cargo test -p octos-cli --test '*'", cli)
        self.assertNotIn("run: cargo test -p octos-cli --tests", cli)


if __name__ == "__main__":
    unittest.main()
