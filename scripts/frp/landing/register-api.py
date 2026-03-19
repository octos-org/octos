#!/usr/bin/env python3
"""Minimal registration API for Octos Cloud.

Handles email OTP verification and tenant creation.
Runs on the VPS alongside Caddy + frps.

Usage:
    SMTP_USER=dspfac@gmail.com SMTP_PASS='xxxx' python3 register-api.py
"""

import json
import os
import random
import smtplib
import string
import time
import uuid
from email.mime.text import MIMEText
from http.server import HTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from threading import Lock

# ── Config ────────────────────────────────────────────────────────────
PORT = int(os.environ.get("REG_API_PORT", "8090"))
SMTP_HOST = os.environ.get("SMTP_HOST", "smtp.gmail.com")
SMTP_PORT = int(os.environ.get("SMTP_PORT", "587"))
SMTP_USER = os.environ.get("SMTP_USER", "dspfac@gmail.com")
SMTP_PASS = os.environ.get("SMTP_PASS", "")
TENANT_DIR = os.environ.get("TENANT_DIR", "/var/lib/octos-cloud/tenants")
DOMAIN = os.environ.get("TUNNEL_DOMAIN", "octos-cloud.org")
FRPS_SERVER = os.environ.get("FRPS_SERVER", "163.192.33.32")

# In-memory OTP store: {email: (code, expiry_ts)}
otp_store = {}
otp_lock = Lock()

# Session store: {token: {email, created_at}}
sessions = {}

# SSH port tracking
SSH_PORT_START = 6001
SSH_PORT_END = 6999

Path(TENANT_DIR).mkdir(parents=True, exist_ok=True)


def send_otp_email(email, code):
    """Send OTP code via Gmail SMTP."""
    msg = MIMEText(
        f"Your Octos Cloud verification code is:\n\n"
        f"    {code}\n\n"
        f"This code expires in 10 minutes.\n\n"
        f"If you didn't request this, please ignore this email.",
        "plain",
        "utf-8",
    )
    msg["Subject"] = f"Octos Cloud - Verification Code: {code}"
    msg["From"] = f"Octos Cloud <{SMTP_USER}>"
    msg["To"] = email

    with smtplib.SMTP(SMTP_HOST, SMTP_PORT) as server:
        server.starttls()
        server.login(SMTP_USER, SMTP_PASS)
        server.send_message(msg)


def next_ssh_port():
    """Find next available SSH port from tenant files."""
    used = set()
    for f in Path(TENANT_DIR).glob("*.json"):
        try:
            data = json.loads(f.read_text())
            used.add(data.get("ssh_port", 0))
        except Exception:
            pass
    for port in range(SSH_PORT_START, SSH_PORT_END + 1):
        if port not in used:
            return port
    raise RuntimeError("SSH port pool exhausted")


