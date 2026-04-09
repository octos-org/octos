#!/usr/bin/env bash
# install.sh — Install octos from pre-built binaries on a fresh machine.
# Self-contained: no repo clone, Rust, or Node.js needed.
#
# Usage:
#   curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
#   curl -fsSL ... | bash -s -- --tunnel --tenant-name alice --frps-token <token>
#
# Options:
#   --version TAG            Release version to install (default: latest)
#   --prefix DIR             Install prefix (default: ~/.octos/bin)
#   --port PORT              octos serve port (default: 8080)
#   --auth-token TOKEN       Dashboard auth token (default: auto-generated)
#   --uninstall              Remove octos and frpc services and binaries
#   --doctor                 Diagnose installation and service health
#
# Optional features:
#   --install-deps           Auto-install missing runtime dependencies
#   --caddy-domain DOMAIN    Set up Caddy with on-demand TLS for wildcard subdomains
#                            (e.g. --caddy-domain crew.example.com → *.crew.example.com)
#                            Requires: wildcard DNS A record pointing to this server
#   --tunnel                 Enable optional frpc tunnel setup
#     --tenant-name NAME     Tenant subdomain (e.g. "alice") for public access
#     --frps-token TOKEN     shared frps auth token
#     --frps-token-file FILE Read shared frps auth token from FILE
#     --frps-server ADDR     frps server address (default: 163.192.33.32)
#     --ssh-port PORT        SSH tunnel remote port (default: 6001)
#     --domain DOMAIN        Tunnel domain (default: octos-cloud.org)

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────
GITHUB_REPO="octos-org/octos"
VERSION="latest"
PREFIX="${OCTOS_PREFIX:-$HOME/.octos/bin}"
DATA_DIR="${OCTOS_HOME:-$HOME/.octos}"
FRPC_VERSION="0.65.0"

TENANT_NAME=""
FRPS_TOKEN="${FRPS_TOKEN:-}"
FRPS_TOKEN_FILE=""
FRPS_SERVER="163.192.33.32"
SSH_PORT="6001"
AUTH_TOKEN=""
TUNNEL_DOMAIN="octos-cloud.org"
ENABLE_TUNNEL=false
CADDY_DOMAIN=""
PORT="8080"
INSTALL_DEPS=false
UNINSTALL=false
RUN_DOCTOR=false

# ── Parse arguments ───────────────────────────────────────────────────
needval() {
    if [ $# -lt 2 ] || case "$2" in -*) true ;; *) false ;; esac; then
        echo "ERROR: $1 requires a value"; exit 1
    fi
}
while [ $# -gt 0 ]; do
    case "$1" in
        --version)       needval "$@"; VERSION="$2"; shift 2 ;;
        --prefix)        needval "$@"; PREFIX="$2"; shift 2 ;;
        --port)          needval "$@"; PORT="$2"; shift 2 ;;
        --auth-token)    needval "$@"; AUTH_TOKEN="$2"; shift 2 ;;
        --caddy-domain)  needval "$@"; CADDY_DOMAIN="$2"; shift 2 ;;
        --tunnel)        ENABLE_TUNNEL=true; shift ;;
        --tenant-name)   needval "$@"; TENANT_NAME="$2"; shift 2 ;;
        --frps-token)    needval "$@"; FRPS_TOKEN="$2"; ENABLE_TUNNEL=true; shift 2 ;;
        --frps-token-file) needval "$@"; FRPS_TOKEN_FILE="$2"; ENABLE_TUNNEL=true; shift 2 ;;
        --frps-server)   needval "$@"; FRPS_SERVER="$2"; shift 2 ;;
        --ssh-port)      needval "$@"; SSH_PORT="$2"; shift 2 ;;
        --domain)        needval "$@"; TUNNEL_DOMAIN="$2"; shift 2 ;;
        --install-deps)  INSTALL_DEPS=true; shift ;;
        --uninstall)     UNINSTALL=true; shift ;;
        --doctor)        RUN_DOCTOR=true; shift ;;
        --help|-h)
            cat << 'HELPEOF'
install.sh — Install octos from pre-built binaries on a fresh machine.
Self-contained: no repo clone, Rust, or Node.js needed.

Usage:
  curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
  curl -fsSL ... | bash -s -- --tunnel --tenant-name alice --frps-token <token>

Options:
  --version TAG            Release version to install (default: latest)
  --prefix DIR             Install prefix (default: ~/.octos/bin)
  --port PORT              octos serve port (default: 8080)
  --auth-token TOKEN       Dashboard auth token (default: auto-generated)
  --uninstall              Remove octos and frpc services and binaries
  --doctor                 Diagnose installation and service health

Optional features:
  --install-deps           Auto-install missing runtime dependencies
  --caddy-domain DOMAIN    Set up Caddy reverse proxy with on-demand TLS
                           (e.g. --caddy-domain crew.example.com)

Optional tunnel (frpc):
  --tunnel                 Enable optional frpc tunnel setup
  --tenant-name NAME       Tenant subdomain (e.g. "alice") for public access
  --frps-token TOKEN       shared frps auth token
  --frps-token-file FILE   Read shared frps auth token from FILE
  --frps-server ADDR       frps server address (default: 163.192.33.32)
  --ssh-port PORT          SSH tunnel remote port (default: 6001)
  --domain DOMAIN          Tunnel domain (default: octos-cloud.org)
HELPEOF
            exit 0
            ;;
        *)
            echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Validate arguments ──────────────────────────────────────────────
