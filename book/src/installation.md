# Installation & Deployment

## Prerequisites

| Requirement | Version | Notes |
|------------|---------|-------|
| Rust | 1.85.0+ | Install via [rustup.rs](https://rustup.rs) |
| macOS | 13+ | Apple Silicon or Intel |
| Linux | glibc 2.31+ | Ubuntu 20.04+, Debian 11+, Fedora 34+ |
| Windows | 10/11 | Native build or WSL2 |

You also need an API key from at least one supported LLM provider.

### Optional Dependencies

| Dependency | Used For | Install |
|-----------|----------|---------|
| Node.js | WhatsApp bridge, PPTX creation skill | `brew install node` / `apt install nodejs` |
| ffmpeg | Media/video skills | `brew install ffmpeg` / `apt install ffmpeg` |
| Chrome/Chromium | Browser automation tool | `brew install --cask chromium` |
| LibreOffice | Office document conversion | `brew install --cask libreoffice` |
| Poppler | PDF rendering (`pdftoppm`) | `brew install poppler` / `apt install poppler-utils` |

## Build from Source

```bash
git clone https://github.com/octos-org/octos
cd octos

# Basic (CLI, chat, run, gateway with CLI channel)
cargo install --path crates/octos-cli

# With messaging channels
cargo install --path crates/octos-cli --features telegram,discord,slack,whatsapp,feishu,email,wecom

# With browser automation (requires Chrome/Chromium)
cargo install --path crates/octos-cli --features browser

# With web UI and REST API
cargo install --path crates/octos-cli --features api

# Verify
octos --version
```

## Deploy Script

For a streamlined installation, use the deploy script:

```bash
# Minimal install (CLI + chat only)
./scripts/local-tenant-deploy.sh --minimal

# Full install (all channels + dashboard + app-skills)
./scripts/local-tenant-deploy.sh --full

# Custom channels
./scripts/local-tenant-deploy.sh --channels telegram,discord,api
```

## Platform-Specific Instructions

### macOS

```bash
# 1. Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. Install optional deps
brew install node ffmpeg poppler
brew install --cask libreoffice

# 3. Clone and deploy
git clone https://github.com/octos-org/octos.git
cd octos
./scripts/local-tenant-deploy.sh --full

# 4. Set API key and run
export ANTHROPIC_API_KEY=sk-ant-...
octos chat
```

**Background service (launchd system daemon):**

The deploy script creates `/Library/LaunchDaemons/io.octos.serve.plist`.

```bash
# Start service (requires sudo)
sudo launchctl load /Library/LaunchDaemons/io.octos.serve.plist

# Stop service
sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist

# Check status
sudo launchctl print system/io.octos.serve

# View logs
tail -f ~/.octos/serve.log
```

### Linux (Ubuntu/Debian)

```bash
# 1. Install system deps
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev

# 2. Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 3. Install optional deps
sudo apt install -y nodejs npm ffmpeg poppler-utils

# 4. Clone and deploy
git clone https://github.com/octos-org/octos.git
cd octos
./scripts/local-tenant-deploy.sh --full

# 5. Set API key and run
export ANTHROPIC_API_KEY=sk-ant-...
octos chat
```

**Background service (systemd system unit):**

The deploy script creates `/etc/systemd/system/octos-serve.service`.

```bash
# Start service
sudo systemctl start octos-serve

# Enable on boot
sudo systemctl enable octos-serve

# Check status
sudo systemctl status octos-serve

# View logs
sudo journalctl -u octos-serve -f

# Stop service
sudo systemctl stop octos-serve
```

### Linux (Fedora/RHEL)

```bash
# System deps
sudo dnf install -y gcc pkg-config openssl-devel

# Then follow Ubuntu steps from step 2 onward
```

### Windows (Native)

Octos builds and runs natively on Windows. Shell commands are executed via `cmd /C`.

```powershell
# 1. Install Rust (download rustup-init.exe from https://rustup.rs)
rustup-init.exe

# 2. Clone and build
git clone https://github.com/octos-org/octos.git
cd octos
cargo install --path crates/octos-cli

# 3. Set API key and run
$env:ANTHROPIC_API_KEY = "sk-ant-..."
octos chat
```

**Windows notes:**

- Sandbox is disabled on Windows (no bubblewrap/sandbox-exec equivalent); shell commands run without isolation. Docker sandbox mode still works if Docker Desktop is installed.
- API keys are stored via Windows Credential Manager.
- Process management uses `taskkill` for cleanup.

### Windows (WSL2)

Alternatively, use WSL2 for a Linux environment:

```powershell
# 1. Install WSL2 (PowerShell as admin)
wsl --install -d Ubuntu

# 2. Open Ubuntu terminal, then follow Linux (Ubuntu) steps above
```

When running `octos serve` inside WSL2, the dashboard is accessible from your Windows browser at `http://localhost:8080` (WSL2 auto-forwards ports).

## Docker

```bash
docker compose --profile gateway up -d
```

## Deploy Script Reference

```
./scripts/local-tenant-deploy.sh [OPTIONS]

Options:
  --minimal          CLI + chat only (no channels, no dashboard)
  --full             All channels + dashboard + app-skills
  --channels LIST    Comma-separated: telegram,discord,slack,whatsapp,feishu,email,twilio,wecom
  --no-skills        Skip building app-skills
  --no-service       Skip launchd/systemd service setup
  --uninstall        Remove binaries and service files
  --debug            Build in debug mode (faster compile, larger binary)
  --prefix DIR       Install prefix (default: ~/.cargo/bin)
  --no-tunnel        Skip frpc tunnel setup even in --full mode
  --tenant-name NAME Tenant subdomain (e.g. "alice")
  --frps-token TOKEN frps auth token
  --frps-server ADDR frps server address (default: 163.192.33.32)
  --ssh-port PORT    SSH tunnel remote port (default: 6001)
  --domain DOMAIN    Tunnel domain (default: octos-cloud.org)
  --auth-token TOKEN Dashboard auth token (default: auto-generated)
```

For Windows native installs, use `.\scripts\install.ps1` (PowerShell).

**What the script does:**

1. Checks prerequisites (Rust, platform deps)
2. Builds the `octos` binary with selected features
3. Builds app-skill binaries (unless `--no-skills`)
4. Signs binaries on macOS (ad-hoc codesign)
5. Creates the runtime data directory and writes `~/.octos/config.json` with `mode = "local"` or `mode = "tenant"`
6. Creates a background service when dashboard/API features are enabled
7. Optionally configures the `frpc` tunnel for tenant deployments

**Uninstall:**

```bash
./scripts/local-tenant-deploy.sh --uninstall
# Data directory (~/.octos) is NOT removed. Delete manually:
rm -rf ~/.octos
```

## Post-Install Verification

### Set API Keys

Set at least one LLM provider key:

```bash
# Add to ~/.bashrc, ~/.zshrc, or ~/.profile
export ANTHROPIC_API_KEY=sk-ant-...
# Or
export OPENAI_API_KEY=sk-...
# Or use OAuth login
octos auth login --provider openai
```

### Verify

```bash
octos --version              # Check binary
octos status                 # Check config + API keys
octos chat --message "Hello" # Quick test
```

## Upgrading

```bash
cd octos
git pull origin main
./scripts/local-tenant-deploy.sh --full   # Rebuilds and reinstalls

# If running as a service, restart it:
# macOS:
sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist
sudo launchctl load /Library/LaunchDaemons/io.octos.serve.plist
# Linux:
sudo systemctl restart octos-serve
```

## Troubleshooting

| Problem | Solution |
|---------|----------|
| `octos: command not found` | Add `~/.cargo/bin` to PATH: `export PATH="$HOME/.cargo/bin:$PATH"` |
| Build fails on Linux | Install `build-essential pkg-config libssl-dev` |
| macOS codesign warning | Run: `codesign -s - ~/.cargo/bin/octos` |
| Dashboard not accessible | Check port: `octos serve --port 8080`, open `http://localhost:8080` |
| WSL2 port not forwarded | Restart WSL: `wsl --shutdown` then reopen terminal |
| Service won't start | Check logs: `tail -f ~/.octos/serve.log` or `journalctl --user -u octos-serve` |
| API key not found | Ensure env var is set in the service environment, not just your shell |
