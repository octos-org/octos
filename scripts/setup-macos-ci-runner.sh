#!/usr/bin/env bash
# Register a repo-owned macOS machine in the `octos-mac` runner pool.
#
# Every pool member gets the same labels, so GitHub spreads matrix shards and
# parallel jobs over whichever members are idle. The runner runs as a
# LaunchAgent in the user's Background session (no GUI login needed) and keeps
# all of its caches (cargo, rustup, npm, playwright) inside its own directory.
# Compiler output is shared between members through an S3-compatible sccache
# bucket (see SELF_HOSTED_RUNNERS.txt for the reference MinIO setup).
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/setup-macos-ci-runner.sh --name NAME [options]

Options:
  --repo OWNER/REPO        GitHub repo to register against (default: octos-org/octos)
  --name NAME              Runner name, e.g. macmini1-octos
  --labels LABELS          Labels (default: self-hosted,macOS,ARM64,octos-mac)
  --dir DIR                Install dir (default: ~/actions-runner-<name>)
  --version VERSION        actions/runner version (default: 2.338.2)
  --token TOKEN            Registration token; if omitted, gh api is used
  --proxy URL              HTTP proxy for GitHub traffic, written only into the
                           runner's .env (system network settings are untouched)
  --sccache-endpoint URL   S3 endpoint of the shared sccache bucket
  --sccache-bucket NAME    Bucket name (default: octos-sccache)
  --help                   Show this help

Environment:
  GITHUB_RUNNER_TOKEN      Registration token override
  SCCACHE_S3_ACCESS_KEY    Access key for the sccache bucket
  SCCACHE_S3_SECRET_KEY    Secret key for the sccache bucket

Requires Homebrew with sccache and cmake installed.
EOF
}

REPO="octos-org/octos"
NAME=""
LABELS="self-hosted,macOS,ARM64,octos-mac"
DIR=""
VERSION="${RUNNER_VERSION:-2.338.2}"
TOKEN="${GITHUB_RUNNER_TOKEN:-}"
PROXY=""
SCCACHE_ENDPOINT=""
SCCACHE_BUCKET="octos-sccache"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --name) NAME="$2"; shift 2 ;;
        --labels) LABELS="$2"; shift 2 ;;
        --dir) DIR="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --token) TOKEN="$2"; shift 2 ;;
        --proxy) PROXY="$2"; shift 2 ;;
        --sccache-endpoint) SCCACHE_ENDPOINT="$2"; shift 2 ;;
        --sccache-bucket) SCCACHE_BUCKET="$2"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage; exit 2 ;;
    esac
done

[[ -n "$NAME" ]] || { usage; exit 2; }
[[ "$(uname -s)" == "Darwin" ]] || { echo "macOS only" >&2; exit 1; }
DIR="${DIR:-$HOME/actions-runner-$NAME}"
BREW_PREFIX="$(brew --prefix)"

if [[ -z "$TOKEN" ]]; then
    TOKEN="$(gh api -X POST "repos/$REPO/actions/runners/registration-token" --jq .token)"
fi

mkdir -p "$DIR"
cd "$DIR"
if [[ ! -x ./config.sh ]]; then
    curl -fsSL ${PROXY:+-x "$PROXY"} -o runner.tgz \
        "https://github.com/actions/runner/releases/download/v$VERSION/actions-runner-osx-arm64-$VERSION.tar.gz"
    tar xzf runner.tgz
    rm runner.tgz
fi

{
    if [[ -n "$PROXY" ]]; then
        for v in https_proxy http_proxy HTTPS_PROXY HTTP_PROXY; do echo "$v=$PROXY"; done
        for v in no_proxy NO_PROXY; do
            echo "$v=localhost,127.0.0.1,::1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12,.local"
        done
    fi
    echo "PATH=$BREW_PREFIX/bin:$BREW_PREFIX/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    echo "CARGO_HOME=$DIR/_cargo"
    echo "RUSTUP_HOME=$DIR/_rustup"
    echo "PLAYWRIGHT_BROWSERS_PATH=$DIR/_playwright"
    echo "npm_config_cache=$DIR/_npm"
    echo "HOMEBREW_NO_AUTO_UPDATE=1"
    echo "HOMEBREW_NO_INSTALL_UPGRADE=1"
    echo "HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1"
    echo "RUSTC_WRAPPER=$BREW_PREFIX/bin/sccache"
    echo "CARGO_INCREMENTAL=0"
    if [[ -n "$SCCACHE_ENDPOINT" ]]; then
        echo "SCCACHE_BUCKET=$SCCACHE_BUCKET"
        echo "SCCACHE_ENDPOINT=$SCCACHE_ENDPOINT"
        echo "SCCACHE_REGION=auto"
        echo "SCCACHE_S3_USE_SSL=false"
        echo "SCCACHE_S3_KEY_PREFIX=octos"
        echo "AWS_ACCESS_KEY_ID=${SCCACHE_S3_ACCESS_KEY:?SCCACHE_S3_ACCESS_KEY is required with --sccache-endpoint}"
        echo "AWS_SECRET_ACCESS_KEY=${SCCACHE_S3_SECRET_KEY:?SCCACHE_S3_SECRET_KEY is required with --sccache-endpoint}"
    else
        echo "SCCACHE_DIR=$DIR/_sccache"
    fi
} > .env
chmod 600 .env

https_proxy="$PROXY" ./config.sh --unattended --replace \
    --url "https://github.com/$REPO" --token "$TOKEN" \
    --name "$NAME" --labels "$LABELS" --work _work

# svc.sh's `launchctl load` needs a GUI login; bootstrap into the user's
# Background domain instead so the runner also starts on headless machines.
./svc.sh install
LABEL="actions.runner.${REPO/\//-}.$NAME"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
plutil -replace LimitLoadToSessionType -string Background "$PLIST"
launchctl bootout "user/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "user/$(id -u)" "$PLIST"
echo "runner $NAME started (launchd user/$(id -u)/$LABEL)"
