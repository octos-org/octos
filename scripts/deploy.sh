#!/usr/bin/env bash
# Deploy octos + app-skill binaries to remote macOS hosts.
#
# Usage:
#   ./scripts/deploy.sh [1|2|all]                          # Built-in Mac Minis
#   ./scripts/deploy.sh user@host --password <pw>          # Custom host + password
#   ./scripts/deploy.sh user@host --key <keyfile>          # Custom host + SSH key
#   ./scripts/deploy.sh user@host                          # Custom host + default SSH key
#
# Options:
#   --password <pw>       Authenticate with password (requires sshpass)
#   --key <keyfile>       Authenticate with SSH key file
#   --remote-bin <path>   Remote binary directory (default: ~/.cargo/bin)
#   --remote-data <path>  Remote data directory (default: ~/.octos)
#   --plist <label>       Launchd plist label (default: io.ominix.octos-serve)
#   --skip-build          Skip local build step
#   --skip-ominix         Skip ominix-api build and deploy
#   --init                Initialize data dir on fresh machine (clean slate)
#   --clone-from <1|2>    Clone profiles & config from built-in Mac Mini
#   --serve-port <port>   Port for octos serve (default: 3000)
#   --factory-reset       Stop all services, wipe old data (~/.crew + ~/.octos),
#                         remove legacy plists, and do a clean install from scratch
#   --keep-profiles       With --factory-reset: preserve profiles/*.json and config.json
#   --test                Run Playwright e2e tests after deploy (web client smoke tests)
#   --no-test             Skip post-deploy tests (default: tests run if --test is set)
#   --caddy-domain <dom>  Set up Caddy reverse proxy with on-demand TLS for *.dom
#                         (requires wildcard DNS A record pointing to the host)
#
# Examples:
#   ./scripts/deploy.sh 1                                   # Mac Mini 1
#   ./scripts/deploy.sh all                                 # Both Mac Minis
#   ./scripts/deploy.sh admin@10.0.1.50 --key ~/.ssh/id_ed25519 --init
#   ./scripts/deploy.sh user@host --password s3cret --clone-from 1
#   ./scripts/deploy.sh user@host --remote-bin /opt/octos/bin --remote-data /opt/octos/data
set -euo pipefail

# --- Built-in targets ---
HOST_1="cloud@69.194.3.128"
PW_1="zjsgf128"
HOST_2="cloud@69.194.3.129"
PW_2="vbasx129"
HOST_3="cloud@69.194.3.203"
PW_3="b_KPfpN7Ge2ggxF-"

# --- Defaults ---
PLIST="io.ominix.octos-serve"
SKIP_BUILD=false
SKIP_OMINIX=false
INIT_FRESH=false
FACTORY_RESET=false
KEEP_PROFILES=false
RUN_TESTS=false
CLONE_FROM=""
REMOTE_DATA=""
SERVE_PORT="3000"
CADDY_DOMAIN=""
BINARIES=(octos news_fetch deep-search deep_crawl send_email account_manager voice clock weather pipeline-guard skill-evolve)

# --- Parse arguments ---
# We build parallel arrays: DEPLOY_HOSTS[], DEPLOY_AUTH_TYPE[], DEPLOY_AUTH_VAL[], DEPLOY_LABEL[]
DEPLOY_HOSTS=()
DEPLOY_AUTH_TYPE=()
DEPLOY_AUTH_VAL=()
DEPLOY_LABEL=()
REMOTE_BIN=""

parse_builtin_target() {
    local idx=$1
    local host pw
    eval "host=\$HOST_$idx; pw=\$PW_$idx"
    DEPLOY_HOSTS+=("$host")
    DEPLOY_AUTH_TYPE+=("password")
    DEPLOY_AUTH_VAL+=("$pw")
    DEPLOY_LABEL+=("Mac Mini $idx")
}

# First pass: extract flags, collect positional args
POSITIONAL=()
CUSTOM_AUTH_TYPE=""
CUSTOM_AUTH_VAL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --password)
            CUSTOM_AUTH_TYPE="password"
            CUSTOM_AUTH_VAL="$2"
            shift 2 ;;
        --key)
            CUSTOM_AUTH_TYPE="key"
            CUSTOM_AUTH_VAL="$2"
            shift 2 ;;
        --remote-bin)
            REMOTE_BIN="$2"
            shift 2 ;;
        --remote-data)
            REMOTE_DATA="$2"
            shift 2 ;;
        --plist)
            PLIST="$2"
            shift 2 ;;
        --skip-build)
            SKIP_BUILD=true
            shift ;;
        --skip-ominix)
            SKIP_OMINIX=true
            shift ;;
        --init)
            INIT_FRESH=true
            shift ;;
        --clone-from)
            CLONE_FROM="$2"
            if [[ "$CLONE_FROM" != "1" && "$CLONE_FROM" != "2" ]]; then
                echo "Error: --clone-from must be 1 or 2 (built-in Mac Mini index)"
                exit 1
            fi
            shift 2 ;;
        --factory-reset)
            FACTORY_RESET=true
            shift ;;
        --keep-profiles)
            KEEP_PROFILES=true
            shift ;;
        --serve-port)
            SERVE_PORT="$2"
            shift 2 ;;
        --test)
            RUN_TESTS=true
            shift ;;
        --no-test)
            RUN_TESTS=false
            shift ;;
        --caddy-domain)
            CADDY_DOMAIN="$2"
            shift 2 ;;
        -h|--help)
            awk '/^#!/{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "$0"
            exit 0 ;;
        *)
            POSITIONAL+=("$1")
            shift ;;
    esac
done

