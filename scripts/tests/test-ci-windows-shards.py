#!/usr/bin/env python3
"""The merged Windows suite must preserve every test and the required aggregate name."""
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
        "bash scripts/cargo-test-glob.sh octos-agent -- --skip should_block_dangerous_docker_cwd",
}
SUITE = "check-windows-suite"
SELECTIVE_OUTPUTS = {
    "Test changed octos-cli modules (PR)": "windows_cli_lib_commands",
    "Test changed octos-cli integration targets (PR)": "windows_cli_integration_commands",
    "Test changed workspace modules (PR)": "windows_workspace_commands",
}
TEST_COMMANDS = [
    "cargo test -p octos-llm --lib vision::tests",
    # The pre-split job has octos-core twice: the PR probe and the full lane.
    "cargo test -p octos-core",
    "cargo test -p octos-core",
    "cargo test -p octos-memory",
    "cargo test -p octos-llm --lib",
    "bash scripts/cargo-test-glob.sh octos-llm",
    "cargo test -p octos-bus",
    "cargo test -p octos-plugin",
    "cargo test -p octos-sandbox",
    "cargo test -p octos-swarm",
    "cargo test -p octos-agent --lib",
    "bash scripts/cargo-test-glob.sh octos-agent",
    "cargo test -p octos-cli --lib",
    "bash scripts/cargo-test-glob.sh octos-cli",
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
    # The three former shards must be gone; their commands live in one suite.
    for removed in (
        "check-windows-cli-lib",
        "check-windows-cli-integration",
        "check-windows-workspace",
    ):
        assert removed not in jobs, f"{removed} must be merged into {SUITE}"
    aggregate = jobs["check-windows"]
    assert aggregate["needs"] == [SUITE], "required name must need the merged suite"
    assert aggregate["runs-on"] == "windows-latest"
    assert aggregate["if"] == "always()"
    assert aggregate["env"]["RESULTS"] == "${{ toJSON(needs) }}"
    gate = "\n".join(str(step.get("run", "")) for step in steps(aggregate))
    assert SUITE in gate
    assert "full_ci" in gate and "cancelled" in gate

    job = jobs[SUITE]
    assert job["runs-on"] == WIN_RUNNER
    assert job["needs"] == "scope"
    job_if = job["if"]
    for output in SELECTIVE_OUTPUTS.values():
        assert f"needs.scope.outputs.{output} != '[]'" in job_if, output
    assert "needs.scope.outputs.full_ci == 'true'" in job_if
    assert job["env"]["RUST_MIN_STACK"] == "8388608"
    caches = [step for step in steps(job) if "Swatinem/rust-cache@" in str(step.get("uses", ""))]
    assert len(caches) == 2, "migration restore plus suite save cache"
    legacy, cache = caches
    assert legacy["with"]["shared-key"] == "windows"
    assert legacy["with"]["save-if"] is False, "legacy cache must never be overwritten"
    assert cache["with"]["shared-key"] == "windows-suite"
    assert cache["with"]["save-if"] == "${{ github.ref == 'refs/heads/main' }}"
    assert "restore-keys" not in str(caches), "unsupported rust-cache input"
    # The pool keeps Rust and a per-slot target directory: no install, no cache.
    assert all(step.get("if") == HOSTED for step in caches)
    toolchain = [step for step in steps(job) if "dtolnay/rust-toolchain@" in str(step.get("uses", ""))]
    assert len(toolchain) == 1 and toolchain[0]["if"] == HOSTED
    select = next(step for step in steps(job) if step.get("name") == "Select preinstalled Rust toolchain (self-hosted)")
    assert select["if"].startswith(SELF_HOSTED) and "RUSTUP_TOOLCHAIN" in select["run"]
    fonts = [step for step in steps(job) if step.get("name") == "Prepare pinned presentation fonts"]
    assert len(fonts) == 1 and fonts[0]["run"] == "source scripts/prepare-pptx-fonts.sh"
    # On the pool a PR runs the full suite: every main-only step also runs
    # there, and the hosted PR probes stay hosted-only.
    for step in steps(job):
        cond = str(step.get("if", ""))
        if step.get("name") in HOSTED_ONLY:
            assert f"github.event_name != 'pull_request' && {HOSTED}" in cond, step.get("name")
        elif "github.event_name != 'pull_request'" in cond:
            assert f"github.event_name != 'pull_request' || {SELF_HOSTED}" in cond, step.get("name")
        if "github.event_name == 'pull_request'" in cond:
            assert f"github.event_name == 'pull_request' && {HOSTED}" in cond, step.get("name")
    all_runs = []
    for step in steps(job):
        run = step.get("run")
        if run is not None:
            all_runs.append(run)
            assert len(run.strip().splitlines()) == 1, f"{step.get('name')}: cargo commands stay one per step"
            assert step.get("shell") != "pwsh"

    variant = next(step for step in steps(job) if step.get("name") in SELF_HOSTED_VARIANT)
    command = SELF_HOSTED_VARIANT[variant["name"]]
    assert variant["run"] == command and variant["if"].startswith(f"${{{{ ({SELF_HOSTED} && !cancelled())")
    all_runs.remove(command)
    for command in set(TEST_COMMANDS):
        assert all_runs.count(command) == TEST_COMMANDS.count(command), (
            f"test command lost or duplicated: {command}"
        )
    assert next(step for step in steps(job) if step.get("name") == "Test octos-cli (lib)")["run"] == "cargo test -p octos-cli --lib"
    assert next(step for step in steps(job) if step.get("name") == "Test octos-cli (integration)")["run"] == "bash scripts/cargo-test-glob.sh octos-cli"

    llm_integration = [
        (job_name, step)
        for job_name, other in jobs.items()
        for step in steps(other)
        if step.get("name") == "Test octos-llm (integration)"
    ]
    assert len(llm_integration) == 3
    for job_name, step in llm_integration:
        if job_name == SUITE:
            assert step["run"] == "bash scripts/cargo-test-glob.sh octos-llm", (
                f"{job_name}: empty integration target set must skip via the shared helper"
            )
        else:
            # check-arm / check-riscv keep their existing gate and commands.
            assert "hashFiles('crates/octos-llm/tests/**/*.rs') != ''" in step["if"]
            assert step["run"] == "cargo test -p octos-llm --tests"

    for step_name, output in SELECTIVE_OUTPUTS.items():
        step = next(step for step in steps(job) if step.get("name") == step_name)
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

    # PR probes stay tied to the workspace selector trigger, as before the
    # merge (they lived in the workspace leg, not the CLI legs).
    for probe in (
        "Test octos-llm media nofollow fixtures (PR)",
        "Test octos-core (PR)",
        "Test voice audio validation (PR)",
    ):
        step = next(step for step in steps(job) if step.get("name") == probe)
        assert "windows_workspace_commands != '[]'" in step["if"], probe

    # T-0517 follow-up: host-empty fallback must preserve the original argv,
    # including feature/compile flags, rather than reconstructing a bare
    # `test -p <crate> --lib` command that silently drops them.
    workspace_run = next(
        step for step in steps(job)
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
    # The suite is not itself a required check; only the aggregate is.
    assert SUITE not in required["needs"]
    for removed in (
        "check-windows-cli-lib",
        "check-windows-cli-integration",
        "check-windows-workspace",
    ):
        assert removed not in required["needs"]
    assert "check-arm" in jobs

    print("windows suite checks ok (21 test commands preserved in one job)")


if __name__ == "__main__":
    main()
