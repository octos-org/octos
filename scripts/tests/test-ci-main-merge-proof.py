#!/usr/bin/env python3
"""Fail-closed tests for same-SHA main-push test deduplication."""
from __future__ import annotations

import importlib.util
import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "ci_main_merge_proof", REPO / "scripts" / "ci-main-merge-proof.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = "3f55e77a1c00d20a2baba61707d7fce3646fdd06"
REPOSITORY = "octos-org/octos"
WORKFLOW_ID = 246644222


def jobs(names=MODULE.PROVEN_JOBS, conclusion="success"):
    return [
        {"name": name, "status": "completed", "conclusion": conclusion}
        for name in names
    ]


def run(conclusion="success", sha=SHA, status="completed", attempt=1,
        repository=REPOSITORY, workflow_id=WORKFLOW_ID):
    return {
        "id": 41336190161,
        "event": "merge_group",
        "head_sha": sha,
        "status": status,
        "conclusion": conclusion,
        "created_at": "2026-11-01T20:00:00Z",
        "html_url": "https://example.test/merge-group",
        "run_attempt": attempt,
        "workflow_id": workflow_id,
        "head_repository": {"full_name": repository},
    }


def rules(extra=()):
    return [{
        "type": "required_status_checks",
        "parameters": {
            "required_status_checks": [
                {"context": context} for context in (*MODULE.REQUIRED_JOBS, *extra)
            ]
        },
    }]


