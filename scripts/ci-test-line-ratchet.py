#!/usr/bin/env python3
"""Test-line ratchet (T-0496): a PR must not net-grow the test suite.

Counts test lines with the exact algorithm of wake/test-lines.py — Rust
test files (`tests/`, `*_tests.rs`, `*_test.rs`, `test_*.rs`, `tests.rs`)
plus inline `#[cfg(test)]` modules, JS/TS `*.test.*`/`*.spec.*` and
`test_*.py`/`_test.py` — over `git archive` of the base and head refs.

pull_request and merge_group enforce: net growth > 0 fails unless the PR
carries `test-growth-approved` added by a human maintainer (see
MAINTAINERS; the shared agent account never counts). Push to main only
reports the numbers.

`--self-test` runs offline cases in temporary git repositories.
"""
from __future__ import annotations

import argparse
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

LABEL = "test-growth-approved"
MAINTAINERS = frozenset({"alan0x", "BH3GEI", "ymote"})

RUST_TEST = re.compile(r"^\s*#\[(tokio::test|test|rstest|test_case|async_std::test|sqlx::test)\b")
JS_TEST = re.compile(r"^\s*(it|test)(\.(each|skip|only)\b[^(]*)?\(")
SCRIPT_TEST = re.compile(r"(\.(test|spec)\.(ts|tsx|js|jsx|mjs)$)|(^test_.*\.py$)|(_test\.py$)")


def rust_test_file(path: str) -> bool:
    parts, base = path.split("/"), os.path.basename(path)
    return "tests" in parts or base.endswith(("_tests.rs", "_test.rs")) or base.startswith("test_") or base == "tests.rs"


def inline_test_lines(lines: list[str]) -> int:
    n, i = 0, 0
    while i < len(lines):
        if re.match(r"^\s*#\[cfg\(test\)\]", lines[i]):
            j = i + 1
            while j < len(lines) and (lines[j].strip().startswith("#[") or not lines[j].strip()):
                j += 1
            if j < len(lines) and re.match(r"^\s*(pub(\(\w+\))?\s+)?mod\s+\w+\s*\{", lines[j]):
                depth, k = 0, j
                while k < len(lines):
                    depth += lines[k].count("{") - lines[k].count("}")
                    if depth <= 0 and k > j:
                        break
                    k += 1
                n += k - i + 1
                i = k + 1
                continue
        i += 1
    return n


def ensure_ref(repo: Path, ref: str) -> None:
    probe = subprocess.run(["git", "cat-file", "-e", ref], cwd=repo, capture_output=True)
    if probe.returncode == 0:
        return
    fetched = subprocess.run(
        ["git", "fetch", "--no-tags", "origin", ref], cwd=repo, capture_output=True, text=True
    )
    if fetched.returncode:
        raise SystemExit(f"cannot resolve {ref}: {fetched.stderr.strip()}")


def measure(repo: Path, ref: str) -> dict[str, int]:
    archive = subprocess.run(
        ["git", "archive", ref], cwd=repo, capture_output=True, check=True
    ).stdout
    files: dict[str, int] = {}
    tar = tarfile.open(fileobj=io.BytesIO(archive))
    for member in tar.getmembers():
        if not member.isfile() or "node_modules/" in member.name:
            continue
        name, base = member.name, os.path.basename(member.name)
        rust = name.endswith(".rs")
        script = SCRIPT_TEST.search(base)
        if not rust and not script:
            continue
        lines = tar.extractfile(member).read().decode("utf-8", "replace").split("\n")
        count = (
            len(lines) if rust_test_file(name) else inline_test_lines(lines)
        ) if rust else len(lines)
        if count:
            files[name] = count
    return files


