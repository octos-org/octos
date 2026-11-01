#!/usr/bin/env python3
"""Check and execute the complete non-heavy workspace test shard.

The two heavy crates have dedicated CI jobs. Every other workspace member is
derived from Cargo.toml at run time so adding a crate cannot silently omit its
tests from this shard.
"""

from __future__ import annotations

import argparse
import subprocess
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HEAVY_CRATES = frozenset({"octos-agent", "octos-cli"})
# Include feature-gated test definitions in the ordinary x86 lane. Live-model
# and network tests remain protected by their existing #[ignore] attributes.
FEATURE_CRATES = {
    "octos-diagnostics": "github",
    "octos-embed-llama": "embed-llama",
    "octos-pyo3": "python",
}


def workspace_packages() -> frozenset[str]:
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    packages = []
    for member in workspace["workspace"]["members"]:
        manifest = ROOT / member / "Cargo.toml"
        packages.append(tomllib.loads(manifest.read_text())["package"]["name"])
    if len(packages) != len(set(packages)):
        raise RuntimeError("duplicate package names in workspace member list")
    return frozenset(packages)


def light_packages() -> tuple[frozenset[str], frozenset[str]]:
    packages = workspace_packages()
    missing_heavy = HEAVY_CRATES - packages
    unknown_features = FEATURE_CRATES.keys() - packages
    if missing_heavy or unknown_features:
        raise RuntimeError(
            "workspace/test shard registry drift: "
            f"missing_heavy={sorted(missing_heavy)}, "
            f"unknown_feature_crates={sorted(unknown_features)}"
        )
    return packages - HEAVY_CRATES, packages


def test_command(package: str) -> list[str]:
    command = ["cargo", "test", "-p", package, "--locked"]
    if feature := FEATURE_CRATES.get(package):
        command.extend(["--features", feature])
    return command


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate and print complete crate coverage without running tests",
    )
    args = parser.parse_args()
    light, all_packages = light_packages()
    print(
        f"workspace crate coverage: {len(all_packages)} total = "
        f"{len(light)} light + {len(HEAVY_CRATES)} dedicated heavy"
    )
    for package in sorted(light):
        print(" ".join(test_command(package)))
    if args.check:
        return

    completed = set()
    for package in sorted(light):
        command = test_command(package)
        print(f"\n==> {' '.join(command)}", flush=True)
        subprocess.run(command, cwd=ROOT, check=True)
        completed.add(package)
    if completed != light:
        raise RuntimeError(
            f"workspace test shard omitted: {sorted(light - completed)}"
        )
    print(f"\nworkspace light shard completed: {len(completed)}/{len(light)} crates")


if __name__ == "__main__":
    main()
