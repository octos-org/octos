#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FEATURES="${FEATURES:-api,telegram,discord,dingtalk,whatsapp,feishu,twilio,wecom,wecom-bot,audio_mp3}"
SKILL_CRATES="${SKILL_CRATES:--p news_fetch -p deep-search -p deep-crawl -p send-email -p account-manager -p voice -p clock -p weather -p smart-home -p skill-evolve}"

usage() {
  cat <<'EOF'
Usage: ./scripts/milestone-ci.sh <suite>

Canonical milestone CI suites:
  dashboard               dashboard install/typecheck/build + embedded asset freshness
  swarm-app               swarm-app install/typecheck/build/test + embedded asset freshness
  hosted-fast             fmt + clippy + workspace test + milestone regressions
  workspace-all-features  workspace/all-features build + test compilation + tests
  release-bundle          release binary + skill crate build

These suites are the single source of truth for milestone deliverable validation.
GitHub workflows and self-hosted validation should call this script instead of
repeating ad hoc command lists.
EOF
}

run_dashboard() {
  pushd dashboard >/dev/null
  npm ci
  npm run typecheck
  npm run build
  popd >/dev/null

  # Ephemeral bundle policy: the compiled dashboard is gitignored and rebuilt
  # on demand. We just verify the canonical script runs cleanly — there is
  # no committed bundle to diff against. See .gitignore for the rationale.
  ./scripts/build-dashboard.sh

  # providers.json is DERIVED from the canonical model_catalog.json via this
  # append-only generator. It exits nonzero on an unsupported parity case (a new
  # catalog family with no env mapping, a lingering web-only model, or a
  # read/write error); propagate that. A clean run that merely appended catalog
  # models exits zero, so the git-status diff below catches ordinary drift.
  if ! python3 scripts/sync-dashboard-providers.py; then
    echo "sync-dashboard-providers.py reported a parity problem (see the WARNING lines above) — resolve it before merging."
    exit 1
  fi
  if [ -n "$(git status --porcelain -- dashboard/src/providers.json)" ]; then
    echo "dashboard/src/providers.json is out of date. Run scripts/sync-dashboard-providers.py and commit the result."
    git status --short -- dashboard/src/providers.json
    exit 1
  fi
}

run_swarm_app() {
  pushd swarm-app >/dev/null
  npm ci
  npm run typecheck
  npm run build
  npx vitest run
  popd >/dev/null

  ./scripts/build-swarm-app.sh
  if [ -n "$(git status --porcelain -- crates/octos-cli/static/swarm)" ]; then
    echo "Embedded swarm-app assets are out of date. Run ./scripts/build-swarm-app.sh and commit changes."
    git status --short -- crates/octos-cli/static/swarm
    exit 1
  fi
}

run_hosted_fast() {
  python3 scripts/lint-tool-descriptions.py --self-test
  python3 scripts/lint-tool-descriptions.py

  cargo fmt --all -- --check
  cargo clippy --workspace -- -D warnings
  cargo test --workspace

  cargo test -p octos-llm test_qos_ranking_changes_lane_selection -- --nocapture
  cargo test -p octos-llm test_derive_cold_start_catalog_assigns_non_zero_scores -- --nocapture
  cargo test -p octos-llm test_compatible_fallbacks_prefers_lower_seeded_qos_score -- --nocapture
  cargo test -p octos-cli gateway_runtime::tests --features api -- --nocapture
  # #1477: the `api` module (incl. voice_turn rich-output marker/splitter/delta
  # helpers) is feature-gated, so `cargo test --workspace` above never compiles
  # it. Run the api-gated unit tests explicitly so this coverage is real.
  cargo test -p octos-cli --features api voice_turn -- --nocapture
  # §6 catalog guard is in the same feature-gated `api` module; run it
  # explicitly so UI Protocol spec/impl drift is caught here too.
  cargo test -p octos-cli --features api spec_section6_catalog_lists_every_advertised_method -- --nocapture
  cargo test -p octos-agent --test activate_tools_regression -- --nocapture
  cargo test -p octos-bus --test file_handle_resolve_tool_path -- --nocapture
}

run_workspace_all_features() {
  cargo build --workspace
  cargo build -p octos-cli --features "$FEATURES"
  cargo test --workspace --no-run
  cargo test --workspace
}

run_release_bundle() {
  cargo build --release -p octos-cli --features "$FEATURES"
  cargo build --release -p octos-sandbox
  # shellcheck disable=SC2086
  cargo build --release ${SKILL_CRATES}
}

SUITE="${1:-}"
case "$SUITE" in
  dashboard)
    run_dashboard
    ;;
  swarm-app)
    run_swarm_app
    ;;
  hosted-fast)
    run_hosted_fast
    ;;
  workspace-all-features)
    run_workspace_all_features
    ;;
  release-bundle)
    run_release_bundle
    ;;
  --help|-h|"")
    usage
    ;;
  *)
    echo "Unknown suite: $SUITE" >&2
    usage >&2
    exit 2
    ;;
esac