# Values are embedded in TOML, XML plist, and systemd unit files.
# Reject characters that would break any of those formats.
normalize_path() {
    local path="$1"
    case "$path" in
        "~")
            printf '%s\n' "$HOME"
            ;;
        "~/"*)
            printf '%s/%s\n' "$HOME" "${path#"~/"}"
            ;;
        /*)
            printf '%s\n' "$path"
            ;;
        *)
            printf '%s/%s\n' "$PWD" "$path"
            ;;
    esac
}

PREFIX="$(normalize_path "$PREFIX")"
DATA_DIR="$(normalize_path "$DATA_DIR")"

validate() {
    local name="$1" value="$2" pattern="$3"
    if ! printf '%s' "$value" | grep -qE "^${pattern}\$"; then
        echo "ERROR: invalid $name: '$value'"
        echo "       Must match: $pattern"
        exit 1
    fi
}

# Validate all non-empty user-controlled values that get embedded in config files.
# Called after CLI parsing AND after every alternate input source (token files,
# interactive prompts, existing config reads).
validate_inputs() {
    [ -n "$TENANT_NAME" ]  && validate "tenant-name" "$TENANT_NAME" '[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?'
    [ -n "$AUTH_TOKEN" ]    && validate "auth-token"  "$AUTH_TOKEN"  '[a-zA-Z0-9._-]+'
    [ -n "$FRPS_TOKEN" ]    && validate "frps-token"  "$FRPS_TOKEN"  '[a-zA-Z0-9._-]+'
    [ -n "$TUNNEL_DOMAIN" ] && validate "domain"      "$TUNNEL_DOMAIN" '[a-zA-Z0-9.-]+'
    [ -n "$FRPS_SERVER" ]   && validate "frps-server" "$FRPS_SERVER" '[a-zA-Z0-9.:-]+'
    [ -n "$SSH_PORT" ]      && validate "ssh-port"    "$SSH_PORT"    '[0-9]+'
    [ -n "$PORT" ]          && validate "port"        "$PORT"        '[0-9]+'
    [ -n "$VERSION" ] && [ "$VERSION" != "latest" ] && validate "version" "$VERSION" '[a-zA-Z0-9._-]+'
    [ -n "$PREFIX" ]        && validate "prefix"      "$PREFIX"      '/[a-zA-Z0-9/._~-]*'
    [ -n "$DATA_DIR" ]      && validate "data-dir"    "$DATA_DIR"    '/[a-zA-Z0-9/._~-]*'
    return 0
}

validate_inputs

OS="$(uname -s)"
ARCH="$(uname -m)"

xml_escape() {
    printf '%s' "$1" | sed \
        -e 's/&/\&amp;/g' \
        -e 's/</\&lt;/g' \
        -e 's/>/\&gt;/g' \
        -e 's/"/\&quot;/g' \
        -e "s/'/\&apos;/g"
}

launchd_env_var_xml() {
    local key="$1" value="$2"
    [ -n "$value" ] || return 0
    printf '        <key>%s</key>\n        <string>%s</string>\n' "$key" "$(xml_escape "$value")"
}

systemd_env_var_line() {
    local key="$1" value="$2"
    [ -n "$value" ] || return 0
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf 'Environment="%s=%s"\n' "$key" "$value"
}

section() { echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
hint()    { echo "          -> $1"; }

# ── Platform helpers ────────────────────────────────────────────────

# Print the install command for a package on the current OS.
# Usage: pkg_hint <package>
pkg_hint() {
    case "$OS" in
        Darwin)
            case "$1" in
                git)       echo "xcode-select --install" ;;
                node)      echo "brew install node" ;;
                python)    echo "brew install python" ;;
                chromium)  echo "brew install --cask google-chrome" ;;
                ffmpeg)    echo "brew install ffmpeg" ;;
            esac
            ;;
        Linux)
            # Detect package manager
            local pm=""
            if command -v apt-get &>/dev/null; then pm="apt"
            elif command -v pacman &>/dev/null; then pm="pacman"
            elif command -v dnf &>/dev/null; then pm="dnf"
            elif command -v yum &>/dev/null; then pm="yum"
            elif command -v apk &>/dev/null; then pm="apk"
            fi
            case "$pm" in
                apt)
                    case "$1" in
                        git)       echo "sudo apt-get install -y git" ;;
                        node)      echo "curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - && sudo apt-get install -y nodejs" ;;
                        python)    echo "sudo apt-get install -y python3" ;;
                        chromium)  echo "sudo apt-get install -y chromium-browser" ;;
                        ffmpeg)    echo "sudo apt-get install -y ffmpeg" ;;
                        iproute2)  echo "sudo apt-get install -y iproute2" ;;
                    esac ;;
                pacman)
                    case "$1" in
                        git)       echo "sudo pacman -S --noconfirm git" ;;
                        node)      echo "sudo pacman -S --noconfirm nodejs npm" ;;
                        python)    echo "sudo pacman -S --noconfirm python" ;;
                        chromium)  echo "sudo pacman -S --noconfirm chromium" ;;
                        ffmpeg)    echo "sudo pacman -S --noconfirm ffmpeg" ;;
                        iproute2)  echo "sudo pacman -S --noconfirm iproute2" ;;
                    esac ;;
                dnf|yum)
                    case "$1" in
                        git)       echo "sudo $pm install -y git" ;;
                        node)      echo "sudo $pm install -y nodejs npm" ;;
                        python)    echo "sudo $pm install -y python3" ;;
                        chromium)  echo "sudo $pm install -y chromium" ;;
                        ffmpeg)    echo "sudo $pm install -y ffmpeg" ;;
                        iproute2)  echo "sudo $pm install -y iproute" ;;
                    esac ;;
                apk)
                    case "$1" in
                        git)       echo "sudo apk add git" ;;
                        node)      echo "sudo apk add nodejs npm" ;;
                        python)    echo "sudo apk add python3" ;;
                        chromium)  echo "sudo apk add chromium" ;;
                        ffmpeg)    echo "sudo apk add ffmpeg" ;;
                        iproute2)  echo "sudo apk add iproute2" ;;
                    esac ;;
                *)
                    echo "install '$1' using your package manager" ;;
            esac
            ;;
        *)
            echo "(see your OS package manager)" ;;
    esac
}

# Install a package using the command from pkg_hint.
# Usage: install_pkg <package>
# Returns 0 on success, 1 on failure.
install_pkg() {
    local pkg="$1"
    local cmd
    cmd=$(pkg_hint "$pkg")
    if [ -z "$cmd" ] || [ "$cmd" = "install '$pkg' using your package manager" ] || [ "$cmd" = "(see your OS package manager)" ]; then
        warn "don't know how to install $pkg on this system"
        return 1
    fi
    echo "    Installing $pkg..."
    if eval "$cmd" >/dev/null 2>&1; then
        return 0
    else
        warn "$pkg install failed"
        hint "$cmd"
        return 1
    fi
}

# Print a service management command.
# Usage: svc_hint <start|stop|restart|status> <serve|frpc>
svc_hint() {
    local action="$1" service="$2"
    case "$OS" in
        Darwin)
            local plist="/Library/LaunchDaemons/io.octos.${service}.plist"
            case "$action" in
                start)   echo "sudo launchctl load $plist" ;;
                stop)    echo "sudo launchctl unload $plist" ;;
                restart) echo "sudo launchctl unload $plist && sudo launchctl load $plist" ;;
                status)  echo "sudo launchctl print system/io.octos.${service}" ;;
            esac
            ;;
        Linux)
            local unit="$service"
            [ "$service" = "serve" ] && unit="octos-serve"
            case "$action" in
                start)   echo "sudo systemctl start $unit" ;;
                stop)    echo "sudo systemctl stop $unit" ;;
                restart) echo "sudo systemctl restart $unit" ;;
                status)  echo "sudo systemctl status $unit" ;;
            esac
            ;;
        *)
            echo "# service management not supported on $OS" ;;
    esac
}

# Write frpc.toml to /etc/frp/frpc.toml.
# Uses globals: FRPS_SERVER, FRPS_TOKEN, TENANT_NAME, TUNNEL_DOMAIN, SSH_PORT
write_frpc_config() {
    local tmp
    tmp=$(mktemp /tmp/frpc.toml.XXXXXX)
    cat > "$tmp" << EOF
serverAddr = "${FRPS_SERVER}"
serverPort = 7000
auth.method = "token"
auth.token = "${FRPS_TOKEN}"
log.to = "/var/log/frpc.log"
log.level = "info"
log.maxDays = 7

[[proxies]]
name = "${TENANT_NAME}-web"
type = "http"
localPort = ${PORT}
customDomains = ["${TENANT_NAME}.${TUNNEL_DOMAIN}"]

[[proxies]]
name = "${TENANT_NAME}-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = ${SSH_PORT}
EOF
    sudo mkdir -p /etc/frp
    sudo mv "$tmp" /etc/frp/frpc.toml
    sudo chown root:root /etc/frp/frpc.toml 2>/dev/null || sudo chown root:wheel /etc/frp/frpc.toml
    sudo chmod 600 /etc/frp/frpc.toml
}

# Download and install frpc binary to /usr/local/bin.
# Uses globals: FRPC_VERSION, FRP_ARCH, OS
install_frpc_binary() {
    if [ -f /usr/local/bin/frpc ]; then
        ok "frpc already installed ($(/usr/local/bin/frpc --version 2>/dev/null || echo 'unknown'))"
        return 0
    fi
    echo "    Installing frpc v${FRPC_VERSION}..."
    case "$FRP_ARCH" in
        amd64|arm64) ;; # ok
        *) err "Unsupported frpc architecture: $FRP_ARCH" ; return 1 ;;
    esac
    local frp_os frp_tarball frp_url frp_tmp
    frp_os=$(echo "$OS" | tr '[:upper:]' '[:lower:]')
    frp_tarball="frp_${FRPC_VERSION}_${frp_os}_${FRP_ARCH}.tar.gz"
    frp_url="https://github.com/fatedier/frp/releases/download/v${FRPC_VERSION}/${frp_tarball}"
    frp_tmp=$(mktemp -d /tmp/frpc-install.XXXXXX)
    curl -fsSL -o "${frp_tmp}/${frp_tarball}" "$frp_url"
    tar -xzf "${frp_tmp}/${frp_tarball}" -C "$frp_tmp"
    sudo mkdir -p /usr/local/bin
    sudo cp "${frp_tmp}/frp_${FRPC_VERSION}_${frp_os}_${FRP_ARCH}/frpc" /usr/local/bin/frpc
    sudo chmod 0755 /usr/local/bin/frpc
    rm -rf "$frp_tmp"
    ok "frpc installed"
}

# Write and load the frpc system service (plist on Darwin, systemd on Linux).
# Idempotent: unloads existing service before reloading.
write_frpc_service() {
    case "$OS" in
        Darwin)
            local plist="/Library/LaunchDaemons/io.octos.frpc.plist"
            local tmp
            tmp=$(mktemp /tmp/io.octos.frpc.plist.XXXXXX)
            cat > "$tmp" << 'PLIST_EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.frpc</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/frpc</string>
        <string>-c</string>
        <string>/etc/frp/frpc.toml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/frpc.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/frpc.log</string>
</dict>
</plist>
PLIST_EOF
            # Clean up legacy LaunchAgent
            launchctl unload "$HOME/Library/LaunchAgents/io.octos.frpc.plist" 2>/dev/null || true
            rm -f "$HOME/Library/LaunchAgents/io.octos.frpc.plist"
            sudo launchctl unload "$plist" 2>/dev/null || true
            sudo mv "$tmp" "$plist"
            sudo chown root:wheel "$plist"
            sudo chmod 644 "$plist"
            sudo launchctl load "$plist"
            ;;
        Linux)
            local unit="/etc/systemd/system/frpc.service"
            local tmp
            tmp=$(mktemp /tmp/frpc.service.XXXXXX)
            cat > "$tmp" << 'UNIT_EOF'
[Unit]
Description=frpc tunnel client for octos
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/frpc -c /etc/frp/frpc.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT_EOF
            sudo mv "$tmp" "$unit"
            sudo chown root:root "$unit"
            sudo chmod 644 "$unit"
            sudo systemctl daemon-reload
            sudo systemctl enable frpc
            sudo systemctl restart frpc
            ;;
        *)
            warn "frpc service setup not supported on $OS"
            return 1
            ;;
    esac
}

# Write and load the octos serve system service (plist on Darwin, systemd on Linux).
# Uses globals: OCTOS_BIN, AUTH_TOKEN, DATA_DIR, PREFIX, HOME
write_octos_service() {
    case "$OS" in
        Darwin)
            # Clean up legacy LaunchAgents (old names that conflict with port 8080)
            local legacy
            for legacy in \
                "$HOME/Library/LaunchAgents/io.octos.octos-serve.plist" \
                "$HOME/Library/LaunchAgents/io.octos.serve.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.crew-serve.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.ominix-api.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.octos-serve.plist"; do
                if [ -f "$legacy" ]; then
                    launchctl unload "$legacy" 2>/dev/null || true
                    rm -f "$legacy"
                fi
            done

            local plist="/Library/LaunchDaemons/io.octos.serve.plist"
            local tmp
            tmp=$(mktemp /tmp/io.octos.serve.plist.XXXXXX)
            cat > "$tmp" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.serve</string>
    <key>ProgramArguments</key>
    <array>
        <string>$OCTOS_BIN</string>
        <string>serve</string>
        <string>--port</string>
        <string>$PORT</string>
        <string>--host</string>
        <string>0.0.0.0</string>
        <string>--auth-token</string>
        <string>$AUTH_TOKEN</string>
    </array>
    <key>UserName</key>
    <string>$(whoami)</string>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$DATA_DIR/serve.log</string>
    <key>StandardErrorPath</key>
    <string>$DATA_DIR/serve.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$PREFIX:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>$HOME</string>
        <key>OCTOS_DATA_DIR</key>
        <string>$DATA_DIR</string>
    <key>OCTOS_AUTH_TOKEN</key>
    <string>$AUTH_TOKEN</string>
$(launchd_env_var_xml "FRPS_TOKEN" "${FRPS_TOKEN:-}")
$(launchd_env_var_xml "SMTP_HOST" "${SMTP_HOST:-}")
$(launchd_env_var_xml "SMTP_PORT" "${SMTP_PORT:-}")
$(launchd_env_var_xml "SMTP_USERNAME" "${SMTP_USERNAME:-}")
$(launchd_env_var_xml "SMTP_PASSWORD" "${SMTP_PASSWORD:-}")
$(launchd_env_var_xml "SMTP_FROM" "${SMTP_FROM:-}")
    </dict>
    <key>WorkingDirectory</key>
    <string>$HOME</string>
</dict>
</plist>
EOF
            echo "    (sudo is needed to install and start the system service)"
            sudo launchctl unload "$plist" 2>/dev/null || true
            sudo mv "$tmp" "$plist"
            sudo chown root:wheel "$plist"
            sudo chmod 644 "$plist"
            sudo launchctl load "$plist"
            ok "octos serve started via launchd"
            ;;

        Linux)
            local unit="/etc/systemd/system/octos-serve.service"
            local tmp
            tmp=$(mktemp /tmp/octos-serve.service.XXXXXX)
            cat > "$tmp" << EOF
[Unit]
Description=octos serve (dashboard + gateway)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$(whoami)
ExecStart=$OCTOS_BIN serve --port $PORT --host 0.0.0.0 --auth-token $AUTH_TOKEN
Restart=on-failure
RestartSec=5
Environment=HOME=$HOME
Environment=OCTOS_DATA_DIR=$DATA_DIR
Environment=OCTOS_HOME=$DATA_DIR
Environment=OCTOS_AUTH_TOKEN=$AUTH_TOKEN
Environment=PATH=$PREFIX:/usr/local/bin:/usr/bin:/bin
$(systemd_env_var_line "FRPS_TOKEN" "${FRPS_TOKEN:-}")
$(systemd_env_var_line "SMTP_HOST" "${SMTP_HOST:-}")
$(systemd_env_var_line "SMTP_PORT" "${SMTP_PORT:-}")
$(systemd_env_var_line "SMTP_USERNAME" "${SMTP_USERNAME:-}")
$(systemd_env_var_line "SMTP_PASSWORD" "${SMTP_PASSWORD:-}")
$(systemd_env_var_line "SMTP_FROM" "${SMTP_FROM:-}")
WorkingDirectory=$HOME

[Install]
WantedBy=multi-user.target
EOF
            echo "    (sudo is needed to install and start the system service)"
            sudo mv "$tmp" "$unit"
            sudo chown root:root "$unit"
            sudo chmod 644 "$unit"
            sudo systemctl daemon-reload
            sudo systemctl enable octos-serve
            sudo systemctl restart octos-serve
            ok "octos serve started via systemd"
            ;;

        *)
            warn "octos serve service setup not supported on $OS"
            hint "Run manually: $OCTOS_BIN serve --port $PORT --host 0.0.0.0 --auth-token $AUTH_TOKEN"
            ;;
    esac
}

# Stop and remove all octos system services (octos serve + frpc).
uninstall_services() {
    case "$OS" in
        Darwin)
            sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist 2>/dev/null || true
            sudo rm -f /Library/LaunchDaemons/io.octos.serve.plist
            sudo launchctl unload /Library/LaunchDaemons/io.octos.frpc.plist 2>/dev/null || true
            sudo rm -f /Library/LaunchDaemons/io.octos.frpc.plist
            # Clean up legacy LaunchAgents
            launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.octos.serve.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.octos.frpc.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.ominix.crew-serve.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.ominix.ominix-api.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.ominix.octos-serve.plist 2>/dev/null || true
            rm -f ~/Library/LaunchAgents/io.octos.*.plist
            rm -f ~/Library/LaunchAgents/io.ominix.*.plist
            ok "launchd services removed"
            ;;
        Linux)
            sudo systemctl stop octos-serve.service 2>/dev/null || true
            sudo systemctl disable octos-serve.service 2>/dev/null || true
            sudo rm -f /etc/systemd/system/octos-serve.service
            sudo systemctl stop frpc.service 2>/dev/null || true
            sudo systemctl disable frpc.service 2>/dev/null || true
            sudo rm -f /etc/systemd/system/frpc.service
            sudo systemctl daemon-reload 2>/dev/null || true
            ok "systemd services removed"
            ;;
        *)
            warn "service removal not supported on $OS — remove services manually"
            ;;
    esac
}

# Detect the installed octos serve port from the service definition.
# Falls back to the current PORT value when no installed service is present.
detect_installed_port() {
    local detected=""
    case "$OS" in
        Darwin)
            local plist="/Library/LaunchDaemons/io.octos.serve.plist"
            if [ -f "$plist" ]; then
                detected=$(grep -A1 '>--port<' "$plist" 2>/dev/null | tail -1 | sed 's/.*<string>\(.*\)<\/string>.*/\1/')
            fi
            ;;
        Linux)
            local unit="/etc/systemd/system/octos-serve.service"
            if [ -f "$unit" ]; then
                detected=$(grep 'ExecStart=' "$unit" 2>/dev/null | sed -n 's/.*--port \([0-9]*\).*/\1/p')
            fi
            ;;
    esac

    if [ -n "$detected" ]; then
        printf '%s\n' "$detected"
    else
        printf '%s\n' "$PORT"
    fi
}

