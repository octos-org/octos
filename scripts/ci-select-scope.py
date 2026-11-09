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
    # T-0467 + T-0225: `include!()` files create no module of their own. A
    # `_tests.rs` include lands in the INCLUDING file's `mod tests`; any other
    # `.rs` include (e.g. ui_protocol_ledger_recovery.rs into
    # ui_protocol_ledger.rs) lands in the including module itself. Without this
    # the selector falls back to the file stem and selects zero tests.
    include_re = re.compile(r'include!\s*\(\s*"([^"]+\.rs)"\s*\)')
    attached: dict[Path, tuple[Path, str]] = {}
    for source in source_root.rglob("*.rs"):
        text = source.read_text(errors="replace")
        for match in declaration.finditer(text):
            target = (source.parent / match.group(1)).resolve()
            attached[target] = (source.resolve(), match.group(2))
        for match in include_re.finditer(text):
            target = (source.parent / match.group(1)).resolve()
            if target not in attached:
                name = "tests" if match.group(1).endswith("_tests.rs") else ""
                attached[target] = (source.resolve(), name)

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


def _head_file_has_tests(source: Path) -> bool:
    """Whether a changed Rust source still exists and contains tests at HEAD.

    Deletion PRs remove test files or empty their modules without the
    selector knowing (its input is the *diff*). Emitting a command for a
    module that no longer exists guarantees a zero-test selection on the
    changed-module runner, which fails closed. Judge by the checked-out
    head tree instead.
    """
    if not source.is_file():
        return False
    text = source.read_text(errors="replace")
    return any(
        marker in text
        for marker in ("#[cfg(test)]", "#[test]", "#[tokio::test", "#[rstest]", "#[test_case")
    )


def _head_module_has_real_tests(repo: Path, root: str, filter_name: str) -> bool:
    """Whether the head tree has a runnable test covered by `filter_name`.

    `#[cfg(test)]` helpers and empty `mod tests` blocks are not enough: after a
    deletion, emitting their old filter makes Windows fail on zero tests.
    """
    source_root = repo / root / "src"
    if not source_root.is_dir():
        return False
    attached = attached_test_modules(repo, root)
    windows_excluded = _windows_excluded_attached_tests(repo, root)
    test_attribute = re.compile(r"#\[(?:tokio::test|test|rstest|test_case|async_std::test|sqlx::test)\b")
    for source in source_root.rglob("*.rs"):
        if str(source.resolve()) in windows_excluded:
            continue
        if not test_attribute.search(source.read_text(errors="replace")):
            continue
        resolved = attached.get(str(source.resolve()))
        if resolved is None:
            relative = source.relative_to(source_root.resolve()).as_posix()[:-3]
            parts = list(PurePosixPath(relative).parts)
            if parts[-1] in ("lib", "main", "mod"):
                parts.pop()
            if parts[-1].endswith("_tests"):
                resolved = parts[-1]
            else:
                if parts[-1] != "tests":
                    parts.append("tests")
                resolved = "::".join(parts)
        if resolved == filter_name or resolved.startswith(filter_name + "::"):
            return True
    return False


def _cfg_excludes_windows(expression: str) -> bool:
    """Conservative evaluator for the platform cfgs used by test targets."""
    compact = "".join(expression.split())
    if compact in ("unix", "not(windows)", 'not(target_os="windows")'):
        return True
    if not (compact.startswith("all(") and compact.endswith(")")):
        return False
    inner = compact[4:-1]
    parts: list[str] = []
    depth = 0
    start = 0
    for index, char in enumerate(inner):
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(inner[start:index])
            start = index + 1
    parts.append(inner[start:])
    return any(_cfg_excludes_windows(part) for part in parts)


@functools.lru_cache
def _windows_excluded_attached_tests(repo: Path, root: str) -> set[str]:
    """Resolve `#[cfg(...)] #[path = ...] mod ...` exclusions to source files."""
    source_root = repo / root / "src"
    if not source_root.is_dir():
        return set()
    declaration = re.compile(
        r'(?m)^[ \t]*#\[cfg\(([^\n]+)\)\]\s*'
        r'#\[path\s*=\s*"([^"]+)"\]\s*'
        r'(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;'
    )
    excluded: set[str] = set()
    for parent in source_root.rglob("*.rs"):
        for match in declaration.finditer(parent.read_text(errors="replace")):
            if _cfg_excludes_windows(match.group(1)):
                excluded.add(str((parent.parent / match.group(2)).resolve()))
    return excluded


