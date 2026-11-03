#!/usr/bin/env python3
"""Keep self-hosted routing opt-in and fork PRs on hosted runners."""
import json
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parents[2]
RUNNER = "${{ vars.OCTOS_CI_SELF_HOSTED == 'true' && (github.event_name == 'merge_group' || github.event_name == 'workflow_dispatch' || (github.event_name == 'push' && github.ref == 'refs/heads/main') || (github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == github.repository)) && fromJSON('[\"self-hosted\",\"linux\",\"x64\",\"octos-ci\"]') || 'ubuntu-latest' }}"
ROUTED = {
    "dashboard",
    "swarm-app",
    "typos",
    "ux-gate-report",
    "bundle-release-check",
    "scope",
    "check",
    "test-octos-agent",
    "test-octos-cli",
    "test-octos-cli-gate",
    "check-matrix",
    "build-linux-x86",
    "e2e-changes",
    "ci-required",
}
# Main (#3008) routes non-PR macOS jobs to the octos-mac pool when enabled;
# PRs stay on macos-15. This task must not alter that expression.
MAC_RUNNER = "${{ github.event_name != 'pull_request' && vars.OCTOS_MAC_POOL == 'on' && fromJSON('[\"self-hosted\",\"macOS\",\"ARM64\",\"octos-mac\"]') || 'macos-15' }}"
WINDOWS_RUNNER = (
    RUNNER.replace("OCTOS_CI_SELF_HOSTED", "OCTOS_CI_WINDOWS")
    .replace('"linux"', '"windows"')
    .replace('"octos-ci"', '"octos-ci-win"')
    .replace("ubuntu-latest", "windows-latest")
)
UNCHANGED = {
    "check-arm": "ubuntu-24.04-arm",
    "check-riscv": "ubuntu-24.04-riscv",
    "check-darwin": MAC_RUNNER,
    "build-macos-arm": MAC_RUNNER,
    "build-linux-arm": "ubuntu-24.04-arm",
    "build-windows": "windows-latest",
    "e2e-tool-regression": MAC_RUNNER,
    "security": MAC_RUNNER,
}


def route(event, *, enabled=True, head_repo="octos-org/octos", ref="refs/heads/topic"):
    eligible = (
        event in {"merge_group", "workflow_dispatch"}
        or (event == "push" and ref == "refs/heads/main")
        or (event == "pull_request" and head_repo == "octos-org/octos")
    )
    return "octos-ci" if enabled and eligible else "ubuntu-latest"


def check_windows_route(job):
    expression = job["runs-on"]
    assert expression == WINDOWS_RUNNER, "check-windows must use the opt-in Windows route"
    # Evaluate the actual route's boolean/JSON subset with concrete event fields.
    cases = [
        ("merge_group", "", "refs/heads/gh-readonly-queue/main/pr-1", True),
        ("workflow_dispatch", "", "refs/heads/topic", True),
        ("push", "", "refs/heads/main", True),
        ("push", "", "refs/heads/topic", False),
        ("pull_request", "octos-org/octos", "refs/pull/1/merge", True),
        ("pull_request", "fork/octos", "refs/pull/1/merge", False),
        ("pull_request", "", "refs/pull/1/merge", False),
        ("pull_request_target", "octos-org/octos", "refs/heads/main", False),
        ("schedule", "", "refs/heads/main", False),
    ]
    for enabled in ("true", "false", ""):
        for event, head_repo, ref, eligible in cases:
            values = {
                "vars.OCTOS_CI_WINDOWS": enabled,
                "github.event_name": event,
                "github.event.pull_request.head.repo.full_name": head_repo,
                "github.repository": "octos-org/octos",
                "github.ref": ref,
            }
            concrete = expression.removeprefix("${{").removesuffix("}}")
            for field, value in values.items():
                concrete = concrete.replace(field, repr(value))
            actual = eval(
                concrete.replace("&&", " and ").replace("||", " or "),
                {"__builtins__": {}, "fromJSON": json.loads},
            )
            expected = (
                ["self-hosted", "windows", "x64", "octos-ci-win"]
                if enabled == "true" and eligible else "windows-latest"
            )
            assert actual == expected, (values, actual, expected)

    # Per-slot target and eight-job budget come from the runner environment.
    for env in [job.get("env", {}), *(step.get("env", {}) for step in job["steps"])]:
        assert "CARGO_TARGET_DIR" not in env and "CARGO_BUILD_JOBS" not in env
    for step in job["steps"]:
        action = step.get("uses", "")
        if action.startswith(("dtolnay/rust-toolchain@", "Swatinem/rust-cache@")):
            assert step["if"] == "runner.environment == 'github-hosted'"
    preinstalled = next(
        step for step in job["steps"]
        if step.get("name") == "Select preinstalled Rust toolchain (self-hosted)"
    )
    assert preinstalled["if"] == "runner.environment == 'self-hosted'"
    assert preinstalled["shell"] == "pwsh"
    assert '"RUSTUP_TOOLCHAIN=$env:RUST_VERSION" >> $env:GITHUB_ENV' in preinstalled["run"]
    assert '"RUSTUP_AUTO_INSTALL=0" >> $env:GITHUB_ENV' in preinstalled["run"]
    assert preinstalled["run"].strip().endswith("rustup run $env:RUST_VERSION cargo --version")
    assert "--install" not in preinstalled["run"]
    print(f"Windows routing matrix ok ({3 * len(cases)} cases)")


