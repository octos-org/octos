#!/usr/bin/env python3
"""Real-git regressions for PR-base-aware quality ratcheting."""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "quality-ratchet.sh"
DEFAULT_BASELINE = {
    "unwrap_panic_prod": 0,
    "files_over_3000": 1,
    "max_file_lines": 50530,
}


def git(repo: Path, *args: str) -> str:
    return subprocess.check_output(
        ["git", *args], cwd=repo, text=True, stderr=subprocess.DEVNULL
    ).strip()


class RatchetRepo:
    def __init__(
        self,
        root: Path,
        *,
        base_lines: int,
        current_lines: int,
        baseline: dict[str, int] | None = None,
        base_unwrap: int = 0,
        current_unwrap: int = 0,
        current_extra_large_file: bool = False,
        merge: bool = True,
    ) -> None:
        self.root = root
        self.baseline = baseline or DEFAULT_BASELINE
        root.mkdir()
        git(root, "init", "-q", "-b", "main")
        git(root, "config", "user.email", "ratchet@example.test")
        git(root, "config", "user.name", "Ratchet Test")
        self.write_source(base_lines, base_unwrap)
        self.write_baseline()
        base = self.commit("base")
        if not merge:
            return
        self.write_source(
            current_lines, current_unwrap, current_extra_large_file
        )
        tree = git(root, "write-tree")
        merged = git(root, "commit-tree", tree, "-p", base, "-p", base)
        git(root, "reset", "--hard", "-q", merged)

    def write_baseline(self) -> None:
        target = self.root / ".github" / "quality-baseline.tsv"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            "".join(f"{name}\t{value}\n" for name, value in self.baseline.items())
        )
        git(self.root, "add", ".github/quality-baseline.tsv")

    def write_source(
        self, lines: int, unwrap: int, extra_large_file: bool = False
    ) -> None:
        source = self.root / "crates" / "fixture" / "src" / "fixture.rs"
        source.parent.mkdir(parents=True, exist_ok=True)
        content = ["panic!();" if i < unwrap else "// filler" for i in range(lines)]
        source.write_text("\n".join(content) + "\n")
        if extra_large_file:
            (source.parent / "extra.rs").write_text("// extra\n" * 3001)
        git(self.root, "add", "crates")

    def commit(self, message: str) -> str:
        git(self.root, "commit", "-q", "-m", message)
        return git(self.root, "rev-parse", "HEAD")

    def run(self, event: str = "pull_request") -> subprocess.CompletedProcess[str]:
        return run_ratchet(self.root, event)

    @staticmethod
    def assert_one_worktree(rows: list[str]) -> None:
        count = sum(row.startswith("worktree ") for row in rows)
        if count != 1:
            raise AssertionError(f"temporary worktree leaked: {rows}")


def run_ratchet(
    repo: Path, event: str = "pull_request", sha: str | None = None
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["GITHUB_EVENT_NAME"] = event
    if sha is not None:
        env["GITHUB_SHA"] = sha
    result = subprocess.run(
        ["bash", str(SCRIPT)],
        cwd=repo,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    worktrees = git(repo, "worktree", "list", "--porcelain").splitlines()
    RatchetRepo.assert_one_worktree(worktrees)
    return result


class QualityRatchetTests(unittest.TestCase):
    def repo(self, **kwargs) -> RatchetRepo:
        temp = tempfile.TemporaryDirectory(prefix="quality-ratchet-test-")
        self.addCleanup(temp.cleanup)
        return RatchetRepo(Path(temp.name) / "repo", **kwargs)

    def test_mainline_debt_not_reintroduced_by_pr_passes(self):
        result = self.repo(base_lines=50535, current_lines=50535).run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_shallow_checkout_fetches_missing_parent_and_passes(self):
        source = self.repo(base_lines=50535, current_lines=50535)
        temp = tempfile.TemporaryDirectory(prefix="quality-ratchet-shallow-")
        self.addCleanup(temp.cleanup)
        bare = Path(temp.name) / "remote.git"
        clone = Path(temp.name) / "checkout"
        subprocess.run(
            ["git", "clone", "-q", "--bare", str(source.root), str(bare)],
            check=True,
        )
        subprocess.run(
            ["git", "clone", "-q", "--depth", "1", f"file://{bare}", str(clone)],
            check=True,
        )
        missing_parent = subprocess.run(
            ["git", "rev-parse", "--verify", "HEAD^1"],
            cwd=clone,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        self.assertNotEqual(missing_parent.returncode, 0)

        result = run_ratchet(clone, sha=git(clone, "rev-parse", "HEAD"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            git(clone, "rev-parse", "HEAD^1"),
            git(source.root, "rev-parse", "HEAD^1"),
        )

    def test_pr_added_lines_fail(self):
        result = self.repo(base_lines=50530, current_lines=50535).run()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_current_not_above_baseline_passes_even_with_lower_base(self):
        result = self.repo(base_lines=50525, current_lines=50528).run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_unreadable_base_falls_back_to_old_failure(self):
        result = self.repo(
            base_lines=50535, current_lines=50535, merge=False
        ).run()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("base metrics unavailable", result.stdout + result.stderr)

    def test_non_pr_keeps_baseline_only_behavior(self):
        result = self.repo(base_lines=50535, current_lines=50535).run("push")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_unwrap_and_file_count_use_the_same_base_formula(self):
        baseline = {
            "unwrap_panic_prod": 0,
            "files_over_3000": 0,
            "max_file_lines": 60000,
        }
        unchanged = self.repo(
            base_lines=3001,
            current_lines=3001,
            baseline=baseline,
            base_unwrap=1,
            current_unwrap=1,
        ).run()
        self.assertEqual(unchanged.returncode, 0, unchanged.stdout + unchanged.stderr)

        more_unwrap = self.repo(
            base_lines=3001,
            current_lines=3001,
            baseline=baseline,
            base_unwrap=1,
            current_unwrap=2,
        ).run()
        self.assertEqual(more_unwrap.returncode, 1, more_unwrap.stdout + more_unwrap.stderr)

        more_files = self.repo(
            base_lines=3001,
            current_lines=3001,
            baseline=baseline,
            base_unwrap=1,
            current_unwrap=1,
            current_extra_large_file=True,
        ).run()
        self.assertEqual(more_files.returncode, 1, more_files.stdout + more_files.stderr)


if __name__ == "__main__":
    unittest.main()
