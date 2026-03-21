# Octos Tunnel Plan

## Overview

Enable users to self-host octos on a home Mac Mini (behind NAT) and expose it to the public internet via a managed tunnel relay. No public IP or port forwarding required on the user's side.

## Architecture

```
Internet
  │
  ▼
Caddy (VPS, *.octos-cloud.org, wildcard SSL)
  │
  ▼
frps (VPS, port 7000 control + 80/443 vhost)
  │  ← frpc connects outbound from each Mini
  ├── alice.octos-cloud.org → Alice's Mini:8080
  ├── bob.octos-cloud.org   → Bob's Mini:8080
  └── VPS:6001-6999         → SSH tunnels (admin only)
```

### Components

| Component | Where | Role |
|-----------|-------|------|
| **Caddy** | VPS | Wildcard SSL termination (`*.octos-cloud.org`), reverse proxy to frps |
| **frps** | VPS | Tunnel relay server, vhost routing by subdomain |
| **frpc** | Each Mini | Tunnel client, connects outbound to frps |
| **octos serve** | Each Mini | Dashboard + gateway + API |

## VPS Requirements

- **Instance**: Oracle Cloud free ARM (4 vCPU, 24GB RAM, 10TB bandwidth) or AWS t3.micro ($8/mo)
- **Ports**: 7000 (frp control), 80 (HTTP), 443 (HTTPS), 6001-6999 (SSH tunnel pool)
- **Domain**: `*.octos-cloud.org` A record → VPS IP
- **SSL**: Wildcard cert via Caddy + Cloudflare DNS challenge (or Let's Encrypt DNS-01)

## Per-Tenant Exposed Services

| URL / Port | Target | Purpose |
|------------|--------|---------|
| `{tenant}.octos-cloud.org` | Mini:8080 | Dashboard + API |
| `{tenant}.octos-cloud.org/api/chat` | Mini:8080 | Web chat |
| `{tenant}.octos-cloud.org/webhook/telegram` | Mini:8080 | Telegram webhook |
| `{tenant}.octos-cloud.org/webhook/feishu/{profile}` | Mini:8080 | Feishu webhook |
| `{tenant}.octos-cloud.org/twilio/webhook` | Mini:8080 | Twilio webhook |
| `VPS:{6000+N}` | Mini:22 | SSH (admin only) |

## Tenant Isolation

frp uses a single shared auth token — no per-tenant isolation. Solutions:

| Approach | Pros | Cons |
|----------|------|------|
| **Auth proxy in front of frps** | Per-tenant tokens, centralized control | Extra service to maintain |
| **octos-managed frpc config** | Token per tenant, auto-generated | Requires octos admin API integration |
| **Built-in tunnel (rathole lib)** | Native Rust, no external binary | More development work |

**Recommended**: Phase 1 uses frp with octos-managed tokens. Phase 2 evaluates replacing frp with a built-in Rust tunnel.

## Onboarding Flow

### Admin creates tenant

```bash
octos admin create-tenant --name alice --domain alice.octos-cloud.org
```

This:
1. Creates a profile in the admin database
2. Assigns subdomain `alice.octos-cloud.org`
3. Generates a unique tunnel token (UUID v4)
4. Allocates an SSH port from the pool (e.g., 6001)
5. Outputs a one-liner setup command for the user

### User sets up their Mini

```bash
curl -fsSL https://octos-cloud.org/setup | bash -s alice <token>
```

This script:
1. Installs `octos` binary (from GitHub releases)
2. Installs `frpc` binary
3. Writes frpc config with the tenant's token and subdomain
4. Writes octos config with API keys (prompted or pre-configured)
5. Creates launchd services for both `octos serve` and `frpc`
6. Starts services
7. Verifies tunnel connectivity

### Result

```
https://alice.octos-cloud.org          → dashboard
https://alice.octos-cloud.org/api/chat → web chat API
ssh -p 6001 cloud@octos-cloud.org      → SSH to Mini
```

## Domain Strategy

| Strategy | Example | SSL | Effort |
|----------|---------|-----|--------|
| **Wildcard subdomain** (recommended) | `alice.octos-cloud.org` | One wildcard cert | Low |
| **User brings own domain** | `chat.alice.com` CNAME → VPS | Per-domain cert | Medium (user configures DNS) |
| **Both** | Wildcard default + custom domain option | Mixed | Full flexibility |

## VPS Setup (one-time)

### 1. Caddy config

```
*.octos-cloud.org {
    tls {
        dns cloudflare {env.CF_API_TOKEN}
    }
    reverse_proxy localhost:443 {
        transport http {
            tls_insecure_skip_verify
        }
    }
}
```

### 2. frps config

```toml
bindPort = 7000
vhostHTTPPort = 80
vhostHTTPSPort = 443
auth.method = "token"
auth.token = "{master-token}"

webServer.port = 7500
webServer.user = "admin"
webServer.password = "{dashboard-password}"
```

### 3. Firewall (AWS Security Group)

```
TCP 7000   0.0.0.0/0    # frp control
TCP 80     0.0.0.0/0    # HTTP
TCP 443    0.0.0.0/0    # HTTPS
TCP 7500   admin-IP/32  # frp dashboard (admin only)
TCP 6001-6999  admin-IP/32  # SSH tunnels (admin only)
```

## Implementation Phases

### Phase 1: Manual setup scripts (PR #31 baseline)
- Merge PR #31's frp scripts as starting point
- Add Caddy config for wildcard SSL
- Add tenant frpc config template
- Document the manual onboarding process

### Phase 2: Admin API integration
- `octos admin create-tenant` command
- Auto-generate frpc configs
- SSH port pool allocation
- Tenant status monitoring via frp dashboard API

### Phase 3: Self-service onboarding
- Web signup at octos-cloud.org
- One-liner install script per tenant
- Auto-configure webhooks (Telegram setWebhook, Feishu URL verification)
- Health monitoring and auto-restart

### Phase 4 (optional): Built-in Rust tunnel
- Replace frpc/frps with native tunnel in `octos serve`
- Use rathole or custom implementation
- Zero external dependencies
- Per-tenant encryption and auth built-in