def label_approved(repo_slug: str, pr_number: int, events_json: Path | None) -> tuple[bool, str]:
    if events_json is not None:
        events = json.loads(events_json.read_text())
    else:
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not token:
            return False, "no GH_TOKEN available to read label events"
        events = []
        page = 1
        while True:
            url = (
                f"https://api.github.com/repos/{repo_slug}/issues/{pr_number}"
                f"/events?per_page=100&page={page}"
            )
            request = urllib.request.Request(
                url, headers={
                    "Authorization": f"Bearer {token}",
                    "Accept": "application/vnd.github+json",
                    "X-GitHub-Api-Version": "2022-11-28",
                }
            )
            with urllib.request.urlopen(request, timeout=30) as response:
                batch = json.loads(response.read())
            events.extend(batch)
            if len(batch) < 100:
                break
            page += 1
    # Compute the label's CURRENT generation: the last labeled/unlabeled
    # event for this label decides both presence and who applied it. A
    # maintainer's historical labeled event that was later removed, or one
    # superseded by a bot re-label, must not exempt.
    applied_by: str | None = None
    for event in events:
        if event.get("label", {}).get("name") != LABEL:
            continue
        if event.get("event") == "labeled":
            applied_by = event.get("actor", {}).get("login")
        elif event.get("event") == "unlabeled":
            applied_by = None
    if applied_by in MAINTAINERS:
        return True, applied_by
    if applied_by:
        return False, f"current {LABEL} label was applied by {applied_by}, not a maintainer"
    return False, "no maintainer-applied label event found"


def growth_detail(base: dict[str, int], head: dict[str, int]) -> list[str]:
    rows = []
    for name in sorted(set(base) | set(head)):
        delta = head.get(name, 0) - base.get(name, 0)
        if delta > 0:
            rows.append(f"  +{delta}: {name}")
    return rows


def run(args: argparse.Namespace) -> int:
    repo = args.repo.resolve()
    ensure_ref(repo, args.base)
    ensure_ref(repo, args.head)
    base = measure(repo, args.base)
    head = measure(repo, args.head)
    base_total = sum(base.values())
    head_total = sum(head.values())
    delta = head_total - base_total
    print(
        f"test-line ratchet: base {args.base} = {base_total} lines, "
        f"head {args.head} = {head_total} lines, delta = {delta:+d}"
    )
    if args.report_only:
        print("report-only event (push): no enforcement")
        return 0
    if delta <= 0:
        print("net test-line change is zero or negative: green")
        return 0
    if args.pr is not None:
        approved, who = label_approved(args.repo_slug, args.pr, args.events_json)
        if approved:
            print(f"{LABEL} applied by maintainer {who}: growth exempt, green")
            return 0
        print(f"exemption check: {who}")
    print(f"::error::net test-line growth +{delta} lines in:")
    for row in growth_detail(base, head):
        print(row)
    print(
        f"Delete the same number of test lines elsewhere, or ask a maintainer "
        f"({', '.join(sorted(MAINTAINERS))}) to apply `{LABEL}`."
    )
    return 1


def git(repo: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)


def fixture_repo(root: Path, growth: int) -> Path:
    repo = root / "fixture"
    (repo / "crates/demo/src").mkdir(parents=True)
    (repo / "crates/demo").joinpath("Cargo.toml").write_text("[package]\nname = \"demo\"\n")
    lib = repo / "crates/demo/src/lib.rs"

    def source(cases: int) -> str:
        body = "".join(f"#[test]\nfn case_{i}() {{}}\n" for i in range(cases))
        return f"pub fn f() {{}}\n\n#[cfg(test)]\nmod tests {{\n{body}}}\n"

    lib.write_text(source(3))
    git(repo, "init", "-q")
    git(repo, "config", "user.email", "ratchet@example.invalid")
    git(repo, "config", "user.name", "ratchet self-test")
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "base")
    git(repo, "checkout", "-qb", "base")
    base_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    if growth != 0:
        lib.write_text(source(3 + growth))
        git(repo, "add", ".")
        git(repo, "commit", "-qm", "change")
    head_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    git(repo, "remote", "add", "origin", str(repo))
    return repo, base_sha, head_sha


