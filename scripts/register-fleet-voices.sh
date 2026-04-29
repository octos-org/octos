#!/usr/bin/env bash
# Register voice clones with ominix-api on each fleet host.
#
# OminiX-API's voice registry is in-memory at startup (loaded from
# ~/.OminiX/models/voices.json). Voice .wav files saved by mofa-fm
# under ~/.octos/profiles/<profile>/data/voice_profiles/ are NOT
# auto-discovered by ominix-api; if voices.json is missing those
# names, every clone request hits mofa-fm's pre-validation and fails
# with "voice 'X' is not registered on ominix-api".
#
# This script writes voices.json on the remote host so /v1/voices
# enumerates every saved voice profile, then nudges the daemon to
# pick up the change. It's idempotent: existing entries are kept
# (operators can hand-tune ref_text/aliases without losing them).
#
# Run as part of post-deploy by invoking ./scripts/deploy.sh, or
# directly when fixing a single host:
#
#   ./scripts/register-fleet-voices.sh           # all minis (skips mini5)
#   ./scripts/register-fleet-voices.sh 1         # mini1 only
#   ./scripts/register-fleet-voices.sh user@host --password <pw>
#
# Mini5 is reserved for coding-green local testing — the script
# refuses to touch it unless --force-mini5 is set.
set -euo pipefail

# --- Built-in targets (mirror deploy.sh) ---
HOST_1="cloud@69.194.3.128"; PW_1="zjsgf128"
HOST_2="cloud@69.194.3.129"; PW_2="vbasx129"
HOST_3="cloud@69.194.3.203"; PW_3="b_KPfpN7Ge2ggxF-"
HOST_4="cloud@69.194.3.66";  PW_4=""        # key auth
HOST_5="cloud@69.194.3.19";  PW_5="zjsgf19"

FORCE_MINI5=false
TARGETS=()
CUSTOM_PW=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --password) CUSTOM_PW="$2"; shift 2 ;;
        --force-mini5) FORCE_MINI5=true; shift ;;
        all)
            TARGETS+=("1" "2" "3" "4")
            shift ;;
        1|2|3|4)
            TARGETS+=("$1")
            shift ;;
        5)
            if [[ "$FORCE_MINI5" == true ]]; then
                TARGETS+=("5")
            else
                echo "ERROR: mini5 is reserved for coding-green; pass --force-mini5 to override" >&2
                exit 2
            fi
            shift ;;
        -h|--help)
            awk '/^#!/{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "$0"
            exit 0 ;;
        *)
            # Treat as user@host
            TARGETS+=("custom:$1")
            shift ;;
    esac
done

if [[ ${#TARGETS[@]} -eq 0 ]]; then
    TARGETS=("1" "2" "3" "4")
fi

# Remote script: scans voice_profiles dirs and merges into voices.json.
# Idempotent — does not overwrite existing entries.
REMOTE_SCRIPT='
set -e
mkdir -p ~/.OminiX/models
python3 << "PYEOF"
import json, os, glob, sys
home = os.path.expanduser("~")
path = os.path.join(home, ".OminiX/models/voices.json")
if os.path.exists(path):
    try:
        cfg = json.loads(open(path).read())
        if not isinstance(cfg, dict):
            cfg = {}
    except Exception as e:
        print(f"warning: existing voices.json was malformed ({e}); rewriting", file=sys.stderr)
        cfg = {}
else:
    cfg = {}
voices = cfg.get("voices") if isinstance(cfg.get("voices"), dict) else {}
added = []
for wav in sorted(glob.glob(os.path.join(home, ".octos/profiles/*/data/voice_profiles/*.wav"))):
    name = os.path.splitext(os.path.basename(wav))[0]
    if name in voices:
        # keep operator-tuned entries
        continue
    voices[name] = {
        "ref_audio": wav,
        "ref_text": "",
        "aliases": [],
        "speed_factor": 1.0,
    }
    added.append(name)
cfg.setdefault("default_voice", "vivian")
cfg.setdefault("models_base_path", "~/.OminiX/models")
cfg["voices"] = voices
with open(path, "w") as f:
    json.dump(cfg, f, indent=2)
shown = added if added else "(none)"
print(f"voices.json: {len(voices)} custom entries; added this run: {shown}")
PYEOF

# Nudge ominix-api so /v1/voices reflects the new file.
# voices.json is loaded once at startup, so we need to bounce the
# daemon. Kickstart -k restarts under launchd if the agent is loaded.
if launchctl print "gui/$(id -u)/io.ominix.ominix-api" >/dev/null 2>&1; then
    launchctl kickstart -k "gui/$(id -u)/io.ominix.ominix-api" 2>/dev/null || true
elif launchctl list | grep -q io.ominix.ominix-api; then
    launchctl unload ~/Library/LaunchAgents/io.ominix.ominix-api.plist 2>/dev/null || true
    sleep 1
    launchctl load ~/Library/LaunchAgents/io.ominix.ominix-api.plist 2>/dev/null || true
fi

# Wait for /v1/voices to come back (max 20s).
for i in $(seq 1 20); do
    sleep 1
    body=$(curl -sf --max-time 2 http://localhost:8080/v1/voices 2>/dev/null || true)
    if [ -n "$body" ]; then
        echo "/v1/voices ready after ${i}s"
        echo "$body" | python3 -c "import json, sys; d=json.load(sys.stdin); names=[v[\"name\"] for v in d[\"voices\"]]; print(\"  voices:\", \", \".join(names))"
        exit 0
    fi
done
echo "WARNING: ominix-api did not return /v1/voices within 20s" >&2
exit 1
'

run_one() {
    local label="$1" host="$2" pw="$3"
    echo
    echo "=== $label ($host) ==="
    if [[ -n "$pw" ]]; then
        sshpass -p "$pw" ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
            -o PreferredAuthentications=password,keyboard-interactive \
            "$host" "$REMOTE_SCRIPT"
    else
        ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
            "$host" "$REMOTE_SCRIPT"
    fi
}

EXIT_CODE=0
for t in "${TARGETS[@]}"; do
    case "$t" in
        custom:*)
            host="${t#custom:}"
            run_one "$host" "$host" "$CUSTOM_PW" || EXIT_CODE=$?
            ;;
        1) run_one "mini1" "$HOST_1" "$PW_1" || EXIT_CODE=$? ;;
        2) run_one "mini2" "$HOST_2" "$PW_2" || EXIT_CODE=$? ;;
        3) run_one "mini3" "$HOST_3" "$PW_3" || EXIT_CODE=$? ;;
        4) run_one "mini4" "$HOST_4" "$PW_4" || EXIT_CODE=$? ;;
        5) run_one "mini5" "$HOST_5" "$PW_5" || EXIT_CODE=$? ;;
    esac
done

exit $EXIT_CODE