# err() exits during install but not during doctor
if [ "$RUN_DOCTOR" = true ]; then
    DOCTOR_ISSUES=0
    err() { echo "    FAIL: $1"; DOCTOR_ISSUES=$((DOCTOR_ISSUES + 1)); }
else
    err() { echo "    ERROR: $1"; echo ""; echo "    Run with --doctor to diagnose:"; echo "      curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash -s -- --doctor"; exit 1; }
fi

# ══════════════════════════════════════════════════════════════════════
# ── Doctor mode ──────────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
if [ "$RUN_DOCTOR" = true ]; then

    # Auto-detect port from installed service config (unless user passed --port)
    if [ "$PORT" = "8080" ]; then
        PORT="$(detect_installed_port)"
    fi

    # ── Binary ───────────────────────────────────────────────────────
    section "octos binary"

    OCTOS_BIN="$PREFIX/octos"
    if [ -f "$OCTOS_BIN" ]; then
        ok "found: $OCTOS_BIN"
        if "$OCTOS_BIN" --version &>/dev/null; then
            ok "version: $("$OCTOS_BIN" --version 2>&1 | head -1)"
        else
            err "binary exists but failed to run"
            if [ "$OS" = "Darwin" ]; then
                hint "Try: xattr -d com.apple.quarantine $OCTOS_BIN && codesign -s - $OCTOS_BIN"
            else
                hint "Try: chmod +x $OCTOS_BIN"
                hint "Check dependencies: ldd $OCTOS_BIN"
            fi
            hint "Or re-run install.sh"
        fi
    else
        if command -v octos &>/dev/null; then
            FOUND="$(command -v octos)"
            warn "not found at $OCTOS_BIN, but found at $FOUND"
            hint "Set OCTOS_PREFIX or add $PREFIX to PATH"
        else
            err "octos binary not found"
            hint "Run install.sh to install"
        fi
    fi

    # ── Data directory ───────────────────────────────────────────────
    section "Data directory"

    if [ -d "$DATA_DIR" ]; then
        ok "found: $DATA_DIR"
        if [ -f "$DATA_DIR/config.json" ]; then
            ok "config.json exists"
        else
            warn "config.json missing"
            hint "Run: octos init"
        fi
    else
        err "$DATA_DIR does not exist"
        hint "Run: octos init --defaults"
    fi

    # ── octos serve process ──────────────────────────────────────────
    section "octos serve"

    OCTOS_PID=$(pgrep -f "octos serve" 2>/dev/null | head -1 || true)
    if [ -n "$OCTOS_PID" ]; then
        OCTOS_CMD=$(ps -p "$OCTOS_PID" -o args= 2>/dev/null || true)
        ok "running (PID: $OCTOS_PID)"
        echo "    CMD: $OCTOS_CMD"
    else
        err "octos serve is not running"
        hint "Start: $(svc_hint start serve)"
        hint "Or manually: $PREFIX/octos serve --port $PORT --host 0.0.0.0"
    fi

    # ── Port check ───────────────────────────────────────────────────
    section "Port $PORT"

    # Detect port listener using available tool (lsof, ss, or netstat)
    PORT_CMD=""
    PORT_PID=""
    PORT_CHECK_AVAILABLE=false
    if command -v lsof &>/dev/null; then
        PORT_CHECK_AVAILABLE=true
        PORT_OWNER=$(lsof -i :$PORT -P -n 2>/dev/null | grep LISTEN | head -1 || true)
        if [ -n "$PORT_OWNER" ]; then
            PORT_CMD=$(echo "$PORT_OWNER" | awk '{print $1}')
            PORT_PID=$(echo "$PORT_OWNER" | awk '{print $2}')
        fi
    elif command -v ss &>/dev/null; then
        PORT_CHECK_AVAILABLE=true
        PORT_OWNER=$(ss -tlnp "sport = :$PORT" 2>/dev/null | tail -n +2 | head -1 || true)
        if [ -n "$PORT_OWNER" ]; then
            # ss output: users:(("octos",pid=1234,fd=5))
            PORT_CMD=$(echo "$PORT_OWNER" | sed -n 's/.*users:(("\([^"]*\)".*/\1/p')
            PORT_PID=$(echo "$PORT_OWNER" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')
        fi
    elif command -v netstat &>/dev/null; then
        PORT_CHECK_AVAILABLE=true
        PORT_OWNER=$(netstat -tlnp 2>/dev/null | grep ":$PORT " | head -1 || true)
        if [ -n "$PORT_OWNER" ]; then
            # netstat output: ... 1234/octos
            PORT_PID=$(echo "$PORT_OWNER" | awk '{print $NF}' | cut -d/ -f1)
            PORT_CMD=$(echo "$PORT_OWNER" | awk '{print $NF}' | cut -d/ -f2)
        fi
    else
        warn "cannot check port $PORT (none of lsof, ss, or netstat found)"
        _iproute_hint=$(pkg_hint iproute2)
        if [ -n "$_iproute_hint" ]; then
            hint "Install one: $_iproute_hint   # provides ss"
        fi
    fi

    if [ -n "$PORT_CMD" ]; then
        if echo "$PORT_CMD" | grep -qi octos; then
            ok "port $PORT held by octos (PID: $PORT_PID)"
        else
            err "port $PORT held by $PORT_CMD (PID: $PORT_PID) — not octos"
            hint "Kill it: kill $PORT_PID"
            if [ "$OS" = "Darwin" ]; then
                hint "If it respawns, find its LaunchAgent/Daemon:"
                hint "  grep -rl '$PORT_CMD' ~/Library/LaunchAgents/ /Library/LaunchDaemons/ 2>/dev/null"
            fi
        fi
    elif [ "$PORT_CHECK_AVAILABLE" = true ]; then
        if [ -n "$OCTOS_PID" ]; then
            err "octos serve is running but nothing is listening on $PORT"
            hint "Check if it's bound to a different port: ps -p $OCTOS_PID -o args="
        else
            warn "nothing listening on port $PORT"
        fi
    fi

    # ── Admin portal ─────────────────────────────────────────────────
    section "Admin portal"

    HTTP_CODE=$(curl -sf -o /dev/null -w "%{http_code}" --max-time 3 "http://localhost:${PORT}/admin/" 2>/dev/null || echo "000")
    case "$HTTP_CODE" in
        200)
            ok "http://localhost:${PORT}/admin/ responds 200"
            ;;
        000)
            err "connection failed (server not reachable on localhost:${PORT})"
            hint "Check 'octos serve' and 'Port ${PORT}' sections above"
            ;;
        401|403)
            warn "responds $HTTP_CODE (auth required)"
            hint "Pass auth token: curl -H 'Authorization: Bearer <token>' http://localhost:${PORT}/admin/"
            ;;
        404)
            err "responds 404 (admin route not found)"
            hint "Binary may be built without 'api' feature. Rebuild with: cargo build --features api"
            ;;
        *)
            warn "responds HTTP $HTTP_CODE"
            hint "Check logs: tail -20 $DATA_DIR/serve.log"
            ;;
    esac

    # ── Service configuration ────────────────────────────────────────
    section "Service configuration"

    case "$OS" in
        Darwin)
            PLIST="/Library/LaunchDaemons/io.octos.serve.plist"
            if [ -f "$PLIST" ]; then
                ok "LaunchDaemon plist exists: $PLIST"
                # Avoid sudo during diagnostics — check if the process is running instead
                if pgrep -f "octos serve" &>/dev/null; then
                    ok "service appears loaded (process running)"
                else
                    warn "plist exists but service does not appear to be running"
                    hint "Check: $(svc_hint status serve)"
                    hint "Load:  $(svc_hint start serve)"
                fi
            else
                warn "no LaunchDaemon plist found"
                hint "Re-run install.sh to set up the service"
            fi

            # Check for legacy/conflicting plists
            LEGACY_FOUND=false
            for p in \
                "$HOME/Library/LaunchAgents/io.octos.octos-serve.plist" \
                "$HOME/Library/LaunchAgents/io.octos.serve.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.crew-serve.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.ominix-api.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.octos-serve.plist"; do
                if [ -f "$p" ]; then
                    err "legacy plist found: $p"
                    hint "Remove: launchctl unload '$p' && rm -f '$p'"
                    LEGACY_FOUND=true
                fi
            done
            if [ "$LEGACY_FOUND" = false ]; then
                ok "no legacy/conflicting plists"
            fi
            ;;

        Linux)
            UNIT="/etc/systemd/system/octos-serve.service"
            if [ -f "$UNIT" ]; then
                ok "systemd unit exists: $UNIT"
                if systemctl is-active octos-serve &>/dev/null; then
                    ok "service is active"
                else
                    warn "service is not active"
                    hint "Start: $(svc_hint start serve)"
                    hint "Check: $(svc_hint status serve)"
                fi
            else
                warn "no systemd unit found"
                hint "Re-run install.sh to set up the service"
            fi
            ;;
        *)
            warn "service configuration check not supported on $OS"
            ;;
    esac

    # ── frpc tunnel (only if tunnel was ever configured) ───────────
    if [ -f /usr/local/bin/frpc ] || [ -f /etc/frp/frpc.toml ]; then
        section "frpc tunnel"

        TENANT=""
        if [ -f /usr/local/bin/frpc ]; then
            ok "frpc installed: $(/usr/local/bin/frpc --version 2>/dev/null || echo 'unknown version')"
        else
            warn "frpc binary not found"
            hint "Re-run install.sh with --tunnel --tenant-name <name> --frps-token <token>"
        fi

        FRPC_PID=$(pgrep -x frpc 2>/dev/null || true)
        if [ -n "$FRPC_PID" ]; then
            ok "frpc running (PID: $FRPC_PID)"
        else
            if [ -f /usr/local/bin/frpc ]; then
                err "frpc installed but not running"
                hint "Start: $(svc_hint start frpc)"
            fi
        fi

        if [ -f /etc/frp/frpc.toml ]; then
            ok "frpc config: /etc/frp/frpc.toml"
            TENANT=$(grep 'customDomains' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*\["\(.*\)"\].*/\1/')
            if [ -n "$TENANT" ]; then
                echo "    Tunnel: https://$TENANT"
            fi
            # Check for placeholder values
            if grep -q 'CHANGE_ME' /etc/frp/frpc.toml 2>/dev/null; then
                warn "frpc config contains placeholder token (CHANGE_ME)"
                hint "Update: sudo nano /etc/frp/frpc.toml"
                hint "Or re-run: bash install.sh --tenant-name <name> --frps-token <token>"
            fi
        elif [ -f /usr/local/bin/frpc ]; then
            warn "frpc installed but no config at /etc/frp/frpc.toml"
            hint "Re-run install.sh with --tenant-name and --frps-token"
        fi

        # Check frpc logs for errors
        if [ -f /var/log/frpc.log ]; then
            FRPC_ERRORS=$(tail -20 /var/log/frpc.log 2>/dev/null | grep -i "error\|failed\|refused" | tail -3)
            if [ -n "$FRPC_ERRORS" ]; then
                warn "recent frpc errors:"
                echo "$FRPC_ERRORS" | while read -r line; do echo "      $line"; done
                hint "Full log: tail -50 /var/log/frpc.log"
            fi
        fi

        # ── Remote access ────────────────────────────────────────────
        section "Remote access"

        ADMIN_OK=false
        [ "$HTTP_CODE" = "200" ] && ADMIN_OK=true

        FRPC_OK=false
        [ -n "$FRPC_PID" ] && FRPC_OK=true

        if [ "$ADMIN_OK" = true ] && [ "$FRPC_OK" = true ]; then
            ok "admin portal works locally and frpc tunnel is running"
            if [ -n "$TENANT" ]; then
                echo "    Remote URL: https://$TENANT"
            fi
        elif [ "$ADMIN_OK" = true ] && [ "$FRPC_OK" = false ]; then
            err "admin portal works locally but frpc is NOT running — remote access is down"
            if [ ! -f /usr/local/bin/frpc ]; then
                hint "frpc binary missing — reinstall the tunnel:"
                hint "  Re-run install.sh with --tunnel --tenant-name <name> --frps-token <token>"
            elif [ ! -f /etc/frp/frpc.toml ]; then
                hint "frpc is installed but not configured"
                hint "  Re-run install.sh with --tenant-name <name> --frps-token <token>"
            else
                hint "frpc is installed and configured but the process is not running"
                hint "  Start: $(svc_hint start frpc)"
            fi
        elif [ "$ADMIN_OK" = false ]; then
            err "admin portal is not responding locally — fix octos serve first (see above)"
            hint "Remote access depends on the local server working first"
        fi
    fi

    # ── Serve logs ───────────────────────────────────────────────────
    section "Recent serve logs"

    SERVE_LOG="$DATA_DIR/serve.log"
    if [ -f "$SERVE_LOG" ]; then
        SERVE_ERRORS=$(tail -30 "$SERVE_LOG" 2>/dev/null | grep -i "error\|panic\|Address already in use" | tail -5)
        if [ -n "$SERVE_ERRORS" ]; then
            warn "recent errors in serve.log:"
            echo "$SERVE_ERRORS" | while read -r line; do echo "      $line"; done
        else
            ok "no recent errors in serve.log"
        fi
        echo "    Last 3 lines:"
        tail -3 "$SERVE_LOG" 2>/dev/null | while read -r line; do echo "      $line"; done
    else
        warn "serve.log not found at $SERVE_LOG"
    fi

    # ── Runtime dependencies ─────────────────────────────────────────
    section "Runtime dependencies"

    command -v git &>/dev/null && ok "git $(git --version | awk '{print $3}')" || warn "git not found"
    command -v node &>/dev/null && ok "Node.js $(node --version)" || warn "Node.js not found (optional)"
    command -v python3 &>/dev/null && ok "Python $(python3 --version 2>&1 | awk '{print $2}')" || { command -v python &>/dev/null && ok "Python $(python --version 2>&1 | awk '{print $2}')" || warn "Python not found"; }
    command -v ffmpeg &>/dev/null && ok "ffmpeg found" || warn "ffmpeg not found (optional)"

    CHROME_FOUND=false
    for chrome_bin in "google-chrome" "google-chrome-stable" "chromium-browser" "chromium"; do
        if command -v "$chrome_bin" &>/dev/null; then
            ok "Browser: $chrome_bin"
            CHROME_FOUND=true
            break
        fi
    done
    if [ "$CHROME_FOUND" = false ] && [ "$OS" = "Darwin" ]; then
        for app in "/Applications/Google Chrome.app" "/Applications/Chromium.app"; do
            if [ -d "$app" ]; then
                ok "Browser: $app"
                CHROME_FOUND=true
                break
            fi
        done
    fi
    [ "$CHROME_FOUND" = false ] && warn "Chromium/Chrome not found (optional)"

    # ── Summary ──────────────────────────────────────────────────────
    section "Summary"
    if [ "$DOCTOR_ISSUES" -eq 0 ]; then
        echo "    All checks passed. Everything looks healthy."
    else
        echo "    Found $DOCTOR_ISSUES issue(s). Review the hints above to fix them."
    fi
    echo ""
    exit 0
fi

# ══════════════════════════════════════════════════════════════════════
# ── Uninstall mode ───────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
if [ "$UNINSTALL" = true ]; then
    section "Uninstalling octos"

    if [ "$PORT" = "8080" ]; then
        PORT="$(detect_installed_port)"
    fi

    uninstall_services

    rm -rf "$PREFIX"
    sudo rm -f /usr/local/bin/frpc
    sudo rm -rf /etc/frp
    ok "binaries and config removed"

    # Stop and clean up Caddy
    HAD_CADDY=false
    [ -f "$DATA_DIR/Caddyfile" ] && HAD_CADDY=true
    if pgrep -x caddy > /dev/null 2>&1; then
        caddy stop 2>/dev/null || true
        ok "stopped Caddy"
    fi
    if [ -f "$DATA_DIR/Caddyfile" ]; then
        rm -f "$DATA_DIR/Caddyfile"
        ok "removed Caddyfile"
    fi

    # Remove firewall rules (serve-port rule may exist from older installs)
    if [ "$OS" = "Linux" ]; then
        _fw_ok=true
        if command -v ufw &>/dev/null; then
            sudo ufw delete allow "$PORT/tcp" 2>/dev/null || true
            if [ "$HAD_CADDY" = true ]; then
                sudo ufw delete allow 80/tcp 2>/dev/null || _fw_ok=false
                sudo ufw delete allow 443/tcp 2>/dev/null || _fw_ok=false
            fi
        elif command -v firewall-cmd &>/dev/null; then
            sudo firewall-cmd --permanent --remove-port="${PORT}/tcp" 2>/dev/null || true
            if [ "$HAD_CADDY" = true ]; then
                sudo firewall-cmd --permanent --remove-port=80/tcp 2>/dev/null || _fw_ok=false
                sudo firewall-cmd --permanent --remove-port=443/tcp 2>/dev/null || _fw_ok=false
            fi
            sudo firewall-cmd --reload 2>/dev/null || _fw_ok=false
        fi
        if [ "$_fw_ok" = true ]; then
            ok "removed firewall rules"
        else
            warn "failed to remove some firewall rules (check privileges)"
        fi
    fi
    if [ "${INSTALL_SUPPRESS_DATA_DIR_HINT:-}" != "1" ]; then
        echo ""
        echo "    Data directory ($DATA_DIR) was NOT removed. Delete manually if desired:"
        echo "      rm -rf $DATA_DIR"
    fi
    exit 0
fi

# ── Resolve --frps-token-file early (before tunnel-only check) ────────
if [ -z "$FRPS_TOKEN" ] && [ -n "$FRPS_TOKEN_FILE" ]; then
    if [ -f "$FRPS_TOKEN_FILE" ]; then
        FRPS_TOKEN=$(cat "$FRPS_TOKEN_FILE")
        validate_inputs
    else
        err "token file not found: $FRPS_TOKEN_FILE"
    fi
fi

# ══════════════════════════════════════════════════════════════════════
# ── Tunnel-only update (when octos is already installed) ─────────────
# ══════════════════════════════════════════════════════════════════════
# If octos binary exists and tunnel is explicitly enabled,
# skip the full install and just update the tunnel configuration.

if [ -f "$PREFIX/octos" ] && [ "$ENABLE_TUNNEL" = true ]; then
    section "Updating tunnel configuration"

    # Fill in missing values from existing frpc config
    if [ -f /etc/frp/frpc.toml ]; then
        if [ -z "$TENANT_NAME" ]; then
            TENANT_NAME=$(grep 'customDomains' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*\["\(.*\)\..*"\].*/\1/')
            if [ -n "$TENANT_NAME" ]; then
                ok "tenant name from existing config: $TENANT_NAME"
            fi
        fi
        if [ "$TUNNEL_DOMAIN" = "octos-cloud.org" ]; then
            EXISTING_TUNNEL_DOMAIN=$(grep 'customDomains' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*\["[^"]*\.\([^"]*\)"\].*/\1/')
            if [ -n "$EXISTING_TUNNEL_DOMAIN" ] && [ "$EXISTING_TUNNEL_DOMAIN" != "octos-cloud.org" ]; then
                TUNNEL_DOMAIN="$EXISTING_TUNNEL_DOMAIN"
                ok "tunnel domain from existing config: $TUNNEL_DOMAIN"
            fi
        fi
        if [ -z "$FRPS_TOKEN" ]; then
            FRPS_TOKEN=$(grep 'auth.token' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*= *"\(.*\)"/\1/')
            if [ -n "$FRPS_TOKEN" ]; then
                ok "shared frps token from existing config: ${FRPS_TOKEN:0:8}..."
            fi
        fi
        if [ "$FRPS_SERVER" = "163.192.33.32" ]; then
            EXISTING_FRPS_SERVER=$(grep 'serverAddr' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*= *"\(.*\)"/\1/')
            if [ -n "$EXISTING_FRPS_SERVER" ] && [ "$EXISTING_FRPS_SERVER" != "163.192.33.32" ]; then
                FRPS_SERVER="$EXISTING_FRPS_SERVER"
                ok "frps server from existing config: $FRPS_SERVER"
            fi
        fi
        if [ "$SSH_PORT" = "6001" ]; then
            EXISTING_SSH_PORT=$(grep 'remotePort' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*= *//')
            if [ -n "$EXISTING_SSH_PORT" ] && [ "$EXISTING_SSH_PORT" != "6001" ]; then
                SSH_PORT="$EXISTING_SSH_PORT"
                ok "ssh port from existing config: $SSH_PORT"
            fi
        fi
    fi

    # Still missing? Prompt interactively
    if [ -z "$TENANT_NAME" ]; then
        echo "    Enter the tenant subdomain (e.g. 'alice' for alice.${TUNNEL_DOMAIN}):"
        printf "    > "
        read -r TENANT_NAME < /dev/tty
        [ -z "$TENANT_NAME" ] && err "Tenant name is required"
    fi
    if [ -z "$FRPS_TOKEN" ]; then
        echo "    Enter the shared frps auth token:"
        printf "    > "
        read -r FRPS_TOKEN < /dev/tty
        [ -z "$FRPS_TOKEN" ] && err "shared frps token is required"
    fi

    validate_inputs

    # Detect architecture for frpc download
    case "$ARCH" in
        x86_64)        FRP_ARCH="amd64" ;;
        aarch64|arm64) FRP_ARCH="arm64" ;;
        *)             err "Unsupported architecture for frpc: $ARCH" ;;
    esac

    echo ""
    echo "    Tunnel configuration:"
    echo "      Tenant:       ${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo "      frps server:  ${FRPS_SERVER}:7000"
    echo "      shared frps token: ${FRPS_TOKEN:0:8}..."
    echo "      SSH port:     ${SSH_PORT}"

    # Install frpc if missing
    install_frpc_binary

    # Write frpc config
    write_frpc_config
    ok "frpc config updated"

    # Restart frpc service
    echo "    (sudo is needed to install the frpc system service)"
    write_frpc_service
    ok "frpc restarted"

    # Verify
    sleep 2
    if pgrep -x frpc > /dev/null 2>&1; then
        ok "frpc is running (PID: $(pgrep -x frpc))"
    else
        warn "frpc does not appear to be running"
        echo "    Check logs: tail -f /var/log/frpc.log"
    fi

    echo ""
    echo "    Tunnel: https://${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo ""
    exit 0
fi

# ══════════════════════════════════════════════════════════════════════
# ── Install mode (default) ───────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════

# ── Detect platform ──────────────────────────────────────────────────
section "Detecting platform"

case "$OS" in
    Darwin) PLATFORM="apple-darwin" ;;
    Linux)  PLATFORM="unknown-linux-gnu" ;;
    *)      err "Unsupported OS: $OS" ;;
esac

case "$ARCH" in
    x86_64)        TRIPLE="x86_64-${PLATFORM}" ; FRP_ARCH="amd64" ;;
    aarch64|arm64) TRIPLE="aarch64-${PLATFORM}" ; FRP_ARCH="arm64" ;;
    *)             err "Unsupported architecture: $ARCH" ;;
