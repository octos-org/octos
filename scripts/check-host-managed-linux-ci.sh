#!/usr/bin/env bash
# Read-only validation for pre-provisioned, unprivileged self-hosted runners.
set -euo pipefail

if ! command -v bwrap >/dev/null 2>&1; then
    echo "error: bubblewrap is required on self-hosted Linux CI runners" >&2
    exit 1
fi

# Do not read or change the shared host's global AppArmor policy. The real
# octos-sandbox tests are the authority for this runner's narrow bwrap profile.
