#!/usr/bin/env bash
set -euo pipefail

# Provision native confinement tests on disposable Ubuntu CI runners.
sudo apt-get update
sudo apt-get install -y bubblewrap

# GitHub's runner restricts unprivileged user namespaces via AppArmor. Enable
# the kernel feature required by the probes; production launchers fail closed
# when a host disables it. This script is only for disposable CI runners.
if test -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns; then
    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi
