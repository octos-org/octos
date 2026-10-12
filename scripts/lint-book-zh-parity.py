#!/usr/bin/env python3
"""Fail the docs deploy when the Chinese book lags the English one.

`book/` and `book-zh/` are maintained under a lockstep convention: an update
to an English chapter lands in the same commit as its Chinese counterpart
(#2643, the 09-12 doc update). Nothing enforced that, so five en-only commits
slipped through (#2365, #2371, #2549, #2587, #2691 — 2026-09-16 through
10-07) and the published `/octos/zh/` silently hid them, including the
security-relevant `--auth-token` argv warning (#2453).

This lint fails the deploy when:

1. A chapter file exists in one book but not the other.
2. A chapter's English copy was committed more recently than its Chinese
   copy — the en update was never carried over — unless the file is listed
   in `STALE_BASELINE` below.

What it cannot see: content dropped while the zh file was still touched
later for unrelated reasons. That class needs eyes on the diff; the gate
only guarantees the process convention going forward.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

EN = Path("book/src")
ZH = Path("book-zh/src")

# Known-stale chapters with drift older than this lint, exempted until a
# backfill lands. Each entry must name the gap so the exemption stays
# auditable; remove the entry once the chapter is carried over.
STALE_BASELINE: dict[str, str] = {
    # Four pre-09-01 en sections have no zh counterpart (structural gap is
    # older than the #2691 drift, which this PR did backfill).
    "providers.md": "pre-2026-09 sections untranslated",
    # One en section with no zh counterpart; last lockstep commit 043544a2.
    "advanced.md": "one section untranslated",
    # Three en sections with no zh counterpart; last lockstep 73bf14c8.
    "skill-development.md": "three sections untranslated",
}


def last_commit_epoch(path: str) -> int:
    """Unix timestamp of the last commit touching path; 0 when untracked."""
    out = subprocess.run(
        ["git", "log", "-1", "--format=%ct", "--", path],
        capture_output=True,
        text=True,
    ).stdout.strip()
    return int(out) if out else 0


def chapters(root: Path) -> set[str]:
    return {str(p.relative_to(root)) for p in root.rglob("*.md")}


def failures() -> list[str]:
    problems: list[str] = []
    en_files, zh_files = chapters(EN), chapters(ZH)
    for missing in sorted(en_files - zh_files):
        problems.append(f"no Chinese chapter for {EN / missing}")
    for extra in sorted(zh_files - en_files):
        problems.append(f"no English chapter for {ZH / extra}")
    for rel in sorted(en_files & zh_files):
        en_ts, zh_ts = last_commit_epoch(str(EN / rel)), last_commit_epoch(str(ZH / rel))
        if en_ts <= zh_ts:
            continue
        if rel in STALE_BASELINE:
            continue
        problems.append(
            f"{ZH / rel} lags {EN / rel}: en committed after the last zh touch "
            f"(en {en_ts}, zh {zh_ts}) — carry the update over"
        )
    return problems


def self_test() -> int:
    import os
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        repo = str(Path(tmp) / "repo")
        subprocess.run(["git", "init", "-q", repo], check=True)
        for folder in ("book/src", "book-zh/src", "scripts"):
            (Path(repo) / folder).mkdir(parents=True)
        # The lint module runs against the CWD's repo; copy itself in and
        # drive every scenario through a controlled throwaway history. The
        # committer date is pinned per commit so %ct ordering is exact.
        lint = Path(__file__).read_text(encoding="utf-8")
        (Path(repo) / "scripts" / "lint-book-zh-parity.py").write_text(lint)

        def commit(side: str, name: str, body: str, when: str) -> None:
            target = Path(repo) / side / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(body, encoding="utf-8")
            env = {
                **os.environ,
                "GIT_AUTHOR_DATE": when,
                "GIT_COMMITTER_DATE": when,
            }
            subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
            subprocess.run(
                ["git", "-c", "user.email=t@t", "-c", "user.name=t",
                 "commit", "-qm", name],
                cwd=repo, env=env, check=True, capture_output=True,
            )

        def run_lint() -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                [sys.executable, "scripts/lint-book-zh-parity.py"],
                cwd=repo, capture_output=True, text=True,
            )

        # Scenario 1: lockstep commit touches both trees — pass.
        commit("book/src", "intro.md", "# Intro\n", "2026-01-01T00:00:00 +0000")
        commit("book-zh/src", "intro.md", "# 引言\n", "2026-01-01T00:00:00 +0000")
        out = run_lint()
        if out.returncode != 0:
            print(f"self-test: lockstep chapter wrongly flagged:\n{out.stdout}")
            return 1
        # Scenario 2: later en-only update — fail names the stale chapter.
        commit("book/src", "intro.md", "# Intro v2\n", "2026-01-02T00:00:00 +0000")
        out = run_lint()
        if out.returncode == 0 or "intro.md" not in out.stdout:
            print(f"self-test: stale chapter not flagged:\n{out.stdout}")
            return 1
        # Scenario 3: zh backfill (newer zh touch) — pass again.
        commit("book-zh/src", "intro.md", "# 引言 v2\n", "2026-01-03T00:00:00 +0000")
        out = run_lint()
        if out.returncode != 0:
            print(f"self-test: backfilled chapter wrongly flagged:\n{out.stdout}")
            return 1
        # Scenario 4: chapter missing on the zh side — fail on the file set.
        commit("book/src", "extra.md", "# Extra\n", "2026-01-04T00:00:00 +0000")
        out = run_lint()
        if out.returncode == 0 or "no Chinese chapter" not in out.stdout:
            print(f"self-test: missing zh chapter not flagged:\n{out.stdout}")
            return 1
        # Scenario 4b: zh chapter added but dated before the en update —
        # the file-set failure becomes a staleness failure.
        commit("book-zh/src", "extra.md", "# 附加\n", "2026-01-03T12:00:00 +0000")
        out = run_lint()
        if out.returncode == 0 or "lags" not in out.stdout:
            print(f"self-test: stale backfill not flagged:\n{out.stdout}{out.stderr}")
            return 1
        # Scenario 5: baseline exemption silences the staleness flag — pass.
        exempted = lint.replace(
            "STALE_BASELINE: dict[str, str] = {",
            'STALE_BASELINE: dict[str, str] = {\n    "extra.md": "test exemption",',
            1,
        )
        (Path(repo) / "scripts" / "lint-book-zh-parity.py").write_text(exempted)
        out = run_lint()
        if out.returncode != 0 or "extra.md" in out.stdout:
            print(f"self-test: baseline exemption not honored:\n{out.stdout}{out.stderr}")
            return 1
    print("ok: self-test")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the built-in scenarios against a throwaway git repo",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    problems = failures()
    for problem in problems:
        print(f"FAIL: {problem}")
    if not problems:
        print(f"ok: book-zh parity ({len(chapters(EN))} chapters)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