def case(root: Path, name: str, growth: int, *extra: str, expect: int) -> None:
    repo, base, head = fixture_repo(root / name, growth)
    result = subprocess.run(
        [
            sys.executable, str(Path(__file__).resolve()), "--repo", str(repo),
            "--base", base, "--head", head, *extra,
        ],
        capture_output=True, text=True,
    )
    if result.returncode != expect:
        raise SystemExit(
            f"self-test {name}: expected exit {expect}, got {result.returncode}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return result.stdout


def self_test(repo_root: Path) -> None:
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        # net reduction: growth -1 removes two lines from the base test module.
        out = case(root, "reduction", -1, expect=0)
        assert "delta = -" in out, out
        out = case(root, "growth", 2, expect=1)
        assert "net test-line growth" in out and "lib.rs" in out, out
        out = case(root, "report", 2, "--report-only", expect=0)
        assert "report-only" in out, out
        maintainer_events = root / "maintainer.json"
        maintainer_events.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "anantheparty"}, "label": {"name": LABEL}},
            {"event": "labeled", "actor": {"login": "alan0x"}, "label": {"name": LABEL}},
        ]))
        out = case(
            root, "label", 2, "--pr", "1", "--repo-slug", "octos-org/octos",
            "--events-json", str(maintainer_events), expect=0,
        )
        assert "alan0x" in out, out
        bot_events = root / "bot.json"
        bot_events.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "anantheparty"}, "label": {"name": LABEL}},
        ]))
        out = case(
            root, "bot-label", 2, "--pr", "1", "--repo-slug", "octos-org/octos",
            "--events-json", str(bot_events), expect=1,
        )
        assert "applied by anantheparty, not a maintainer" in out, out
        # A maintainer's approval that was later removed must not exempt.
        removed_events = root / "removed.json"
        removed_events.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "alan0x"}, "label": {"name": LABEL}},
            {"event": "unlabeled", "actor": {"login": "ymote"}, "label": {"name": LABEL}},
        ]))
        out = case(
            root, "removed-label", 2, "--pr", "1", "--repo-slug", "octos-org/octos",
            "--events-json", str(removed_events), expect=1,
        )
        assert "no maintainer-applied label event" in out, out
        # A bot re-label after maintainer approval supersedes the generation.
        relabel_events = root / "relabel.json"
        relabel_events.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "alan0x"}, "label": {"name": LABEL}},
            {"event": "unlabeled", "actor": {"login": "anantheparty"}, "label": {"name": LABEL}},
            {"event": "labeled", "actor": {"login": "anantheparty"}, "label": {"name": LABEL}},
        ]))
        out = case(
            root, "bot-relabel", 2, "--pr", "1", "--repo-slug", "octos-org/octos",
            "--events-json", str(relabel_events), expect=1,
        )
        assert "applied by anantheparty, not a maintainer" in out, out

    workflow = (repo_root / ".github" / "workflows" / "ci.yml").read_text()
    check = workflow.split("  check:\n", 1)[1].split("  test-octos-agent:\n", 1)[0]
    assert "Test-line ratchet" in check
    assert "python3 scripts/ci-test-line-ratchet.py --self-test" in check
    assert "--report-only" in check
    assert "github.event.merge_group.base_sha" in check
    assert "github.event.pull_request.base.sha" in check
    assert "github.event.pull_request.head.sha" in check
    assert "github.event.pull_request.number" in check
    assert "check-arm" in workflow
    print("test-line ratchet self-test ok")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--pr", type=int)
    parser.add_argument("--repo-slug", default="octos-org/octos")
    parser.add_argument("--events-json", type=Path)
    parser.add_argument("--report-only", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test(args.repo.resolve())
        return
    if not args.base or not args.head:
        raise SystemExit("--base and --head are required")
    raise SystemExit(run(args))


if __name__ == "__main__":
    main()