esac

# Pre-built binaries are only available for these combinations.
# Fail early instead of downloading a 404.
case "$TRIPLE" in
    aarch64-apple-darwin|x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;; # published in release workflow
    x86_64-apple-darwin)
        err "macOS x86_64 does not have pre-built binaries yet."
        hint "Build from source: cargo install --path crates/octos-cli"
        ;;
esac

ok "$OS $ARCH ($TRIPLE)"

# ── Check / install runtime dependencies ─────────────────────────────
if [ "$INSTALL_DEPS" = true ]; then
    section "Runtime dependencies (auto-install)"
else
    section "Checking runtime dependencies"
fi

# git — needed for skill installation
if command -v git &>/dev/null; then
    ok "git $(git --version | awk '{print $3}')"
elif [ "$INSTALL_DEPS" = true ]; then
    install_pkg git && ok "git installed" || true
else
    warn "git not found"
    echo "    Enables: skill installation (octos skills install)"
    echo "    Install:"
    echo "      $(pkg_hint git)"
fi

# Node.js / npm
if command -v node &>/dev/null; then
    ok "Node.js $(node --version)"
elif [ "$INSTALL_DEPS" = true ]; then
    install_pkg node && ok "Node.js installed" || true
else
    warn "Node.js not found"
    echo "    Enables: WhatsApp bridge, custom skills with package.json, pptxgenjs"
    echo "    Install:"
    echo "      $(pkg_hint node)"
