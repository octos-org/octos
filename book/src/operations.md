# Operations

This chapter covers day-to-day operational tasks: upgrading, credential management, and service management.

---

## Upgrading

Pull the latest source and rebuild:

```bash
cd octos
git pull origin main
./scripts/local-deploy.sh --full   # Rebuilds and reinstalls
```

If running as a service, restart it after the upgrade:

```bash
# macOS (launchd):
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist

# Linux (systemd):
systemctl --user restart octos-serve
```

---

## Keychain Integration

Octos supports storing API keys in the macOS Keychain instead of plaintext in profile JSON files. This provides hardware-backed encryption on Apple Silicon and OS-level access control.

### Architecture

```
                     +------------------------------+
  octos auth set-key |     macOS Keychain            |
  -----------------> |  (AES encrypted, per-user)    |
                     |                               |
                     |  service: "octos"             |
                     |  account: "OPENAI_API_KEY"    |
                     |  password: "sk-proj-abc..."   |
                     +---------------+--------------+
                                     | get_password()
  Profile JSON                       |
  +------------------+               v
  | env_vars: {      |   resolve_env_vars()
  |   "OPENAI_API_   |   if "keychain:" ->
  |    KEY":          |   lookup from Keychain
  |    "keychain:"   |   else -> use literal
  | }                |
  +------------------+               |
                                     v
                               Gateway process
```

**Resolution chain**: `"keychain:"` marker in profile config triggers a Keychain lookup (3-second timeout). If the Keychain is unavailable, the key is skipped with a warning.

**Backward compatible**: Literal values in `env_vars` pass through unchanged. No migration is required -- adopt keychain per-key at your own pace. Mixed plaintext and keychain entries are fully supported.

### CLI Commands

```bash
# Unlock keychain for SSH sessions (required before set-key via SSH)
octos auth unlock --password <login-password>
octos auth unlock                               # interactive prompt

# Store a key in Keychain + update profile to use keychain marker
octos auth set-key OPENAI_API_KEY sk-proj-abc123
octos auth set-key OPENAI_API_KEY              # interactive prompt

# With specific profile
octos auth set-key GEMINI_API_KEY AIzaSy... -p my-profile

# List all keys and their storage status
octos auth keys
octos auth keys -p my-profile

# Remove from Keychain + clean up profile
octos auth remove-key OPENAI_API_KEY
```

### Keychain Entry Format

- **Service**: `octos` (constant for all entries)
- **Account**: The environment variable name (e.g., `OPENAI_API_KEY`)
- **Password**: The actual secret value

Verify with:

```bash
security find-generic-password -s octos -a OPENAI_API_KEY -w
```

### SSH and Headless Server Setup

The macOS Keychain is tied to the GUI login session. SSH sessions cannot access a locked keychain -- macOS tries to show a dialog, which hangs on a headless server.

**Why SSH fails by default**: macOS `securityd` unlocks the keychain per-session. The GUI session's unlock does not automatically propagate to SSH sessions.

**Solution**: Unlock the keychain and disable auto-lock. Run once per boot (or add to your deploy script):

```bash
ssh user@<host>

# Unlock the keychain (requires login password)
octos auth unlock --password <login-password>

# That's it -- auto-lock is disabled automatically.
# The keychain stays unlocked until reboot.
# Auto-login will re-unlock it on reboot.
```

Or with raw `security` commands:

```bash
# Unlock
security unlock-keychain -p '<password>' ~/Library/Keychains/login.keychain-db

# Disable auto-lock timer (so it doesn't re-lock after idle)
security set-keychain-settings ~/Library/Keychains/login.keychain-db
```

**Common issues:**

| Symptom | Cause | Fix |
|---------|-------|-----|
| "User interaction is not allowed" | Keychain locked (SSH session) | `octos auth unlock --password <pw>` |
| Keychain lookup timed out (3s) | Keychain locked (LaunchAgent) | Enable auto-login, reboot |
| "keychain marker found but no secret" | Key never stored or wrong keychain | Re-run `octos auth set-key` after unlock |
| Gateway hangs at startup | Keychain lookup blocking | Update to latest octos binary |

### Security Comparison

| Threat | Plaintext JSON | Keychain |
|--------|---------------|----------|
| File stolen (backup, git, scp) | All keys exposed | Only `"keychain:"` markers visible |
| Malware reads disk | Simple file read exposes keys | Must bypass OS Keychain ACL |
| Other user on machine | File permissions help, root can read | Encrypted per-user |
| Process memory dump | Keys in env vars | Keys only briefly in memory |
| Accidental log output | Profile JSON leaks keys | Only reference strings logged |

### Server Deployment Recommendations

The macOS Keychain was designed for interactive desktop use. On headless servers, it introduces reliability issues. Choose your credential storage based on deployment type:

| Deployment | Recommended Storage | Reason |
|------------|-------------------|--------|
| **Developer laptop** | Keychain (`"keychain:"`) | GUI session keeps keychain unlocked; ACL prompts are fine |
| **Mac with auto-login + GUI** | Keychain (`"keychain:"`) | Works if ACL dialogs were approved once via screen sharing |
| **Headless Mac (SSH only)** | Plain text in `env_vars` or launchd plist | Most reliable; no unlock/ACL dependencies |
| **Linux server** | Plain text in env vars | No macOS Keychain available |

**Why Keychain is unreliable on headless servers:**

1. **Requires the macOS login password** -- To unlock the keychain via SSH, you need the user's login password stored somewhere, reducing the security benefit.
2. **Re-locks on reboot/sleep** -- The LaunchAgent that starts `octos serve` runs before GUI login, so the keychain is locked at that point.
3. **Re-locks after idle timeout** -- Even after unlock, macOS may re-lock. The `set-keychain-settings` workaround can be reset by macOS updates.
4. **ACL prompts block headless access** -- If the binary was not the one that originally stored the secret, macOS may pop an unanswerable GUI dialog.
5. **Session isolation** -- Unlocking from SSH does not unlock for the LaunchAgent session, and vice versa.

**Plain text setup for servers:**

```json
{
  "env_vars": {
    "OPENAI_API_KEY": "sk-proj-abc123",
    "SMTP_PASSWORD": "xxxx xxxx xxxx xxxx",
    "SMTP_HOST": "smtp.gmail.com",
    "SMTP_PORT": "587",
    "SMTP_USERNAME": "user@gmail.com",
    "SMTP_FROM": "user@gmail.com"
  }
}
```

Protect the files with filesystem permissions:

```bash
chmod 600 ~/.octos/profiles/*.json
chmod 600 ~/Library/LaunchAgents/io.octos.octos-serve.plist
```

---

## Service Management

### macOS (launchd)

Create a LaunchAgent plist to run octos as a persistent service:

```bash
# Load the service
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist

# Unload the service
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist

# Check status
launchctl list | grep octos
```

If the service needs environment variables (e.g., SMTP credentials), add them to the plist:

```xml
<key>EnvironmentVariables</key>
<dict>
    <key>SMTP_PASSWORD</key>
    <string>xxxx xxxx xxxx xxxx</string>
</dict>
```

Check logs at `~/.octos/serve.log`.

### Linux (systemd)

Manage the service with systemd user units:

```bash
# Start / stop / restart
systemctl --user start octos-serve
systemctl --user stop octos-serve
systemctl --user restart octos-serve

# Enable on boot
systemctl --user enable octos-serve

# Check status and logs
systemctl --user status octos-serve
journalctl --user -u octos-serve
```
