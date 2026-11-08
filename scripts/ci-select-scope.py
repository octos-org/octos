#!/usr/bin/env python3
"""Map changed files to a conservative crate+direct-dependent CI selection."""
from __future__ import annotations

import argparse
import fnmatch
import functools
import json
import re
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


@functools.lru_cache
def attached_test_modules(repo: Path, root: str) -> dict[str, str]:
    """Resolve nested `#[path = ...] mod ...` attachments to test-name paths."""
    source_root = repo / root / "src"
    if not source_root.is_dir():
        return {}
    declaration = re.compile(
        r'(?m)^[ \t]*#\[path\s*=\s*"([^"]+)"\]\s*'
        r'(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;'
    )
    # T-0467: `include!("x_tests.rs")` files create no module of their own —
    # they live in the INCLUDING file's `mod tests`. Without this the selector
    # falls back to the file stem and the changed-module filter selects zero
    # tests (the same trap as T-0309).
    include_re = re.compile(r'include!\s*\(\s*"([^"]+_tests\.rs)"\s*\)')
    attached: dict[Path, tuple[Path, str]] = {}
    for source in source_root.rglob("*.rs"):
        text = source.read_text(errors="replace")
        for match in declaration.finditer(text):
            target = (source.parent / match.group(1)).resolve()
            attached[target] = (source.resolve(), match.group(2))
        for match in include_re.finditer(text):
            target = (source.parent / match.group(1)).resolve()
            if target not in attached:
                attached[target] = (source.resolve(), "tests")

    def default_module(source: Path) -> str:
        relative = source.relative_to(source_root.resolve()).as_posix()[:-3]
        parts = list(PurePosixPath(relative).parts)
        if parts[-1] in ("lib", "main", "mod"):
            parts.pop()
        return "::".join(parts)

    resolved: dict[Path, str] = {}

    def resolve(source: Path) -> str:
        if source in resolved:
            return resolved[source]
        if source not in attached:
            return default_module(source)
        parent, name = attached[source]
        parent_module = resolve(parent)
        # An `include!` into a file that is itself a `mod tests` attachment
        # already resolves to `...::tests`; don't append a second `tests`.
        if name == "tests" and parent_module.split("::")[-1] == "tests":
            value = parent_module
        else:
            value = "::".join(filter(None, (parent_module, name)))
        resolved[source] = value
        return value

    return {str(target): resolve(target) for target in attached}


