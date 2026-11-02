#!/usr/bin/env python3
"""Map changed files to a conservative crate+direct-dependent CI selection."""
from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
from pathlib import Path, PurePosixPath

ALWAYS_FULL = (
    "Cargo.lock", "Cargo.toml", "rust-toolchain", "rust-toolchain.toml",
    ".github/*", "scripts/*", "e2e/*", "api/*", "*/build.rs",
    "model_catalog.json",
)
CROSS_CRATE_FULL = (
    "crates/octos-core/src/ui_protocol*",
    "crates/octos-cli/src/peers/*",
    "crates/octos-cli/src/api/ui_protocol*",
    "crates/octos-cli/src/api/host_managed*",
    "crates/octos-cli/src/api/session*",
    "crates/octos-cli/tests/serve_*",
    "crates/octos-cli/src/session_actor*",
    "crates/octos-cli/src/process*",
    "crates/octos-cli/src/commands/serve*",
    "crates/octos-agent/src/sandbox/*",
    "crates/octos-agent/src/tools/*",
    "crates/octos-agent/src/exec_env*",
    "crates/octos-agent/src/mcp*",
    "crates/octos-bus/src/session*",
    "crates/octos-plugin/src/lifecycle*",
    "crates/octos-ffi/*", "crates/octos-pyo3/*", "crates/octos-uniffi/*",
    "crates/octos-wasm/*", "crates/octos-sandbox/*",
    # Default-feature package tests do not even compile these optional
    # channel/ABI/feature surfaces. Keep their targeted matrix in full-ci.
    "crates/octos-agent/*", "crates/octos-bus/*", "crates/octos-diagnostics/*", "crates/octos-embed-llama/*",
    "crates/octos-cli/src/commands/gateway/*", "crates/octos-cli/src/commands/channel*",
    "crates/octos-cli/src/commands/acp*", "crates/octos-cli/src/commands/oup*",
    "crates/octos-cli/src/runtime/*", "crates/octos-cli/src/oup*", "crates/octos-cli/tests/oup*",
    "crates/app-skills/*/SKILL.md", "crates/platform-skills/*/SKILL.md",
    "*matrix*",
)
PLATFORM_MARKERS = ("windows", "macos", "darwin", "linux", "process", "shell", "spawn", "signal", "sandbox")


def matches(path: str, patterns: tuple[str, ...]) -> bool:
    return any(fnmatch.fnmatch(path, pattern) for pattern in patterns)


def is_doc(path: str) -> bool:
    return path.endswith((".md", ".mdx")) or path.startswith(("docs/", "book/", "book-zh/"))


def is_runtime_markdown(path: str) -> bool:
    return PurePosixPath(path).name == "SKILL.md" or any(
        marker in path for marker in ("/tests/", "/fixtures/", "/resource-skills/", "/resources/", "/assets/", "/templates/")
    )


def has_platform_cfg(repo: Path, path: str) -> bool:
    if not path.endswith(".rs"):
        return False
    source = repo / path
    if not source.is_file():
        return False
    text = source.read_text(errors="replace")
    return "cfg(" in text and any(marker in text for marker in ("windows", "target_os", "target_arch", "unix"))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--base")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--files", nargs="*", help="explicit files for a dry run instead of git diff")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument(
        "--full-ci",
        action="store_true",
        help="select every package for a merge group/main/full-ci-label override",
    )
    args = parser.parse_args()
    repo = args.repo.resolve()
    if args.files is not None:
        files = sorted(set(args.files))
    else:
        if not args.base:
            raise SystemExit("--base is required when --files is not used")
        result = subprocess.run(
            ["git", "diff", "--name-only", f"{args.base}...{args.head}"],
            cwd=repo, text=True, capture_output=True,
        )
        if result.returncode:
            raise SystemExit(f"git diff failed: {result.stderr}")
        files = sorted(set(result.stdout.splitlines()))
    if not files:
        raise SystemExit("empty changed-file list; refusing empty CI evidence")
    inventory = json.loads(args.inventory.read_text())
    packages = inventory["packages"]
    roots = sorted(
        ((str(PurePosixPath(p["manifest_path"]).parent), p["name"]) for p in packages),
        key=lambda item: len(item[0]), reverse=True,
    )
    direct = inventory["declared_direct_dependents"]
    changed = set()
    reasons = [
        {"path": "--full-ci", "decision": "full-ci", "reason": "event or label requires the complete matrix"}
    ] if args.full_ci else []
    full_ci = args.full_ci
    for path in files:
        if any(marker in PurePosixPath(path).name.lower() for marker in PLATFORM_MARKERS):
            full_ci = True
            reasons.append({"path": path, "decision": "full-ci", "reason": "process/shell/sandbox/platform marker"})
            continue
        if matches(path, ALWAYS_FULL):
            full_ci = True
            reasons.append({"path": path, "decision": "full-ci", "reason": "lock/toolchain/CI or reviewed repository-wide path"})
            continue
        if is_doc(path) and not is_runtime_markdown(path):
            reasons.append({"path": path, "decision": "docs", "reason": "documentation-only path"})
            continue
        if matches(path, CROSS_CRATE_FULL):
            full_ci = True
            reasons.append({"path": path, "decision": "full-ci", "reason": "reviewed cross-crate/feature/runtime path"})
            continue
        if has_platform_cfg(repo, path):
            full_ci = True
            reasons.append({"path": path, "decision": "full-ci", "reason": "source contains OS/architecture cfg; retain platform matrix"})
            continue
        owner = next((name for root, name in roots if path == root or path.startswith(root + "/")), None)
        if owner is not None:
            # Test/fixture Markdown inside a Cargo package may be an
            # include_str! input. Select its owner and recompile it.
            changed.add(owner)
            reasons.append({"path": path, "decision": "select", "reason": f"Cargo metadata owner: {owner}"})
            continue
        if path.startswith(("dashboard/", "swarm-app/")):
            reasons.append({"path": path, "decision": "web", "reason": "explicit web-component path; no Cargo owner"})
            continue
        # Unmapped build/source configuration may affect the whole workspace.
        # Never turn a newly added root .toml/.lock/.rs into a broken selector:
        # fail closed to the complete matrix just like other unknown assets.
        # New root assets may be compiled runtime inputs (model_catalog.json
        # was the concrete example). Unknown non-documentation fails closed.
        full_ci = True
        reasons.append({"path": path, "decision": "full-ci", "reason": "unmapped non-documentation asset; fail closed"})
    dependents = sorted({dependent for name in changed for dependent in direct[name]})
    selected = sorted(changed | set(dependents))
    docs_only = not changed and not full_ci and all(item["decision"] == "docs" for item in reasons)
    if full_ci:
        selected = sorted(package["name"] for package in packages)
    mode = "full-ci" if full_ci else "docs" if docs_only else "selective"
    output = {
        "schema": "octos-ci-selection-v1",
        "inventory_head": inventory["head"],
        "files": files,
        "changed_packages": sorted(changed),
        "direct_dependents": dependents,
        "selected_packages": selected,
        "mode": mode,
        "docs_only": docs_only,
        "full_ci": full_ci,
        "reasons": reasons,
        "selector_rule": "changed Cargo packages plus all declared direct dependents (normal/build/dev/optional/target superset)",
    }
    args.out.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    print(json.dumps({key: output[key] for key in ("changed_packages", "direct_dependents", "selected_packages", "mode", "docs_only", "full_ci")}, sort_keys=True))


if __name__ == "__main__":
    main()
