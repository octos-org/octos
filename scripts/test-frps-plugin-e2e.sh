#!/usr/bin/env bash
# End-to-end test for the frps plugin: real frps + real frpc + octos serve.
#
# This test would have caught the v2 design flaw where built-in VerifyLogin
# rejects the tenant's per-tenant privilege_key. The unit tests in
# frps_plugin.rs only hit the axum endpoint directly, so the frps → plugin
# handoff (and ordering vs VerifyLogin) is never exercised.
#
# Skips cleanly when `frps` or `frpc` aren't installed. Run manually:
#   bash scripts/test-frps-plugin-e2e.sh
# or pull binaries first with setup-frps.sh / setup-frpc.sh.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail()  { echo "FAIL: $*" >&2; exit 1; }
skip()  { echo "SKIP: $*" >&2; exit 0; }
ok()    { echo "  ✓ $*"; }
info()  { echo "==> $*"; }

command -v frps >/dev/null 2>&1 || skip "frps binary not on PATH — install via scripts/frp/setup-frps.sh"
command -v frpc >/dev/null 2>&1 || skip "frpc binary not on PATH — install via scripts/frp/setup-frpc.sh"
command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v python3 >/dev/null 2>&1 || fail "python3 is required"

WORK=$(mktemp -d /tmp/octos-frps-e2e.XXXXXX)
EXIT_STATUS=1
KEEP_LOGS="${KEEP_LOGS:-0}"
trap 'cleanup' EXIT

OCTOS_PID=""
FRPS_PID=""
FRPC_PID=""

cleanup() {
    [ -n "$FRPC_PID" ] && kill "$FRPC_PID" 2>/dev/null || true
    [ -n "$FRPS_PID" ] && kill "$FRPS_PID" 2>/dev/null || true
    [ -n "$OCTOS_PID" ] && kill "$OCTOS_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    if [ "$EXIT_STATUS" -ne 0 ] || [ "$KEEP_LOGS" = "1" ]; then
        echo "(logs preserved in $WORK)" >&2
    else
        rm -rf "$WORK" || true
    fi
}

# Pick ephemeral ports to avoid collisions.
pick_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

OCTOS_PORT=$(pick_port)
FRPS_BIND_PORT=$(pick_port)
FRPS_VHOST_HTTP=$(pick_port)
FRPS_VHOST_HTTPS=$(pick_port)
FRPS_DASHBOARD_PORT=$(pick_port)
LOCAL_APP_PORT=$(pick_port)
SSH_REMOTE_PORT=$(pick_port)

info "working dir: $WORK"
info "octos:$OCTOS_PORT  frps:$FRPS_BIND_PORT  local app:$LOCAL_APP_PORT  ssh remote:$SSH_REMOTE_PORT"

# ── 1. Build octos CLI with the api feature ─────────────────────────
info "building octos-cli with api feature"
(cd "$ROOT_DIR" && cargo build -q -p octos-cli --features api) \
    || fail "cargo build failed"

OCTOS_BIN="$ROOT_DIR/target/debug/octos"
[ -x "$OCTOS_BIN" ] || fail "octos binary not found at $OCTOS_BIN"

# ── 2. Start octos serve with an isolated data dir ─────────────────
AUTH_TOKEN=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
export OCTOS_HOME="$WORK/octos-home"
mkdir -p "$OCTOS_HOME"

cat > "$OCTOS_HOME/config.json" <<EOF
{
  "mode": "cloud",
  "tunnel_domain": "octos-cloud.test",
  "frps_server": "127.0.0.1",
  "frps_port": $FRPS_BIND_PORT,
  "auth_token": "$AUTH_TOKEN"
}
EOF

info "starting octos serve on :$OCTOS_PORT"
"$OCTOS_BIN" serve --port "$OCTOS_PORT" --host 127.0.0.1 --auth-token "$AUTH_TOKEN" \
    > "$WORK/octos.log" 2>&1 &
OCTOS_PID=$!

# Wait for octos to accept connections.
for i in $(seq 1 40); do
    if curl -sf -o /dev/null "http://127.0.0.1:$OCTOS_PORT/api/health" 2>/dev/null; then
        break
    fi
    sleep 0.25
    if ! kill -0 "$OCTOS_PID" 2>/dev/null; then
        tail -40 "$WORK/octos.log" >&2
        fail "octos serve crashed during startup"
    fi
done
ok "octos serve is up"