class MainMergeProofTests(unittest.TestCase):
    def evaluate(self, runs=None, job_rows=None, rechecked_run=None,
                 rules_data=None, current_attempt=1, job_hook=None):
        runs = [run()] if runs is None else runs
        job_rows = jobs() if job_rows is None else job_rows
        rechecked_run = rechecked_run or (runs[0] if runs else None)
        rules_data = rules() if rules_data is None else rules_data
        job_calls = 0

        def api(path):
            nonlocal job_calls
            if path == "actions/workflows/ci.yml":
                return {"id": WORKFLOW_ID, "path": ".github/workflows/ci.yml"}
            if path == "rules/branches/main":
                return rules_data
            if path.startswith("actions/workflows/ci.yml/runs?"):
                return {"total_count": len(runs), "workflow_runs": runs}
            if path.startswith("actions/runs/41336190161/jobs"):
                job_calls += 1
                rows = job_hook(job_calls, job_rows) if job_hook else job_rows
                return {"total_count": len(rows), "jobs": rows}
            if path == "actions/runs/41336190161":
                return rechecked_run
            raise AssertionError(path)

        return MODULE.evaluate(SHA, api, REPOSITORY, current_attempt)

    def test_exact_sha_and_double_checked_required_success_allows_dedup(self):
        result = self.evaluate()
        self.assertTrue(result["skip_duplicate_tests"])
        self.assertEqual(result["merge_group_run_id"], 41336190161)
        self.assertEqual(result["merge_group_run_attempt"], 1)
        self.assertEqual(result["workflow_id"], WORKFLOW_ID)
        self.assertEqual(result["head_repository"], REPOSITORY)
        self.assertEqual(set(result["required_jobs"]), set(MODULE.PROVEN_JOBS))

    def test_in_progress_run_needs_only_required_job_success(self):
        # Merge-group run 41450468069 merged 11s after the required jobs went
        # green, but its run-level success arrived 18m25s later. Non-required
        # work is deliberately still running and must not block required dedup.
        rows = jobs(MODULE.REQUIRED_JOBS) + [
            {"name": "e2e-tool-regression", "status": "in_progress", "conclusion": None},
            {"name": "build-release", "status": "queued", "conclusion": None},
        ]
        result = self.evaluate(
            runs=[run(status="in_progress", conclusion=None)],
            job_rows=rows,
        )
        self.assertTrue(result["skip_duplicate_tests"])
        self.assertEqual(set(result["required_jobs"]), set(MODULE.REQUIRED_JOBS))
        self.assertNotIn("CI required", result["required_jobs"])

    def test_missing_or_different_sha_fails_closed(self):
        self.assertFalse(self.evaluate(runs=[])["skip_duplicate_tests"])
        self.assertFalse(self.evaluate(runs=[run(sha="0" * 40)])["skip_duplicate_tests"])

    def test_unsuccessful_or_incomplete_run_fails_closed(self):
        for conclusion in (None, "failure", "cancelled", "timed_out"):
            with self.subTest(conclusion=conclusion):
                self.assertFalse(self.evaluate(runs=[run(conclusion=conclusion)])["skip_duplicate_tests"])
        for status in ("queued",):
            with self.subTest(status=status):
                self.assertFalse(self.evaluate(runs=[run(status=status)])["skip_duplicate_tests"])
        self.assertFalse(
            self.evaluate(runs=[run(status="in_progress", conclusion="success")])[
                "skip_duplicate_tests"
            ]
        )

    def test_current_or_candidate_rerun_fails_closed(self):
        self.assertFalse(self.evaluate(current_attempt=2)["skip_duplicate_tests"])
        self.assertFalse(self.evaluate(runs=[run(attempt=2)])["skip_duplicate_tests"])

    def test_wrong_repo_workflow_or_changed_required_registry_fails_closed(self):
        self.assertFalse(self.evaluate(runs=[run(repository="other/repo")])["skip_duplicate_tests"])
        self.assertFalse(self.evaluate(runs=[run(workflow_id=1)])["skip_duplicate_tests"])
        self.assertFalse(self.evaluate(rules_data=rules(["new-required"]))["skip_duplicate_tests"])

    def test_missing_skipped_failed_or_duplicate_required_fails_closed(self):
        rows = [
            jobs(MODULE.PROVEN_JOBS[:-1]),
            jobs()[:-1] + [jobs(["typos"], "skipped")[0]],
            jobs()[:-1] + [jobs(["typos"], "failure")[0]],
            jobs()[:-1] + [{"name": "typos", "status": "in_progress", "conclusion": None}],
            jobs() + [jobs(["security"])[0]],
        ]
        for conclusion in ("neutral", "timed_out", "cancelled", "action_required"):
            rows.append(jobs()[:-1] + [jobs(["security"], conclusion)[0]])
        for row in rows:
            with self.subTest(row=row[-2:]):
                self.assertFalse(self.evaluate(job_rows=row)["skip_duplicate_tests"])

    def test_final_run_or_job_recheck_change_fails_closed(self):
        changed = run(sha="0" * 40)
        self.assertFalse(self.evaluate(rechecked_run=changed)["skip_duplicate_tests"])
        self.assertFalse(self.evaluate(rechecked_run=run(attempt=2))["skip_duplicate_tests"])

        def weaken_second(call, rows):
            return rows if call == 1 else jobs()[:-1] + [jobs(["security"], "skipped")[0]]

        self.assertFalse(self.evaluate(job_hook=weaken_second)["skip_duplicate_tests"])

    def test_truncated_job_page_fails_closed(self):
        def api(path):
            if path == "actions/workflows/ci.yml":
                return {"id": WORKFLOW_ID, "path": ".github/workflows/ci.yml"}
            if path == "rules/branches/main":
                return rules()
            if path.startswith("actions/workflows/ci.yml/runs?"):
                return {"total_count": 1, "workflow_runs": [run()]}
            if "page=2" in path:
                return {"total_count": 101, "jobs": []}
            return {"total_count": 101, "jobs": jobs()}

        self.assertFalse(MODULE.evaluate(SHA, api)["skip_duplicate_tests"])

    def test_api_and_schema_failures_return_full_ci_decision(self):
        def fail(_path):
            raise OSError("offline")

        result = MODULE.evaluate(SHA, fail)
        self.assertFalse(result["skip_duplicate_tests"])
        self.assertIn("offline", result["reason"])
        self.assertFalse(MODULE.evaluate(SHA, lambda _path: {})["skip_duplicate_tests"])

    def test_workflow_keeps_fail_closed_scope_and_cache_jobs(self):
        scope = workflow_job("scope")
        self.assertIn("main_dedup=false", scope)
        self.assertIn('if [ "${{ github.event_name }}" = "push" ] && [ "${{ github.ref }}" = "refs/heads/main" ]', scope)
        self.assertIn('"${{ github.run_attempt }}" = "1"', scope)
        self.assertIn("if ! main_dedup=$(jq", scope)
        self.assertIn("main_dedup: ${{ steps.scope.outputs.main_dedup }}", scope)

        selected = {
            "check": 1,
            "test-octos-agent": 1,
            "test-octos-cli": 1,
            "check-matrix": 1,
            "check-windows-cli-lib": 1,
            "check-windows-cli-integration": 1,
            "check-windows-workspace": 1,
            "security": 1,
        }
        for name, compile_steps in selected.items():
            with self.subTest(job=name):
                section = workflow_job(name)
                self.assertNotIn("main_dedup", section.split("    steps:", 1)[0])
                self.assertIn("needs:", section.split("    steps:", 1)[0])
                self.assertIn("scope", section.split("    steps:", 1)[0])
                self.assertEqual(section.count("(dedup main)"), compile_steps)
                self.assertLess(section.index("uses: Swatinem/rust-cache@"), section.index("(dedup main)"))
                light_prefix = section.split("      - uses: dtolnay/rust-toolchain@", 1)[0] if name == "check" else ""
                if name == "check":
                    for required_light in ("Tool description exclusivity lint", "UPCR filename registry lint", "Test-line ratchet", "Main merge-proof and dedup structure self-test"):
                        self.assertIn(required_light, light_prefix)
                for step in re.split(r"(?=      - )", section):
                    if "uses: Swatinem/rust-cache@" in step:
                        self.assertNotIn("main_dedup", step)
                        if "shared-key: windows\n" in step and "save-if: false" in step:
                            # Migration-only restore: never overwrite the old
                            # monolithic cache shared by the three shards.
                            self.assertIn("Restore pre-split Windows cache", step)
                        else:
                            self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' }}", step)
                    if "uses: actions/upload-artifact@" in step:
                        self.assertIn("main_dedup != 'true'", step)
                    if re.search(r"^        run:", step, re.MULTILINE):
                        step_name = next(
                            (line.strip() for line in step.splitlines() if line.strip().startswith("- name:")),
                            "unnamed run step",
                        )
                        if "(dedup main)" in step:
                            self.assertIn(
                                "main_dedup == 'true'",
                                step,
                                f"{name}/{step_name}: dedup-only steps need `if: ... main_dedup == 'true'`",
                            )
                        elif "ci-ensure-rustup.sh" in step:
                            # T-0543: a toolchain setup step, not a compile or
                            # test step. Keeping it unguarded is required: the
                            # following dtolnay/rust-toolchain `uses:` step has
                            # no dedup condition, so a skipped reuse step would
                            # still pay the per-job download after a same-SHA
                            # merge proof.
                            self.assertNotIn("main_dedup", step)
                        elif name == "check" and step in light_prefix:
                            self.assertNotIn("main_dedup", step)
                        else:
                            self.assertIn(
                                "main_dedup != 'true'",
                                step,
                                f"{name}/{step_name}: add `if: ... main_dedup != 'true'` so this compile/test step still runs after a same-SHA merge proof; keep existing event/cancel conditions with &&",
                            )
        for name in ("check-darwin", "e2e-tool-regression", "build-linux-x86", "build-windows"):
            with self.subTest(nonrequired_job=name):
                self.assertNotIn(
                    "main_dedup",
                    workflow_job(name),
                    "unfinished non-required checks/builds must still run on main",
                )

        windows = "".join(
            workflow_job(name)
            for name in (
                "check-windows-cli-lib",
                "check-windows-cli-integration",
                "check-windows-workspace",
            )
        )
        for crate in ("octos-llm", "octos-agent", "octos-cli"):
            self.assertIn(f"run: bash scripts/cargo-test-glob.sh {crate}", windows)
            self.assertNotIn(f"run: cargo test -p {crate} --tests", windows)
        self.assertIn("test-mode: [lib, integration]", workflow_job("test-octos-agent"))
        self.assertIn("part: [unit, integration, oup, oup-minimal]", workflow_job("test-octos-cli"))
        self.assertNotIn("main_dedup", workflow_job("ci-required"))
        self.assertNotIn("main_dedup", workflow_job("check-arm"))


def workflow_job(name):
    text = (REPO / ".github" / "workflows" / "ci.yml").read_text()
    start = text.index(f"  {name}:\n")
    match = re.search(r"^  [a-zA-Z0-9_-]+:\s*$", text[start + 1:], re.MULTILINE)
    end = start + 1 + match.start() if match else len(text)
    return text[start:end]


if __name__ == "__main__":
    unittest.main()
