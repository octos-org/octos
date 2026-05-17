# Nix

Octos provides a first-class Nix flake for reproducible builds, development shells, and system-wide integration on NixOS and macOS (via nix-darwin).

## Supported Systems

- `x86_64-linux`
- `aarch64-linux`
- `aarch64-darwin`

## Flake Outputs

```
.
├── packages.<system>
│   ├── default          → octos (minimal)
│   ├── octos            → octos CLI (no features)
│   ├── octos-minimal    → alias for octos
│   └── octos-full       → octos with all channels + app-skills
├── devShells.<system>
│   └── default          → Rust + Nix tooling shell
├── nixosModules.default → NixOS module (programs.octos)
├── darwinModules.default → nix-darwin module (programs.octos)
├── formatter.<system>   → nixfmt-tree
└── checks.<system>
    ├── darwin-module    → Darwin module evaluation test
    └── nixos-module-vm  → NixOS VM test (Linux only)
```

## Quick Start

### Running Without Installing

```bash
nix run github:octos-org/octos#octos -- --version
nix run github:octos-org/octos#octos -- status
nix run github:octos-org/octos#octos-full -- chat --message "Hello"
```

### Building Packages

```bash
# Minimal build (CLI only, no channel features)
nix build .#octos

# Full build (all channels + app-skills)
nix build .#octos-full

# Default package (same as octos)
nix build .
```

### Custom Feature Sets

You can override features on any package:

```bash
# Only Telegram + API
nix build .#octos --override-input features '["api" "telegram"]'

# Or in your own flake:
let
  myOctos = octos.packages.${system}.octos.override {
    features = [ "api" "telegram" "discord" ];
    enableAppSkills = true;
  };
in
```

## NixOS Module

### Basic Usage

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    octos.url = "github:octos-org/octos";
  };
}
```

```nix
# octos.nix
{ inputs, ... }: {
  imports = [ inputs.octos.nixosModules.default ];

  programs.octos = {
    enable = true;
  };
}
```

### With Channels and Skills

```nix
programs.octos = {
  enable = true;
  channels = [ "telegram" "discord" ];
  enableAppSkills = true;
};
```

### Full Configuration with Service

```nix
programs.octos = {
  enable = true;
  enableAllChannels = true;
  enableAppSkills = true;
  enableExtraPackages = true;  # chromium, nodejs, ffmpeg, libreoffice, poppler-utils

  service = {
    enable = true;
    host = "127.0.0.1";
    port = 8080;
    dataDir = "/var/lib/octos";
    authToken = "your-secret-token";
  };
};
```

This creates a `systemd` service (`octos-serve.service`) that runs `octos serve` automatically on boot.

## nix-darwin Module

### Basic Usage

```nix
# darwin-configuration.nix
{ inputs, ... }: {
  imports = [ inputs.octos.darwinModules.default ];

  programs.octos = {
    enable = true;
    channels = [ "telegram" ];
    enableAppSkills = true;

    service = {
      enable = true;
      host = "127.0.0.1";
      port = 8080;
      dataDir = "/var/lib/octos";
      authToken = "your-secret-token";
    };
  };
}
```

This creates a launchd daemon (`org.octos.serve`) managed by the system.

## Module Options Reference

| Option                               | Type         | Default            | Description                                               |
| ------------------------------------ | ------------ | ------------------ | --------------------------------------------------------- |
| `programs.octos.enable`              | bool         | `false`            | Enable the octos module                                   |
| `programs.octos.package`             | package      | `octos`            | Base octos package to use                                 |
| `programs.octos.finalPackage`        | package      | (computed)         | Read-only; the resolved package after overrides           |
| `programs.octos.channels`            | list of enum | `null`             | Channels to enable. `null` preserves the package default  |
| `programs.octos.enableAllChannels`   | bool         | `false`            | Enable all supported channels                             |
| `programs.octos.enableAppSkills`     | bool or null | `null`             | Include app-skill binaries. `null` preserves package default |
| `programs.octos.enableExtraPackages` | bool         | `false`            | Install chromium, nodejs, ffmpeg, libreoffice, poppler-utils |
| `programs.octos.service.enable`      | bool         | `false`            | Enable `octos serve` as a system service                  |
| `programs.octos.service.host`        | string       | `"127.0.0.1"`      | Host to bind the dashboard to                             |
| `programs.octos.service.port`        | int          | `8080`             | Port for the dashboard                                    |
| `programs.octos.service.dataDir`     | string       | `"/var/lib/octos"` | Data directory for sessions, memory, etc.                 |
| `programs.octos.service.authToken`   | string       | (required)         | Auth token for dashboard access                           |

## Contributing

Before submitting a PR, you must run `nix build .#checks.x86_64-linux.formatting --print-build-logs` to ensure code formatting.

