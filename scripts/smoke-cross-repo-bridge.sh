#!/usr/bin/env bash
# Cross-repo smoke test: octos installs the agibot-a2 skill from the
# octos-robot-skills bridge, runs the hardware_lifecycle phases (preflight,
# init via `dora start ...`, ready_check), discovers tools over HTTP, and
# fires one round-trip tool call. Not run in CI — requires a real dora
# coordinator + the private vendor adapter installed locally.
#
# Pre-conditions:
#   - dora CLI on PATH (`brew install dora-rs` or build from source)
#   - octos-robot-skills cloned somewhere; set $BRIDGE_REPO to its path
#   - agibot-a2-dora-node pip-installed in the bridge's venv
#   - this branch (feat/http-tool-transport) installed as the `octos` binary
#
# Usage:
#   BRIDGE_REPO=/path/to/octos-robot-skills scripts/smoke-cross-repo-bridge.sh

set -euo pipefail

BRIDGE_REPO="${BRIDGE_REPO:?BRIDGE_REPO must point to your local octos-robot-skills clone}"
OCTOS_BIN="${OCTOS_BIN:-$HOME/.cargo/bin/octos}"
BRIDGE_HEALTH_URL="${BRIDGE_HEALTH_URL:-http://127.0.0.1:8765/healthz}"

# `octos skills` resolves the skills dir from --cwd (or the configured
# profile). We use --cwd "$HOME" so the install lands in the canonical
# "$HOME/.octos/skills" location — the same place `octos chat` reads
# from with no extra config. The PR #1260 reviewer asked us not to
# introduce a separate --skills-dir flag for this smoke; this matches.
INSTALL_CWD="$HOME"
SKILL_NAME="agibot-a2"

echo "[1/6] verify prerequisites"
test -f "$BRIDGE_REPO/skills/$SKILL_NAME/SKILL.md" || {
  echo "  bridge repo not found at $BRIDGE_REPO; set BRIDGE_REPO" >&2
  exit 1
}
command -v dora >/dev/null || { echo "  dora CLI not on PATH" >&2; exit 1; }
test -x "$OCTOS_BIN" || {
  echo "  octos binary not at $OCTOS_BIN; build with:" >&2
  echo "    cargo install --path crates/octos-cli --force" >&2
  exit 1
}

echo "[2/6] cargo install local octos with this branch's changes"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
( cd "$REPO_ROOT" && ~/.cargo/bin/cargo install --path crates/octos-cli --force )

echo "[3/6] stage skill + dataflows into a temp source dir"
STAGING="$(mktemp -d -t octos-smoke-XXXXXX)"
trap 'rm -rf "$STAGING"' EXIT
cp -r "$BRIDGE_REPO/skills/$SKILL_NAME" "$STAGING/$SKILL_NAME"
# The lifecycle's `init` step (`dora start dataflows/a2-bridge.yaml`)
# resolves dataflow paths relative to the installed skill directory,
# so they must be inside the install source before `octos skills install`
# fires the lifecycle.
mkdir -p "$STAGING/$SKILL_NAME/dataflows"
cp "$BRIDGE_REPO/dataflows/a2-bridge.yaml" "$STAGING/$SKILL_NAME/dataflows/a2-bridge.yaml"
cp "$BRIDGE_REPO/dataflows/venv-python" "$STAGING/$SKILL_NAME/dataflows/venv-python" 2>/dev/null || true
chmod +x "$STAGING/$SKILL_NAME/dataflows/venv-python" 2>/dev/null || true

echo "[4/6] install skill (runs preflight → init → ready_check via lifecycle)"
"$OCTOS_BIN" skills --cwd "$INSTALL_CWD" install "$STAGING/$SKILL_NAME" --force || {
  echo "  skill install failed" >&2
  exit 1
}

echo "[5/6] confirm bridge HTTP is up"
for i in {1..15}; do
  if curl -fsS -m 2 "$BRIDGE_HEALTH_URL" >/dev/null 2>&1; then
    echo "  bridge healthy after ${i}s"
    break
  fi
  if [[ $i -eq 15 ]]; then
    echo "  bridge never came up at $BRIDGE_HEALTH_URL" >&2
    exit 1
  fi
  sleep 1
done

echo "[6/6] fire robot.heartbeat through octos chat"
RESPONSE=$("$OCTOS_BIN" chat --no-interactive --prompt 'call robot.heartbeat once and report the JSON ok flag' 2>&1 || true)
echo "$RESPONSE" | grep -qE '"ok"\s*:\s*true' || {
  echo "  octos did not return ok=true; full response below:" >&2
  echo "$RESPONSE" >&2
  exit 1
}

echo
echo "smoke OK"
echo
echo "cleanup: octos skills --cwd $INSTALL_CWD remove $SKILL_NAME"
"$OCTOS_BIN" skills --cwd "$INSTALL_CWD" remove "$SKILL_NAME" || true
