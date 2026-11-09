#!/usr/bin/env bash
# Glob integration runner: `cargo test -p <crate> --test '*'` fails with
# "no test target in package" once a deletion PR removes a crate's last
# integration target. Centralize the skip so per-crate deletion PRs need
# no workflow edit. A zero-target glob is a skip (the crate's lib suite
# still runs in its own step); a non-empty glob always runs, and any real
# failure still fails the step.
#
# Usage: scripts/cargo-test-glob.sh <package> [extra cargo-test args...]
set -euo pipefail

package="${1:?package name required}"
shift

count="$(
  cargo metadata --no-deps --format-version=1 |
    python3 -c '
import json
import sys

meta = json.load(sys.stdin)
package = sys.argv[1]
print(sum(
    1
    for pkg in meta["packages"]
    if pkg["name"] == package
    for target in pkg["targets"]
    if "test" in target["kind"]
))
' "$package"
)"

if [ "$count" = "0" ]; then
  echo "$package: no integration test targets at this commit; skipping glob run"
  exit 0
fi

exec cargo test -p "$package" --test '*' "$@"
