# @octos-org/octos

One-line installer for the [Octos](https://github.com/octos-org/octos) server — a
Rust-native, API-first Agentic OS.

```bash
npm install -g @octos-org/octos
octos serve
```

This package downloads the prebuilt release bundle for your platform and installs
the `octos` server **together with its bundled skills** (`news_fetch`,
`deep-search`, `deep_crawl`, `send_email`, `account_manager`, `voice`, `clock`,
`weather`). The skills are kept as siblings of the `octos` binary so that
`octos serve` can discover them at startup.

## Supported platforms

- macOS Apple Silicon (`darwin-arm64`)
- Linux x86_64 (`linux-x64`)
- Linux ARM64 (`linux-arm64`)
- Windows x64 (`win32-x64`)

macOS Intel is not supported (no prebuilt build is published).

## Environment overrides

- `OCTOS_SKIP_DOWNLOAD=1` — skip the postinstall download (offline / CI).
- `OCTOS_BUNDLE_URL=<url>` — install from a specific bundle URL (`file://` works).
- `HTTPS_PROXY` — honored when downloading.

## Alternatives

```bash
# Homebrew
brew install octos-org/tap/octos

# Shell installer (sets up octos serve as a service)
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
```