fi

# Python
if command -v python3 &>/dev/null; then
    ok "Python $(python3 --version 2>&1 | awk '{print $2}')"
elif command -v python &>/dev/null; then
    ok "Python $(python --version 2>&1 | awk '{print $2}')"
elif [ "$INSTALL_DEPS" = true ]; then
    install_pkg python && ok "Python installed" || true
else
    warn "Python not found"
    echo "    Enables: MCP servers, custom skills, data processing"
    echo "    Install:"
    echo "      $(pkg_hint python)"
fi

# Chromium / Chrome
CHROME_FOUND=false
for chrome_bin in "google-chrome" "google-chrome-stable" "chromium-browser" "chromium"; do
    if command -v "$chrome_bin" &>/dev/null; then
        ok "Browser: $chrome_bin"
        CHROME_FOUND=true
        break
    fi
done
if [ "$CHROME_FOUND" = false ] && [ "$OS" = "Darwin" ]; then
    for app in "/Applications/Google Chrome.app" "/Applications/Chromium.app"; do
        if [ -d "$app" ]; then
            ok "Browser: $app"
            CHROME_FOUND=true
            break
        fi
    done
fi
if [ "$CHROME_FOUND" = false ]; then
    if [ "$INSTALL_DEPS" = true ]; then
        install_pkg chromium && ok "Chromium installed" || true
    else
        warn "Chromium/Chrome not found"
        echo "    Enables: browser tool (web browsing, screenshots), deep-crawl skill"
        echo "    Install:"
        echo "      $(pkg_hint chromium)"
    fi
