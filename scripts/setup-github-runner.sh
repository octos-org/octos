#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/setup-github-runner.sh --name NAME --labels LABELS [options]

Options:
  --repo OWNER/REPO        GitHub repo to register against (default: octos-org/octos)
  --url URL                Full GitHub repo URL (default: https://github.com/<repo>)
  --name NAME              Runner name
  --labels LABELS          Comma-separated labels, e.g. self-hosted,linux,x64,octos-fast
  --dir DIR                Runner install dir (default: ~/.github-runners/<name>)
  --version VERSION        actions/runner version (default: 2.328.0)
  --token TOKEN            Registration token; if omitted, gh api is used
  --replace                Replace an existing runner with the same name
  --ephemeral              Register as ephemeral runner
  --service                Install and start as Linux service via svc.sh
  --help                   Show this help

Environment:
  GITHUB_RUNNER_TOKEN      Optional registration token override

Examples:
  scripts/setup-github-runner.sh \
    --name mini3-octos-fast \
    --labels self-hosted,linux,x64,octos-fast

  scripts/setup-github-runner.sh \
    --name arm-builder \
    --labels self-hosted,linux,arm64,octos-arm \
    --service
EOF
}

REPO="octos-org/octos"
URL=""
NAME=""
LABELS=""
VERSION="${RUNNER_VERSION:-2.328.0}"
TOKEN="${GITHUB_RUNNER_TOKEN:-}"
REPLACE=0
EPHEMERAL=0
INSTALL_SERVICE=0
DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --url) URL="$2"; shift 2 ;;
        --name) NAME="$2"; shift 2 ;;
        --labels) LABELS="$2"; shift 2 ;;
        --dir) DIR="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --token) TOKEN="$2"; shift 2 ;;
        --replace) REPLACE=1; shift ;;
        --ephemeral) EPHEMERAL=1; shift ;;
        --service) INSTALL_SERVICE=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage; exit 1 ;;
    esac
done

if [[ -z "$NAME" || -z "$LABELS" ]]; then
    echo "--name and --labels are required" >&2
    usage
    exit 1
fi

if [[ -z "$URL" ]]; then
    URL="https://github.com/$REPO"
fi

if [[ -z "$DIR" ]]; then
    DIR="$HOME/.github-runners/$NAME"
fi

uname_s="$(uname -s)"
uname_m="$(uname -m)"
case "$uname_s" in
    Linux) os="linux" ;;
    Darwin) os="osx" ;;
    *)
        echo "Unsupported OS: $uname_s" >&2
        exit 1
        ;;
esac
case "$uname_m" in
    x86_64|amd64) arch="x64" ;;
    arm64|aarch64) arch="arm64" ;;
    *)
        echo "Unsupported architecture: $uname_m" >&2
        exit 1
        ;;
esac

if [[ -z "$TOKEN" ]]; then
    if ! command -v gh >/dev/null 2>&1; then
        echo "gh is required to auto-fetch a runner registration token" >&2
        exit 1
    fi
    TOKEN="$(gh api -X POST "repos/$REPO/actions/runners/registration-token" --jq .token)"
fi

runner_pkg="actions-runner-${os}-${arch}-${VERSION}.tar.gz"
runner_url="https://github.com/actions/runner/releases/download/v${VERSION}/${runner_pkg}"

mkdir -p "$DIR"
cd "$DIR"

if [[ ! -x ./config.sh ]]; then
    echo "Downloading $runner_url"
    curl -fsSL "$runner_url" -o "$runner_pkg"
    tar xzf "$runner_pkg"
    rm -f "$runner_pkg"
fi

config_args=(
    --unattended
    --url "$URL"
    --token "$TOKEN"
    --name "$NAME"
    --labels "$LABELS"
)

if [[ "$REPLACE" -eq 1 ]]; then
    config_args+=(--replace)
fi

if [[ "$EPHEMERAL" -eq 1 ]]; then
    config_args+=(--ephemeral)
fi

./config.sh "${config_args[@]}"

echo
echo "Runner configured:"
echo "  dir:    $DIR"
echo "  name:   $NAME"
echo "  labels: $LABELS"
echo

if [[ "$INSTALL_SERVICE" -eq 1 ]]; then
    if [[ "$os" != "linux" ]]; then
        echo "--service is only automated on Linux" >&2
        exit 1
    fi
    sudo ./svc.sh install
    sudo ./svc.sh start
    echo "Runner service installed and started."
else
    echo "Start command:"
    echo "  cd $DIR && ./run.sh"
fi
