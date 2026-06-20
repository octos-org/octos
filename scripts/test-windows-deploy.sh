#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/deploy.ps1"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

require_grep() {
    local pattern="$1"
    local path="$2"
    local message="$3"
    grep -qE "$pattern" "$path" || fail "$message"
}

[ -f "$SCRIPT" ] || fail "scripts/deploy.ps1 is missing"

require_grep 'param\(' "$SCRIPT" "deploy.ps1 must expose parameters"
require_grep '\[string\]\$HostName' "$SCRIPT" "deploy.ps1 must accept a target host"
require_grep '\[switch\]\$DryRun' "$SCRIPT" "deploy.ps1 must support dry-run mode"
require_grep '\[switch\]\$Uninstall' "$SCRIPT" "deploy.ps1 must support uninstall mode"
require_grep 'octos-bundle-x86_64-pc-windows-msvc\.zip' "$SCRIPT" "deploy.ps1 must target the Windows release bundle"
require_grep 'ssh' "$SCRIPT" "deploy.ps1 must use OpenSSH for remote execution"
require_grep 'scp' "$SCRIPT" "deploy.ps1 must use SCP for local bundle uploads"
require_grep 'EncodedCommand' "$SCRIPT" "deploy.ps1 must send encoded PowerShell to the remote host"
require_grep '\$nssmExe install' "$SCRIPT" "deploy.ps1 must register OctosServe through NSSM"
require_grep 'SERVICE_AUTO_START' "$SCRIPT" "deploy.ps1 must configure auto-start service behavior"
require_grep 'OCTOS_HOME=' "$SCRIPT" "deploy.ps1 must set the remote Octos data path"
require_grep 'C:\\octos' "$SCRIPT" "deploy.ps1 must document the default Windows install root"

if command -v pwsh >/dev/null 2>&1; then
    out="$(pwsh -NoProfile -ExecutionPolicy Bypass -File "$SCRIPT" \
        -HostName win.example.invalid \
        -User deploy \
        -Port 2222 \
        -IdentityFile "$ROOT_DIR/.ssh/test-key" \
        -Version v0.0.0-test \
        -RemoteRoot 'C:\octos-ci' \
        -ServiceName OctosServeTest \
        -ServePort 50080 \
        -AuthToken test-token \
        -DryRun 2>&1)"

    grep -q '\[dry-run\] remote PowerShell script' <<<"$out" \
        || fail "dry run should print the remote script"
    grep -q '\$nssmExe install' <<<"$out" \
        || fail "dry run should show service registration"
    grep -q 'C:\\octos-ci' <<<"$out" \
        || fail "dry run should include the requested remote root"
    grep -q 'OctosServeTest' <<<"$out" \
        || fail "dry run should include the requested service name"
    grep -q -- '--auth-token' <<<"$out" \
        || fail "dry run should include auth-token serve argument"
    grep -q 'test-token' <<<"$out" \
        || fail "dry run should include the requested auth token"
    grep -q 'ssh -p 2222' <<<"$out" \
        || fail "dry run should show SSH port handling"

    uninstall_out="$(pwsh -NoProfile -ExecutionPolicy Bypass -File "$SCRIPT" \
        -HostName win.example.invalid \
        -RemoteRoot 'C:\octos-ci' \
        -ServiceName OctosServeTest \
        -Uninstall \
        -Purge \
        -DryRun 2>&1)"
    grep -q 'nssm.exe' <<<"$uninstall_out" \
        || fail "uninstall dry run should prefer NSSM removal"
    grep -q 'sc.exe delete' <<<"$uninstall_out" \
        || fail "uninstall dry run should include sc.exe fallback"
    grep -q 'Remove-Item -Recurse -Force' <<<"$uninstall_out" \
        || fail "purge dry run should include remote root removal"
else
    echo "pwsh not found; static deploy.ps1 checks only"
fi

echo "windows deploy tests passed"
