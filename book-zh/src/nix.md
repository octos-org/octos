# Nix

Octos 提供一流的 Nix Flake 支持，用于可重现构建、开发环境和系统级集成（NixOS 与 macOS / nix-darwin）。

## 支持的系统

- `x86_64-linux`
- `aarch64-linux`
- `aarch64-darwin`

## Flake 输出

```
.
├── packages.<system>
│   ├── default          → octos（最小版本）
│   ├── octos            → octos CLI（无频道功能）
│   ├── octos-minimal    → octos 的别名
│   └── octos-full       → octos（全部频道 + app-skills）
├── devShells.<system>
│   └── default          → Rust + Nix 工具链的开发环境
├── nixosModules.default → NixOS 模块（programs.octos）
├── darwinModules.default → nix-darwin 模块（programs.octos）
├── formatter.<system>   → nixfmt-tree
└── checks.<system>
    ├── darwin-module    → Darwin 模块求值测试
    └── nixos-module-vm  → NixOS 虚拟机测试（仅 Linux）
```

## 快速上手

### 不安装直接运行

```bash
nix run github:octos-org/octos#octos -- --version
nix run github:octos-org/octos#octos -- status
nix run github:octos-org/octos#octos-full -- chat --message "Hello"
```

### 构建软件包

```bash
# 最小构建（仅 CLI，无频道功能）
nix build .#octos

# 完整构建（全部频道 + app-skills）
nix build .#octos-full

# 默认软件包（等同于 octos）
nix build .
```

### 自定义功能集

你可以覆盖任何软件包的功能：

```bash
# 仅 Telegram + API
nix build .#octos --override-input features '["api" "telegram"]'

# 或者在你自己的 flake 中：
let
  myOctos = octos.packages.${system}.octos.override {
    features = [ "api" "telegram" "discord" ];
    enableAppSkills = true;
  };
in
```

## NixOS 模块

### 基本用法

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

### 启用频道和技能

```nix
programs.octos = {
  enable = true;
  channels = [ "telegram" "discord" ];
  enableAppSkills = true;
};
```

### 完整配置（含服务）

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

这将创建一个 `systemd` 服务（`octos-serve.service`），开机自动运行 `octos serve`。

## nix-darwin 模块

### 基本用法

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

这将创建一个由系统管理的 launchd 守护进程（`org.octos.serve`）。

## 模块选项参考

| 选项                                 | 类型         | 默认值             | 说明                                                      |
| ------------------------------------ | ------------ | ------------------ | --------------------------------------------------------- |
| `programs.octos.enable`              | bool         | `false`            | 启用 octos 模块                                           |
| `programs.octos.package`             | package      | `octos`            | 要使用的基础 octos 软件包                                 |
| `programs.octos.finalPackage`        | package      | （自动计算）       | 只读；应用覆盖后的最终软件包                              |
| `programs.octos.channels`            | enum 列表    | `null`             | 要启用的频道。`null` 保留软件包的默认功能                 |
| `programs.octos.enableAllChannels`   | bool         | `false`            | 启用所有支持的频道                                        |
| `programs.octos.enableAppSkills`     | bool 或 null | `null`             | 包含 app-skill 二进制文件。`null` 保留软件包默认值        |
| `programs.octos.enableExtraPackages` | bool         | `false`            | 安装 chromium、nodejs、ffmpeg、libreoffice、poppler-utils |
| `programs.octos.service.enable`      | bool         | `false`            | 将 `octos serve` 作为系统服务启用                         |
| `programs.octos.service.host`        | string       | `"127.0.0.1"`      | 面板绑定的主机地址                                        |
| `programs.octos.service.port`        | int          | `8080`             | 面板端口                                                  |
| `programs.octos.service.dataDir`     | string       | `"/var/lib/octos"` | 会话、记忆等数据的存储目录                                |
| `programs.octos.service.authToken`   | string       | （必填）           | 面板访问的认证令牌                                        |

## 贡献指南

提交 PR 前，必须运行 `nix build .#checks.x86_64-linux.formatting --print-build-logs` 来确保代码格式正确。

### 基本设计

Nix 集成遵循三个核心原则：

