#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FEATURES="${FEATURES:-api,telegram,discord,whatsapp,feishu,twilio,wecom,wecom-bot}"
SKILL_CRATES="${SKILL_CRATES:--p news_fetch -p deep-search -p deep-crawl -p send-email -p account-manager -p voice -p clock -p weather -p pipeline-guard -p skill-evolve}"

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

  ./scripts/build-dashboard.sh
  if [ -n "$(git status --porcelain -- crates/octos-cli/static/admin)" ]; then
    echo "Embedded dashboard assets are out of date. Run ./scripts/build-dashboard.sh and commit changes."
    git status --short -- crates/octos-cli/static/admin
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
  cargo fmt --all -- --check
  cargo clippy --workspace -- -D warnings
  cargo test --workspace

  cargo test -p octos-llm test_qos_ranking_changes_lane_selection -- --nocapture
  cargo test -p octos-llm test_derive_cold_start_catalog_assigns_non_zero_scores -- --nocapture
  cargo test -p octos-llm test_compatible_fallbacks_prefers_lower_seeded_qos_score -- --nocapture
  cargo test -p octos-cli gateway_runtime::tests --features api -- --nocapture
  cargo test -p octos-agent --test activate_tools_regression -- --nocapture
}

run_workspace_all_features() {
  cargo build --workspace
  cargo build -p octos-cli --features "$FEATURES"
  cargo test --workspace --no-run
  cargo test --workspace
}

run_release_bundle() {
  cargo build --release -p octos-cli --features "$FEATURES"
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