### Basic Design

The Nix integration follows three principles:

1. **Feature parity with Cargo** — Every Cargo feature flag is exposed through the Nix package system. The `cli.nix` derivation reads `Cargo.toml` directly and maps features to `cargoBuildFlags`.

2. The module uses a **transparent override** strategy: if no feature-related options are set (`channels`, `enableAllChannels`, `service.enable`, `enableAppSkills`), the module returns your `package` option as-is. This means pre-configured packages like `octos-full` remain bit-identical and reuse existing build caches. Once you set any customization, the module applies `.override` with the computed feature set. The `api` feature is auto-added whenever channels or the service is enabled.

3. **Cross-platform parity** — NixOS and nix-darwin modules share the same `options.nix` definition. Platform-specific differences (systemd vs launchd, tmpfiles vs activationScripts) live only in the platform-specific module files.

### Architecture

```
flake.nix                          # Entry point — wires everything together
├── nix/
│   ├── packages/
│   │   ├── default.nix            # Composite package (CLI + optional app-skills)
│   │   ├── cli.nix                # Rust build derivation for octos-cli
│   │   ├── app-skills.nix         # Rust build derivation for app-skill binaries
│   │   └── admin-dashboard.nix    # npm build for the web dashboard
│   ├── modules/
│   │   ├── options.nix            # Shared option definitions (both platforms)
│   │   ├── nixos.nix              # NixOS-specific config (systemd, tmpfiles)
│   │   └── darwin.nix             # Darwin-specific config (launchd, activationScripts)
│   └── tests/
│       ├── nixos.nix              # NixOS VM integration test
│       └── darwin.nix             # Darwin module evaluation test
│   └── shell.nix                  # Development shell
```

### Local Development

```bash
nix develop          # Enter the dev shell with Rust + Nix tooling
nix develop -c cargo build --workspace
nix develop -c cargo test --workspace
```

```bash
nix fmt          # Format all nix files with nixfmt
nix fmt -- --check  # Check formatting without writing
```

### Testing

#### NixOS VM Test

```bash
nix build .#checks.x86_64-linux.nixos-module-vm --print-build-logs
```

Runs a full NixOS VM that:

- Installs octos with Telegram + Discord channels and app-skills
- Starts the `octos-serve` systemd service
- Verifies the service responds on the configured port
- Checks that all app-skill binaries are on PATH
- Validates data directory permissions

#### Darwin Module Test

```bash
nix build .#checks.x86_64-linux.darwin-module --print-build-logs
```

Evaluates the nix-darwin module and verifies the launchd daemon plist is correctly generated. This test runs on Linux as well (cross-platform evaluation).

### Things to Watch Out For

- `cargoFeature` is sorted and deduplicated before the build, ensuring deterministic derivation hashes.

- **`doCheck = false`** — Rust derivations skip `cargo test` during build. Tests are run separately via `cargo test --workspace` in the dev shell. This avoids redundant test runs and keeps builds fast.

- **`authToken` is required** when `service.enable = true`. The module does not generate a default token — you must provide one.

- **`channels` vs `enableAllChannels`** — Setting `channels = [ ]` (empty list) is different from `channels = null`. An empty list means "no channels", while `null` means "use the package's default". Use `enableAllChannels = true` to enable all channels.

- **`enableExtraPackages`** installs system-wide dependencies (chromium, nodejs, etc.). These are runtime dependencies for skills that use browser automation, media processing, or Office conversion. They are NOT included by default to keep the closure size manageable.

- **npm lock import** — The admin dashboard uses `importNpmLock` for reproducible npm dependency resolution. If `package-lock.json` changes, the flake will automatically pick up the new lock.

- **Workspace TOML parsing** — Both `cli.nix` and `app-skills.nix` parse `Cargo.toml` with `builtins.fromTOML`. This requires the TOML to be valid — a broken `Cargo.toml` will cause flake evaluation to fail before Cargo even runs.