def gen_token():
    return uuid.uuid4().hex + uuid.uuid4().hex


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        print(f"[{time.strftime('%H:%M:%S')}] {fmt % args}")

    def _json_response(self, status, data):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Access-Control-Allow-Methods", "GET,POST,OPTIONS")
        self.send_header("Content-Length", len(body))
        self.end_headers()
        self.wfile.write(body)

    def _text_response(self, status, text):
        body = text.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", len(body))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        if length == 0:
            return {}
        return json.loads(self.rfile.read(length))

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Access-Control-Allow-Methods", "GET,POST,OPTIONS")
        self.end_headers()

    def do_POST(self):
        if self.path == "/api/auth/send-code":
            self._handle_send_code()
        elif self.path == "/api/auth/verify":
            self._handle_verify()
        elif self.path == "/api/admin/tenants":
            self._handle_create_tenant()
        else:
            self._text_response(404, "not found")

    def do_GET(self):
        if self.path == "/api/admin/tenants":
            self._handle_list_tenants()
        elif self.path.startswith("/api/admin/tenants/") and self.path.endswith("/setup-script"):
            tenant_id = self.path.split("/")[4]
            self._handle_setup_script(tenant_id)
        else:
            self._text_response(404, "not found")

    def _handle_send_code(self):
        try:
            data = self._read_body()
            email = data.get("email", "").strip().lower()
            if not email or "@" not in email:
                self._text_response(400, "invalid email")
                return

            code = "".join(random.choices(string.digits, k=6))
            with otp_lock:
                otp_store[email] = (code, time.time() + 600)  # 10 min expiry

            send_otp_email(email, code)
            self._json_response(200, {"ok": True, "message": "code sent"})
            print(f"  OTP sent to {email}")

        except Exception as e:
            print(f"  ERROR sending OTP: {e}")
            self._text_response(500, f"failed to send code: {e}")

    def _handle_verify(self):
        try:
            data = self._read_body()
            email = data.get("email", "").strip().lower()
            code = data.get("code", "").strip()

            with otp_lock:
                stored = otp_store.get(email)
                if not stored:
                    self._text_response(401, "no code sent to this email")
                    return
                stored_code, expiry = stored
                if time.time() > expiry:
                    del otp_store[email]
                    self._text_response(401, "code expired")
                    return
                if code != stored_code:
                    self._text_response(401, "incorrect code")
                    return
                del otp_store[email]

            # Create session
            token = gen_token()
            sessions[token] = {"email": email, "created_at": time.time()}
            self._json_response(200, {"ok": True, "token": token, "email": email})
            print(f"  Verified: {email}")

        except Exception as e:
            self._text_response(500, str(e))

    def _handle_create_tenant(self):
        try:
            # Check auth
            auth = self.headers.get("Authorization", "")
            token = auth.replace("Bearer ", "") if auth.startswith("Bearer ") else ""
            session = sessions.get(token)
            if not session:
                self._text_response(401, "unauthorized")
                return

            data = self._read_body()
            name = data.get("name", "").strip().lower()

            # Validate name
            if not name or not all(c.isalnum() or c == "-" for c in name):
                self._text_response(400, "invalid node name (lowercase alphanumeric + hyphens)")
                return
            if name.startswith("-") or name.endswith("-"):
                self._text_response(400, "name must not start or end with hyphen")
                return
            if len(name) > 64:
                self._text_response(400, "name too long (max 64)")
                return

            # Check duplicate
            tenant_file = Path(TENANT_DIR) / f"{name}.json"
            if tenant_file.exists():
                self._text_response(409, f"node '{name}' already taken")
                return

            ssh_port = next_ssh_port()
            auth_token = gen_token()
            now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

            tenant = {
                "id": name,
                "name": name,
                "subdomain": name,
                "tunnel_token": str(uuid.uuid4()),
                "ssh_port": ssh_port,
                "local_port": 8080,
                "auth_token": auth_token,
                "email": session["email"],
                "status": "pending",
                "created_at": now,
                "updated_at": now,
            }

            tenant_file.write_text(json.dumps(tenant, indent=2))
            os.chmod(tenant_file, 0o600)

            self._json_response(200, tenant)
            print(f"  Tenant created: {name} ({session['email']})")

        except Exception as e:
            self._text_response(500, str(e))

    def _handle_list_tenants(self):
        tenants = []
        for f in sorted(Path(TENANT_DIR).glob("*.json")):
            try:
                tenants.append(json.loads(f.read_text()))
            except Exception:
                pass
        self._json_response(200, tenants)

    def _handle_setup_script(self, tenant_id):
        tenant_file = Path(TENANT_DIR) / f"{tenant_id}.json"
        if not tenant_file.exists():
            self._text_response(404, f"tenant '{tenant_id}' not found")
            return
        tenant = json.loads(tenant_file.read_text())

        # Read frps token
        frps_token_file = Path("/etc/frp/frps-token.txt")
        frps_token = frps_token_file.read_text().strip() if frps_token_file.exists() else "REPLACE_WITH_FRPS_TOKEN"

        script = f"""#!/usr/bin/env bash
# Setup script for {tenant['subdomain']}.{DOMAIN}
set -euo pipefail

SUBDOMAIN="{tenant['subdomain']}"
FRPS_SERVER="{FRPS_SERVER}"
FRPS_PORT=7000
FRPS_TOKEN="{frps_token}"
LOCAL_PORT={tenant['local_port']}
SSH_PORT={tenant['ssh_port']}
DOMAIN="{DOMAIN}"

echo "==> Setting up octos tunnel for ${{SUBDOMAIN}}.${{DOMAIN}}"

# Install frpc
FRPC_VERSION="0.61.1"
ARCH=$(uname -m)
case "$ARCH" in
    x86_64) FRP_ARCH="amd64" ;;
    aarch64|arm64) FRP_ARCH="arm64" ;;
    *) echo "Unsupported: $ARCH"; exit 1 ;;
esac
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

if [ ! -f /usr/local/bin/frpc ]; then
    echo "    Installing frpc..."
    TMPDIR=$(mktemp -d); trap 'rm -rf "$TMPDIR"' EXIT
    curl -fsSL -o "$TMPDIR/frp.tar.gz" \\
        "https://github.com/fatedier/frp/releases/download/v${{FRPC_VERSION}}/frp_${{FRPC_VERSION}}_${{OS}}_${{FRP_ARCH}}.tar.gz"
    tar -xzf "$TMPDIR/frp.tar.gz" -C "$TMPDIR"
    sudo install -m 0755 "$TMPDIR/frp_${{FRPC_VERSION}}_${{OS}}_${{FRP_ARCH}}/frpc" /usr/local/bin/frpc
fi

# Write config
sudo mkdir -p /etc/frp
sudo tee /etc/frp/frpc.toml > /dev/null << 'FRPCEOF'
serverAddr = "{FRPS_SERVER}"
serverPort = 7000
auth.method = "token"
auth.token = "{frps_token}"
log.to = "/var/log/frpc.log"
log.level = "info"
log.maxDays = 7

[[proxies]]
name = "{tenant['subdomain']}-web"
type = "http"
localPort = {tenant['local_port']}
customDomains = ["{tenant['subdomain']}.{DOMAIN}"]

[[proxies]]
name = "{tenant['subdomain']}-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = {tenant['ssh_port']}
FRPCEOF

# Create service
if [ "$OS" = "darwin" ]; then
    mkdir -p ~/Library/LaunchAgents
    cat > ~/Library/LaunchAgents/io.octos.frpc.plist << 'PEOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>io.octos.frpc</string>
    <key>ProgramArguments</key><array>
        <string>/usr/local/bin/frpc</string><string>-c</string><string>/etc/frp/frpc.toml</string>
    </array>
    <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>/tmp/frpc.log</string>
    <key>StandardErrorPath</key><string>/tmp/frpc.log</string>
</dict></plist>
PEOF
    launchctl unload ~/Library/LaunchAgents/io.octos.frpc.plist 2>/dev/null || true
    launchctl load ~/Library/LaunchAgents/io.octos.frpc.plist
else
    sudo tee /etc/systemd/system/frpc.service > /dev/null << 'SEOF'
[Unit]
Description=frpc tunnel
After=network.target
[Service]
Type=simple
ExecStart=/usr/local/bin/frpc -c /etc/frp/frpc.toml
Restart=always
RestartSec=5
[Install]
WantedBy=multi-user.target
SEOF
    sudo systemctl daemon-reload && sudo systemctl enable frpc && sudo systemctl restart frpc
fi

echo ""
echo "==> Done! Dashboard: https://${{SUBDOMAIN}}.${{DOMAIN}}"
echo "    SSH: ssh -p ${{SSH_PORT}} $(whoami)@${{DOMAIN}}"
"""
        self._text_response(200, script)


if __name__ == "__main__":
    if not SMTP_PASS:
        print("WARNING: SMTP_PASS not set, email sending will fail")

    print(f"Octos Cloud Registration API")
    print(f"  Port:     {PORT}")
    print(f"  SMTP:     {SMTP_USER} via {SMTP_HOST}:{SMTP_PORT}")
    print(f"  Tenants:  {TENANT_DIR}")
    print(f"  Domain:   {DOMAIN}")
    print()

    server = HTTPServer(("127.0.0.1", PORT), Handler)
    print(f"Listening on http://127.0.0.1:{PORT}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopped.")
