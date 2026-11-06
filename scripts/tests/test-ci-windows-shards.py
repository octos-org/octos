#!/usr/bin/env python3
"""Windows sharding must preserve every test and the required aggregate name."""
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parents[2]
# Same-repo PRs go to the repo-owned Windows pool when OCTOS_CI_WINDOWS is on;
# merge groups, main and fork PRs stay on hosted runners.
WIN_RUNNER = "${{ vars.OCTOS_CI_WINDOWS == 'true' && github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == github.repository && fromJSON('[\"self-hosted\",\"windows\",\"x64\",\"octos-ci-win\"]') || 'windows-latest' }}"
HOSTED = "runner.environment == 'github-hosted'"
SELF_HOSTED = "runner.environment == 'self-hosted'"
SHARDS = {
    "check-windows-cli-lib": "windows-cli-lib",
    "check-windows-cli-integration": "windows-cli-integration",
    "check-windows-workspace": "windows-workspace",
}
TEST_COMMANDS = [
    "cargo test -p octos-cli --features api --lib auth",
    "cargo test -p octos-cli --features api --lib relocate",
    "cargo test -p octos-llm --lib vision::tests",
    # The pre-split job has octos-core twice: the PR probe and the full lane.
    "cargo test -p octos-core",
    "cargo test -p octos-core",
    "cargo test -p octos-memory",
    "cargo test -p octos-llm --lib",
    "cargo test -p octos-llm --test '*'",
    "cargo test -p octos-bus",
    "cargo test -p octos-plugin",
    "cargo test -p octos-sandbox",
    "cargo test -p octos-swarm",
    "cargo test -p octos-agent --lib",
    "cargo test -p octos-agent --test '*'",
    "cargo test -p octos-cli --lib",
    "cargo test -p octos-cli --test '*'",
    "cargo test -p harness-starter-generic",
    "cargo test -p harness-starter-report",
    "cargo test -p harness-starter-audio",
    "cargo test -p harness-starter-coding",
    "cargo test --workspace --doc",
]


def steps(job):
    return job.get("steps", [])


def main() -> None:
    workflow = yaml.safe_load((REPO / ".github/workflows/ci.yml").read_text())
    jobs = workflow["jobs"]
    aggregate = jobs["check-windows"]
    assert aggregate["needs"] == list(SHARDS), "required name must need exactly three shards"
    assert aggregate["runs-on"] == "windows-latest"
    assert aggregate["if"] == "always()"
    assert aggregate["env"]["RESULTS"] == "${{ toJSON(needs) }}"
    gate = "\n".join(str(step.get("run", "")) for step in steps(aggregate))
    for shard in SHARDS:
        assert shard in gate
    assert "full_ci" in gate and "cancelled" in gate

    all_runs = []
    for name, cache_key in SHARDS.items():
        job = jobs[name]
        assert job["runs-on"] == WIN_RUNNER
        assert job["needs"] == "scope"
        assert job["if"] == "needs.scope.outputs.full_ci == 'true'"
        assert job["env"]["RUST_MIN_STACK"] == "8388608"
        caches = [step for step in steps(job) if "Swatinem/rust-cache@" in str(step.get("uses", ""))]
        assert len(caches) == 2, f"{name}: migration restore plus shard save cache"
        legacy, cache = caches
        assert legacy["with"]["shared-key"] == "windows"
        assert legacy["with"]["save-if"] is False, "legacy cache must never be overwritten"
        assert cache["with"]["shared-key"] == cache_key
        assert cache["with"]["save-if"] == "${{ github.ref == 'refs/heads/main' }}"
        assert "restore-keys" not in str(caches), "unsupported rust-cache input"
        # The pool keeps Rust and a per-slot target directory: no install, no cache.
        assert all(step.get("if") == HOSTED for step in caches)
        toolchain = [step for step in steps(job) if "dtolnay/rust-toolchain@" in str(step.get("uses", ""))]
        assert len(toolchain) == 1 and toolchain[0]["if"] == HOSTED
        select = next(step for step in steps(job) if step.get("name") == "Select preinstalled Rust toolchain (self-hosted)")
        assert select["if"].startswith(SELF_HOSTED) and "RUSTUP_TOOLCHAIN" in select["run"]
        # On the pool a PR runs the full suite: every main-only step also runs
        # there, and the hosted PR probes stay hosted-only.
        for step in steps(job):
            cond = str(step.get("if", ""))
            if "github.event_name != 'pull_request'" in cond:
                assert f"github.event_name != 'pull_request' || {SELF_HOSTED}" in cond, f"{name}: {step.get('name')}"
            if "github.event_name == 'pull_request'" in cond:
                assert f"github.event_name == 'pull_request' && {HOSTED}" in cond, f"{name}: {step.get('name')}"
        for step in steps(job):
            run = step.get("run")
            if run is not None:
                all_runs.append(run)
                assert len(run.strip().splitlines()) == 1, f"{name}: cargo commands stay one per step"
                assert step.get("shell") != "pwsh"

    for command in set(TEST_COMMANDS):
        assert all_runs.count(command) == TEST_COMMANDS.count(command), (
            f"test command lost or duplicated: {command}"
        )
    assert next(step for step in steps(jobs["check-windows-cli-lib"]) if step.get("name") == "Test octos-cli (lib)")["run"] == "cargo test -p octos-cli --lib"
    assert next(step for step in steps(jobs["check-windows-cli-integration"]) if step.get("name") == "Test octos-cli (integration)")["run"] == "cargo test -p octos-cli --test '*'"
    assert "Test octos-cli (lib)" not in str(steps(jobs["check-windows-workspace"]))
    assert "Test octos-cli (integration)" not in str(steps(jobs["check-windows-workspace"]))

    required = jobs["ci-required"]
    assert "check-windows" in required["needs"]
    assert all(shard not in required["needs"] for shard in SHARDS)
    assert "check-arm" in jobs
    print(f"windows shard checks ok ({len(TEST_COMMANDS)} test commands preserved)")


if __name__ == "__main__":
    main()
