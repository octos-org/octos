#!/usr/bin/env python3
"""Test-line ratchet (T-0496): a PR must not net-grow the test suite.

Counts test lines with the exact algorithm of wake/test-lines.py — Rust
test files (`tests/`, `*_tests.rs`, `*_test.rs`, `test_*.rs`, `tests.rs`)
plus inline `#[cfg(test)]` / `#[cfg(all(..., test, ...))]` modules, JS/TS `*.test.*`/`*.spec.*` and
`test_*.py`/`_test.py` — over `git archive` of the base and head refs.

pull_request and merge_group enforce: net growth > 0 fails unless EITHER
(a) the PR carries `test-growth-approved` added by a human maintainer, or
(b) a human maintainer's LATEST review on the PR's CURRENT head is
APPROVED (shu 2026-11-19 ruling: approving the current head IS approving
its test growth — one maintainer action instead of two). See MAINTAINERS;
the shared agent account never counts, approvals on stale heads do not
count, and a later CHANGES_REQUESTED/DISMISSED supersedes an earlier
approval. shu 2026-11-25 caps the review path: growth above
REVIEW_CAP lines is exempt ONLY by the maintainer-applied label, never
by a review — an approved PR with too many tests must be shrunk, or
explicitly label-exempted by a maintainer's own choice. The baseline is
the merge-base of the event base and head refs, so a PR behind main is
not credited with deleting main's newer tests. Push to main only
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
# shu 2026-11-25: a maintainer's APPROVED review exempts at most this many
# net test lines; beyond it only the maintainer-applied label exempts.
REVIEW_CAP = 150

RUST_TEST = re.compile(r"^\s*#\[(tokio::test|test|rstest|test_case|async_std::test|sqlx::test)\b")
JS_TEST = re.compile(r"^\s*(it|test)(\.(each|skip|only)\b[^(]*)?\(")
SCRIPT_TEST = re.compile(r"(\.(test|spec)\.(ts|tsx|js|jsx|mjs)$)|(^test_.*\.py$)|(_test\.py$)")

# `#[cfg(test)]`, or a bare `test` argument of `#[cfg(all(...))]`: compiled
# only for tests. A test nested in `any(...)` is not test-only.
CFG_ATTR = re.compile(r"^\s*#\[cfg\((.*)\)\]")


def test_cfg(line: str) -> bool:
    match = CFG_ATTR.match(line)
    if match is None:
        return False
    expr = match.group(1).strip()
    if expr == "test":
        return True
    if not (expr.startswith("all(") and expr.endswith(")")):
        return False
    args, start, depth = expr[4:-1], 0, 0
    quoted = escaped = False
    for i, char in enumerate(args):
        if escaped:
            escaped = False
        elif quoted:
            if char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
        elif char == '"':
            quoted = True
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif char == "," and depth == 0:
            if args[start:i].strip() == "test":
                return True
            start = i + 1
    return args[start:].strip() == "test"


def rust_test_file(path: str) -> bool:
    parts, base = path.split("/"), os.path.basename(path)
    return "tests" in parts or base.endswith(("_tests.rs", "_test.rs")) or base.startswith("test_") or base == "tests.rs"


def inline_test_lines(lines: list[str]) -> int:
    n, i = 0, 0
    while i < len(lines):
        if test_cfg(lines[i]):
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


def merge_base(repo: Path, base: str, head: str) -> str:
    result = subprocess.run(
        ["git", "merge-base", base, head], cwd=repo, capture_output=True, text=True
    )
    if result.returncode:
        raise SystemExit(
            f"cannot find merge-base for {base} and {head}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


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


def review_approved(
    repo_slug: str, pr_number: int, reviews_json: Path | None
) -> tuple[bool, str]:
    """A maintainer's LATEST review on the PR's CURRENT head is APPROVED.

    The head comes from `pulls/{n}` (`head.sha`), so a merge_group run
    (whose GITHUB_SHA is the queue's synthetic merge commit) still checks
    the real PR head. Each maintainer's latest review decides: an
    approval on an older commit, or one superseded by a later
    CHANGES_REQUESTED / DISMISSED, does not exempt. Reviews carry no
    actor-generation problem like labels — the list is the full history.
    """
    if reviews_json is not None:
        payload = json.loads(reviews_json.read_text())
        head_sha, reviews = payload["head_sha"], payload["reviews"]
    else:
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not token:
            return False, "no GH_TOKEN available to read reviews"
        headers = {
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        }

        def get(url: str):
            request = urllib.request.Request(url, headers=headers)
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.loads(response.read())

        base = f"https://api.github.com/repos/{repo_slug}/pulls/{pr_number}"
        head_sha = get(base)["head"]["sha"]
        reviews = []
        page = 1
        while True:
            batch = get(f"{base}/reviews?per_page=100&page={page}")
            reviews.extend(batch)
            if len(batch) < 100:
                break
            page += 1
    # A review COMMENT does not supersede an approval — GitHub's own
    # approval accounting treats COMMENTED (and PENDING) reviews as
    # non-decisions, and alan0x routinely leaves a plain comment after
    # approving. Skip them when picking each maintainer's latest review
    # so the ratchet and the merge gate read the same verdict.
    latest: dict[str, dict] = {}
    for review in reviews:
        login = review.get("user", {}).get("login")
        if login not in MAINTAINERS:
            continue
        if review.get("state") in ("COMMENTED", "PENDING"):
            continue
        latest[login] = review  # the list is chronological; last wins
    # Any ONE maintainer approving the current head exempts — check all of
    # them before diagnosing, or a stale approval from one maintainer would
    # mask a current-head approval from another.
    for login, review in latest.items():
        if review.get("state") == "APPROVED" and review.get("commit_id") == head_sha:
            return True, f"{login} approved the current head {head_sha[:9]}"
    for login, review in latest.items():
        if review.get("state") == "APPROVED":
            return False, (
                f"{login}'s approval is on {str(review.get('commit_id'))[:9]}, "
                f"not the current head {head_sha[:9]}"
            )
    if latest:
        states = ", ".join(f"{login}: {r.get('state')}" for login, r in latest.items())
        return False, f"latest maintainer reviews are not APPROVED ({states})"
    return False, "no maintainer review found"


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
    comparison = merge_base(repo, args.base, args.head)
    base = measure(repo, comparison)
    head = measure(repo, args.head)
    base_total = sum(base.values())
    head_total = sum(head.values())
    delta = head_total - base_total
    print(
        f"test-line ratchet: base {args.base} (merge-base {comparison}) = "
        f"{base_total} lines, head {args.head} = {head_total} lines, delta = {delta:+d}"
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
        print(f"exemption check (label): {who}")
        if delta > REVIEW_CAP:
            # shu 2026-11-25: an approval only covers up to REVIEW_CAP net
            # test lines; larger growth needs the maintainer-applied label.
            print(f"::error::net test-line growth +{delta} lines exceeds the per-PR limit of {REVIEW_CAP}:")
            for row in growth_detail(base, head):
                print(row)
            print(
                f"Shrink this PR's tests to +{REVIEW_CAP} lines or fewer: merge "
                f"duplicate cases (table-driven tests), or delete redundant old "
                f"tests to offset the new ones. A maintainer's APPROVED review "
                f"no longer exempts above {REVIEW_CAP} lines — only a maintainer "
                f"({', '.join(sorted(MAINTAINERS))}) applying `{LABEL}` can, and "
                f"that is their own call to make."
            )
            return 1
        approved, who = review_approved(args.repo_slug, args.pr, args.reviews_json)
        if approved:
            print(f"review exemption: {who}: growth exempt, green")
            return 0
        print(f"exemption check (review): {who}")
    print(f"::error::net test-line growth +{delta} lines in:")
    for row in growth_detail(base, head):
        print(row)
    print(
        f"First, offset this in the same PR: delete the same number of test "
        f"lines — remove whole test functions not on this crate's TESTS-KEPT.md "
        f"list, without compressing lines."
    )
    print(
        f"If it truly cannot be offset, state why in the PR and ask a "
        f"maintainer ({', '.join(sorted(MAINTAINERS))}) to either approve the "
        f"current head (an APPROVED review IS the test-growth approval, up to "
        f"{REVIEW_CAP} lines), or apply `{LABEL}`."
    )
    return 1


def git(repo: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)


def fixture_repo(root: Path, growth: int, pad: int = 0) -> Path:
    repo = root / "fixture"
    (repo / "crates/demo/src").mkdir(parents=True)
    (repo / "crates/demo").joinpath("Cargo.toml").write_text("[package]\nname = \"demo\"\n")
    lib = repo / "crates/demo/src/lib.rs"

    def source(cases: int, padding: int = 0) -> str:
        body = "".join(f"#[test]\nfn case_{i}() {{}}\n" for i in range(cases))
        # One-line comments inside the module so odd deltas are exact.
        extra = "".join("// pad\n" for _ in range(padding))
        return f"pub fn f() {{}}\n\n#[cfg(test)]\nmod tests {{\n{body}{extra}}}\n"

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
    if growth != 0 or pad != 0:
        lib.write_text(source(3 + growth, pad))
        git(repo, "add", ".")
        git(repo, "commit", "-qm", "change")
    head_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    git(repo, "remote", "add", "origin", str(repo))
    return repo, base_sha, head_sha


def stale_branch_repo(root: Path) -> tuple[Path, str, str]:
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
    main_branch = subprocess.run(
        ["git", "branch", "--show-current"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    git(repo, "checkout", "-qb", "stale-pr")
    (repo / "branch-note.txt").write_text("non-test branch change\n")
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "pr change")
    pr_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    git(repo, "checkout", "-q", main_branch)
    lib.write_text(source(4))
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "main adds a test")
    main_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    git(repo, "remote", "add", "origin", str(repo))
    return repo, main_sha, pr_sha


def run_case(name: str, repo: Path, base: str, head: str, *extra: str, expect: int) -> str:
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


def case(root: Path, name: str, growth: int, *extra: str, expect: int) -> None:
    repo, base, head = fixture_repo(root / name, growth)
    return run_case(name, repo, base, head, *extra, expect=expect)


def self_test(repo_root: Path) -> None:
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        # net reduction: growth -1 removes two lines from the base test module.
        out = case(root, "reduction", -1, expect=0)
        assert "delta = -" in out, out
        out = case(root, "growth", 2, expect=1)
        assert "net test-line growth" in out and "lib.rs" in out, out
        assert "offset this in the same PR" in out, out
        assert "If it truly cannot be offset" in out, out
        stale_repo, stale_base, stale_head = stale_branch_repo(root / "stale-pr")
        out = run_case("stale-pr", stale_repo, stale_base, stale_head, expect=0)
        assert "delta = +0" in out, out
        assert "merge-base" in out, out
        out = case(root, "report", 2, "--report-only", expect=0)
        assert "report-only" in out, out
        no_reviews = root / "no-reviews.json"
        no_reviews.write_text(json.dumps({"head_sha": "0" * 40, "reviews": []}))
        # Mirror pinning for the review-only cases below: without
        # --events-json the label path would consult the live API under
        # CI's GH_TOKEN, so pair them with an empty events fixture too.
        no_events = root / "no-events.json"
        no_events.write_text("[]")
        maintainer_events = root / "maintainer.json"
        maintainer_events.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "anantheparty"}, "label": {"name": LABEL}},
            {"event": "labeled", "actor": {"login": "alan0x"}, "label": {"name": LABEL}},
        ]))
        out = case(
            root, "label", 2, "--pr", "1", "--repo-slug", "octos-org/octos",
            "--events-json", str(maintainer_events), "--reviews-json", str(no_reviews),
            expect=0,
        )
        assert "alan0x" in out, out
        bot_events = root / "bot.json"
        bot_events.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "anantheparty"}, "label": {"name": LABEL}},
        ]))
        out = case(
            root, "bot-label", 2, "--pr", "1", "--repo-slug", "octos-org/octos",
            "--events-json", str(bot_events), "--reviews-json", str(no_reviews),
            expect=1,
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
            "--events-json", str(removed_events), "--reviews-json", str(no_reviews),
            expect=1,
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
            "--events-json", str(relabel_events), "--reviews-json", str(no_reviews),
            expect=1,
        )
        assert "applied by anantheparty, not a maintainer" in out, out

        # The label-only cases above are pinned offline on the review path
        # via `no_reviews` (CI has GH_TOKEN; without the fixture the script
        # would consult the live API and a real approval could mask them).

        # shu 2026-11-19 ruling: a maintainer's APPROVED review on the PR's
        # current head is itself the test-growth approval.
        def reviews_fixture(path: Path, head_sha: str, *reviews: dict) -> str:
            path.write_text(json.dumps({"head_sha": head_sha, "reviews": list(reviews)}))
            return str(path)

        repo, base, head = fixture_repo(root / "review-approve", 2)
        fixture = reviews_fixture(root / "approve.json", head, {
            "user": {"login": "BH3GEI"}, "state": "APPROVED", "commit_id": head,
        })
        out = run_case("review-approve", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events), "--reviews-json", fixture, expect=0)
        assert "BH3GEI approved the current head" in out, out

        repo, base, head = fixture_repo(root / "review-stale", 2)
        fixture = reviews_fixture(root / "stale.json", head, {
            "user": {"login": "BH3GEI"}, "state": "APPROVED", "commit_id": "0" * 40,
        })
        out = run_case("review-stale", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events), "--reviews-json", fixture, expect=1)
        assert "not the current head" in out, out

        repo, base, head = fixture_repo(root / "review-agent", 2)
        fixture = reviews_fixture(root / "agent.json", head, {
            "user": {"login": "anantheparty"}, "state": "APPROVED", "commit_id": head,
        })
        out = run_case("review-agent", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events), "--reviews-json", fixture, expect=1)
        assert "no maintainer review found" in out, out

        repo, base, head = fixture_repo(root / "review-superseded", 2)
        fixture = reviews_fixture(root / "superseded.json", head,
            {"user": {"login": "alan0x"}, "state": "APPROVED", "commit_id": head},
            {"user": {"login": "alan0x"}, "state": "CHANGES_REQUESTED", "commit_id": head},
        )
        out = run_case("review-superseded", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events), "--reviews-json", fixture, expect=1)
        assert "not APPROVED" in out and "alan0x: CHANGES_REQUESTED" in out, out

        # alan0x review: one maintainer's STALE approval must not mask
        # another maintainer's current-head approval.
        repo, base, head = fixture_repo(root / "review-two", 2)
        fixture = reviews_fixture(root / "two.json", head,
            {"user": {"login": "alan0x"}, "state": "APPROVED", "commit_id": "0" * 40},
            {"user": {"login": "BH3GEI"}, "state": "APPROVED", "commit_id": head},
        )
        out = run_case("review-two", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events), "--reviews-json", fixture, expect=0)
        assert "BH3GEI approved the current head" in out, out

        # Claude ruling 2026-11-20: a plain review COMMENT after approving
        # the current head does not revoke the approval.
        repo, base, head = fixture_repo(root / "review-commented", 2)
        fixture = reviews_fixture(root / "commented.json", head,
            {"user": {"login": "alan0x"}, "state": "APPROVED", "commit_id": head},
            {"user": {"login": "alan0x"}, "state": "COMMENTED", "commit_id": head},
        )
        out = run_case("review-commented", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events), "--reviews-json", fixture, expect=0)
        assert "alan0x approved the current head" in out, out

        # Label still exempts without any approval (original behavior).
        label_only = root / "label-only.json"
        label_only.write_text(json.dumps([
            {"event": "labeled", "actor": {"login": "ymote"}, "label": {"name": LABEL}},
        ]))
        repo, base, head = fixture_repo(root / "label-only", 2)
        out = run_case("label-only", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos",
                       "--events-json", str(label_only), "--reviews-json", str(no_reviews),
                       expect=0)
        assert "ymote" in out, out

        # shu 2026-11-25 cap: an APPROVED review exempts at most 150 net
        # lines; beyond it, only the maintainer-applied label exempts.
        repo, base, head = fixture_repo(root / "cap-150-approve", 75)
        fixture = reviews_fixture(root / "cap150.json", head,
            {"user": {"login": "alan0x"}, "state": "APPROVED", "commit_id": head})
        out = run_case("cap-150-approve", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events),
                       "--reviews-json", fixture, expect=0)
        assert "delta = +150" in out and "alan0x approved the current head" in out, out

        repo, base, head = fixture_repo(root / "cap-151-approve", 75, pad=1)
        fixture = reviews_fixture(root / "cap151.json", head,
            {"user": {"login": "alan0x"}, "state": "APPROVED", "commit_id": head})
        out = run_case("cap-151-approve", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos", "--events-json", str(no_events),
                       "--reviews-json", fixture, expect=1)
        assert "delta = +151" in out and "per-PR limit of 150" in out, out
        assert "table-driven" in out and "APPROVED review" in out, out

        repo, base, head = fixture_repo(root / "cap-151-label", 75, pad=1)
        out = run_case("cap-151-label", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos",
                       "--events-json", str(maintainer_events), "--reviews-json", str(no_reviews),
                       expect=0)
        assert "delta = +151" in out and "alan0x" in out, out

        repo, base, head = fixture_repo(root / "cap-151-bot-label", 75, pad=1)
        out = run_case("cap-151-bot-label", repo, base, head, "--pr", "1",
                       "--repo-slug", "octos-org/octos",
                       "--events-json", str(bot_events), "--reviews-json", str(no_reviews),
                       expect=1)
        assert "delta = +151" in out and "not a maintainer" in out, out
        # 11-25 wake/test-lines.py parity: #[cfg(all(..., test, ...))] is
        # test-only; cfg(any(test, ...)) and cfg(feature = "test") are not.
        def cfg_fixture(root: Path, attr: str) -> tuple[Path, str, str]:
            repo = root / "fixture"
            (repo / "crates/demo/src").mkdir(parents=True)
            (repo / "crates/demo").joinpath("Cargo.toml").write_text("[package]\nname = \"demo\"\n")
            lib = repo / "crates/demo/src/lib.rs"
            lib.write_text("pub fn f() {}\n")
            git(repo, "init", "-q")
            git(repo, "config", "user.email", "ratchet@example.invalid")
            git(repo, "config", "user.name", "ratchet self-test")
            git(repo, "add", ".")
            git(repo, "commit", "-qm", "base")
            base_sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo,
                                      capture_output=True, text=True).stdout.strip()
            lib.write_text(
                f'pub fn f() {{}}\n\n#[cfg({attr})]\nmod tests {{\n'
                f'#[test]\nfn added() {{}}\n}}\n'
            )
            git(repo, "add", ".")
            git(repo, "commit", "-qm", "change")
            head_sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo,
                                      capture_output=True, text=True).stdout.strip()
            git(repo, "remote", "add", "origin", str(repo))
            return repo, base_sha, head_sha

        for i, attr in enumerate(('all(test, feature = "api")', "all(unix, test)",
                     'all(test, any(feature = "api", unix))')):
            repo, b, h = cfg_fixture(root / f"cfgpos-{i}", attr)
            out = run_case(f"cfgcounted:{attr}", repo, b, h, expect=1)
            assert "delta = +5" in out and "net test-line growth" in out, out
        for i, attr in enumerate(('any(test, feature = "x")', 'feature = "test"',
                     "all(feature = \"test-utils\", unix)",
                     'all(unix, any(feature = "api", test))')):
            repo, b, h = cfg_fixture(root / f"cfgneg-{i}", attr)
            out = run_case(f"cfgnotcounted:{attr}", repo, b, h, expect=0)
            assert "delta = +0" in out, out

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
    parser.add_argument("--reviews-json", type=Path)
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
