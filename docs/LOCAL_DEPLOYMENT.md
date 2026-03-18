# Local Deployment Guide

Deploy octos on your own machine (macOS, Linux, or Windows).

## Quick Start

```bash
# Minimal install (CLI + chat only)
./scripts/local-deploy.sh --minimal

# Full install (all channels + dashboard + app-skills)
./scripts/local-deploy.sh --full

# Custom channels
./scripts/local-deploy.sh --channels telegram,discord,api
```

## Prerequisites

| Requirement | Version | Notes |
|------------|---------|-------|
| Rust | 1.85.0+ | Install via [rustup.rs](https://rustup.rs) |
| macOS | 13+ | Apple Silicon or Intel |
| Linux | glibc 2.31+ | Ubuntu 20.04+, Debian 11+, Fedora 34+ |
| Windows | 10/11 | Native build or WSL2 |

### Optional Dependencies

| Dependency | Used For | Install |
|-----------|----------|---------|
| Node.js | WhatsApp bridge, pptxgenjs | `brew install node` / `apt install nodejs` |
| ffmpeg | Media/video skills | `brew install ffmpeg` / `apt install ffmpeg` |
| Chrome/Chromium | Browser tool | `brew install --cask chromium` |
| LibreOffice | Office doc conversion | `brew install --cask libreoffice` |
| Poppler | PDF rendering | `brew install poppler` / `apt install poppler-utils` |

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
./scripts/local-deploy.sh --full

# 4. Set API key and run
export ANTHROPIC_API_KEY=sk-ant-...
crew chat
```

**Background service (launchd):**

The deploy script creates `~/Library/LaunchAgents/io.octos.octos-serve.plist`.

```bash
# Start service (survives reboot)
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist

# Stop service
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist

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
./scripts/local-deploy.sh --full

# 5. Set API key and run
export ANTHROPIC_API_KEY=sk-ant-...
crew chat
```

**Background service (systemd user unit):**

The deploy script creates `~/.config/systemd/user/crew-serve.service`.

```bash
# Start service
systemctl --user start crew-serve

# Enable on boot (requires lingering)
loginctl enable-linger $USER
systemctl --user enable crew-serve

# View logs
journalctl --user -u crew-serve -f

# Stop service
systemctl --user stop crew-serve
```

### Linux (Fedora/RHEL)

```bash
# System deps
sudo dnf install -y gcc pkg-config openssl-devel

# Then follow Ubuntu steps from step 2 onward
```

### Windows (Native)

octos builds and runs natively on Windows. Shell commands are executed via `cmd /C`.

```powershell
# 1. Install Rust (download rustup-init.exe from https://rustup.rs)
rustup-init.exe

# 2. Clone and build
git clone https://github.com/octos-org/octos.git
cd octos
cargo install --path crates/octos-cli

# 3. Set API key and run
$env:ANTHROPIC_API_KEY = "sk-ant-..."
crew chat
```

**PowerShell CI script:**

A local CI script is provided for Windows:

```powershell
.\scripts\ci.ps1           # Run fmt + clippy + tests
.\scripts\ci.ps1 -Fix      # Auto-fix formatting
.\scripts\ci.ps1 -Quick    # Skip clippy (just fmt + test)
```

**Windows notes:**

- Sandbox is disabled on Windows (no bubblewrap/sandbox-exec equivalent); shell commands run without isolation. Docker sandbox mode still works if Docker Desktop is installed.
- API keys are stored via Windows Credential Manager (`keyring` crate).
- Process management uses `taskkill` for cleanup.

### Windows (WSL2)

Alternatively, you can use WSL2 for a Linux environment:

```powershell
# 1. Install WSL2 (PowerShell as admin)
wsl --install -d Ubuntu

# 2. Open Ubuntu terminal, then follow Linux (Ubuntu) steps above
```

**Accessing the dashboard from Windows:**

When running `crew serve` inside WSL2, the dashboard is accessible from your Windows browser at `http://localhost:8080/admin/` (WSL2 auto-forwards ports).

## Deploy Script Reference

On Windows, use `.\scripts\local-deploy.ps1` (PowerShell) with the same options.

```
./scripts/local-deploy.sh [OPTIONS]

Options:
  --minimal          CLI + chat only (no channels, no dashboard)
  --full             All channels + dashboard + app-skills
  --channels LIST    Comma-separated: telegram,discord,slack,whatsapp,feishu,email,twilio,wecom
  --no-skills        Skip building app-skills
  --no-service       Skip launchd/systemd service setup
  --uninstall        Remove binaries and service files
  --debug            Build in debug mode (faster compile, larger binary)
  --prefix DIR       Install prefix (default: ~/.cargo/bin)
```

**What the script does:**

1. Checks prerequisites (Rust, platform deps)
2. Builds `crew` binary with selected features
3. Builds app-skill binaries (unless `--no-skills`)
4. Signs binaries on macOS (ad-hoc codesign)
5. Runs `crew init` if `~/.octos` doesn't exist
6. Creates background service file (launchd on macOS, systemd on Linux)

**Uninstall:**

```bash
./scripts/local-deploy.sh --uninstall
# Data directory (~/.octos) is NOT removed. Delete manually:
rm -rf ~/.octos
```

## Post-Install Configuration

### API Keys

Set at least one LLM provider key:

```bash
# Add to ~/.bashrc, ~/.zshrc, or ~/.profile
export ANTHROPIC_API_KEY=sk-ant-...
# Or
export OPENAI_API_KEY=sk-...
# Or use OAuth login
crew auth login --provider openai
```

### Config File

Edit `~/.octos/config.json` (or `.octos/config.json` in project directory):

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "gateway": {
    "channels": [
      { "type": "telegram", "settings": { "token_env": "TELEGRAM_BOT_TOKEN" } }
    ]
  }
}
```

### Verify Installation

```bash
crew --version          # Check binary
crew status             # Check config + API keys
crew chat --message "Hello"  # Quick test
```

## Upgrading

```bash
cd octos
git pull origin main
./scripts/local-deploy.sh --full   # Rebuilds and reinstalls

# If running as a service, restart it:
# macOS:
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist
# Linux:
systemctl --user restart crew-serve
```

## Troubleshooting

| Problem | Solution |
|---------|----------|
| `crew: command not found` | Add `~/.cargo/bin` to PATH: `export PATH="$HOME/.cargo/bin:$PATH"` |
| Build fails on Linux | Install `build-essential pkg-config libssl-dev` |
| macOS codesign warning | Run: `codesign -s - ~/.cargo/bin/crew` |
| Dashboard not accessible | Check port: `crew serve --port 8080`, open `http://localhost:8080/admin/` |
| WSL2 port not forwarded | Restart WSL: `wsl --shutdown` then reopen terminal |
| Service won't start | Check logs: `tail -f ~/.octos/serve.log` or `journalctl --user -u crew-serve` |
| API key not found | Ensure env var is set in the service environment, not just your shell |
| Windows: `crew` not found | Ensure `%USERPROFILE%\.cargo\bin` is in your PATH |
| Windows: shell commands fail | Commands run via `cmd /C`; use Windows-compatible syntax |
