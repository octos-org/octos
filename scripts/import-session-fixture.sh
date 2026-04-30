#!/usr/bin/env bash
# Import a production session JSONL as a regression fixture for the
# `jsonl_replay_thread_binding` harness.
#
# Usage:
#   scripts/import-session-fixture.sh <mini-host> <remote-jsonl-path> <local-fixture-name>
#
# Example:
#   scripts/import-session-fixture.sh mini3 \
#     /var/lib/octos/sessions/web-1777402538752.jsonl \
#     fixed-three-user-overflow.jsonl
#
# The fixture lands in:
#   crates/octos-bus/tests/fixtures/jsonl/<local-fixture-name>
#
# After import, scrub PII from `content` fields if needed, and add a
# corresponding `#[test]` to crates/octos-bus/tests/jsonl_replay_thread_binding.rs
# wiring the fixture into the regression suite.

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <mini-host> <remote-jsonl-path> <local-fixture-name>" >&2
  exit 2
fi

REMOTE_HOST="$1"
REMOTE_PATH="$2"
LOCAL_NAME="$3"

# Resolve the fixtures directory relative to the repo root, regardless of
# where the script is invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/crates/octos-bus/tests/fixtures/jsonl"

if [[ ! -d "${FIXTURES_DIR}" ]]; then
  echo "fixtures dir not found: ${FIXTURES_DIR}" >&2
  exit 1
fi

# Reject path traversal and odd filenames; we only allow plain
# alphanumeric / dash / underscore / dot fixture names ending in .jsonl.
if [[ ! "${LOCAL_NAME}" =~ ^[A-Za-z0-9._-]+\.jsonl$ ]]; then
  echo "local-fixture-name must match [A-Za-z0-9._-]+\\.jsonl, got: ${LOCAL_NAME}" >&2
  exit 1
fi

DEST="${FIXTURES_DIR}/${LOCAL_NAME}"

echo "Importing ${REMOTE_HOST}:${REMOTE_PATH}"
echo "       -> ${DEST}"

# scp -p preserves modification times so we have a record of when the
# session was captured on the production host.
scp -p "${REMOTE_HOST}:${REMOTE_PATH}" "${DEST}"

# Sanity-check: each line should parse as JSON and have a `role` field.
python3 - "${DEST}" <<'PY'
import json, sys
path = sys.argv[1]
n = 0
with open(path, "r", encoding="utf-8") as f:
    for i, line in enumerate(f, 1):
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError as e:
            sys.exit(f"line {i}: invalid JSON: {e}")
        if "role" not in r:
            sys.exit(f"line {i}: missing 'role'")
        n += 1
print(f"ok: {n} records parsed")
PY

cat <<EOF

Imported. Next steps:
  1. Scrub PII from \`content\` fields if needed (the harness only reads
     role, thread_id, client_message_id, response_to_client_message_id).
  2. Add a #[test] in crates/octos-bus/tests/jsonl_replay_thread_binding.rs
     that calls check_jsonl on this fixture and asserts the expectation.
  3. Run: cargo test -p octos-bus --test jsonl_replay_thread_binding
EOF