def main() -> None:
    workflow = yaml.safe_load((REPO / ".github/workflows/ci.yml").read_text())
    jobs = workflow["jobs"]
    check_windows_route(jobs["check-windows"])
    for name in ROUTED:
        assert jobs[name]["runs-on"] == RUNNER, f"{name} must share the opt-in Linux route"
    for name, runner in UNCHANGED.items():
        assert jobs[name]["runs-on"] == runner, f"{name} runner changed unexpectedly"
    assert "check-arm" in jobs and "test-octos-cli-gate" in jobs
    assert route("pull_request") == "octos-ci"
    assert route("pull_request", head_repo="fork/octos") == "ubuntu-latest"
    assert route("pull_request", enabled=False) == "ubuntu-latest"
    assert route("push") == "ubuntu-latest"
    assert route("push", ref="refs/heads/main") == "octos-ci"
    triggers = workflow[True] if True in workflow else workflow["on"]
    riscv_input = triggers["workflow_dispatch"]["inputs"]["check_riscv"]
    assert riscv_input["type"] == "boolean" and riscv_input["default"] is False
    assert jobs["check-riscv"]["if"] == (
        "github.event_name == 'workflow_dispatch' && inputs.check_riscv"
    )
    # Job-level env cannot reference the `runner` context (evaluated before a
    # runner is assigned — an expression there makes the workflow a startup
    # failure). Hosted values stay literal; a first step raises to 16 via
    # $GITHUB_ENV only on self-hosted runners.
    for name, hosted in {
        "check": "1",
        "test-octos-agent": "1",
        "test-octos-cli": "1",
        "check-matrix": "2",
    }.items():
        assert jobs[name]["env"]["CARGO_BUILD_JOBS"] == hosted
        first = jobs[name]["steps"][0]
        assert first["name"] == "Raise build parallelism on self-hosted runner"
        expected_if = "runner.environment == 'self-hosted'"
        if name in {"test-octos-agent", "test-octos-cli", "check-matrix"}:
            # Every compile/test step in these jobs keeps the dedup guard.
            expected_if += " && needs.scope.outputs.main_dedup != 'true'"
        assert first["if"] == expected_if
        assert "CARGO_BUILD_JOBS=16" in first["run"]
    # check-arm and check-riscv keep their literal caps and NO self-hosted
    # parallelism step (16GB ARM-class runners OOM with 16 jobs).
    for name in ("check-arm", "check-riscv"):
        assert jobs[name]["env"]["CARGO_BUILD_JOBS"] == "2"
        assert "Raise build parallelism" not in str(jobs[name]["steps"])

    workflow_text = (REPO / ".github/workflows/ci.yml").read_text()
    assert workflow_text.count("name: Install Linux sandbox prerequisites") == 3
    assert workflow_text.count("name: Verify Linux sandbox prerequisites (self-hosted)") == 3
    # Every sandbox setup/verify step is split by runner class (other steps in
    # the workflow may use runner.environment for unrelated purposes, so count
    # per-name, not the raw string).
    install_if = [
        step.get("if", "")
        for name in ("check", "check-arm", "check-riscv")
        for step in jobs[name]["steps"]
        if step.get("name") == "Install Linux sandbox prerequisites"
    ]
    assert len(install_if) == 3
    assert all("runner.environment == 'github-hosted'" in cond for cond in install_if)
    verify_if = [
        step.get("if", "")
        for name in ("check", "check-arm", "check-riscv")
        for step in jobs[name]["steps"]
        if step.get("name") == "Verify Linux sandbox prerequisites (self-hosted)"
    ]
    assert len(verify_if) == 3
    assert all("runner.environment == 'self-hosted'" in cond for cond in verify_if)
    sandbox_check = (REPO / "scripts/check-host-managed-linux-ci.sh").read_text()
    assert "sudo" not in sandbox_check
    assert "apparmor_restrict_unprivileged_userns" not in sandbox_check
    assert "command -v bwrap" in sandbox_check

    bundle = (REPO / "scripts/bundle-release.sh").read_text()
    assert '${CARGO_TARGET_DIR:-target}/release' in bundle
    fixture = (REPO / "scripts/test-bundle-release.sh").read_text()
    assert 'export CARGO_TARGET_DIR="$WORK_DIR/custom-target"' in fixture
    manifests = (REPO / "scripts/validate-skill-manifests.sh").read_text()
    assert 'TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"' in manifests
    bindings = (REPO / "scripts/check-oup-bindings.py").read_text()
    assert 'os.environ.get("CARGO_TARGET_DIR"' in bindings
    runtime = (REPO / "scripts/tests/test-oup-runtime.py").read_text()
    assert 'os.environ.get("CARGO_TARGET_DIR"' in runtime
    assert '"${CARGO_TARGET_DIR:-target}/release/octos" serve' in workflow_text
    print(f"self-hosted routing and external-target checks ok ({len(ROUTED)} jobs)")


if __name__ == "__main__":
    main()