fi

# ffmpeg
if command -v ffmpeg &>/dev/null; then
    ok "ffmpeg found"
elif [ "$INSTALL_DEPS" = true ]; then
    install_pkg ffmpeg && ok "ffmpeg installed" || true
else
    warn "ffmpeg not found"
    echo "    Enables: voice/audio skills, media transcoding"
    echo "    Install:"
    echo "      $(pkg_hint ffmpeg)"
fi

# ── Resolve download source ──────────────────────────────────────────
section "Resolving release"

TARBALL="octos-bundle-${TRIPLE}.tar.gz"
DOWNLOAD_BASE="${OCTOS_DOWNLOAD_URL:-}"

# Auto-detect: check if tarball is next to the script or in the current directory
if [ -z "$DOWNLOAD_BASE" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
    if [ -f "$SCRIPT_DIR/$TARBALL" ]; then
        DOWNLOAD_BASE="file://$SCRIPT_DIR"
    elif [ -f "./$TARBALL" ]; then
        DOWNLOAD_BASE="file://$(pwd)"
    fi
fi

if [ -n "$DOWNLOAD_BASE" ]; then
    # Local file or self-hosted server
    DOWNLOAD_URL="${DOWNLOAD_BASE}/${TARBALL}"
    ok "source: $DOWNLOAD_URL"
else
    # Default: GitHub Releases
    if [ "$VERSION" = "latest" ]; then
        VERSION=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
            | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
        if [ -z "$VERSION" ]; then
            err "Could not determine latest release. Specify --version explicitly."
        fi
    fi
    DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/download/${VERSION}/${TARBALL}"
    ok "version: $VERSION"
fi

# ── Download and install octos ────────────────────────────────────────
section "Installing octos"

INSTALL_TMP=$(mktemp -d /tmp/octos-install.XXXXXX)
trap 'rm -rf "$INSTALL_TMP"' EXIT

if [[ "$DOWNLOAD_URL" == file://* ]]; then
    LOCAL_PATH="${DOWNLOAD_URL#file://}"
    echo "    Copying from $LOCAL_PATH..."
    if ! cp "$LOCAL_PATH" "${INSTALL_TMP}/${TARBALL}"; then
        err "File not found: $LOCAL_PATH"
    fi
else
    echo "    Downloading $TARBALL..."
    if ! curl -fsSL -o "${INSTALL_TMP}/${TARBALL}" "$DOWNLOAD_URL"; then
        err "Download failed. Check that release $VERSION has a binary for $TRIPLE."
    fi
fi

tar -xzf "${INSTALL_TMP}/${TARBALL}" -C "$INSTALL_TMP"

mkdir -p "$PREFIX"
for bin in "$INSTALL_TMP"/*; do
    [ -f "$bin" ] || continue
    cp "$bin" "$PREFIX/"
    chmod +x "$PREFIX/$(basename "$bin")"
done
ok "binaries installed to $PREFIX"

# Save install and doctor scripts for later use.
# Try copying the running script first; fall back to downloading from the release.
SCRIPT_SELF="${BASH_SOURCE[0]:-$0}"
RELEASE_BASE="https://github.com/${GITHUB_REPO}/releases/latest/download"
if [ -f "$SCRIPT_SELF" ] && [ "$(wc -l < "$SCRIPT_SELF" 2>/dev/null)" -gt 10 ]; then
    cp "$SCRIPT_SELF" "$PREFIX/install.sh"
else
    curl -fsSL -o "$PREFIX/install.sh" "${RELEASE_BASE}/install.sh" 2>/dev/null || true
fi
[ -f "$PREFIX/install.sh" ] && chmod +x "$PREFIX/install.sh"
if [[ "$SCRIPT_SELF" == /* ]] && [ -f "$(dirname "$SCRIPT_SELF")/octos-doctor.sh" ]; then
    cp "$(dirname "$SCRIPT_SELF")/octos-doctor.sh" "$PREFIX/octos-doctor.sh"
else
    curl -fsSL -o "$PREFIX/octos-doctor.sh" "${RELEASE_BASE}/octos-doctor.sh" 2>/dev/null || true
fi
[ -f "$PREFIX/octos-doctor.sh" ] && chmod +x "$PREFIX/octos-doctor.sh"
if [ -f "$PREFIX/install.sh" ]; then
    ok "scripts saved to $PREFIX"
else
    warn "could not save helper scripts to $PREFIX"
fi

# Clear quarantine and sign on macOS
if [ "$OS" = "Darwin" ]; then
    for bin in "$PREFIX"/*; do
        xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
        codesign -s - "$bin" 2>/dev/null || true
    done
    ok "quarantine cleared and binaries signed (ad-hoc)"
fi

# Add to PATH if needed
if ! echo "$PATH" | grep -q "$PREFIX"; then
    warn "$PREFIX is not in your PATH"
    echo "    Add this to your shell profile:"
    echo "      export PATH=\"$PREFIX:\$PATH\""
fi

# ── Initialize octos workspace ────────────────────────────────────────
section "Initializing octos"

# Temporarily add PREFIX to PATH for subsequent commands
export PATH="$PREFIX:$PATH"
export OCTOS_HOME="$DATA_DIR"

if [ ! -d "$DATA_DIR" ]; then
    # octos init always writes to $cwd/.octos/, which won't match a custom
    # DATA_DIR.  When DATA_DIR is the default ~/.octos we can let init create
    # it; otherwise we set up the directory structure directly.
    if [ "$DATA_DIR" = "$HOME/.octos" ]; then
        "$PREFIX/octos" init --cwd "$HOME" --defaults 2>/dev/null || "$PREFIX/octos" init --cwd "$HOME" 2>/dev/null || true
        ok "workspace initialized via octos init"
    else
        mkdir -p "$DATA_DIR"
        ok "created custom data directory: $DATA_DIR"
    fi
else
    ok "$DATA_DIR already exists (skipping init)"
fi

# Ensure required subdirectories, config, and bootstrap files exist.
# These match what `octos init --defaults` creates (see init.rs).
mkdir -p "$DATA_DIR"/{profiles,memory,sessions,skills,logs,research,history}
if [ ! -f "$DATA_DIR/config.json" ]; then
    # Auto-detect provider from available API keys
    if [ -n "${OPENAI_API_KEY:-}" ]; then
        _PROV="openai"; _MODEL="gpt-4.1-mini"; _ENV="OPENAI_API_KEY"
    elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
        _PROV="anthropic"; _MODEL="claude-sonnet-4-20250514"; _ENV="ANTHROPIC_API_KEY"
    elif [ -n "${GEMINI_API_KEY:-}" ]; then
        _PROV="gemini"; _MODEL="gemini-2.5-flash"; _ENV="GEMINI_API_KEY"
    elif [ -n "${DEEPSEEK_API_KEY:-}" ]; then
        _PROV="deepseek"; _MODEL="deepseek-chat"; _ENV="DEEPSEEK_API_KEY"
    elif [ -n "${KIMI_API_KEY:-}" ]; then
        _PROV="moonshot"; _MODEL="kimi-k2.5"; _ENV="KIMI_API_KEY"
    elif [ -n "${DASHSCOPE_API_KEY:-}" ]; then
        _PROV="dashscope"; _MODEL="qwen3.5-plus"; _ENV="DASHSCOPE_API_KEY"
    else
        # No key detected — use openai as default, user must configure
        _PROV="openai"; _MODEL="gpt-4.1-mini"; _ENV="OPENAI_API_KEY"
    fi
    _MODE="local"
    _EXTRA_CONFIG=""
    if [ -n "$TENANT_NAME" ] || [ "$ENABLE_TUNNEL" = true ]; then
        _MODE="tenant"
    fi
    if [ "$ENABLE_TUNNEL" = true ]; then
        _EXTRA_CONFIG=$(cat <<INITEOF
,
  "tunnel_domain": "$TUNNEL_DOMAIN",
  "frps_server": "$FRPS_SERVER"
INITEOF
)
    fi
    cat > "$DATA_DIR/config.json" << INITEOF
{
  "provider": "$_PROV",
  "model": "$_MODEL",
  "api_key_env": "$_ENV",
  "mode": "$_MODE"$_EXTRA_CONFIG
}
INITEOF
    chmod 600 "$DATA_DIR/config.json"
    ok "auto-detected provider: $_PROV ($_ENV)"
fi
[ ! -f "$DATA_DIR/.gitignore" ] && cat > "$DATA_DIR/.gitignore" << 'INITEOF'
# Ignore task state and database files
tasks/
sessions/
*.redb
INITEOF
[ ! -f "$DATA_DIR/AGENTS.md" ] && printf '# Agent Instructions\n\nCustomize agent behavior and guidelines here.\n' > "$DATA_DIR/AGENTS.md"
[ ! -f "$DATA_DIR/SOUL.md" ]   && printf '# Soul — Who You Are\n\n## Core Principles\n\n- Help, don'\''t perform. Skip filler phrases — just do the thing.\n- Be resourceful. Come back with answers, not questions.\n- Have a voice. You can disagree and suggest alternatives.\n- Match the medium. Telegram gets concise replies. CLI gets detail.\n\n## Trust & Safety\n\n- Private things stay private.\n- External actions need care. Internal actions are yours.\n- Never send half-finished replies to messaging channels.\n' > "$DATA_DIR/SOUL.md"
[ ! -f "$DATA_DIR/USER.md" ]   && printf '# User Info\n\nAdd your information and preferences here.\n' > "$DATA_DIR/USER.md"
ok "data directory: $DATA_DIR"

# ── Generate auth token ──────────────────────────────────────────────
if [ -z "$AUTH_TOKEN" ]; then
    AUTH_TOKEN=$(openssl rand -hex 32)
fi

# ── Set up octos serve as system service ──────────────────────────────
section "Setting up octos serve"

OCTOS_BIN="$PREFIX/octos"
write_octos_service

# ── Verify octos serve ────────────────────────────────────────────────
section "Verifying octos serve"
RETRIES=10
while [ $RETRIES -gt 0 ]; do
    if curl -sf --max-time 2 "http://localhost:${PORT}/admin/" > /dev/null 2>&1; then
        ok "octos serve is running on http://localhost:${PORT}"
        break
    fi
    RETRIES=$((RETRIES - 1))
    sleep 1
done
if [ $RETRIES -eq 0 ]; then
    warn "octos serve did not respond within 10 seconds"
    echo "    Check logs: tail -f $DATA_DIR/serve.log"
fi

# ── Firewall (Caddy only) ─────────────────────────────────────────────
if [ -n "$CADDY_DOMAIN" ] && [ "$OS" = "Linux" ]; then
    section "Configuring firewall for Caddy"
    if command -v ufw &>/dev/null; then
        _caddy_ok=true
        echo "    Running: sudo ufw allow 80/tcp"
        sudo ufw allow 80/tcp >/dev/null 2>&1 || _caddy_ok=false
        echo "    Running: sudo ufw allow 443/tcp"
        sudo ufw allow 443/tcp >/dev/null 2>&1 || _caddy_ok=false
        if [ "$_caddy_ok" = true ]; then
            ok "ufw: ports 80,443 open for Caddy"
        else
            warn "failed to open Caddy ports (requires elevated privileges)"
        fi
    elif command -v firewall-cmd &>/dev/null; then
        echo "    Running: sudo firewall-cmd --permanent --add-port=80/tcp --add-port=443/tcp"
        if sudo firewall-cmd --permanent --add-port=80/tcp --add-port=443/tcp >/dev/null 2>&1 \
            && sudo firewall-cmd --reload >/dev/null 2>&1; then
            ok "firewalld: ports 80,443 open for Caddy"
        else
            warn "failed to open Caddy ports (requires elevated privileges)"
        fi
    else
        warn "no firewall manager found (ufw/firewalld) — ensure ports 80 and 443 are accessible"
    fi
fi


# ── Tunnel setup (frpc) ──────────────────────────────────────────────
if [ "$ENABLE_TUNNEL" = true ]; then
    section "Tunnel setup"

    # Prompt for missing inputs (use placeholders if skipped — frpc still gets installed)
    TENANT_PLACEHOLDER=false
    TOKEN_PLACEHOLDER=false

    if [ -z "$TENANT_NAME" ] || [ -z "$FRPS_TOKEN" ]; then
        echo ""
        echo "    Tunnel setup requires a tenant name, shared frps token, and SSH port."
        echo "    If you don't have these yet, register at:"
        echo "      https://${TUNNEL_DOMAIN}"
        echo "    You'll receive your setup command with all values pre-filled."
        echo ""
    fi

    if [ -z "$TENANT_NAME" ]; then
        echo "    Enter the tenant subdomain (e.g. 'alice' for alice.${TUNNEL_DOMAIN}):"
        echo "    (press Enter to use placeholder — you can update later)"
        printf "    > "
        read -r TENANT_NAME < /dev/tty
        if [ -z "$TENANT_NAME" ]; then
            raw_hostname="$(hostname -s)"
            # Derive a slug from the hostname: lowercase, allow [a-z0-9-] only.
            TENANT_NAME="$(printf '%s' "$raw_hostname" \
                | tr '[:upper:]' '[:lower:]' \
                | sed 's/[^a-z0-9-]/-/g; s/^-*//; s/-*$//; s/--*/-/g')"
            if [ -z "$TENANT_NAME" ]; then
                warn "Could not derive a valid tenant from hostname '$raw_hostname'. Please enter a tenant name."
                printf "    > "
                read -r TENANT_NAME < /dev/tty
            else
                TENANT_PLACEHOLDER=true
                warn "Using placeholder tenant: $TENANT_NAME"
            fi
        fi
    fi

    if [ -z "$FRPS_TOKEN" ] && [ -n "$FRPS_TOKEN_FILE" ]; then
        if [ -f "$FRPS_TOKEN_FILE" ]; then
            FRPS_TOKEN=$(cat "$FRPS_TOKEN_FILE")
            echo "    shared frps token loaded from $FRPS_TOKEN_FILE"
        else
            err "token file not found: $FRPS_TOKEN_FILE"
        fi
    fi

    if [ -z "$FRPS_TOKEN" ]; then
        echo ""
        echo "    Enter the shared frps auth token (press Enter to use placeholder):"
        printf "    > "
        read -r FRPS_TOKEN < /dev/tty
        if [ -z "$FRPS_TOKEN" ]; then
            FRPS_TOKEN="CHANGE_ME"
            TOKEN_PLACEHOLDER=true
            warn "Using placeholder token — frpc will not connect until updated"
        fi
    fi

    validate_inputs

    # ── Confirm before proceeding ─────────────────────────────────────
    echo ""
    echo "    Tunnel configuration:"
    echo "      Tenant:       ${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo "      frps server:  ${FRPS_SERVER}:7000"
    if [ "$TOKEN_PLACEHOLDER" = true ]; then
        echo "      shared frps token: CHANGE_ME (placeholder)"
    else
        echo "      shared frps token: ${FRPS_TOKEN:0:8}..."
    fi
    echo "      SSH port:     ${SSH_PORT}"
    echo "      Local port:   ${PORT}"

    if [ "$TENANT_PLACEHOLDER" = true ] || [ "$TOKEN_PLACEHOLDER" = true ]; then
        echo ""
        echo "    frpc will be installed with placeholders. Update the config later:"
        echo "      sudo nano /etc/frp/frpc.toml"
        echo "    Then restart frpc:"
        echo "      $(svc_hint restart frpc)"
        echo ""
        echo "    Or re-run: bash install.sh --tenant-name <name> --frps-token <token>"
    fi

    echo ""
    echo "    Press Enter to continue, or Ctrl+C to abort."
    read -r < /dev/tty

    # ── Install frpc ──────────────────────────────────────────────────
    install_frpc_binary

    # ── Write frpc config ─────────────────────────────────────────────
    write_frpc_config
    ok "frpc config written to /etc/frp/frpc.toml"

    # ── Create frpc system service ────────────────────────────────────
    echo "    (sudo is needed to install the frpc system service)"
    write_frpc_service
    ok "frpc service installed"

    # ── Verify tunnel ─────────────────────────────────────────────────
    section "Verifying tunnel"
    sleep 3
    if pgrep -x frpc > /dev/null 2>&1; then
        ok "frpc is running (PID: $(pgrep -x frpc))"
    else
        warn "frpc does not appear to be running"
        echo "    Check logs: tail -f /var/log/frpc.log"
    fi

    if curl -sf --max-time 3 "http://localhost:${PORT}/api/status" > /dev/null 2>&1; then
        ok "octos serve is running on port ${PORT}"
    else
        warn "octos serve is not responding on port ${PORT} (tunnel will retry once it starts)"
    fi
fi

# ── Caddy reverse proxy (optional) ────────────────────────────────────
if [ -n "$CADDY_DOMAIN" ]; then
    section "Setting up Caddy reverse proxy"

    # Install Caddy if missing
    if ! command -v caddy &>/dev/null; then
        echo "    Installing Caddy..."
        case "$OS" in
            Darwin)
                brew install caddy 2>/dev/null || err "Failed to install Caddy (brew required)" ;;
            Linux)
                sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl 2>/dev/null
                curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg 2>/dev/null
                curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
                sudo apt-get update -qq && sudo apt-get install -y caddy 2>/dev/null || err "Failed to install Caddy" ;;
            *)
                err "Caddy auto-install not supported on $OS — install manually: https://caddyserver.com/docs/install" ;;
        esac
        ok "Caddy installed"
    else
        ok "Caddy already installed: $(caddy version 2>/dev/null | head -1)"
    fi

    # Determine serve port (match what octos serve uses)
    CADDY_UPSTREAM="localhost:${PORT}"

    # Write Caddyfile
    CADDYFILE_PATH="$DATA_DIR/Caddyfile"
    cat > "$CADDYFILE_PATH" << CADDYEOF
{
    on_demand_tls {
        ask http://localhost:9999/check
    }
}