# Second pass: resolve positional args into targets
if [[ ${#POSITIONAL[@]} -eq 0 ]]; then
    # Default: deploy to both built-in targets
    parse_builtin_target 1
    parse_builtin_target 2
else
    for arg in "${POSITIONAL[@]}"; do
        case "$arg" in
            1)
                parse_builtin_target 1 ;;
            2)
                parse_builtin_target 2 ;;
            3)
                parse_builtin_target 3 ;;
            all)
                parse_builtin_target 1
                parse_builtin_target 2
                parse_builtin_target 3 ;;
            *@*)
                # Custom user@host
                DEPLOY_HOSTS+=("$arg")
                if [[ -n "$CUSTOM_AUTH_TYPE" ]]; then
                    DEPLOY_AUTH_TYPE+=("$CUSTOM_AUTH_TYPE")
                    DEPLOY_AUTH_VAL+=("$CUSTOM_AUTH_VAL")
                else
                    DEPLOY_AUTH_TYPE+=("key")
                    DEPLOY_AUTH_VAL+=("default")
                fi
                DEPLOY_LABEL+=("$arg") ;;
            *)
                echo "Error: unknown target '$arg'"
                echo "Usage: $0 [1|2|all|user@host] [--password pw|--key keyfile]"
                exit 1 ;;
        esac
    done
fi

if [[ ${#DEPLOY_HOSTS[@]} -eq 0 ]]; then
    echo "No deploy targets specified."
    exit 1
fi

# --- SSH/SCP helpers ---
# These use the per-target auth arrays.
_ssh_opts=(-o StrictHostKeyChecking=no -o ConnectTimeout=10)

ssh_target() {
    # ssh_target <index> <command...>
    local i=$1; shift
    local host="${DEPLOY_HOSTS[$i]}"
    local auth_type="${DEPLOY_AUTH_TYPE[$i]}"
    local auth_val="${DEPLOY_AUTH_VAL[$i]}"

    case "$auth_type" in
        password)
            # Try password auth first, fall back to keyboard-interactive (required by some hosts)
            sshpass -p "$auth_val" ssh "${_ssh_opts[@]}" -o PreferredAuthentications=password,keyboard-interactive "$host" "$@" ;;
        key)
            if [[ "$auth_val" == "default" ]]; then
                ssh "${_ssh_opts[@]}" "$host" "$@"
            else
                ssh "${_ssh_opts[@]}" -i "$auth_val" "$host" "$@"
            fi ;;
    esac
}

scp_target() {
    # scp_target <index> <src> <dest>
    local i=$1; shift
    local auth_type="${DEPLOY_AUTH_TYPE[$i]}"
    local auth_val="${DEPLOY_AUTH_VAL[$i]}"

    case "$auth_type" in
        password)
            sshpass -p "$auth_val" scp "${_ssh_opts[@]}" -o PreferredAuthentications=password,keyboard-interactive "$@" ;;
        key)
            if [[ "$auth_val" == "default" ]]; then
                scp "${_ssh_opts[@]}" "$@"
            else
                scp "${_ssh_opts[@]}" -i "$auth_val" "$@"
            fi ;;
    esac
}

ssh_sudo_target() {
    # ssh_sudo_target <index> <command string>
    local i=$1; shift
    local host="${DEPLOY_HOSTS[$i]}"
    local auth_type="${DEPLOY_AUTH_TYPE[$i]}"
    local auth_val="${DEPLOY_AUTH_VAL[$i]}"
    local remote_cmd="$*"
    local quoted_cmd
    printf -v quoted_cmd "%q" "$remote_cmd"

    case "$auth_type" in
        password)
            local quoted_pw
            printf -v quoted_pw "%q" "$auth_val"
            sshpass -p "$auth_val" ssh "${_ssh_opts[@]}" -o PreferredAuthentications=password,keyboard-interactive \
                "$host" "printf '%s\n' ${quoted_pw} | sudo -S -p '' bash -lc ${quoted_cmd}" ;;
        key)
            if [[ "$auth_val" == "default" ]]; then
                ssh "${_ssh_opts[@]}" "$host" "sudo bash -lc ${quoted_cmd}"
            else
                ssh "${_ssh_opts[@]}" -i "$auth_val" "$host" "sudo bash -lc ${quoted_cmd}"
            fi ;;
    esac
}

# Resolve remote $HOME (cached per target to avoid extra SSH roundtrips)
REMOTE_HOME_CACHE=()
resolve_remote_home() {
    local i=$1
    if [[ -z "${REMOTE_HOME_CACHE[$i]:-}" ]]; then
        REMOTE_HOME_CACHE[$i]=$(ssh_target "$i" 'echo $HOME')
    fi
    echo "${REMOTE_HOME_CACHE[$i]}"
}

resolve_remote_bin() {
    local i=$1
    if [[ -n "$REMOTE_BIN" ]]; then
        echo "$REMOTE_BIN"
    else
        echo "$(resolve_remote_home "$i")/.cargo/bin"
    fi
}

resolve_remote_data() {
    local i=$1
    if [[ -n "$REMOTE_DATA" ]]; then
        echo "$REMOTE_DATA"
    else
        echo "$(resolve_remote_home "$i")/.octos"
    fi
}

# Clone profiles & prompts from a built-in Mac Mini to local tmpdir
clone_data_from_builtin() {
    local src_idx=$1
    local tmpdir=$(mktemp -d)
    echo "==> Cloning data from Mac Mini $src_idx..."

    local src_host src_pw
    eval "src_host=\$HOST_$src_idx; src_pw=\$PW_$src_idx"
    local _scp_opts=("${_ssh_opts[@]}" -o PubkeyAuthentication=no)

    # Download profiles
    mkdir -p "$tmpdir/profiles"
    sshpass -p "$src_pw" scp "${_scp_opts[@]}" \
        "$src_host:.octos/profiles/*.json" "$tmpdir/profiles/" 2>/dev/null || true
    echo "    Profiles: $(ls "$tmpdir/profiles/"*.json 2>/dev/null | wc -l | tr -d ' ') found"

    # Download prompts (use tar to avoid scp -r double-nesting)
    sshpass -p "$src_pw" ssh "${_ssh_opts[@]}" -o PubkeyAuthentication=no \
        "$src_host" "tar -cf - -C .octos prompts 2>/dev/null" \
        | tar -xf - -C "$tmpdir" 2>/dev/null || true

    # Download individual files
    for f in persona.md status_words.json; do
        sshpass -p "$src_pw" scp "${_scp_opts[@]}" \
            "$src_host:.octos/$f" "$tmpdir/" 2>/dev/null || true
    done

    echo "$tmpdir"
}

