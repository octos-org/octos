#!/usr/bin/env python3
"""Fail-closed proof that this exact main SHA passed a complete merge group."""
from __future__ import annotations

import argparse
import json
import os
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable

REQUIRED_JOBS = (
    "check",
    "check-matrix",
    "check-windows",
    "test-octos-agent (lib)",
    "test-octos-agent (integration)",
    "security",
    "typos",
    "test-octos-cli-gate",
)
PROVEN_JOBS = REQUIRED_JOBS + ("CI required",)
WORKFLOW_PATH = ".github/workflows/ci.yml"
ApiGet = Callable[[str], Any]


def decision(reason: str, **extra: Any) -> dict[str, Any]:
    return {"skip_duplicate_tests": False, "reason": reason, **extra}


def page_url(path: str, page: int) -> str:
    base, separator, query = path.partition("?")
    values = urllib.parse.parse_qs(query, keep_blank_values=True)
    values["page"] = [str(page)]
    return base + separator + urllib.parse.urlencode(values, doseq=True)


def paginate(api_get: ApiGet, path: str, key: str) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    page = 1
    while True:
        data = api_get(page_url(path, page))
        rows = data.get(key)
        total = data.get("total_count")
        if not isinstance(rows, list) or not isinstance(total, int):
            raise RuntimeError(f"incomplete pagination schema for {key}")
        items.extend(rows)
        if len(items) == total:
            return items
        if len(items) > total or not rows or page >= 100:
            raise RuntimeError(f"incomplete pagination for {key}")
        page += 1


def required_rules(api_get: ApiGet) -> None:
    rules = api_get("rules/branches/main")
    if not isinstance(rules, list):
        raise RuntimeError("main rules response is not a list")
    contexts = {
        check["context"]
        for rule in rules
        if rule.get("type") == "required_status_checks"
        for check in rule.get("parameters", {}).get("required_status_checks", [])
    }
    if contexts != set(REQUIRED_JOBS):
        raise RuntimeError(f"required context registry changed: {sorted(contexts)}")


def successful_jobs(jobs: list[dict[str, Any]]) -> bool:
    by_name: dict[str, list[dict[str, Any]]] = {}
    for job in jobs:
        by_name.setdefault(job.get("name", ""), []).append(job)
    return all(
        len(by_name.get(name, [])) == 1
        and by_name[name][0].get("status") == "completed"
        and by_name[name][0].get("conclusion") == "success"
        for name in PROVEN_JOBS
    )


def valid_run(run: dict[str, Any], sha: str, repository: str, workflow_id: int) -> bool:
    return (
        run.get("event") == "merge_group"
        and run.get("head_sha", "").lower() == sha.lower()
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
        and run.get("run_attempt") == 1
        and run.get("workflow_id") == workflow_id
        and run.get("head_repository", {}).get("full_name") == repository
    )


def evaluate(
    sha: str,
    api_get: ApiGet,
    repository: str = "octos-org/octos",
    current_run_attempt: int = 1,
) -> dict[str, Any]:
    """Return skip=true only after a fixed run and its jobs pass a second GET."""
    try:
        if current_run_attempt != 1:
            return decision("current push is a rerun; full CI retained")
        workflow = api_get("actions/workflows/ci.yml")
        workflow_id = workflow.get("id")
        if workflow.get("path") != WORKFLOW_PATH or not isinstance(workflow_id, int):
            raise RuntimeError("CI workflow identity mismatch")
        required_rules(api_get)
        query = urllib.parse.urlencode(
            {"event": "merge_group", "head_sha": sha, "per_page": 100}
        )
        runs = paginate(api_get, f"actions/workflows/ci.yml/runs?{query}", "workflow_runs")
        candidates = [
            run for run in runs
            if valid_run(run, sha, repository, workflow_id)
        ]
        for run in sorted(candidates, key=lambda item: item.get("created_at", ""), reverse=True):
            run_id = run["id"]
            jobs_path = f"actions/runs/{run_id}/jobs?per_page=100&filter=latest"
            if not successful_jobs(paginate(api_get, jobs_path, "jobs")):
                continue

            # Pin the decision to this run ID, never to a mutable latest run.
            # Re-read both resources before emitting the skip decision.
            rechecked = api_get(f"actions/runs/{run_id}")
            if rechecked.get("id") != run_id or not valid_run(
                rechecked, sha, repository, workflow_id
            ):
                raise RuntimeError("merge-group run changed during final verification")
            if not successful_jobs(paginate(api_get, jobs_path, "jobs")):
                raise RuntimeError("required jobs changed during final verification")
            return {
                "skip_duplicate_tests": True,
                "reason": "exact-sha merge group passed every required job twice",
                "merge_group_run_id": run_id,
                "merge_group_url": run.get("html_url"),
                "merge_group_run_attempt": 1,
                "workflow_id": workflow_id,
                "head_repository": repository,
                "required_jobs": list(PROVEN_JOBS),
                "final_verification": "same run and latest jobs re-fetched successfully",
            }
        return decision("no exact-sha successful merge group with all required jobs")
    except Exception as error:  # Network/schema failures must retain full CI.
        return decision(f"merge-group proof unavailable: {type(error).__name__}: {error}")


def github_api(token: str, repository: str) -> ApiGet:
    def get(path: str) -> Any:
        request = urllib.request.Request(
            f"https://api.github.com/repos/{repository}/{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "octos-ci-main-dedup",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    return get


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--sha", default=os.environ.get("GITHUB_SHA", ""))
    args = parser.parse_args()
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    try:
        current_attempt = int(os.environ.get("GITHUB_RUN_ATTEMPT", "0"))
    except ValueError:
        current_attempt = -1
    if not args.sha:
        result = decision("missing GITHUB_SHA")
    elif not repository:
        result = decision("missing GITHUB_REPOSITORY")
    elif not token:
        result = decision("missing GitHub token")
    else:
        result = evaluate(args.sha, github_api(token, repository), repository, current_attempt)
    args.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