# ── 3. Register a tenant, capture its tunnel_token ─────────────────
info "creating tenant 'alice' via admin API"
CREATE_RESP=$(curl -sf -H "Authorization: Bearer $AUTH_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"name":"alice","local_port":'"$LOCAL_APP_PORT"'}' \
    "http://127.0.0.1:$OCTOS_PORT/api/admin/tenants") \
    || { tail -40 "$WORK/octos.log" >&2; fail "tenant create returned error"; }

TUNNEL_TOKEN=$(python3 -c 'import sys,json; d=json.loads(sys.argv[1]); print(d["tunnel_token"])' "$CREATE_RESP")
SSH_PORT=$(python3 -c 'import sys,json; d=json.loads(sys.argv[1]); print(d["ssh_port"])' "$CREATE_RESP")
[ -n "$TUNNEL_TOKEN" ] || fail "tenant response missing tunnel_token"
ok "tenant created (tunnel_token=${TUNNEL_TOKEN:0:8}...)"

# ── 4. Write frps + frpc configs ───────────────────────────────────
cat > "$WORK/frps.toml" <<EOF
bindPort = $FRPS_BIND_PORT
vhostHTTPPort = $FRPS_VHOST_HTTP
vhostHTTPSPort = $FRPS_VHOST_HTTPS
webServer.port = $FRPS_DASHBOARD_PORT
webServer.user = "admin"
webServer.password = "admin"

allowPorts = [
  { start = 1024, end = 65535 }
]

log.to = "$WORK/frps.log"
log.level = "trace"

# Empty server token — plugin rewrites privilege_key to md5("" + ts).
auth.method = "token"
auth.token = ""

[[httpPlugins]]
name = "octos-auth"
addr = "127.0.0.1:$OCTOS_PORT"
path = "/api/internal/frps-auth"
ops = ["Login", "NewProxy"]
EOF

cat > "$WORK/frpc.toml" <<EOF
serverAddr = "127.0.0.1"
serverPort = $FRPS_BIND_PORT

# Both sides empty — plugin authenticates via metadatas.token.
auth.method = "token"
auth.token = ""
metadatas.token = "$TUNNEL_TOKEN"

log.to = "$WORK/frpc.log"
log.level = "debug"
loginFailExit = true

[[proxies]]
name = "alice-web"
type = "http"
localPort = $LOCAL_APP_PORT
customDomains = ["alice.octos-cloud.test"]

[[proxies]]
name = "alice-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = $SSH_PORT
EOF

# ── 5. Start frps ──────────────────────────────────────────────────
info "starting frps on :$FRPS_BIND_PORT"
frps -c "$WORK/frps.toml" > "$WORK/frps.stdout" 2>&1 &
FRPS_PID=$!
sleep 1
kill -0 "$FRPS_PID" 2>/dev/null || { cat "$WORK/frps.log" 2>/dev/null; fail "frps failed to start"; }
ok "frps running"

# ── 6. Start frpc and watch for login success ──────────────────────
info "starting frpc (expecting successful login)"
frpc -c "$WORK/frpc.toml" > "$WORK/frpc.stdout" 2>&1 &
FRPC_PID=$!

# Wait up to 10s for "login to server success" in frpc log.
LOGIN_OK=0
for i in $(seq 1 40); do
    sleep 0.25
    if grep -Fq "login to the server failed" "$WORK/frpc.log" 2>/dev/null \
         || grep -Fq "login to the server failed" "$WORK/frpc.stdout" 2>/dev/null; then
        echo "── frpc log ────────────────────────────" >&2
        tail -40 "$WORK/frpc.log" 2>/dev/null >&2 || true
        tail -40 "$WORK/frpc.stdout" 2>&1 >&2 || true
        echo "── frps log ────────────────────────────" >&2
        tail -40 "$WORK/frps.log" 2>/dev/null >&2 || true
        echo "── octos log ───────────────────────────" >&2
        tail -40 "$WORK/octos.log" 2>&1 >&2 || true
        fail "frpc reported login failure — plugin fix regressed"
    fi
    if grep -Fq "login to server success" "$WORK/frpc.log" 2>/dev/null \
         || grep -Fq "login to server success" "$WORK/frpc.stdout" 2>/dev/null; then
        LOGIN_OK=1
        break
    fi
    if ! kill -0 "$FRPC_PID" 2>/dev/null; then
        echo "── frpc log ────────────────────────────" >&2
        tail -40 "$WORK/frpc.log" 2>/dev/null >&2 || true
        tail -40 "$WORK/frpc.stdout" 2>&1 >&2 || true
        fail "frpc exited early"
    fi
done

[ "$LOGIN_OK" -eq 1 ] || fail "frpc did not log 'login to the server success' within 10s"
ok "frpc logged in successfully — plugin authenticated tenant via metadatas.token"

# ── 7. Verify proxy is registered on frps ──────────────────────────
# frps dashboard API exposes proxy stats; we just confirm our HTTP proxy
# shows up. Use basic auth admin/admin.
PROXIES_JSON=$(curl -sf -u admin:admin "http://127.0.0.1:$FRPS_DASHBOARD_PORT/api/proxy/http" || true)
if ! echo "$PROXIES_JSON" | grep -Fq '"alice-web"'; then
    echo "$PROXIES_JSON" >&2
    fail "alice-web proxy not registered on frps dashboard"
fi
ok "alice-web proxy is registered on frps"

# ── 8. Negative: wrong token must be rejected ──────────────────────
info "verifying wrong token is rejected"
kill "$FRPC_PID" 2>/dev/null || true
wait "$FRPC_PID" 2>/dev/null || true
FRPC_PID=""

sed -i.bak "s/metadatas.token = \"$TUNNEL_TOKEN\"/metadatas.token = \"wrong-token-0000\"/" "$WORK/frpc.toml"
: > "$WORK/frpc.log"
: > "$WORK/frpc.stdout"

frpc -c "$WORK/frpc.toml" > "$WORK/frpc.stdout" 2>&1 &
FRPC_PID=$!

FAILED=0
for i in $(seq 1 40); do
    sleep 0.25
    if grep -Eq "login to the server failed|authentication failed" "$WORK/frpc.log" 2>/dev/null \
         || grep -Eq "login to the server failed|authentication failed" "$WORK/frpc.stdout" 2>/dev/null; then
        FAILED=1
        break
    fi
    if ! kill -0 "$FRPC_PID" 2>/dev/null; then
        # Exited — check if it was due to auth rejection.
        if grep -Eq "login to the server failed|authentication failed" "$WORK/frpc.log" "$WORK/frpc.stdout" 2>/dev/null; then
            FAILED=1
        fi
        break
    fi
done

[ "$FAILED" -eq 1 ] || fail "wrong token was NOT rejected — tenant isolation broken"
ok "wrong token correctly rejected"

echo
echo "frps plugin e2e tests passed"
EXIT_STATUS=0
