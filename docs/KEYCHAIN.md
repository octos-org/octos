# macOS Keychain Integration

octos supports storing API keys in the macOS Keychain instead of plaintext in
profile JSON files. This provides hardware-backed encryption on Apple Silicon
and OS-level access control.

## Architecture

```
                     ┌──────────────────────────────┐
  crew auth set-key  │     macOS Keychain            │
  ─────────────────► │  (AES encrypted, per-user)    │
                     │                               │
                     │  service: "octos"            │
                     │  account: "OPENAI_API_KEY"     │
                     │  password: "sk-proj-abc..."    │
                     └──────────────┬───────────────┘
                                    │ get_password()
  Profile JSON                      │
  ┌─────────────────┐               ▼
  │ env_vars: {     │   process_manager.rs
  │   "OPENAI_API_  │   resolve_env_vars()
  │    KEY":        │──────────────────────► cmd.env(key, real_value)
  │    "keychain:"  │   if "keychain:" →        │
  │ }               │   lookup from Keychain    │
  └─────────────────┘   else → use literal      ▼
                                           Gateway process
```

**Resolution chain**: `"keychain:"` marker → Keychain lookup (3s timeout) → warn & skip if unavailable
**Backward compatible**: Literal values pass through unchanged

## How It Works

1. **Marker convention**: Profile `env_vars` stores `"keychain:"` as the value instead of the actual secret
2. **Resolution**: At gateway spawn time, `process_manager.rs` calls `resolve_env_vars()` which replaces `"keychain:"` markers with real secrets from the Keychain
3. **Timeout**: Each keychain lookup has a 3-second timeout to prevent hangs on headless servers
4. **Graceful degradation**: If the Keychain is locked or unavailable, the key is skipped with a warning log

## CLI Commands

```bash
# Unlock keychain for SSH sessions (required before set-key via SSH)
crew auth unlock --password <login-password>
crew auth unlock                               # interactive prompt

# Store a key in Keychain + update profile to use keychain marker
crew auth set-key OPENAI_API_KEY sk-proj-abc123
crew auth set-key OPENAI_API_KEY              # interactive prompt

# With specific profile
crew auth set-key GEMINI_API_KEY AIzaSy... -p dspfac

# List all keys and their storage status
crew auth keys
crew auth keys -p dspfac

# Remove from Keychain + clean up profile
crew auth remove-key OPENAI_API_KEY
```

## SSH / Headless Server Setup

The macOS Keychain is tied to the GUI login session. SSH sessions cannot access
a **locked** keychain — macOS tries to show a dialog, which hangs forever on a
headless server. This section explains how to make it work.

### Why SSH fails by default

```
GUI session (auto-login)          SSH session
┌─────────────────────┐          ┌─────────────────────┐
│ securityd unlocks    │          │ securityd sees       │
│ login keychain at    │          │ keychain is locked    │
│ login time           │          │ for THIS session     │
│                      │          │                      │
│ keychain ops: ✅     │          │ keychain ops: ❌     │
│                      │          │ "User interaction    │
│                      │          │  is not allowed"     │
└─────────────────────┘          └─────────────────────┘
```

macOS `securityd` unlocks the keychain per-session. The GUI session's unlock
does **not** automatically propagate to SSH sessions.

### Solution: unlock + disable auto-lock

Run once per boot (or add to deploy script):

```bash
ssh cloud@<host>

# 1. Unlock the keychain (requires login password)
crew auth unlock --password <login-password>

# 2. That's it — auto-lock is disabled automatically.
#    The keychain stays unlocked until reboot.
#    Auto-login will re-unlock it on reboot.
```

Or with raw `security` commands:

```bash
# Unlock
security unlock-keychain -p '<password>' ~/Library/Keychains/login.keychain-db

# Disable auto-lock timer (so it doesn't re-lock after idle)
security set-keychain-settings ~/Library/Keychains/login.keychain-db
```

### Deploy workflow

```bash
# First deploy: unlock + store keys
ssh 
crew auth unlock --password 
crew auth set-key OPENAI_API_KEY sk-proj-...
crew auth set-key GEMINI_API_KEY AIzaSy...
# keys are now in keychain, profiles updated to "keychain:" markers

# Subsequent deploys: just update binary, restart
# Keychain stays unlocked (auto-login re-unlocks on reboot)
scp crew :/Users/cloud/.cargo/bin/crew
ssh  'codesign -f -s - ~/.cargo/bin/crew'
```

### What can go wrong

| Symptom | Cause | Fix |
|---------|-------|-----|
| "User interaction is not allowed" | Keychain locked (SSH session) | `crew auth unlock --password <pw>` |
| Keychain lookup timed out (3s) | Keychain locked (LaunchAgent) | Enable auto-login, reboot |
| "keychain marker found but no secret" | Key stored in wrong keychain or never stored | Re-run `crew auth set-key` after unlock |
| Gateway hangs at startup | Keychain lookup blocking (pre-timeout fix) | Update to latest crew binary |