def _windows_empty_integration_target(source: Path) -> bool:
    """Recognize an integration target with no Windows-runnable tests."""
    text = source.read_text(errors="replace")
    crate_cfg = re.search(r'(?m)^#!\[cfg\((.*)\)\]\s*$', text)
    if crate_cfg and _cfg_excludes_windows(crate_cfg.group(1)):
        return True
    gated_module = re.search(
        r'(?m)^#\[cfg\((?:unix|not\(windows\)|not\(target_os = "windows"\))\)\]\s*'
        r'(?:#\[[^\n]+\]\s*)*mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{',
        text,
    )
    if gated_module is None:
        return False
    prefix = text[: gated_module.start()]
    if "#[test]" in prefix or "#[tokio::test" in prefix:
        return False
    # Do not let a leading Unix module hide later non-gated tests or modules.
    if re.search(r'(?m)^#\[(?:tokio::test|test|rstest|test_case)\b', text):
        return False
    return len(re.findall(r'(?m)^mod\s+[A-Za-z_][A-Za-z0-9_]*\b', text)) == 1


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
    elif package == "octos-bus":
        # Changed api_channel/matrix_user_channel modules are behind features;
        # without these flags Windows selects zero tests and rejects the run.
        command += ["--features", "api,matrix"]
    if relative.startswith("tests/"):
        target_path = PurePosixPath(relative)
        target = target_path.stem
        if relative.count("/") > 1:
            # Nested helpers are not Cargo targets themselves; run their owning
            # integration target rather than silently dropping the change.
            target = target_path.parts[1]
            if not (repo / root / "tests" / f"{target}.rs").is_file():
                # The owning target was deleted; only helpers survive in the
                # diff. There is nothing selectable left.
                return None
        elif not (repo / path).is_file():
            # The integration target itself was deleted at HEAD.
            return None
        if target:
            target_source = repo / root / "tests" / f"{target}.rs"
            if _windows_empty_integration_target(target_source):
                return None
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
        if not _head_file_has_tests(repo / path):
            return None
        filter_name = "tests"
    elif parts[-1].endswith("_tests"):
        if not _head_file_has_tests(repo / path):
            return None
        filter_name = attached_test_modules(repo, root).get(
            str((repo / path).resolve()), parts[-1]
        )
    else:
        source = repo / path
        if not _head_file_has_tests(source):
            return None
        # T-0225: an include!()-ed SOURCE file has no module of its own; its
        # tests live under the INCLUDING module (often a non-"tests" submodule
        # like recovery_write_tests). Emit the including module — a substring
        # filter covering every test inside it, whatever the submodule is named.
        resolved = attached_test_modules(repo, root).get(str(source.resolve()))
        text = source.read_text(errors="replace")
        has_real_tests = bool(
            re.search(r"#\[(tokio::)?test\]", text)
            or re.search(r"mod\s+(\w*_)?tests\b", text)
            or re.search(r"include!\s*\(", text)
        )
        if resolved is not None and has_real_tests:
            filter_name = resolved
        elif resolved is not None:
            # Attached source file WITHOUT runnable tests (helpers only, e.g.
            # retained_references): emitting its own module selects zero tests.
            # Fail closed to the parent's tests filter (alan0x).
            filter_name = resolved.rsplit("::", 1)[0] + "::tests" if "::" in resolved else "tests"
        elif not has_real_tests:
            # Unattached helper-only source file: fail closed to the parent's
            # tests filter too.
            filter_name = "::".join(parts[:-1] + ["tests"]) if len(parts) > 1 else "tests"
        elif parts[-1] == "tests":
            # A `tests.rs` / `tests/mod.rs` file IS the `mod tests` itself
            # (declared under `#[cfg(test)]` in the parent); appending another
            # `::tests` would select zero.
            filter_name = "::".join(parts)
        else:
            filter_name = "::".join(parts + ["tests"])
    if not _head_module_has_real_tests(repo, root, filter_name):
        return None
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
