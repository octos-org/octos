#!/usr/bin/env python3
"""Generate or drift-check a workspace/dependency/test-target inventory.

Uses only Cargo metadata and source reads. No test or CI job is executed.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

HEAVY = frozenset({"octos-agent", "octos-cli"})
PROPOSED_2935_FEATURES = {
    "octos-diagnostics": "github",
    "octos-embed-llama": "embed-llama",
    "octos-pyo3": "python",
}


def run(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, text=True, capture_output=True)
    if result.returncode:
        sys.stderr.write(result.stdout + result.stderr)
        raise SystemExit(f"command failed ({result.returncode}): {' '.join(command)}")
    return result.stdout


def load_metadata(repo: Path, metadata: Path | None) -> tuple[dict, str]:
    if metadata:
        return json.loads(metadata.read_text()), f"metadata file: {metadata}"
    command = ["cargo", "metadata", "--format-version", "1", "--locked"]
    return json.loads(run(command, repo)), " ".join(command)


def relative(path: str, repo: Path) -> str:
    try:
        return str(Path(path).relative_to(repo))
    except ValueError:
        return path


def source_references(path: Path, crate_map: dict[str, str]) -> list[str]:
    if not path.is_file() or not crate_map:
        return []
    text = path.read_text(errors="replace")
    pattern = re.compile(r"(?<![A-Za-z0-9_])(" + "|".join(map(re.escape, sorted(crate_map))) + r")\s*::")
    return sorted({crate_map[match.group(1)] for match in pattern.finditer(text)})


def build(repo: Path, data: dict, command: str) -> dict:
    members = set(data["workspace_members"])
    packages = sorted((p for p in data["packages"] if p["id"] in members), key=lambda p: p["name"])
    if len(packages) != 40 or not packages:
        raise SystemExit(f"unexpected workspace package count: {len(packages)}")
    names = frozenset(p["name"] for p in packages)
    id_name = {p["id"]: p["name"] for p in packages}
    declared_edges = []
    package_rows = []
    integration_targets = []
    for package in packages:
        name = package["name"]
        for dep in package["dependencies"]:
            if dep["name"] in names:
                declared_edges.append({
                    "dependent": name,
                    "dependency": dep["name"],
                    "kind": dep["kind"] or "normal",
                    "optional": dep["optional"],
                    "target": dep["target"],
                    "rename": dep["rename"],
                })
        test_targets = []
        for target in package["targets"]:
            src = relative(target["src_path"], repo)
            row = {
                "name": target["name"],
                "kind": target["kind"],
                "src_path": src,
                "test": target["test"],
                "doctest": target["doctest"],
            }
            if target["test"] or "test" in target["kind"]:
                test_targets.append(row)
            if "test" in target["kind"]:
                crate_map = {
                    crate.replace("-", "_"): crate for crate in sorted(names - {name})
                }
                integration_targets.append({
                    "package": name,
                    **row,
                    "workspace_source_references": source_references(repo / src, crate_map),
                })
        package_rows.append({
            "name": name,
            "class": "dedicated-heavy" if name in HEAVY else "light",
            "proposed_2935_features": PROPOSED_2935_FEATURES.get(name),
            "manifest_path": relative(package["manifest_path"], repo),
            "test_targets": test_targets,
        })

    resolved_edges = []
    for node in data["resolve"]["nodes"]:
        if node["id"] not in members:
            continue
        for dep in node["deps"]:
            if dep["pkg"] in members:
                resolved_edges.append({
                    "dependent": id_name[node["id"]],
                    "dependency": id_name[dep["pkg"]],
                    "dep_kinds": [
                        {"kind": kind["kind"] or "normal", "target": kind["target"]}
                        for kind in dep["dep_kinds"]
                    ],
                })
    declared_edges.sort(key=lambda e: (e["dependency"], e["dependent"], e["kind"], str(e["target"])))
    resolved_edges.sort(key=lambda e: (e["dependency"], e["dependent"]))
    integration_targets.sort(key=lambda t: (t["package"], t["name"]))

    def dependents(edges: list[dict]) -> dict:
        return {
            name: sorted({edge["dependent"] for edge in edges if edge["dependency"] == name})
            for name in sorted(names)
        }

    return {
        "schema": "octos-ci-workspace-inventory-v1",
        "head": run(["git", "rev-parse", "HEAD"], repo).strip(),
        "metadata_command": command,
        "counts": {
            "workspace": len(packages),
            "light": sum(p["class"] == "light" for p in package_rows),
            "dedicated_heavy": sum(p["class"] == "dedicated-heavy" for p in package_rows),
            "integration_test_targets": len(integration_targets),
            "declared_internal_edges": len(declared_edges),
            "resolved_internal_edges": len(resolved_edges),
        },
        "packages": package_rows,
        "declared_edges": declared_edges,
        "resolved_edges": resolved_edges,
        "declared_direct_dependents": dependents(declared_edges),
        "resolved_direct_dependents": dependents(resolved_edges),
        "integration_test_targets": integration_targets,
        "notes": [
            "Declared edges include optional, target-gated, normal, build, and dev dependencies for a conservative selector superset.",
            "Resolved edges describe this Cargo metadata feature resolution only; Cargo.lock/toolchain changes require full-ci.",
            "The light/heavy and three feature entries mirror PR #2935 separately; they are not claimed as main CI entries.",
        ],
    }


def projection(inventory: dict) -> dict:
    keys = [
        "counts", "packages", "declared_edges", "resolved_edges",
        "declared_direct_dependents", "resolved_direct_dependents", "integration_test_targets",
    ]
    return {key: inventory[key] for key in keys}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--metadata", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--check", type=Path, help="existing inventory JSON to compare")
    args = parser.parse_args()
    repo = args.repo.resolve()
    data, command = load_metadata(repo, args.metadata)
    inventory = build(repo, data, command)
    if args.check:
        baseline = json.loads(args.check.read_text())
        if projection(baseline) != projection(inventory):
            raise SystemExit("workspace/dependency/test-target drift detected")
        print(f"no workspace drift: {inventory['counts']}")
        return
    if not args.out:
        raise SystemExit("--out is required unless --check is used")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n")
    print(json.dumps(inventory["counts"], sort_keys=True))


if __name__ == "__main__":
    main()