## Security Comparison

| Threat | Plaintext JSON | Keychain |
|--------|---------------|----------|
| File stolen (backup, git, scp) | All keys exposed | Only `"keychain:"` markers |
| Malware reads disk | Simple `cat` exposes keys | Must bypass OS Keychain ACL |
| Other user on machine | 0600 helps, root can read | Encrypted per-user |
| Process memory dump | Keys in env vars | Keys only briefly in memory |
| Accidental log output | Profile JSON leaks keys | Only reference strings |

## OTP Email (SMTP Password)

The dashboard OTP email system reads its SMTP password with this fallback chain:

1. Process environment (`std::env::var("SMTP_PASSWORD")`) — works via LaunchAgent
2. Profile `env_vars` — works when plaintext
3. Keychain lookup — works when profile has `"keychain:"` marker and keychain is unlocked

This means OTP email works regardless of how `crew serve` is started (LaunchAgent or nohup).

## Keychain Entry Format

- **Service**: `octos` (constant for all entries)
- **Account**: The environment variable name (e.g., `OPENAI_API_KEY`)
- **Password**: The actual secret value

Verify with:
```bash
security find-generic-password -s octos -a OPENAI_API_KEY -w
```

## Admin Dashboard

The web dashboard shows Keychain-backed values as `🔑 (keychain)` instead of the
usual `sk-1***def` masked format. The `save_with_merge()` logic preserves the
`"keychain:"` marker during profile updates.

## Implementation Files

| File | Role |
|------|------|
| `crates/octos-cli/src/auth/keychain.rs` | Core keyring wrapper (set/get/delete/resolve/unlock) |
| `crates/octos-cli/src/commands/auth.rs` | CLI subcommands (set-key, keys, remove-key, unlock) |
| `crates/octos-cli/src/process_manager.rs` | Resolves markers before env injection |
| `crates/octos-cli/src/profiles.rs` | Masking + merge logic for dashboard |
| `crates/octos-cli/src/otp.rs` | SMTP password fallback from profile/keychain |

## Limitations for Headless / Server Deployments

The macOS Keychain was designed for interactive desktop use. On headless servers
(Mac Minis accessed only via SSH), it introduces several reliability problems
that make **plain text env vars the recommended approach** for production.

### Why Keychain Is Unreliable on Servers

1. **Requires the macOS login password** — To unlock the keychain via SSH, crew
   must call `security unlock-keychain -p <password>`. This means you need to
   store the user's login password somewhere (deploy script, plist, etc.),
   defeating much of the security benefit.

2. **Re-locks on reboot/sleep** — macOS re-locks the keychain on every reboot
   and after sleep. The LaunchAgent that starts `crew serve` runs before any
   user logs in via GUI, so the keychain is locked at that point. Auto-login
   helps but is not guaranteed (e.g., after macOS updates that require manual
   login).

3. **Re-locks after idle timeout** — Even after a successful unlock, macOS may
   re-lock the keychain after an idle timeout. We disable this with
   `security set-keychain-settings`, but macOS updates can reset it.

4. **ACL prompts block headless access** — Even with an unlocked keychain, if
   the `crew` binary was not the application that originally stored the secret,
   macOS may pop a GUI dialog asking "Allow crew to access this keychain item?"
   This dialog cannot be answered over SSH and blocks the lookup until the
   3-second timeout fires.

5. **LaunchAgent vs SSH session isolation** — `securityd` tracks keychain
   unlock state per-session. Unlocking from SSH does not unlock for the
   LaunchAgent session, and vice versa. A reboot creates a new session where
   neither has access until someone unlocks again.

### Recommendation by Deployment Type

| Deployment | Credential Storage | Why |
|------------|-------------------|-----|
| **Developer laptop** | Keychain (`"keychain:"`) | GUI session keeps keychain unlocked, ACL prompts are fine |
| **Mac Mini with auto-login + GUI** | Keychain (`"keychain:"`) | Works if someone approved ACL dialogs once via VNC/Screen Sharing |
| **Mac Mini headless (SSH only)** | Plain text in `env_vars` or launchd plist `EnvironmentVariables` | Most reliable; no unlock/ACL dependencies |
| **Linux server** | Plain text in env vars | No macOS Keychain available |

### Plain Text Setup for Servers

Set credentials directly in profile `env_vars`:

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

For `crew serve` OTP email, also set `SMTP_PASSWORD` in the launchd plist:

```xml
<key>EnvironmentVariables</key>
<dict>
    <key>SMTP_PASSWORD</key>
    <string>xxxx xxxx xxxx xxxx</string>
</dict>
```

Protect the files with filesystem permissions:

```bash
chmod 600 ~/.octos/profiles/*.json
chmod 600 ~/Library/LaunchAgents/io.ominix.octos-serve.plist
```

## Backward Compatibility

- Existing plaintext `env_vars` continue to work unchanged
- No migration required — adopt keychain per-key at your own pace
- Mix of plaintext and keychain entries is fully supported