# Generate octos serve launchd plist.
# Build content locally with all values resolved, pipe to remote via stdin.
generate_plist() {
    local i=$1
    local rbin=$2
    local rdata=$3
    local port=$4
    local rhome
    rhome=$(resolve_remote_home "$i")

    echo "==> Generating octos serve launchd plist (port $port)..."

    cat <<PEOF | ssh_target "$i" "mkdir -p ~/Library/LaunchAgents && cat > ~/Library/LaunchAgents/${PLIST}.plist"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${rbin}/octos</string>
        <string>serve</string>
        <string>--port</string>
        <string>${port}</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/dev/null</string>
    <key>StandardErrorPath</key>
    <string>/dev/null</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>${rbin}:${rhome}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>${rhome}</string>
        <key>OCTOS_DATA_DIR</key>
        <string>${rdata}</string>
        <key>OCTOS_AUTH_TOKEN</key>
        <string>octos-admin-2026</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>${rhome}</string>
</dict>
</plist>
PEOF
    echo "    plist written"
}

stop_remote_caddy_services() {
    local i=$1

    echo "==> Removing stale Caddy services..."
    ssh_target "$i" 'bash -lc '"'"'
        uid=$(id -u)
        launchctl bootout "gui/${uid}/homebrew.mxcl.caddy" 2>/dev/null || true
        launchctl unload "$HOME/Library/LaunchAgents/homebrew.mxcl.caddy.plist" 2>/dev/null || true
        rm -f "$HOME/Library/LaunchAgents/homebrew.mxcl.caddy.plist"
        pkill -x caddy 2>/dev/null || true
    '"'"
    ssh_sudo_target "$i" "
        launchctl bootout system/com.caddyserver.caddy 2>/dev/null || true
        launchctl unload /Library/LaunchDaemons/com.caddyserver.caddy.plist 2>/dev/null || true
        launchctl bootout system/io.octos.caddy 2>/dev/null || true
        launchctl unload /Library/LaunchDaemons/io.octos.caddy.plist 2>/dev/null || true
        rm -f /Library/LaunchDaemons/com.caddyserver.caddy.plist /Library/LaunchDaemons/io.octos.caddy.plist
        pkill -x caddy 2>/dev/null || true
    "
}

generate_caddy_launchd_plist() {
    local i=$1
    local rdata=$2
    local rhome
    local tmp_plist="${rdata}/io.octos.caddy.plist.tmp"
    rhome=$(resolve_remote_home "$i")

    echo "==> Generating managed Caddy LaunchDaemon..."
    cat <<PEOF | ssh_target "$i" "cat > '${tmp_plist}'"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.caddy</string>
    <key>ProgramArguments</key>
    <array>
        <string>/opt/homebrew/bin/caddy</string>
        <string>run</string>
        <string>--config</string>
        <string>${rdata}/Caddyfile</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/caddy.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/caddy.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>${rhome}</string>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>${rhome}</string>
</dict>
</plist>
PEOF

    ssh_sudo_target "$i" "
        mkdir -p /Library/LaunchDaemons
        mv '${tmp_plist}' /Library/LaunchDaemons/io.octos.caddy.plist
        chown root:wheel /Library/LaunchDaemons/io.octos.caddy.plist
        chmod 644 /Library/LaunchDaemons/io.octos.caddy.plist
    "
    echo "    Caddy plist written"
}

# --- Build from octos repo ---
OCTOS_REPO="https://github.com/octos-org/octos.git"
OCTOS_BUILD_DIR="${OCTOS_BUILD_DIR:-$HOME/.cache/octos}"

if [[ "$SKIP_BUILD" == false ]]; then
    echo "==> Syncing octos repo to $OCTOS_BUILD_DIR..."
    if [ -d "$OCTOS_BUILD_DIR/.git" ]; then
        (cd "$OCTOS_BUILD_DIR" && git fetch origin && git reset --hard origin/main)
    else
        git clone "$OCTOS_REPO" "$OCTOS_BUILD_DIR"
    fi

    echo "==> Building release binaries..."
    (cd "$OCTOS_BUILD_DIR" && cargo build --release -p octos-cli --features telegram,whatsapp,feishu,twilio,wecom,api)
    (cd "$OCTOS_BUILD_DIR" && cargo build --release -p news_fetch -p deep-search -p deep-crawl -p send-email -p account-manager -p voice -p clock -p weather -p pipeline-guard -p skill-evolve)

    # Build ominix-api if source is available
    OMINIX_DIR="${OMINIX_DIR:-$HOME/home/ominix-api}"
    if [[ "$SKIP_OMINIX" == false ]] && [ -d "$OMINIX_DIR" ]; then
        echo "==> Building ominix-api..."
        (cd "$OMINIX_DIR" && cargo build --release -p ominix-api)
        codesign -s - "$OMINIX_DIR/target/release/ominix-api" 2>/dev/null || true
    fi

    echo "==> Signing binaries locally..."
    for bin in "${BINARIES[@]}"; do
        codesign -s - "$OCTOS_BUILD_DIR/target/release/$bin" 2>/dev/null || true
    done
else
    echo "==> Skipping build (--skip-build)"
    OCTOS_BUILD_DIR="${OCTOS_BUILD_DIR:-$HOME/.cache/octos}"
    OMINIX_DIR="${OMINIX_DIR:-$HOME/home/ominix-api}"
fi

# --- Pre-fetch clone data if needed ---
CLONE_TMPDIR=""
if [[ -n "$CLONE_FROM" ]]; then
    CLONE_TMPDIR=$(clone_data_from_builtin "$CLONE_FROM")
fi