def changed_test_command(repo: Path, path: str, package: str, root: str) -> list[str] | None:
    """Map a changed Rust test/module to one focused Cargo command.

    `_tests.rs` resolves its real (possibly renamed/nested) `#[path]` module.
    Inline test modules use
    their source path. Integration tests select their exact target. The caller
    lists and executes every emitted command, rejecting an empty selection.
    """
    if not path.endswith(".rs"):
        return None
    relative = path.removeprefix(root + "/")
    command = ["test", "-p", package]
    if package == "octos-cli":
        command += ["--features", "api"]
    if relative.startswith("tests/"):
        target_path = PurePosixPath(relative)
        target = target_path.stem
        if relative.count("/") > 1:
            # Nested helpers are not Cargo targets themselves; run their owning
            # integration target rather than silently dropping the change.
            target = target_path.parts[1]
        if target:
            return command + ["--test", target]
        return None
    if relative.startswith("src/bin/") and relative.count("/") == 2:
        return command + ["--bin", PurePosixPath(relative).stem]
    if not relative.startswith("src/"):
        return None
    module_path = PurePosixPath(relative[4:-3])
    parts = list(module_path.parts)
    if parts[-1] == "lib" or parts[-1] == "main":
        parts.pop()
    elif parts[-1] == "mod":
        parts.pop()
    if not parts:
        source = repo / path
        if not source.is_file():
            return None
        text = source.read_text(errors="replace")
        if not any(marker in text for marker in ("#[cfg(test)]", "#[test]", "#[tokio::test")):
            return None
        filter_name = "tests"
    elif parts[-1].endswith("_tests"):
        filter_name = attached_test_modules(repo, root).get(
            str((repo / path).resolve()), parts[-1]
        )
    else:
        source = repo / path
        if not source.is_file():
            return None
        text = source.read_text(errors="replace")
        if "#[cfg(test)]" not in text and "#[test]" not in text:
            return None
        filter_name = "::".join(parts + ["tests"])
    # Feature-gated bus modules (api_channel is #[cfg(feature = "api")]) must
    # be selected with their feature on, or the filter matches zero tests and
    # the changed-module runner fails the job.
    if package == "octos-bus" and filter_name.split("::")[0] == "api_channel":
        command += ["--features", "api"]
    if package == "octos-research":
        if filter_name == "metasearch::impersonate::tests":
            # This module is cfg(all(feature = "impersonate", not(windows))).
            # No Windows command can select a test; the crate still gets its
            # normal changed-package check without a guaranteed-empty filter.
            return None
        if filter_name == "browser::tests":
            command += ["--features", "browser"]
    return command + ["--lib", filter_name]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--base")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--files", nargs="*", help="explicit files for a dry run instead of git diff")
    parser.add_argument("--deleted-files", nargs="*", default=[], help="deleted/rename-old paths for a dry run")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument(
        "--full-ci",
        action="store_true",
        help="select every package for a merge group/main/full-ci-label override",
    )
    args = parser.parse_args()
    repo = args.repo.resolve()
    deleted = set(args.deleted_files)
    if args.files is not None:
        files = sorted(set(args.files))
    else:
        if not args.base:
            raise SystemExit("--base is required when --files is not used")
        result = subprocess.run(
            ["git", "diff", "--name-status", "-M", f"{args.base}...{args.head}"],
            cwd=repo, text=True, capture_output=True,
        )
        if result.returncode:
            raise SystemExit(f"git diff failed: {result.stderr}")
        changed_files = []
        for line in result.stdout.splitlines():
            fields = line.split("\t")
            status = fields[0]
            if status.startswith(("R", "C")):
                if len(fields) != 3:
                    raise SystemExit(f"cannot parse renamed diff entry: {line}")
                old, new = fields[1:]
                changed_files.extend((old, new))
                if status.startswith("R"):
                    deleted.add(old)
            elif len(fields) == 2:
                changed_files.append(fields[1])
                if status == "D":
                    deleted.add(fields[1])
            else:
                raise SystemExit(f"cannot parse diff entry: {line}")
        files = sorted(set(changed_files))
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
    windows_commands: dict[str, set[tuple[str, ...]]] = {
        "windows_cli_lib_commands": set(),
        "windows_cli_integration_commands": set(),
        "windows_workspace_commands": set(),
    }
    for path in files:
        owner = next(((root, name) for root, name in roots if path == root or path.startswith(root + "/")), None)
        if owner is not None and path not in deleted:
            command = changed_test_command(repo, path, owner[1], owner[0])
            if command is not None:
                if owner[1] == "octos-cli" and "--test" in command:
                    shard = "windows_cli_integration_commands"
                elif owner[1] == "octos-cli":
                    shard = "windows_cli_lib_commands"
                else:
                    shard = "windows_workspace_commands"
                windows_commands[shard].add(tuple(command))
                reasons.append({
                    "path": path,
                    "decision": "windows-test",
                    "reason": "changed Rust test/module gets a focused Windows PR command",
                })
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
        if owner is not None:
            _, package = owner
            # Test/fixture Markdown inside a Cargo package may be an
            # include_str! input. Select its owner and recompile it.
            changed.add(package)
            reasons.append({"path": path, "decision": "select", "reason": f"Cargo metadata owner: {package}"})
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
        "deleted_files": sorted(deleted),
        "changed_packages": sorted(changed),
        "direct_dependents": dependents,
        "selected_packages": selected,
        "mode": mode,
        "docs_only": docs_only,
        "full_ci": full_ci,
        "reasons": reasons,
        "selector_rule": "changed Cargo packages plus all declared direct dependents (normal/build/dev/optional/target superset)",
        **{
            key: [list(command) for command in sorted(commands)]
            for key, commands in windows_commands.items()
        },
    }
    args.out.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    print(json.dumps({key: output[key] for key in ("changed_packages", "direct_dependents", "selected_packages", "mode", "docs_only", "full_ci")}, sort_keys=True))


if __name__ == "__main__":
    main()