:9999 {
    respond /check 200
}

${CADDY_DOMAIN} {
    handle /api/* {
        reverse_proxy ${CADDY_UPSTREAM}
    }
    handle /admin* {
        reverse_proxy ${CADDY_UPSTREAM}
    }
    handle /auth/* {
        reverse_proxy ${CADDY_UPSTREAM}
    }
    handle /webhook/* {
        reverse_proxy ${CADDY_UPSTREAM}
    }
    handle {
        reverse_proxy ${CADDY_UPSTREAM}
    }
}

*.${CADDY_DOMAIN} {
    tls {
        on_demand
    }

    @api path /api/*
    @admin path /admin*
    @auth path /auth/*

    handle @api {
        reverse_proxy ${CADDY_UPSTREAM} {
            header_up X-Profile-Id {labels.2}
        }
    }
    handle @admin {
        reverse_proxy ${CADDY_UPSTREAM} {
            header_up X-Profile-Id {labels.2}
        }
    }
    handle @auth {
        reverse_proxy ${CADDY_UPSTREAM} {
            header_up X-Profile-Id {labels.2}
        }
    }
    handle {
        reverse_proxy ${CADDY_UPSTREAM}
    }
}
CADDYEOF
    caddy fmt --overwrite "$CADDYFILE_PATH" 2>/dev/null || true
    ok "Caddyfile written to $CADDYFILE_PATH"

    # Validate
    if caddy validate --config "$CADDYFILE_PATH" 2>/dev/null; then
        ok "Caddyfile is valid"
    else
        warn "Caddyfile validation failed — check $CADDYFILE_PATH"
    fi

    # Start or reload Caddy
    if pgrep -x caddy > /dev/null 2>&1; then
        caddy reload --config "$CADDYFILE_PATH" 2>/dev/null
        ok "Caddy reloaded"
    else
        caddy start --config "$CADDYFILE_PATH" 2>/dev/null
        ok "Caddy started"
    fi

    echo ""
    echo "    Caddy is proxying:"
    echo "      https://${CADDY_DOMAIN}          → octos dashboard"
    echo "      https://*.${CADDY_DOMAIN}        → profile subdomains (on-demand TLS)"
    echo ""
    echo "    Prerequisites:"
    echo "      DNS: A record for ${CADDY_DOMAIN} → this server's public IP"
    echo "      DNS: A record for *.${CADDY_DOMAIN} → this server's public IP"
    echo "      Ports 80 and 443 must be open"
fi

# ── Summary ───────────────────────────────────────────────────────────
section "Installation complete!"
echo ""
echo "    Binary:     $PREFIX/octos"
echo "    Data dir:   $DATA_DIR"
echo "    Config:     $DATA_DIR/config.json"
echo "    Auth token: $AUTH_TOKEN"
echo "    Logs:       tail -f $DATA_DIR/serve.log"
echo ""
echo "  Next steps:"
echo "    1. Setup LLM models:  octos init"
echo "    2. Install skills:    octos skills install --all"
echo "    3. Start chatting:    octos chat"
echo "    4. Open local dashboard: http://localhost:${PORT}/admin/"
if [ "$ENABLE_TUNNEL" = true ] && [ -n "$TENANT_NAME" ]; then
    echo ""
    echo "  Public tunnel:"
    echo "    Dashboard:  https://${TENANT_NAME}.${TUNNEL_DOMAIN}"
elif [ -n "$TENANT_NAME" ]; then
    echo ""
    echo "  Reserved public name:"
    echo "    ${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo "    To enable public access, re-run with --tunnel"
fi
if [ -n "$CADDY_DOMAIN" ]; then
    echo ""
    echo "  Caddy (on-demand TLS):"
    echo "    Dashboard:  https://${CADDY_DOMAIN}"
    echo "    Profiles:   https://{name}.${CADDY_DOMAIN}"
    echo "    Caddyfile:  $DATA_DIR/Caddyfile"
fi
echo ""
echo "  Manage services:"
echo "    Status:  $(svc_hint status serve)"
echo "    Stop:    $(svc_hint stop serve)"
echo "    Start:   $(svc_hint start serve)"
echo ""
echo "  Later:"
if [ -f "$PREFIX/install.sh" ]; then
    [ "$ENABLE_TUNNEL" != true ] && echo "    Enable tunnel:  $PREFIX/install.sh --tunnel"
    echo "    Diagnose:       $PREFIX/install.sh --doctor"
else
    [ "$ENABLE_TUNNEL" != true ] && echo "    Enable tunnel:  curl -fsSL ${RELEASE_BASE}/install.sh | bash -s -- --tunnel"
    echo "    Diagnose:       curl -fsSL ${RELEASE_BASE}/install.sh | bash -s -- --doctor"
fi
echo ""
