#!/usr/bin/env bash
# fleet-install-skills.sh — Install mofa-skills onto every fleet mini through
# `octos skills install`, so every install routes through the same code path
# that verifies manifest sha256 sums and resolves the correct per-profile
# install directory.
#
# This script REPLACES the legacy raw-scp deploy in mofa-skills/scripts/
# deploy-mini.sh, which bypassed sha256 verification and hard-coded
# operator-side profile paths.
#
# Usage:
#   scripts/fleet-install-skills.sh
#   scripts/fleet-install-skills.sh --host 69.194.3.128,69.194.3.129
#   scripts/fleet-install-skills.sh --profile dspfac --skill mofa-cli,mofa-cards
#   scripts/fleet-install-skills.sh --dry-run
#
# Environment:
#   OCTOS_FLEET_HOSTS         Space-or-comma-separated host override (same as --host)
#   MOFA_SKILLS_DIR           Path to mofa-skills checkout (default: ~/home/mofa-skills)
#   OCTOS_REMOTE_BIN          Path to octos binary on remote (default:
#                             /Users/cloud/.octos/bin/octos)
#   OCTOS_REMOTE_USER         SSH user on remote hosts (default: cloud)
#
# Per host, per profile, per skill:
#   1. rsync the local skill directory to a remote staging path
#   2. SSH in, run `OCTOS_PROFILE_ID=<p> octos skills --profile <p> install
#      <staging_path> --force` so the install routes through the same code
#      path as the runtime tool (verifies manifest sha256, builds binaries,
#      writes .source for `skills update`)
#   3. On failure, log host+profile+skill+stderr tail and CONTINUE; do not
#      abort the whole run on one failure
#
# After all hosts, print a summary table: OK / FAIL / SKIP per
# (host x profile x skill).

set -eEuo pipefail

# ─── Defaults ────────────────────────────────────────────────────────────
DEFAULT_HOSTS="69.194.3.128 69.194.3.129 69.194.3.203 69.194.3.66 69.194.3.19"
DEFAULT_MOFA_DIR="$HOME/home/mofa-skills"
DEFAULT_REMOTE_BIN="/Users/cloud/.octos/bin/octos"
DEFAULT_REMOTE_USER="cloud"
DEFAULT_REMOTE_STAGING="/tmp/octos-fleet-install-staging"

HOSTS_ARG="${OCTOS_FLEET_HOSTS:-}"
PROFILES_ARG=""
SKILLS_ARG=""
MOFA_DIR="${MOFA_SKILLS_DIR:-$DEFAULT_MOFA_DIR}"
REMOTE_BIN="${OCTOS_REMOTE_BIN:-$DEFAULT_REMOTE_BIN}"
REMOTE_USER="${OCTOS_REMOTE_USER:-$DEFAULT_REMOTE_USER}"
REMOTE_STAGING="${OCTOS_REMOTE_STAGING:-$DEFAULT_REMOTE_STAGING}"
DRY_RUN=false
FORCE=true   # default: re-install on every run; idempotent because content hashes match
VERBOSE=false

