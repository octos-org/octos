#!/usr/bin/env python3
"""T-0610 — e2e-tool-regression build/serve profile pairing + coverage guard.

The job was switched from `cargo build --release` to a dev-profile build
(merge_group's single largest repeated compile, ~29m/round). The only
invariant that must never drift is the PAIRING: the profile the build step
produces and the path the serve step launches must agree, and the feature
set / test steps must stay intact. A future edit that flips one side, drops
a feature, or deletes a step fails here loudly instead of turning the
required check into a silent no-op (the #1923 lesson).

`--self-test` proves the guard fails on mismatched profiles, a reduced
feature set, and a removed test step.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

CI = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "ci.yml"
FEATURES = "octos-cli/api,octos-cli/telegram"


def job_section(text: str) -> str:
    match = re.search(r"^  e2e-tool-regression:\n", text, re.M)
    if not match:
        raise AssertionError("e2e-tool-regression job is missing from ci.yml")
    start = match.end()
    nxt = re.search(r"^  \w[\w-]*:\n", text[start:], re.M)
    return text[start : start + nxt.start()] if nxt else text[start:]


def check(text: str) -> list[str]:
    problems: list[str] = []
    section = job_section(text)

    build = re.search(r"cargo build (?P<flags>[^\n]*-p octos-cli[^\n]*)", section)
    if not build:
        problems.append("no `cargo build -p octos-cli` step in e2e-tool-regression")
        return problems
    flags = build.group("flags")
    if FEATURES not in flags:
        problems.append(
            f"build feature set changed: expected {FEATURES!r} in {flags!r} "
            "(coverage must not shrink)"
        )
    release_build = "--release" in flags

    serve = re.search(r"\$\{CARGO_TARGET_DIR:-target\}/(?P<profile>\w+)/octos", section)
    if not serve:
        problems.append("no `${CARGO_TARGET_DIR:-target}/<profile>/octos` serve launch")
        return problems
    serve_profile = serve.group("profile")
    expected = "release" if release_build else "debug"
    if serve_profile != expected:
        problems.append(
            f"profile mismatch: build is {'--release' if release_build else 'dev'} "
            f"but serve launches {serve_profile}/octos (expected {expected})"
        )

    for step in (
        "Run e2e tool regression tests",
        "Run m9 protocol conformance (model-independent)",
        "Start octos serve",
        "Stop octos serve",
    ):
        if step not in section:
            problems.append(f"required step {step!r} is missing")
    return problems


def expect(text: str, want_problems: int, label: str) -> None:
    problems = check(text)
    if len(problems) != want_problems:
        raise AssertionError(
            f"{label}: expected {want_problems} problem(s), got {problems}"
        )


def self_test() -> None:
    base = CI.read_text()
    expect(base, 0, "current ci.yml")

    # Build dev but serve still release: the exact drift this guard exists for.
    expect(
        base.replace("/debug/octos", "/release/octos"),
        1,
        "serve/build profile mismatch",
    )
    # Reduced feature coverage must fail.
    expect(
        base.replace(FEATURES, "octos-cli/api"),
        1,
        "feature set reduced",
    )
    # A removed test step must fail (silent no-op required check).
    expect(
        base.replace("Run e2e tool regression tests", "Run renamed specs"),
        1,
        "e2e spec step removed",
    )
    # Release build with release serve is coherent and must PASS (the guard
    # pins pairing, not a specific profile).
    expect(
        base.replace(
            'cargo build -p octos-cli --features "octos-cli/api,octos-cli/telegram"',
            'cargo build --release -p octos-cli --features "octos-cli/api,octos-cli/telegram"',
        ).replace("/debug/octos", "/release/octos"),
        0,
        "coherent release pairing",
    )
    print("e2e build-profile guard self-test ok")


def main() -> None:
    if "--self-test" in sys.argv:
        self_test()
        return
    problems = check(CI.read_text())
    if problems:
        for problem in problems:
            print(f"::error::{problem}")
        raise SystemExit(1)
    print("e2e-tool-regression: build/serve profiles paired, features and steps intact")


if __name__ == "__main__":
    main()
