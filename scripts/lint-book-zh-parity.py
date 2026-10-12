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
   copy — the en update was never carried over — unless the file carries a
   `STALE_BASELINE` entry whose cutoff date covers the en commit.
3. The checkout cannot judge staleness at all: not a git repository, a
   shallow clone (history truncated epochs would both read 0 and pass),
   or no `book/src` next to the CWD.

What it cannot see: content dropped while the zh file was still touched
later for unrelated reasons. That class needs eyes on the diff; the gate
only guarantees the process convention going forward. The %ct anchor also
trusts committer dates — a rebased zh commit can look fresh without being
carried over; the docstring is the honest boundary of what is mechanical.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from datetime import datetime
from pathlib import Path

EN = Path("book/src")
ZH = Path("book-zh/src")

# Known-stale chapters with drift older than this lint, exempted until a
# backfill lands. Each entry names its gap and a cutoff date: the exemption
# only covers en commits from ON or BEFORE the cutoff — any later en-only
# update to the chapter is flagged like anywhere else. Remove the entry once
# the recorded gap is translated.
STALE_BASELINE: dict[str, tuple[str, str]] = {
    # Four pre-09 sections have no zh counterpart (8437c548 QoS ranking /
    # provider metadata / hedge mode, b33811c4 providing the API key). The
    # 2026-09-16..10-07 drift (#2691 et al.) is backfilled; anything newer
    # must be carried over.
    "providers.md": ("four pre-2026-09 sections untranslated", "2026-10-12"),
    # One en section with no zh counterpart, as of lockstep 043544a2.
    "advanced.md": ("one en section untranslated", "2026-09-23"),
    # Three en sections with no zh counterpart, as of lockstep 73bf14c8.
    "skill-development.md": ("three en sections untranslated", "2026-06-21"),
}


def git_lines(*args: str) -> str:
    return subprocess.run(
        ["git", *args], capture_output=True, text=True
    ).stdout.strip()


def last_commit_epoch(path: str) -> int:
    """Unix timestamp of the last commit touching path; 0 when untracked."""
    out = git_lines("log", "-1", "--format=%ct", "--", path)
    return int(out) if out else 0


def chapters(root: Path) -> set[str]:
    return {str(p.relative_to(root)) for p in root.rglob("*.md")}


def cutoff_epoch(date: str) -> int:
    return datetime.fromisoformat(f"{date}T00:00:00+00:00").timestamp()


def failures() -> list[str]:
    problems: list[str] = []
    if not EN.is_dir() or not ZH.is_dir():
        return [f"run from the repository root ({EN} / {ZH} not found)"]
    if git_lines("rev-parse", "--is-shallow-repository") != "false":
        problems.append(
            "shallow checkout: per-chapter staleness needs full history "
            "(set fetch-depth: 0)"
        )
    en_files, zh_files = chapters(EN), chapters(ZH)
    for missing in sorted(en_files - zh_files):
        problems.append(f"no Chinese chapter for {EN / missing}")
    for extra in sorted(zh_files - en_files):
        problems.append(f"no English chapter for {ZH / extra}")
    for rel in sorted(en_files & zh_files):
        en_ts, zh_ts = last_commit_epoch(str(EN / rel)), last_commit_epoch(str(ZH / rel))
        if en_ts <= zh_ts:
            continue
        if rel in STALE_BASELINE and en_ts <= cutoff_epoch(STALE_BASELINE[rel][1]):
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

        def run_lint(
            cwd: str = repo, script: str = "scripts/lint-book-zh-parity.py"
        ) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                [sys.executable, script],
                cwd=cwd, capture_output=True, text=True,
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
        # Scenario 5: a baseline entry exempts only en commits up to its
        # cutoff — the 01-04 en update inside the cutoff is silenced.
        exempted = lint.replace(
            "STALE_BASELINE: dict[str, tuple[str, str]] = {",
            'STALE_BASELINE: dict[str, tuple[str, str]] = {\n'
            '    "extra.md": ("test exemption", "2026-01-05"),',
            1,
        )
        (Path(repo) / "scripts" / "lint-book-zh-parity.py").write_text(exempted)
        out = run_lint()
        if out.returncode != 0 or "extra.md" in out.stdout:
            print(f"self-test: in-cutoff exemption not honored:\n{out.stdout}{out.stderr}")
            return 1
        # Scenario 5b: an en update AFTER the cutoff is flagged despite the
        # entry — the baseline cannot blind the gate to new incidents.
        commit("book/src", "extra.md", "# Extra v2\n", "2026-01-06T00:00:00 +0000")
        out = run_lint()
        if out.returncode == 0 or "extra.md" not in out.stdout:
            print(f"self-test: post-cutoff update not flagged:\n{out.stdout}{out.stderr}")
            return 1
        # Scenario 6: shallow clone — the gate refuses to judge instead of
        # silently passing on truncated epochs.
        shallow = str(Path(tmp) / "shallow")
        subprocess.run(
            ["git", "clone", "-q", "--depth=1", f"file://{repo}", shallow],
            check=True, capture_output=True,
        )
        out = run_lint(cwd=shallow)
        if out.returncode == 0 or "shallow" not in out.stdout:
            print(f"self-test: shallow clone not flagged:\n{out.stdout}{out.stderr}")
            return 1
        # Scenario 7: degenerate CWD without the books — loud failure, no green.
        out = run_lint(
            cwd=tmp, script=os.path.join(repo, "scripts", "lint-book-zh-parity.py")
        )
        if out.returncode == 0 or "repository root" not in out.stdout:
            print(f"self-test: missing book dirs not flagged:\n{out.stdout}{out.stderr}")
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