1. **与 Cargo 功能对等** — 每个 Cargo 功能标志都通过 Nix 包系统暴露。`cli.nix` 派生直接读取 `Cargo.toml`，并将功能映射到 `cargoBuildFlags`。

2. 模块采用**透明覆盖**策略：当未设置任何功能相关的选项（`channels`、`enableAllChannels`、`service.enable`、`enableAppSkills`）时，模块直接返回用户选择的 `package`，不做任何修改。这保证了构建缓存的复用，并确保 `octos-full` 无论是直接使用还是通过模块使用都保持完全一致。一旦设置了任何自定义选项，模块会使用计算出的功能集调用 `.override`。当启用频道或服务时，`api` 功能会自动添加。

3. **跨平台一致性** — NixOS 和 nix-darwin 模块共享同一个 `options.nix` 定义。平台差异（systemd vs launchd、tmpfiles vs activationScripts）仅存在于平台特定的模块文件中。

### 架构

```
flake.nix                          # 入口点 — 连接所有组件
├── nix/
│   ├── packages/
│   │   ├── default.nix            # 组合包（CLI + 可选 app-skills）
│   │   ├── cli.nix                # octos-cli 的 Rust 构建派生
│   │   ├── app-skills.nix         # app-skill 二进制文件的 Rust 构建派生
│   │   └── admin-dashboard.nix    # Web 面板的 npm 构建
│   ├── modules/
│   │   ├── options.nix            # 共享选项定义（两个平台通用）
│   │   ├── nixos.nix              # NixOS 专属配置（systemd、tmpfiles）
│   │   └── darwin.nix             # Darwin 专属配置（launchd、activationScripts）
│   └── tests/
│       ├── nixos.nix              # NixOS VM 集成测试
│       └── darwin.nix             # Darwin 模块求值测试
│   └── shell.nix                  # 开发环境
```

### 本地开发

```bash
nix develop          # 进入包含 Rust + Nix 工具链的开发环境
nix develop -c cargo build --workspace
nix develop -c cargo test --workspace
```

```bash
nix fmt          # 使用 nixfmt 格式化所有 nix 文件
nix fmt -- --check  # 检查格式化而不写入
```

### 测试

#### NixOS 虚拟机测试

```bash
nix build .#checks.x86_64-linux.nixos-module-vm --print-build-logs
```

运行一个完整的 NixOS 虚拟机，它会：

- 安装带有 Telegram + Discord 频道和 app-skills 的 octos
- 启动 `octos-serve` systemd 服务
- 验证服务在配置的端口上可访问
- 检查所有 app-skill 二进制文件是否在 PATH 中
- 验证数据目录权限

#### Darwin 模块测试

```bash
nix build .#checks.x86_64-linux.darwin-module --print-build-logs
```

求值 nix-darwin 模块并验证 launchd 守护进程 plist 是否正确生成。此测试也可以在 Linux 上运行（跨平台求值）。

### 注意事项

- cargoFeature 在 build 之前会排序 + 去重，确保派生哈希值确定性。

- **`doCheck = false`** — Rust 派生在构建时跳过 `cargo test`。测试通过开发环境中的 `cargo test --workspace` 单独运行。这避免了重复测试并保持构建速度。

- **`authToken` 必填** — 当 `service.enable = true` 时，模块不会生成默认令牌，你必须提供一个。

- **`channels` 与 `enableAllChannels` 的区别** — 设置 `channels = [ ]`（空列表）不同于 `channels = null`。空列表表示"不启用任何频道"，而 `null` 表示"使用软件包的默认值"。使用 `enableAllChannels = true` 启用所有频道。

- **`enableExtraPackages`** 安装系统级运行时依赖（chromium、nodejs 等）。这些是使用浏览器自动化、媒体处理或 Office 转换的技能所需的运行时依赖。默认不包含，以保持闭包大小可控。

- **npm lock 导入** — 管理面板使用 `importNpmLock` 实现可重现的 npm 依赖解析。如果 `package-lock.json` 发生变化，flake 会自动获取新的锁。

- **Workspace TOML 解析** — `cli.nix` 和 `app-skills.nix` 都使用 `builtins.fromTOML` 解析 `Cargo.toml`。这要求 TOML 格式正确 — 如果 `Cargo.toml` 有语法错误，flake 求值会在 Cargo 运行之前就失败。