# --- Deploy to each target ---
for ((i=0; i<${#DEPLOY_HOSTS[@]}; i++)); do
    REMOTE="${DEPLOY_HOSTS[$i]}"
    LABEL="${DEPLOY_LABEL[$i]}"
    RBIN=$(resolve_remote_bin "$i")
    RDATA=$(resolve_remote_data "$i")
    RHOME=$(resolve_remote_home "$i")

    echo ""
    echo "========================================"
    echo "==> Deploying to $LABEL ($REMOTE)"
    echo "==> Remote bin:  $RBIN"
    echo "==> Remote data: $RDATA"
    echo "========================================"

    # --- Factory reset (wipe everything, clean slate) ---
    if [[ "$FACTORY_RESET" == true ]]; then
        echo "==> FACTORY RESET: wiping old data and services on $LABEL..."

        # Stop all octos-related launchd services
        ssh_target "$i" "launchctl unload ~/Library/LaunchAgents/${PLIST}.plist 2>/dev/null || true"
        ssh_target "$i" "launchctl unload ~/Library/LaunchAgents/io.ominix.ominix-api.plist 2>/dev/null || true"
        stop_remote_caddy_services "$i"
        # Remove legacy plists (old names from crew era)
        ssh_target "$i" 'bash -c '"'"'
            for plist in ~/Library/LaunchAgents/io.ominix.*.plist; do
                [ -f "$plist" ] || continue
                launchctl unload "$plist" 2>/dev/null || true
                rm -f "$plist"
                echo "    Removed $(basename "$plist")"
            done
        '"'"
        # Kill any lingering processes
        ssh_target "$i" "pkill -f 'octos serve' 2>/dev/null || true; pkill -f 'octos gateway' 2>/dev/null || true; pkill -f 'crew serve' 2>/dev/null || true"
        sleep 1

        # Save profiles to local tmp if --keep-profiles
        SAVED_PROFILES_DIR=""
        if [[ "$KEEP_PROFILES" == true ]]; then
            SAVED_PROFILES_DIR=$(mktemp -d)
            echo "    Downloading profiles to preserve..."
            # Try both old (.crew) and new (.octos) data dirs; prefer .octos
            for data_candidate in "$RHOME/.octos" "$RHOME/.crew"; do
                scp_target "$i" "$REMOTE:${data_candidate}/profiles/*.json" "$SAVED_PROFILES_DIR/" 2>/dev/null && break || true
            done
            # Also save config.json
            for data_candidate in "$RHOME/.octos" "$RHOME/.crew"; do
                scp_target "$i" "$REMOTE:${data_candidate}/config.json" "$SAVED_PROFILES_DIR/" 2>/dev/null && break || true
            done
            SAVED_COUNT=$(ls "$SAVED_PROFILES_DIR"/*.json 2>/dev/null | wc -l | tr -d ' ')
            echo "    Saved $SAVED_COUNT files to local tmp"
        fi

        # Remove old data directories
        for old_dir in "$RHOME/.crew" "$RHOME/.octos"; do
            ssh_target "$i" "if [ -d '$old_dir' ]; then echo '    Removing $old_dir...'; rm -rf '$old_dir'; fi"
        done

        # Remove old binaries (both crew and octos names)
        ssh_target "$i" "rm -f '${RBIN}/crew' '${RBIN}/octos'" 2>/dev/null || true
        for bin in "${BINARIES[@]}"; do
            ssh_target "$i" "rm -f '${RBIN}/${bin}'" 2>/dev/null || true
        done
        echo "    Factory reset complete. Proceeding with clean install..."

        # Force init mode so data directory gets set up
        INIT_FRESH=true
    fi

    # Ensure remote dirs exist
    ssh_target "$i" "mkdir -p '$RBIN'"
    ssh_target "$i" "mkdir -p '$RDATA'"

    # --- Data directory setup (--init or --clone-from) ---
    if [[ "$INIT_FRESH" == true ]] || [[ -n "$CLONE_FROM" ]]; then
        # Safety check: detect existing installation
        EXISTING_PROFILES=$(ssh_target "$i" "ls '$RDATA'/profiles/*.json 2>/dev/null | wc -l | tr -d ' '" 2>/dev/null || echo "0")
        EXISTING_SESSIONS=$(ssh_target "$i" "ls '$RDATA'/sessions/ 2>/dev/null | wc -l | tr -d ' '" 2>/dev/null || echo "0")

        if [[ "$EXISTING_PROFILES" -gt 0 ]]; then
            echo ""
            echo "    ⚠  EXISTING INSTALLATION DETECTED at $RDATA"
            echo "       Profiles: $EXISTING_PROFILES, Sessions: $EXISTING_SESSIONS"
            echo ""
            echo "    Options:"
            echo "      y = Overwrite (existing profiles will be backed up to $RDATA/backup/)"
            echo "      n = Skip data setup (keep existing data, only update binaries)"
            echo "      q = Abort deploy for this target"
            echo ""
            read -rp "    Proceed with data setup? [y/n/q] " answer
            case "$answer" in
                y|Y)
                    # Backup existing data
                    BACKUP_TS=$(date +%Y%m%d_%H%M%S)
                    BACKUP_DIR="$RDATA/backup/$BACKUP_TS"
                    echo "    Backing up to $BACKUP_DIR..."
                    ssh_target "$i" "mkdir -p '$BACKUP_DIR' && \
                        cp -a '$RDATA/profiles' '$BACKUP_DIR/' 2>/dev/null; \
                        cp -a '$RDATA/prompts' '$BACKUP_DIR/' 2>/dev/null; \
                        cp '$RDATA/persona.md' '$BACKUP_DIR/' 2>/dev/null; \
                        cp '$RDATA/status_words.json' '$BACKUP_DIR/' 2>/dev/null; \
                        cp '$RDATA/cron.json' '$BACKUP_DIR/' 2>/dev/null; \
                        true"
                    echo "    Backup complete."
                    ;;
                n|N)
                    echo "    Skipping data setup, keeping existing data."
                    # Still generate plist if it doesn't exist
                    if ! ssh_target "$i" "[ -f ~/Library/LaunchAgents/${PLIST}.plist ]" 2>/dev/null; then
                        generate_plist "$i" "$RBIN" "$RDATA" "$SERVE_PORT"
                    fi
                    # Jump past the data setup block
                    SKIP_DATA_SETUP=true
                    ;;
                q|Q)
                    echo "    Aborting deploy for $LABEL."
                    continue
                    ;;
                *)
                    echo "    Invalid choice. Aborting deploy for $LABEL."
                    continue
                    ;;
            esac
        fi

        if [[ "${SKIP_DATA_SETUP:-}" != "true" ]]; then
            echo "==> Setting up data directory..."

            # Create directory structure
            ssh_target "$i" "mkdir -p '$RDATA'/{profiles,prompts,sessions,skills,media,memory,history,hooks,users,voices,research,backup}"

            if [[ -n "$CLONE_TMPDIR" ]]; then
                # Upload cloned profiles
                echo "    Uploading profiles..."
                for pfile in "$CLONE_TMPDIR"/profiles/*.json; do
                    [ -f "$pfile" ] || continue
                    pname=$(basename "$pfile")
                    echo "      $pname"
                    scp_target "$i" "$pfile" "$REMOTE:$RDATA/profiles/$pname"
                done
                # Upload cloned prompts
                if ls "$CLONE_TMPDIR"/prompts/* &>/dev/null; then
                    echo "    Uploading prompts..."
                    for pfile in "$CLONE_TMPDIR"/prompts/*; do
                        [ -f "$pfile" ] || continue
                        scp_target "$i" "$pfile" "$REMOTE:$RDATA/prompts/"
                    done
                fi
                # Upload persona.md
                if [ -f "$CLONE_TMPDIR/persona.md" ]; then
                    echo "    Uploading persona.md..."
                    scp_target "$i" "$CLONE_TMPDIR/persona.md" "$REMOTE:$RDATA/"
                fi
                # Upload status_words.json
                if [ -f "$CLONE_TMPDIR/status_words.json" ]; then
                    echo "    Uploading status_words.json..."
                    scp_target "$i" "$CLONE_TMPDIR/status_words.json" "$REMOTE:$RDATA/"
                fi
            else
                # Clean slate — create minimal cron.json
                ssh_target "$i" "[ -f '$RDATA/cron.json' ] || echo '{\"version\":1,\"jobs\":[]}' > '$RDATA/cron.json'"
                echo "    Clean slate initialized (no profiles — create with 'octos profile create')"
            fi

            # Generate octos serve launchd plist
            generate_plist "$i" "$RBIN" "$RDATA" "$SERVE_PORT"
        fi
        unset SKIP_DATA_SETUP
    fi

    # Restore saved profiles after factory reset
    if [[ -n "${SAVED_PROFILES_DIR:-}" ]] && [[ -d "${SAVED_PROFILES_DIR:-}" ]]; then
        echo "==> Restoring preserved profiles..."
        ssh_target "$i" "mkdir -p '$RDATA/profiles'"
        for pfile in "$SAVED_PROFILES_DIR"/*.json; do
            [ -f "$pfile" ] || continue
            pname=$(basename "$pfile")
            if [[ "$pname" == "config.json" ]]; then
                echo "    config.json"
                scp_target "$i" "$pfile" "$REMOTE:$RDATA/config.json"
            else
                echo "    $pname"
                scp_target "$i" "$pfile" "$REMOTE:$RDATA/profiles/$pname"
            fi
        done
        rm -rf "$SAVED_PROFILES_DIR"
        SAVED_PROFILES_DIR=""
        echo "    Profiles restored."
    fi

    echo "==> Uploading binaries..."
    for bin in "${BINARIES[@]}"; do
        echo "    $bin"
        scp_target "$i" "$OCTOS_BUILD_DIR/target/release/$bin" "$REMOTE:/tmp/${bin}.new"
    done

    # Upload ominix-api if built
    if [[ "$SKIP_OMINIX" == false ]] && [ -d "$OMINIX_DIR" ] && [ -f "$OMINIX_DIR/target/release/ominix-api" ]; then
        echo "    ominix-api"
        scp_target "$i" "$OMINIX_DIR/target/release/ominix-api" "$REMOTE:/tmp/ominix-api.new"
        if [ -f "$OMINIX_DIR/target/release/mlx.metallib" ]; then
            echo "    mlx.metallib"
            scp_target "$i" "$OMINIX_DIR/target/release/mlx.metallib" "$REMOTE:/tmp/mlx.metallib.new"
        fi
    fi

    # Upload octos-web (chat client) if dist exists
    OCTOS_WEB_DIR="${OCTOS_WEB_DIR:-$HOME/home/octos-web}"
    if [ -d "$OCTOS_WEB_DIR/dist" ]; then
        echo "==> Uploading octos-web chat client..."
        tar czf /tmp/octos-web-dist.tar.gz -C "$OCTOS_WEB_DIR/dist" .
        scp_target "$i" /tmp/octos-web-dist.tar.gz "$REMOTE:/tmp/octos-web-dist.tar.gz"
        ssh_target "$i" "mkdir -p '${RHOME}/octos-web' && tar xzf /tmp/octos-web-dist.tar.gz -C '${RHOME}/octos-web'"
        echo "    octos-web deployed to ~/octos-web"
    fi

    # Always regenerate plist to pick up env var changes
    generate_plist "$i" "$RBIN" "$RDATA" "$SERVE_PORT"

    echo "==> Stopping launchd service..."
    ssh_target "$i" "launchctl unload ~/Library/LaunchAgents/${PLIST}.plist 2>/dev/null || true"
    sleep 1
    ssh_target "$i" "pkill -f 'octos serve' 2>/dev/null || true; pkill -f 'octos gateway' 2>/dev/null || true"
    sleep 1

    # Enable admin shell API and set auth token for diagnostics
    echo "==> Patching config for admin shell..."
    ssh_target "$i" 'python3 << '"'"'PYEOF'"'"'
import json, pathlib, sys
p = pathlib.Path("'"${RDATA}"'/config.json")
print(f"config path: {p}", file=sys.stderr)
print(f"exists: {p.exists()}", file=sys.stderr)
c = json.loads(p.read_text()) if p.exists() else {}
c["allow_admin_shell"] = True
c["auth_token"] = "octos-admin-2026"
p.write_text(json.dumps(c, indent=2))
print(f"written ok, auth_token len={len(c.get(chr(97)+chr(117)+chr(116)+chr(104)+chr(95)+chr(116)+chr(111)+chr(107)+chr(101)+chr(110), chr(63)))}", file=sys.stderr)
PYEOF
'

    echo "==> Replacing binaries on remote..."
    for bin in "${BINARIES[@]}"; do
        ssh_target "$i" "mv /tmp/${bin}.new '${RBIN}/${bin}' && codesign --force -s - '${RBIN}/${bin}'"
    done

    # Replace ominix-api if uploaded
    if [[ "$SKIP_OMINIX" == false ]] && ssh_target "$i" "[ -f /tmp/ominix-api.new ]" 2>/dev/null; then
        echo "==> Replacing ominix-api on remote..."
        ssh_target "$i" "launchctl unload ~/Library/LaunchAgents/io.ominix.ominix-api.plist 2>/dev/null || true; sleep 1"
        ssh_target "$i" "mv /tmp/ominix-api.new '${RBIN}/ominix-api' && codesign --force -s - '${RBIN}/ominix-api'"
        if ssh_target "$i" "[ -f /tmp/mlx.metallib.new ]" 2>/dev/null; then
            ssh_target "$i" "mv /tmp/mlx.metallib.new '${RBIN}/mlx.metallib'"
        fi
    fi

    # (Re)generate ominix-api launchd plist with auto-detected models
    if [[ "$SKIP_OMINIX" == false ]]; then
        echo "==> Configuring ominix-api service..."
        ssh_target "$i" 'bash -c '"'"'mkdir -p ~/.ominix
REMOTE_USER_HOME="$HOME"
OMINIX_BIN="$(command -v ominix-api 2>/dev/null || echo "$HOME/.cargo/bin/ominix-api")"
if [ -d ~/.ominix/models ]; then MODELS_DIR=~/.ominix/models
elif [ -d ~/.OminiX/models ]; then MODELS_DIR=~/.OminiX/models
else MODELS_DIR=~/.ominix/models; mkdir -p "$MODELS_DIR"; fi
ASR_MODEL="$(find "$MODELS_DIR" -maxdepth 1 \( -type d -o -type l \) \( -name "Qwen3-ASR-*" -o -name "qwen3-asr-*" \) 2>/dev/null | head -1)"
# Prefer CustomVoice model (has preset speakers); fall back to any TTS model
TTS_MODEL="$(find "$MODELS_DIR" -maxdepth 1 \( -type d -o -type l \) -iname "*customvoice*" 2>/dev/null | head -1)"
[ -z "$TTS_MODEL" ] && TTS_MODEL="$(find "$MODELS_DIR" -maxdepth 1 \( -type d -o -type l \) \( -name "Qwen3-TTS-*" -o -name "qwen3-tts-*" \) 2>/dev/null | head -1)"
ARGS="        <string>$OMINIX_BIN</string>
        <string>--port</string>
        <string>8080</string>
        <string>--models-dir</string>
        <string>$MODELS_DIR</string>"
[ -n "$ASR_MODEL" ] && ARGS="$ARGS
        <string>--asr-model</string>
        <string>$ASR_MODEL</string>"
[ -n "$TTS_MODEL" ] && ARGS="$ARGS
        <string>--tts-model</string>
        <string>$TTS_MODEL</string>"
cat > ~/Library/LaunchAgents/io.ominix.ominix-api.plist << PEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.ominix.ominix-api</string>
    <key>ProgramArguments</key>
    <array>
$ARGS
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$REMOTE_USER_HOME/.ominix/api.log</string>
    <key>StandardErrorPath</key>
    <string>$REMOTE_USER_HOME/.ominix/api.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$REMOTE_USER_HOME/.local/bin:$REMOTE_USER_HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    </dict>
</dict>
</plist>
PEOF
echo "  ASR model: ${ASR_MODEL:-NOT FOUND}"
echo "  TTS model: ${TTS_MODEL:-NOT FOUND}"
echo "  ominix-api plist generated"'"'"
        ssh_target "$i" "launchctl load ~/Library/LaunchAgents/io.ominix.ominix-api.plist 2>/dev/null || true"
        echo "    ominix-api service started"
    fi

    echo "==> Ensuring macOS dependencies are installed..."
    ssh_target "$i" 'bash -c '\''
        # --- Homebrew ---
        if ! command -v brew &>/dev/null; then
            echo "  Installing Homebrew..."
            NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
            eval "$(/opt/homebrew/bin/brew shellenv)"
        else
            echo "  Homebrew: OK"
            eval "$(/opt/homebrew/bin/brew shellenv 2>/dev/null || /usr/local/bin/brew shellenv 2>/dev/null)" || true
        fi

        # --- Rust toolchain (for cargo-based skill builds) ---
        if ! command -v cargo &>/dev/null; then
            echo "  Installing Rust toolchain..."
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
            source "$HOME/.cargo/env"
        else
            echo "  Rust/cargo: OK"
        fi

        # --- CLI tools via brew ---
        # Map: brew_formula -> command_to_check
        check_and_install() {
            local formula=$1 cmd=$2
            if command -v "$cmd" &>/dev/null; then
                echo "  $cmd ($formula): OK"
            else
                echo "  Installing $formula..."
                brew install "$formula"
            fi
        }
        check_and_install ffmpeg ffmpeg
        check_and_install poppler pdftoppm
        check_and_install node node
        check_and_install git git
        check_and_install gh gh
        check_and_install docker docker

        # --- Colima (lightweight Docker runtime for macOS) ---
        if ! command -v colima &>/dev/null; then
            echo "  Installing Colima..."
            brew install colima
        else
            echo "  Colima: OK"
        fi
        # Fix Docker credential helper error
        mkdir -p "$HOME/.docker"
        if [ ! -f "$HOME/.docker/config.json" ] || grep -q '"credsStore"' "$HOME/.docker/config.json" 2>/dev/null; then
            echo '{"credsStore":""}' > "$HOME/.docker/config.json"
        fi
        # Ensure Colima is running (Docker daemon)
        if command -v colima &>/dev/null && command -v docker &>/dev/null; then
            docker context use colima 2>/dev/null || true
            if ! docker info &>/dev/null 2>&1; then
                echo "  Starting Colima..."
                colima start --cpu 2 --memory 4 --disk 20 2>/dev/null || true
                docker context use colima 2>/dev/null || true
                brew services start colima 2>/dev/null || true
            else
                echo "  Docker daemon: OK"
            fi
            # Pre-pull common sandbox images
            for img in ubuntu:24.04 python:3.12-alpine; do
                if ! docker image inspect "$img" &>/dev/null 2>&1; then
                    echo "  Pulling $img..."
                    docker pull "$img" 2>/dev/null || true
                else
                    echo "  Image $img: OK"
                fi
            done
        fi

        # --- LibreOffice (for PPTX/DOCX conversion) ---
        if [ -d "/Applications/LibreOffice.app" ] || command -v soffice &>/dev/null; then
            echo "  LibreOffice: OK"
        else
            echo "  Installing LibreOffice..."
            brew install --cask libreoffice --no-quarantine 2>/dev/null || true
        fi

        # --- Chrome/Chromium (for deep-crawl CDP) ---
        # Prefer Google Chrome over Chromium (Chromium via brew gets quarantined on macOS)
        if [ -d "/Applications/Google Chrome.app" ]; then
            echo "  Chrome/Chromium: OK (Google Chrome)"
        elif command -v chromium &>/dev/null; then
            echo "  Chrome/Chromium: OK (chromium)"
        else
            echo "  Installing Google Chrome..."
            brew install --cask google-chrome --no-quarantine 2>/dev/null || true
        fi
        # Remove broken chromium wrapper if Google Chrome is available
        if [ -d "/Applications/Google Chrome.app" ] && [ -f /opt/homebrew/bin/chromium ]; then
            rm -f /opt/homebrew/bin/chromium 2>/dev/null
        fi

        # --- Global npm packages (for PPTX skill) ---
        if command -v npm &>/dev/null; then
            NPM_PKGS=(pptxgenjs sharp react react-dom react-icons)
            for pkg in "${NPM_PKGS[@]}"; do
                if ! npm ls -g "$pkg" &>/dev/null; then
                    echo "  npm install -g $pkg..."
                    npm install -g "$pkg" 2>/dev/null || true
                else
                    echo "  npm $pkg: OK"
                fi
            done
        fi

        # --- Ensure ~/.cargo/bin is in PATH for launchd ---
        mkdir -p "$HOME/.cargo/bin"
    '\'''

    # Upload provider baseline + model catalog for adaptive router pre-seeding
    for qos_file in provider_baseline.json model_catalog.json; do
        QOS_PATH="$OCTOS_BUILD_DIR/$qos_file"
        if [ -f "$QOS_PATH" ]; then
            echo "    $qos_file"
            scp_target "$i" "$QOS_PATH" "$REMOTE:$RDATA/$qos_file"
        fi
    done

    echo "==> Cleaning stale skill dirs (bootstrap recreates them)..."
    for skill in news deep-search deep-crawl send-email account-manager voice clock weather; do
        ssh_target "$i" "rm -rf '${RDATA}/skills/${skill}'" 2>/dev/null || true
    done
    ssh_target "$i" "rm -rf '${RDATA}/bundled-app-skills' '${RDATA}/platform-skills'" 2>/dev/null || true

    # Download voice models if missing
    echo "==> Checking and downloading voice models..."
    ssh_target "$i" 'bash -c '"'"'
        # Determine models directory
        if [ -d ~/.ominix/models ]; then MODELS_DIR=~/.ominix/models
        elif [ -d ~/.OminiX/models ]; then MODELS_DIR=~/.OminiX/models
        else MODELS_DIR=~/.ominix/models; mkdir -p "$MODELS_DIR"; fi

        # Ensure huggingface-cli is available
        if ! command -v huggingface-cli &>/dev/null; then
            echo "  Installing huggingface_hub..."
            pip3 install -q huggingface_hub 2>/dev/null || pip install -q huggingface_hub 2>/dev/null || {
                echo "  ERROR: Could not install huggingface_hub. Skipping model downloads."
                exit 0
            }
        fi

        # Models to download: repo_id local_dir_name (no associative arrays — bash 3 compat)
        set -- \
            "mlx-community/Qwen3-ASR-1.7B-8bit"                        "Qwen3-ASR-1.7B-8bit" \
            "mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit"       "Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit" \
            "mlx-community/Qwen3-TTS-12Hz-1.7B-Base-8bit"              "Qwen3-TTS-12Hz-1.7B-Base-8bit"

        while [ $# -ge 2 ]; do
            repo="$1"; local_name="$2"; shift 2
            local_path="$MODELS_DIR/$local_name"
            if [ -d "$local_path" ] && [ "$(ls -A "$local_path" 2>/dev/null)" ]; then
                echo "  $local_name: already downloaded, skipping"
            else
                echo "  $local_name: downloading from $repo..."
                huggingface-cli download "$repo" --local-dir "$local_path" || {
                    echo "  WARNING: Failed to download $local_name"
                }
            fi
        done
        echo "  Voice models check complete."
    '"'"

    echo "==> Starting launchd service..."
    ssh_target "$i" "launchctl load ~/Library/LaunchAgents/${PLIST}.plist"

    echo "==> Verifying..."
    sleep 2
    ssh_target "$i" "launchctl list | grep octos || echo 'WARNING: service not found'"
    # --- Caddy setup (optional) ---
    if [[ -n "$CADDY_DOMAIN" ]]; then
        echo "==> Setting up Caddy for $CADDY_DOMAIN..."
        ssh_target "$i" 'bash -lc '"'"'
            # Install Caddy if missing
            if ! command -v caddy &>/dev/null; then
                if command -v brew &>/dev/null; then
                    brew install caddy
                elif command -v apt-get &>/dev/null; then
                    sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl
                    curl -1sLf "https://dl.cloudsmith.io/public/caddy/stable/gpg.key" | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
                    curl -1sLf "https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt" | sudo tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
                    sudo apt-get update -qq && sudo apt-get install -y caddy
                else
                    echo "ERROR: Cannot install Caddy — install manually"
                    exit 1
                fi
            fi
            echo "  Caddy: $(caddy version 2>/dev/null | head -1)"
        '"'"

        CADDY_UPSTREAM="localhost:${SERVE_PORT}"
        CADDY_LABEL_INDEX=$(awk -F. '{print NF}' <<< "$CADDY_DOMAIN")
        ssh_target "$i" "cat > '${RDATA}/Caddyfile' <<'CEOF'
{
    on_demand_tls {
        ask http://localhost:9999/check
    }
}

:9999 {
    respond /check 200
}

'"${CADDY_DOMAIN}"' {
    handle /admin/chat {
        redir / permanent
    }
    handle /api/* {
        reverse_proxy '"${CADDY_UPSTREAM}"'
    }
    handle /admin* {
        reverse_proxy '"${CADDY_UPSTREAM}"'
    }
    handle /auth/* {
        reverse_proxy '"${CADDY_UPSTREAM}"'
    }
    handle /webhook/* {
        reverse_proxy '"${CADDY_UPSTREAM}"'
    }
    handle {
        root * '"${RHOME}"'/octos-web
        try_files {path} /index.html
        file_server
    }
}

*.'"${CADDY_DOMAIN}"' {
    tls {
        on_demand
    }

    @admin_chat path /admin/chat
    @api path /api/*
    @admin path /admin*
    @auth path /auth/*
    @webhook path /webhook/*

    handle @admin_chat {
        redir / permanent
    }
    handle @api {
        reverse_proxy '"${CADDY_UPSTREAM}"' {
            header_up X-Profile-Id {labels.'"${CADDY_LABEL_INDEX}"'}
        }
    }
    handle @admin {
        reverse_proxy '"${CADDY_UPSTREAM}"' {
            header_up X-Profile-Id {labels.'"${CADDY_LABEL_INDEX}"'}
        }
    }
    handle @auth {
        reverse_proxy '"${CADDY_UPSTREAM}"' {
            header_up X-Profile-Id {labels.'"${CADDY_LABEL_INDEX}"'}
        }
    }
    handle @webhook {
        reverse_proxy '"${CADDY_UPSTREAM}"' {
            header_up X-Profile-Id {labels.'"${CADDY_LABEL_INDEX}"'}
        }
    }
    handle {
        root * '"${RHOME}"'/octos-web
        try_files {path} /index.html
        file_server
    }
}
CEOF
            caddy fmt --overwrite '${RDATA}/Caddyfile' 2>/dev/null || true
            if caddy validate --config '${RDATA}/Caddyfile' 2>/dev/null; then
                echo "  Caddyfile valid"
            else
                echo "  WARNING: Caddyfile validation failed"
                exit 1
            fi
        "
        stop_remote_caddy_services "$i"
        generate_caddy_launchd_plist "$i" "$RDATA"
        ssh_sudo_target "$i" "
            launchctl bootstrap system /Library/LaunchDaemons/io.octos.caddy.plist
            launchctl enable system/io.octos.caddy 2>/dev/null || true
            launchctl kickstart -k system/io.octos.caddy
        "
        echo "  Caddy configured for ${CADDY_DOMAIN} + *.${CADDY_DOMAIN}"
    fi

    echo "==> $LABEL deploy complete."
done

# Cleanup
if [[ -n "$CLONE_TMPDIR" ]] && [[ -d "$CLONE_TMPDIR" ]]; then
    rm -rf "$CLONE_TMPDIR"
fi

# --- Post-deploy e2e tests ---
if [[ "$RUN_TESTS" == "true" ]]; then
    E2E_DIR="$(cd "$(dirname "$0")/../e2e" && pwd)"
    if [[ ! -f "$E2E_DIR/package.json" ]]; then
        echo "⚠️  e2e/ directory not found, skipping tests"
    else
        # Install deps if needed
        if [[ ! -d "$E2E_DIR/node_modules" ]]; then
            echo "==> Installing e2e test dependencies..."
            (cd "$E2E_DIR" && npm ci --silent)
        fi

        # Map deployed hosts to test URLs
        resolve_test_url() {
            case "$1" in
                "$HOST_1") echo "https://dspfac.crew.ominix.io" ;;
                "$HOST_2") echo "https://dspfac.bot.ominix.io" ;;
                *) echo "" ;;
            esac
        }

        TESTS_PASSED=0
        TESTS_FAILED=0

        for i in "${!DEPLOY_HOSTS[@]}"; do
            host="${DEPLOY_HOSTS[$i]}"
            label="${DEPLOY_LABEL[$i]}"
            test_url="$(resolve_test_url "$host")"

            if [[ -z "$test_url" ]]; then
                echo "==> Skipping tests for $label (no test URL mapped)"
                continue
            fi

            echo ""
            echo "==> Running e2e tests against $label ($test_url)..."
            if (cd "$E2E_DIR" && OCTOS_TEST_URL="$test_url" npx playwright test web-client --reporter=list 2>&1); then
                echo "✓  $label: all tests passed"
                TESTS_PASSED=$((TESTS_PASSED + 1))
            else
                echo "✗  $label: tests failed"
                TESTS_FAILED=$((TESTS_FAILED + 1))
            fi
        done

        echo ""
        if [[ $TESTS_FAILED -gt 0 ]]; then
            echo "⚠️  Tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
        elif [[ $TESTS_PASSED -gt 0 ]]; then
            echo "✓  Tests: all $TESTS_PASSED targets passed"
        fi
    fi
fi

echo ""
echo "All deployments complete."
