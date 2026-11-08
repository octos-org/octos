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
# Steps the pool runs as a variant instead (it has no Docker daemon for the CI
# account); the hosted step keeps the exact command counted below.
HOSTED_ONLY = {"Test octos-agent (integration)"}
SELF_HOSTED_VARIANT = {
    "Test octos-agent (integration, self-hosted)":
        "cargo test -p octos-agent --test '*' -- --skip should_block_dangerous_docker_cwd",
}
SHARDS = {
    "check-windows-cli-lib": "windows-cli-lib",
    "check-windows-cli-integration": "windows-cli-integration",
    "check-windows-workspace": "windows-workspace",
}
SELECTIVE_OUTPUTS = {
    "check-windows-cli-lib": "windows_cli_lib_commands",
    "check-windows-cli-integration": "windows_cli_integration_commands",
    "check-windows-workspace": "windows_workspace_commands",
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


def simulate_workspace_fallback(command, module_tests, crate_tests):
    """Model the PowerShell branch for structural regression cases.

    A non-empty module never falls back. A host-empty lib filter removes one
    trailing element; a host-empty `--test <name>` / `--bin <name>` target
    removes the selector and target name together. A host-empty whole crate
    remains a hard error.
    """
    if module_tests > 0:
        return command
    strip = 2 if "--test" in command or "--bin" in command else 1
    fallback = command[:-strip]
    if crate_tests == 0:
        raise ValueError("full-crate fallback selected zero tests")
    return fallback


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
        output = SELECTIVE_OUTPUTS[name]
        assert job["if"] == f"needs.scope.outputs.full_ci == 'true' || needs.scope.outputs.{output} != '[]'"
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
            if step.get("name") in HOSTED_ONLY:
                assert f"github.event_name != 'pull_request' && {HOSTED}" in cond, f"{name}: {step.get('name')}"
            elif "github.event_name != 'pull_request'" in cond:
                assert f"github.event_name != 'pull_request' || {SELF_HOSTED}" in cond, f"{name}: {step.get('name')}"
            if "github.event_name == 'pull_request'" in cond:
                assert f"github.event_name == 'pull_request' && {HOSTED}" in cond, f"{name}: {step.get('name')}"
        for step in steps(job):
            run = step.get("run")
            if run is not None:
                all_runs.append(run)
                assert len(run.strip().splitlines()) == 1, f"{name}: cargo commands stay one per step"
                assert step.get("shell") != "pwsh"

    workspace = steps(jobs["check-windows-workspace"])
    for step_name, command in SELF_HOSTED_VARIANT.items():
        variant = next(step for step in workspace if step.get("name") == step_name)
        assert variant["run"] == command and variant["if"].startswith(f"${{{{ ({SELF_HOSTED} && !cancelled())")
        all_runs.remove(command)
    for command in set(TEST_COMMANDS):
        assert all_runs.count(command) == TEST_COMMANDS.count(command), (
            f"test command lost or duplicated: {command}"
        )
    assert next(step for step in steps(jobs["check-windows-cli-lib"]) if step.get("name") == "Test octos-cli (lib)")["run"] == "cargo test -p octos-cli --lib"
    assert next(step for step in steps(jobs["check-windows-cli-integration"]) if step.get("name") == "Test octos-cli (integration)")["run"] == "cargo test -p octos-cli --test '*'"
    assert "Test octos-cli (lib)" not in str(steps(jobs["check-windows-workspace"]))
    assert "Test octos-cli (integration)" not in str(steps(jobs["check-windows-workspace"]))

    selective_names = {
        "check-windows-cli-lib": "Test changed octos-cli modules (PR)",
        "check-windows-cli-integration": "Test changed octos-cli integration targets (PR)",
        "check-windows-workspace": "Test changed workspace modules (PR)",
    }
    for job_name, step_name in selective_names.items():
        step = next(step for step in steps(jobs[job_name]) if step.get("name") == step_name)
        output = SELECTIVE_OUTPUTS[job_name]
        assert step["env"]["WINDOWS_TEST_COMMANDS"] == f"${{{{ needs.scope.outputs.{output} }}}}"
        assert "github.event_name == 'pull_request'" in step["if"]
        assert "github.event_name != 'pull_request'" not in step["if"]
        for needle in ("-- --list", "$count -lt 1", "& cargo @command"):
            assert needle in step["run"], f"{step_name}: missing {needle}"
        # #3080 regression (T-0478): without -NoEnumerate, PowerShell unrolls a
        # single-element command array and splats the string char by char.
        assert "ConvertFrom-Json -NoEnumerate" in step["run"], (
            f"{step_name}: single-command arrays must not enumerate"
        )

    # T-0517 follow-up: host-empty fallback must preserve the original argv,
    # including feature/compile flags, rather than reconstructing a bare
    # `test -p <crate> --lib` command that silently drops them.
    workspace_run = next(
        step for step in steps(jobs["check-windows-workspace"])
        if step.get("name") == "Test changed workspace modules (PR)"
    )["run"]
    assert "$command = @($command | Select-Object -SkipLast 1)" in workspace_run, (
        "fallback must remove only the trailing module/test filter"
    )
    assert "$command = @('test', '-p', $package, '--lib')" not in workspace_run, (
        "fallback must not reconstruct argv and discard features"
    )
    feature_rich = [
        "test", "-p", "octos-cli", "--features", "api", "--lib", "auth::tests",
    ]
    assert simulate_workspace_fallback(feature_rich, 0, 40) == feature_rich[:-1], (
        "feature-rich host-empty module must retain --features api on fallback"
    )
    assert simulate_workspace_fallback(feature_rich, 3, 40) == feature_rich, (
        "a non-empty module must never fall back"
    )
    assert "$command -contains '--test' -or $command -contains '--bin'" in workspace_run
    assert "$command = @($command | Select-Object -SkipLast 2)" in workspace_run, (
        "host-empty --test/--bin selections must remove selector and target name"
    )
    integration = [
        "test", "-p", "octos-bus", "--features", "api", "--test", "bus_channel",
    ]
    assert simulate_workspace_fallback(integration, 0, 20) == integration[:-2], (
        "a host-empty integration target must not leave a dangling --test"
    )
    assert simulate_workspace_fallback(integration, 2, 20) == integration, (
        "a non-empty integration target must not fall back"
    )
    binary = ["test", "-p", "octos-plugin", "--bin", "plugin"]
    assert simulate_workspace_fallback(binary, 0, 12) == ["test", "-p", "octos-plugin"], (
        "a host-empty binary target must remove --bin and its name together"
    )
    try:
        simulate_workspace_fallback(feature_rich, 0, 0)
    except ValueError:
        pass
    else:
        raise AssertionError("a zero-test full-crate fallback must remain fatal")

    required = jobs["ci-required"]
    assert "check-windows" in required["needs"]
    assert all(shard not in required["needs"] for shard in SHARDS)
    assert "check-arm" in jobs
    print(f"windows shard checks ok ({len(TEST_COMMANDS)} test commands preserved)")


if __name__ == "__main__":
    main()