# ─── Argument parsing ────────────────────────────────────────────────────
usage() {
    cat <<'USAGE'
fleet-install-skills.sh — Install mofa-skills onto fleet minis via
`octos skills install` (sha256-verified, per-profile).

Usage:
  fleet-install-skills.sh [OPTIONS]

Options:
  --host LIST          Comma-or-space separated hosts (default: mini1-5 IPs)
                       Overrides OCTOS_FLEET_HOSTS
  --profile LIST       Comma-separated profile IDs (default: every profile
                       enumerated from ~/.octos/profiles/*/data on each host)
  --skill LIST         Comma-separated skill names (default: every dir under
                       MOFA_SKILLS_DIR with SKILL.md AND manifest.json)
  --mofa-dir PATH      Path to mofa-skills checkout (default: ~/home/mofa-skills)
  --remote-bin PATH    Path to octos binary on the remote
                       (default: /Users/cloud/.octos/bin/octos)
  --remote-user USER   SSH user (default: cloud)
  --no-force           Do NOT pass --force; `octos skills install` will SKIP
                       skills that already exist
  --dry-run            Print every command that WOULD run; execute nothing
  --verbose, -v        Echo each command before running it
  --help, -h           Show this help

Environment overrides:
  OCTOS_FLEET_HOSTS         Same as --host
  MOFA_SKILLS_DIR           Same as --mofa-dir
  OCTOS_REMOTE_BIN          Same as --remote-bin
  OCTOS_REMOTE_USER         Same as --remote-user
  OCTOS_REMOTE_STAGING      Remote staging path (default: /tmp/octos-fleet-install-staging)

Examples:
  # Dry-run against the full fleet
  scripts/fleet-install-skills.sh --dry-run

  # Single host, single profile, single skill
  scripts/fleet-install-skills.sh \
      --host 69.194.3.129 --profile dspfac --skill mofa-cli

  # Override host list via env
  OCTOS_FLEET_HOSTS=69.194.3.66 scripts/fleet-install-skills.sh
USAGE
}

needval() {
    if [ $# -lt 2 ] || case "$2" in -*) true ;; *) false ;; esac; then
        echo "ERROR: $1 requires a value" >&2
        exit 1
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --host)        needval "$@"; HOSTS_ARG="$2"; shift 2 ;;
        --profile)     needval "$@"; PROFILES_ARG="$2"; shift 2 ;;
        --skill)       needval "$@"; SKILLS_ARG="$2"; shift 2 ;;
        --mofa-dir)    needval "$@"; MOFA_DIR="$2"; shift 2 ;;
        --remote-bin)  needval "$@"; REMOTE_BIN="$2"; shift 2 ;;
        --remote-user) needval "$@"; REMOTE_USER="$2"; shift 2 ;;
        --no-force)    FORCE=false; shift ;;
        --dry-run)     DRY_RUN=true; shift ;;
        --verbose|-v)  VERBOSE=true; shift ;;
        --help|-h)     usage; exit 0 ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

# ─── Helpers ─────────────────────────────────────────────────────────────
log()    { echo "    $*"; }
warn()   { echo "    WARN: $*" >&2; }
err()    { echo "    ERROR: $*" >&2; }
section(){ echo ""; echo "==> $*"; }

run_or_print() {
    if [ "$VERBOSE" = "true" ]; then
        echo "    \$ $*"
    fi
    if [ "$DRY_RUN" = "true" ]; then
        echo "    [dry-run] $*"
        return 0
    fi
    "$@"
}

# Normalise a "a,b c" list into a newline-separated stream.
list_to_lines() {
    printf '%s' "$1" | tr ',' '\n' | tr ' ' '\n' | awk 'NF'
}

# SSH wrapper with ControlMaster reuse (see CLAUDE memory reference_minis_ssh.md).
# Each call runs through the cached socket if present, falling back to a fresh
# connection otherwise. -BatchMode forbids password prompts: the operator is
# expected to have keys configured (mini4) or sshpass-wrapped aliases (mini1/2/3/5).
ssh_remote() {
    local host="$1"; shift
    ssh \
        -o ControlMaster=auto \
        -o ControlPath="$HOME/.ssh/cm/%r@%h:%p" \
        -o ControlPersist=60 \
        -o BatchMode=yes \
        -o StrictHostKeyChecking=no \
        -o ConnectTimeout=10 \
        "${REMOTE_USER}@${host}" \
        "$@"
}

# shellcheck disable=SC2329  # invoked indirectly via run_or_print
rsync_to_remote() {
    local host="$1"; local src="$2"; local dest="$3"
    rsync -az --delete \
        -e "ssh -o ControlMaster=auto -o ControlPath=$HOME/.ssh/cm/%r@%h:%p -o ControlPersist=60 -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10" \
        "$src" "${REMOTE_USER}@${host}:${dest}"
}

# ─── Resolve hosts ───────────────────────────────────────────────────────
if [ -z "$HOSTS_ARG" ]; then
    HOSTS_ARG="$DEFAULT_HOSTS"
fi
HOSTS=()
while IFS= read -r h; do
    [ -n "$h" ] && HOSTS+=("$h")
done < <(list_to_lines "$HOSTS_ARG")

if [ ${#HOSTS[@]} -eq 0 ]; then
    err "no hosts to process"
    exit 1
fi

# ─── Resolve skills (catalog from MOFA_SKILLS_DIR) ───────────────────────
ALL_SKILLS=()
if [ ! -d "$MOFA_DIR" ]; then
    err "MOFA_SKILLS_DIR does not exist: $MOFA_DIR"
    exit 1
fi
# Canonical "all skills" = every dir under MOFA_SKILLS_DIR matching `mofa-*`
# that contains BOTH SKILL.md and manifest.json. The deploy-mini.sh legacy
# script used SKILL.md alone, but we tighten to manifest.json so we only
# install skills with the metadata `octos skills install` expects.
for d in "$MOFA_DIR"/mofa-*/; do
    [ -d "$d" ] || continue
    [ -f "$d/SKILL.md" ] || continue
    [ -f "$d/manifest.json" ] || continue
    ALL_SKILLS+=("$(basename "$d")")
done

if [ -n "$SKILLS_ARG" ]; then
    SKILLS=()
    while IFS= read -r s; do
        [ -z "$s" ] && continue
        if ! printf '%s\n' "${ALL_SKILLS[@]}" | grep -qx "$s"; then
            err "unknown skill: '$s' (not under $MOFA_DIR with SKILL.md+manifest.json)"
            exit 1
        fi
        SKILLS+=("$s")
    done < <(list_to_lines "$SKILLS_ARG")
else
    SKILLS=("${ALL_SKILLS[@]}")
fi

if [ ${#SKILLS[@]} -eq 0 ]; then
    err "no skills to install (looked in $MOFA_DIR for mofa-* with SKILL.md+manifest.json)"
    exit 1
fi

# ─── Profile enumeration per host ────────────────────────────────────────
# If --profile was given, every host installs into the same explicit set.
# Otherwise, query each host: `ls ~/.octos/profiles/*/data` -> profile IDs.
EXPLICIT_PROFILES=()
if [ -n "$PROFILES_ARG" ]; then
    while IFS= read -r p; do
        [ -n "$p" ] && EXPLICIT_PROFILES+=("$p")
    done < <(list_to_lines "$PROFILES_ARG")
fi

# Enumerate profiles on a single host. Echoes one profile ID per line.
# On SSH failure prints nothing (caller handles empty as host-unreachable).
enumerate_profiles() {
    local host="$1"
    # Sniff ~/.octos/profiles for any subdir that contains a `data` child.
    # We don't trust globbing the operator's shell so we run a tiny `find`.
    ssh_remote "$host" \
        "find ~/.octos/profiles -mindepth 2 -maxdepth 2 -type d -name data 2>/dev/null \
         | sed 's|.*/profiles/||; s|/data\$||' \
         | sort -u" \
        2>/dev/null || true
}

# ─── Plan summary ────────────────────────────────────────────────────────
section "Fleet install plan"
log "Hosts:        ${HOSTS[*]}"
log "Skills:       ${SKILLS[*]}"
log "Mofa dir:     $MOFA_DIR"
log "Remote bin:   $REMOTE_BIN"
log "Remote user:  $REMOTE_USER"
log "Remote stage: $REMOTE_STAGING"
if [ -n "$PROFILES_ARG" ]; then
    log "Profiles:     ${EXPLICIT_PROFILES[*]} (explicit)"
else
    log "Profiles:     <enumerate per host>"
fi
if [ "$FORCE" = "true" ]; then
    log "Force:        yes (--force on every install)"
else
    log "Force:        no (skip skills that already exist)"
fi
if [ "$DRY_RUN" = "true" ]; then
    log "Mode:         DRY-RUN (no remote mutation)"
fi

mkdir -p "$HOME/.ssh/cm" 2>/dev/null || true

# ─── Results matrix ──────────────────────────────────────────────────────
# Each entry: "<host>|<profile>|<skill>|<status>|<note>"
# Status is one of: OK, FAIL, SKIP
RESULTS=()

record() {
    local host="$1" profile="$2" skill="$3" status="$4" note="${5:-}"
    RESULTS+=("$host|$profile|$skill|$status|$note")
}

# ─── Per-host work ───────────────────────────────────────────────────────
for host in "${HOSTS[@]}"; do
    section "Host: $host"

    # ── Profiles for this host ──
    if [ ${#EXPLICIT_PROFILES[@]} -gt 0 ]; then
        PROFILES=("${EXPLICIT_PROFILES[@]}")
    else
        PROFILES=()
        if [ "$DRY_RUN" = "true" ]; then
            # In dry-run we never SSH; show that we WOULD enumerate.
            log "[dry-run] would enumerate profiles via SSH on $host"
            PROFILES=("<dry-run-enumerated>")
        else
            log "Enumerating profiles via SSH..."
            while IFS= read -r p; do
                [ -n "$p" ] && PROFILES+=("$p")
            done < <(enumerate_profiles "$host")
            if [ ${#PROFILES[@]} -eq 0 ]; then
                warn "no profiles found on $host (host unreachable or no profiles directory)"
                for skill in "${SKILLS[@]}"; do
                    record "$host" "<none>" "$skill" "SKIP" "no profiles"
                done
                continue
            fi
            log "Found profiles: ${PROFILES[*]}"
        fi
    fi

    # ── Stage skills onto host (one rsync per skill) ──
    if [ "$DRY_RUN" != "true" ]; then
        run_or_print ssh_remote "$host" "mkdir -p '$REMOTE_STAGING'"
    else
        log "[dry-run] would: ssh $host mkdir -p $REMOTE_STAGING"
    fi

    for skill in "${SKILLS[@]}"; do
        src="$MOFA_DIR/$skill"
        if [ ! -d "$src" ]; then
            warn "skill '$skill' missing locally at $src"
            for p in "${PROFILES[@]}"; do
                record "$host" "$p" "$skill" "FAIL" "missing locally"
            done
            continue
        fi
        # Stage to $REMOTE_STAGING/$skill (rsync with trailing-slash semantics
        # so contents end up at the dest, not nested).
        if [ "$DRY_RUN" = "true" ]; then
            log "[dry-run] would: rsync $src/ -> $host:$REMOTE_STAGING/$skill/"
        else
            if ! run_or_print rsync_to_remote "$host" "$src/" "$REMOTE_STAGING/$skill/"; then
                warn "rsync failed for skill=$skill on host=$host"
                for p in "${PROFILES[@]}"; do
                    record "$host" "$p" "$skill" "FAIL" "rsync staging failed"
                done
                continue
            fi
        fi

        # Per-profile install via the routed CLI. This is THE point of the
        # script: every install goes through `octos skills install <local-path>`
        # so the per-profile dir is resolved server-side and (when the
        # manifest declares binaries) the sha256 of the downloaded binary
        # is verified.
        for profile in "${PROFILES[@]}"; do
            force_flag=""
            [ "$FORCE" = "true" ] && force_flag="--force"
            install_cmd="OCTOS_PROFILE_ID='$profile' '$REMOTE_BIN' skills --profile '$profile' install '$REMOTE_STAGING/$skill' $force_flag"

            if [ "$DRY_RUN" = "true" ]; then
                log "[dry-run] would: ssh $host -- $install_cmd"
                record "$host" "$profile" "$skill" "OK" "dry-run"
                continue
            fi

            # Capture stderr tail on failure for the summary.
            tmp_err="$(mktemp -t octos-fleet-err.XXXXXX)"
            if ssh_remote "$host" "$install_cmd" >/dev/null 2>"$tmp_err"; then
                record "$host" "$profile" "$skill" "OK" ""
                log "OK   $profile / $skill"
            else
                tail_msg=$(tail -n 3 "$tmp_err" | tr '\n' '|' | sed 's/|$//')
                record "$host" "$profile" "$skill" "FAIL" "$tail_msg"
                warn "FAIL $profile / $skill: $tail_msg"
            fi
            rm -f "$tmp_err"
        done
    done

    # ── Cleanup staging ──
    if [ "$DRY_RUN" != "true" ]; then
        run_or_print ssh_remote "$host" "rm -rf '$REMOTE_STAGING'" || true
    fi
done

# ─── Summary ─────────────────────────────────────────────────────────────
section "Summary"
ok_count=0
fail_count=0
skip_count=0
printf '    %-18s  %-20s  %-22s  %-4s  %s\n' "HOST" "PROFILE" "SKILL" "RES" "NOTE"
printf '    %-18s  %-20s  %-22s  %-4s  %s\n' "----" "-------" "-----" "---" "----"
for row in "${RESULTS[@]}"; do
    IFS='|' read -r host profile skill status note <<<"$row"
    printf '    %-18s  %-20s  %-22s  %-4s  %s\n' "$host" "$profile" "$skill" "$status" "$note"
    case "$status" in
        OK)   ok_count=$((ok_count + 1)) ;;
        FAIL) fail_count=$((fail_count + 1)) ;;
        SKIP) skip_count=$((skip_count + 1)) ;;
    esac
done

echo ""
log "Totals: OK=$ok_count FAIL=$fail_count SKIP=$skip_count"
if [ "$DRY_RUN" = "true" ]; then
    log "(dry-run — no remote state was modified)"
fi

if [ "$fail_count" -gt 0 ]; then
    exit 2
fi
exit 0
